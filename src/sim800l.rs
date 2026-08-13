//! Управление модулем SIM800L: питание, последовательность AT-инициализации,
//! вход и выход из PPP (data) режима.

use atat::asynch::AtatClient;
use atat::Error as AtError;
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Timer};
use embedded_io_async::Write;

use crate::config;
use crate::modem::*;

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
pub async fn bring_up<A: AtatClient>(client: &mut A, apn: &str) -> Result<(), BringUpError> {
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

    // 5. Ждём регистрации в GSM и GPRS.
    wait_registration(client).await?;

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

async fn wait_registration<A: AtatClient>(client: &mut A) -> Result<(), BringUpError> {
    for attempt in 0..config::REGISTRATION_ATTEMPTS {
        if let Ok(csq) = client.send(&GetSignalQuality).await {
            debug!("SIM800L: CSQ rssi={} ber={}", csq.rssi, csq.ber);
        }

        let gsm = client.send(&GetNetworkRegistration).await;
        let gprs = client.send(&GetGprsRegistration).await;

        let gsm_ok = gsm.as_ref().map(|r| r.is_registered()).unwrap_or(false);
        let gprs_ok = gprs.as_ref().map(|r| r.is_registered()).unwrap_or(false);

        if gsm_ok && gprs_ok {
            info!("SIM800L: зарегистрированы (попытка {})", attempt);
            return Ok(());
        }

        debug!("SIM800L: регистрация gsm={} gprs={}", gsm_ok, gprs_ok);
        Timer::after(Duration::from_secs(2)).await;
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
