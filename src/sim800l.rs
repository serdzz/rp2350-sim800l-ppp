//! Управление модулем SIM800L: питание, последовательность AT-инициализации,
//! вход и выход из PPP (data) режима.

use core::sync::atomic::{AtomicBool, Ordering};

use atat::Error as AtError;
use atat::asynch::AtatClient;
use embassy_futures::select::{Either, select};
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;

use crate::UrcSub;
use crate::config;
use crate::modem::*;

/// Настройки уже записаны в NV-память модуля в этом сеансе работы MCU.
///
/// `AT&W` расходует ресурс перезаписи, а цикл переподключения при просадке
/// питания повторяется каждые несколько секунд — запись на каждом витке
/// быстро израсходовала бы ресурс. Одного раза за загрузку контроллера
/// достаточно: после успешной записи модуль держит `ATE0` и `AT+IPR` сам,
/// в том числе после аварийного сброса.
static SETTINGS_SAVED: AtomicBool = AtomicBool::new(false);

/// Что могло пойти не так при подъёме канала.
// allow(dead_code): поле `At(AtError)` читается только через `{:?}` в логе,
// а derive(Debug) не считается использованием при анализе мёртвого кода.
#[allow(dead_code)]
#[derive(Debug)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub enum BringUpError {
    /// Модуль не отвечает на `AT`.
    NoResponse,
    /// SIM не готова (нет карты, нужен PIN).
    SimNotReady,
    /// Не зарегистрировались в сети за отведённое время.
    NotRegistered,
    /// Не прицепились к GPRS.
    NotAttached,
    /// Модем не отдал `CONNECT` на строку дозвона.
    DialFailed,
    /// Модем не принял `AT+CMUX`.
    CmuxRefused,
    /// Модуль перезагрузился посреди инициализации (пришёл `RDY`).
    ///
    /// Почти всегда означает провал питания: регистрация в сети идёт на полной
    /// мощности передатчика, и просадка сбрасывает модуль ровно в этот момент.
    ModemReset,
    /// Ошибка транспорта/протокола AT.
    At(AtError),
}

impl From<AtError> for BringUpError {
    fn from(e: AtError) -> Self {
        Self::At(e)
    }
}

/// Аппаратное включение модуля.
///
/// PWRKEY у SIM800L активен низким уровнем: удержание ~1.2 с переключает
/// питание. На платах с автостартом (PWRKEY подтянут к земле резистором)
/// эта функция безвредна — просто пропустите вызов, если PWRKEY не разведён.
pub async fn power_on(pwrkey: &mut Output<'_>) {
    info!("SIM800L: импульс PWRKEY");
    pwrkey.set_high(); // неактивное состояние (через транзистор — уровень инвертирован)
    Timer::after(Duration::from_millis(100)).await;
    pwrkey.set_low();
    Timer::after(Duration::from_millis(1200)).await;
    pwrkey.set_high();
    // Модулю нужно ~3 с до первого «RDY».
    Timer::after(Duration::from_secs(3)).await;
}

/// Полная инициализация: от «модуль отвечает» до `CONNECT`.
///
/// После успешного возврата UART находится в data-режиме и его нужно отдать
/// [`embassy_net_ppp::Runner::run`].
pub async fn bring_up<A: AtatClient>(
    client: &mut A,
    apn: &str,
    urc: &mut UrcSub,
) -> Result<(), BringUpError> {
    // 1. Синхронизация. Первые "AT" заодно запускают автоопределение скорости.
    sync(client).await?;

    // 2. Эхо выключаем — иначе дайджестер разбирает собственные же посылки.
    client.send_retry(&DisableEcho).await?;
    // Ошибки в текстовом виде: намного понятнее в логе.
    let _ = client.send(&SetVerboseErrors { n: 2 }).await;
    // Фиксируем скорость: в PPP автобод не работает.
    let _ = client
        .send(&SetBaudRate {
            rate: config::UART_BAUDRATE,
        })
        .await;

    // 2a. Закрепляем скорость и эхо в профиле модуля, иначе после сброса по
    //     питанию он вернётся в автобод и начнёт отвечать на чужой скорости.
    if !SETTINGS_SAVED.load(Ordering::Relaxed) {
        match client.send(&SaveSettings).await {
            Ok(_) => {
                SETTINGS_SAVED.store(true, Ordering::Relaxed);
                info!("SIM800L: настройки закреплены в профиле (AT&W)");
            }
            Err(e) => warn!("SIM800L: AT&W не выполнен: {:?}", e),
        }
    }

    // 3. Радиотракт в полную функциональность.
    let _ = client.send(&SetFunctionality { fun: 1 }).await;

    // 4. SIM-карта.
    let pin = client
        .send_retry(&GetPinStatus)
        .await
        .map_err(|_| BringUpError::SimNotReady)?;
    info!("SIM800L: CPIN = {}", pin.code.as_str());
    if pin.code.as_str() != "READY" {
        return Err(BringUpError::SimNotReady);
    }

    // 4a. Кто эта SIM. Пишем до регистрации: если сеть откажет, по MCC/MNC
    //     будет видно, свой это оператор или роуминг.
    log_sim_identity(client).await;

    // 5. Ждём регистрации в GSM и GPRS.
    wait_registration(client, urc).await?;

    // 6. PDP-контекст под наш APN.
    info!("SIM800L: APN = {}", apn);
    client
        .send(&SetPdpContext {
            cid: 1,
            pdp_type: "IP",
            apn,
        })
        .await?;

    // 7. Attach к GPRS.
    attach_gprs(client).await?;

    // 8. Гасим встроенный TCP/IP-стек модема — иначе ATD*99# вернёт ERROR.
    //    Ошибку игнорируем: если стек и не поднимался, будет ERROR, и это норма.
    let _ = client.send(&ShutIpStack).await;

    // 9. Дозвон. Ответ `CONNECT` дайджестер atat считает успехом.
    info!("SIM800L: дозвон {}", config::DIAL_STRING);
    client
        .send(&DialPpp {
            number: config::DIAL_STRING,
        })
        .await
        .map_err(|e| {
            warn!("SIM800L: дозвон не удался: {:?}", e);
            BringUpError::DialFailed
        })?;

    info!("SIM800L: CONNECT — переходим в PPP");
    Ok(())
}

/// Перевести модем в мультиплексный режим.
///
/// После успеха обычный AT-обмен по этому UART заканчивается: порт надо
/// передать насосу из [`crate::cmux_transport`], а `atat` посадить на
/// логический канал. Вызывать в самом конце настройки — всё, что удобнее
/// сделать простыми AT-командами, должно быть сделано до.
// allow(dead_code): вызывается только на пути с мультиплексором.
#[allow(dead_code)]
pub async fn enter_cmux<A: AtatClient>(client: &mut A, n1: u16) -> Result<(), BringUpError> {
    info!("SIM800L: переходим в мультиплексный режим, N1 = {}", n1);
    client
        .send(&SetCmuxMode {
            mode: 0,       // basic option
            subset: 0,     // только UIH
            port_speed: 5, // 115200 бод по кодировке 27.007
            n1,
        })
        .await
        .map_err(|e| {
            warn!("SIM800L: AT+CMUX отклонён: {:?}", e);
            BringUpError::CmuxRefused
        })?;
    Ok(())
}

/// Дожидаемся ответа на `AT`. Модуль может ещё грузиться после подачи питания.
async fn sync<A: AtatClient>(client: &mut A) -> Result<(), BringUpError> {
    for attempt in 1..=20u32 {
        if client.send(&At).await.is_ok() {
            debug!("SIM800L: отвечает (попытка {})", attempt);
            return Ok(());
        }
        Timer::after(Duration::from_millis(500)).await;
    }
    Err(BringUpError::NoResponse)
}

/// Пишет в лог идентификаторы SIM.
///
/// Ошибки не фатальны: без IMSI регистрация всё равно возможна, это чисто
/// диагностика. ВНИМАНИЕ: IMSI и ICCID — персональные идентификаторы абонента,
/// не выкладывайте такой лог в публичный доступ как есть.
async fn log_sim_identity<A: AtatClient>(client: &mut A) {
    match client.send(&GetImsi).await {
        Ok(imsi) => {
            let s = imsi.text.as_str();
            info!(
                "SIM800L: IMSI {} — домашняя сеть MCC {} / MNC {}",
                s,
                imsi_mcc(s),
                imsi_mnc(s)
            );
        }
        Err(e) => warn!("SIM800L: IMSI не прочитан: {:?}", e),
    }

    match client.send(&GetIccid).await {
        Ok(iccid) => info!("SIM800L: ICCID {}", iccid.text.as_str()),
        Err(e) => warn!("SIM800L: ICCID не прочитан: {:?}", e),
    }
}

/// Расшифровка `<stat>` из `+CREG?` / `+CGREG?` (3GPP TS 27.007).
///
/// Именно этот код отвечает на вопрос «почему нет регистрации»: `2` — сеть не
/// находится (питание, антенна или 2G погашен), `3` — сеть видна, но не пускает
/// (вопрос к SIM или оператору).
fn stat_name(stat: Option<u8>) -> &'static str {
    match stat {
        Some(0) => "не ищет",
        Some(1) => "зарегистрирован (дома)",
        Some(2) => "ищет сеть",
        Some(3) => "ОТКАЗАНО",
        Some(4) => "неизвестно",
        Some(5) => "зарегистрирован (роуминг)",
        Some(_) => "нерасшифрованный код",
        None => "нет ответа",
    }
}

/// `Option<u8>` в число для лога: -1 = ответа не было.
///
/// `Option` нельзя отдать в формат напрямую: `log` требует `Display`, которого
/// у него нет, а `defmt` — свой `Format`. Через `i16` работают оба бэкенда.
fn stat_code(stat: Option<u8>) -> i16 {
    stat.map(i16::from).unwrap_or(-1)
}

/// `<rssi>` из `+CSQ` в дБм. 99 = «не измерено», отдаём 0.
fn rssi_dbm(rssi: u8) -> i16 {
    if rssi >= 99 {
        0
    } else {
        -113 + 2 * rssi as i16
    }
}

/// Признак того, что модуль пережил незапланированную перезагрузку.
///
/// `RDY` однозначен, но после провала питания приходит битым и до URC-канала
/// доезжает примерно в одном случае из пяти — на живом стенде это 3 пойманных
/// сброса из 14. `Call Ready` / `SMS Ready` ловятся почти всегда, но их же
/// модуль шлёт и при нормальном старте, уже после того как мы вычистили
/// очередь URC. Цена — один лишний цикл инициализации (~13 с) при первом
/// подключении; дальше модуль их не повторяет. Отключается в [`config`].
///
/// `NORMAL POWER DOWN` при штатной работе не приходит вовсе, поэтому включён
/// всегда.
fn is_reset_urc(urc: &Urc) -> bool {
    match urc {
        Urc::Ready | Urc::PowerDown => true,
        Urc::CallReady | Urc::SmsReady => config::TREAT_READY_URCS_AS_RESET,
        Urc::PdpDeactivated => false,
    }
}

/// Ждёт признака того, что модуль загрузился заново.
async fn wait_for_modem_reset(urc: &mut UrcSub) {
    loop {
        if is_reset_urc(&urc.next_message_pure().await) {
            return;
        }
    }
}

async fn wait_registration<A: AtatClient>(
    client: &mut A,
    urc: &mut UrcSub,
) -> Result<(), BringUpError> {
    // Выбрасываем URC, накопившиеся за время включения модуля: `RDY` от штатной
    // загрузки не должен выглядеть как перезагрузка посреди работы.
    while urc.try_next_message_pure().is_some() {}

    // Регистрация — самая долгая фаза (минуты). Если модуль под нами уйдёт в
    // ребут, продолжать опрос бессмысленно: выходим сразу, а не через таймаут.
    match select(poll_registration(client), wait_for_modem_reset(urc)).await {
        Either::First(result) => result,
        Either::Second(()) => Err(BringUpError::ModemReset),
    }
}

async fn poll_registration<A: AtatClient>(client: &mut A) -> Result<(), BringUpError> {
    for attempt in 0..config::REGISTRATION_ATTEMPTS {
        let csq = client.send(&GetSignalQuality).await;
        let gsm = client.send(&GetNetworkRegistration).await;
        let gprs = client.send(&GetGprsRegistration).await;

        let gsm_stat = gsm.as_ref().ok().map(|r| r.stat);
        let gprs_stat = gprs.as_ref().ok().map(|r| r.stat);
        let rssi = csq.as_ref().map(|c| c.rssi).unwrap_or(99);

        debug!(
            "SIM800L: CSQ {} ({} дБм) | CREG {} — {} | CGREG {} — {}",
            rssi,
            rssi_dbm(rssi),
            stat_code(gsm_stat),
            stat_name(gsm_stat),
            stat_code(gprs_stat),
            stat_name(gprs_stat),
        );

        if matches!(gsm_stat, Some(1) | Some(5)) && matches!(gprs_stat, Some(1) | Some(5)) {
            info!("SIM800L: зарегистрированы (попытка {})", attempt);
            return Ok(());
        }

        // Текущий оператор — редко, команда не бесплатная по времени.
        if attempt % 5 == 0 {
            match client.send(&GetOperator).await {
                Ok(op) => debug!("SIM800L: COPS? -> {}", op.text.as_str()),
                Err(e) => debug!("SIM800L: COPS? не ответил: {:?}", e),
            }
        }

        Timer::after(Duration::from_secs(2)).await;
    }

    // Регистрации не случилось — сканируем эфир, чтобы понять, есть ли тут 2G.
    if config::SCAN_OPERATORS_ON_FAILURE {
        warn!("SIM800L: регистрации нет; сканирую сети (AT+COPS=?, до 3 мин)…");
        match client.send(&ScanOperators).await {
            Ok(list) if list.text.is_empty() => {
                warn!("SIM800L: сканирование вернуло пустой список — 2G не виден")
            }
            Ok(list) => warn!("SIM800L: видимые сети: {}", list.text.as_str()),
            Err(e) => warn!("SIM800L: сканирование не удалось: {:?}", e),
        }
    }

    Err(BringUpError::NotRegistered)
}

async fn attach_gprs<A: AtatClient>(client: &mut A) -> Result<(), BringUpError> {
    // Уже прицеплены?
    if let Ok(state) = client.send(&GetGprsAttach).await
        && state.state == 1
    {
        return Ok(());
    }

    for _ in 0..3 {
        if client.send(&SetGprsAttach { state: 1 }).await.is_ok() {
            info!("SIM800L: GPRS attached");
            return Ok(());
        }
        Timer::after(Duration::from_secs(2)).await;
    }
    Err(BringUpError::NotAttached)
}

/// Возврат из data-режима в командный: `+++` с охранными паузами, затем `ATH`.
///
/// Пишем напрямую в UART, минуя `atat`: на этом этапе ingress-задача уже не
/// крутится, а ответ `OK` нам не важен — важно, чтобы модем вышел из PPP.
pub async fn escape_data_mode<W: Write>(writer: &mut W) {
    // Охранная пауза до и после — иначе модем примет "+++" за данные.
    Timer::after(Duration::from_millis(1100)).await;
    let _ = writer.write_all(b"+++").await;
    let _ = writer.flush().await;
    Timer::after(Duration::from_millis(1100)).await;
    let _ = writer.write_all(b"ATH\r").await;
    let _ = writer.flush().await;
    Timer::after(Duration::from_millis(500)).await;
}
