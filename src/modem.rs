//! Описание AT-команд SIM800L для крейта `atat`.
//!
//! ВАЖНО про типы: `atat` тянет `heapless` 0.8, а `embassy-net` — 0.9.
//! Все поля команд/ответов обязаны использовать `atat::heapless`, иначе
//! производный `Deserialize` не сойдётся по типам.
//!
//! ВАЖНО про URC: дайджестер `atat` проверяет URC-теги ДО разбора ответа на
//! команду. Поэтому в [`Urc`] нельзя объявлять префиксы, которые приходят как
//! ответ на наши же запросы (`+CREG`, `+CSQ`, `+CPIN`, ...) — иначе ответ
//! уедет в URC-канал, а `send()` отвалится по таймауту.

#![allow(dead_code)]

use atat::atat_derive::{AtatCmd, AtatResp, AtatUrc};
use atat::heapless::String;

// ---------------------------------------------------------------------------
// Ответы
// ---------------------------------------------------------------------------

/// Команда отвечает только кодом результата (`OK` / `CONNECT` / `ERROR`).
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct NoResponse;

/// `+CPIN: READY`
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct PinStatus {
    #[at_arg(position = 0)]
    pub code: String<16>,
}

/// `+CSQ: <rssi>,<ber>`
///
/// `rssi`: 0 = -115 dBm, 31 = -52 dBm, 99 = неизвестно.
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct SignalQuality {
    #[at_arg(position = 0)]
    pub rssi: u8,
    #[at_arg(position = 1)]
    pub ber: u8,
}

/// `+CREG: <n>,<stat>` и `+CGREG: <n>,<stat>`.
///
/// `stat`: 1 = home network, 5 = roaming — оба означают «зарегистрированы».
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct RegistrationStatus {
    #[at_arg(position = 0)]
    pub n: u8,
    #[at_arg(position = 1)]
    pub stat: u8,
}

impl RegistrationStatus {
    pub fn is_registered(&self) -> bool {
        matches!(self.stat, 1 | 5)
    }
}

/// `+CGATT: <state>`
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct GprsAttachState {
    #[at_arg(position = 0)]
    pub state: u8,
}

/// `AT+CGMR` / `AT+GSN` — свободная строка.
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct InfoText {
    #[at_arg(position = 0)]
    pub text: String<64>,
}

/// Тело ответа как есть, без разбора на поля.
///
/// Нужен там, где формат плавает. Пример — `AT+COPS?`: незарегистрированный
/// модуль отвечает `+COPS: 0` (одно поле), зарегистрированный —
/// `+COPS: 0,0,"Operator"` (три). Структура с фиксированным числом полей на
/// первом варианте развалилась бы с `Error::Parse`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct RawLine {
    pub text: String<384>,
}

impl atat::AtatResp for RawLine {}

/// Копирует тело ответа, обрезая по вместимости буфера.
///
/// Обрезка — сознательный выбор: список сетей от `AT+COPS=?` бывает длиннее
/// любого разумного буфера, а для диагностики хватает начала.
pub fn parse_raw_line(resp: &[u8]) -> Result<RawLine, core::str::Utf8Error> {
    let s = core::str::from_utf8(resp)?;
    let mut text = String::new();
    for c in s.chars() {
        if text.push(c).is_err() {
            break;
        }
    }
    Ok(RawLine { text })
}

// ---------------------------------------------------------------------------
// Команды
// ---------------------------------------------------------------------------

/// `AT` — проверка связи и автоопределение скорости.
#[derive(Clone, AtatCmd)]
#[at_cmd("", NoResponse, timeout_ms = 1000, attempts = 3)]
pub struct At;

/// `ATE0` — выключить эхо (иначе дайджестер жуёт лишние байты).
#[derive(Clone, AtatCmd)]
#[at_cmd("E0", NoResponse, timeout_ms = 1000, attempts = 3)]
pub struct DisableEcho;

/// `AT+CMEE=2` — текстовые коды ошибок вместо числовых.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CMEE", NoResponse, timeout_ms = 1000)]
pub struct SetVerboseErrors {
    #[at_arg(position = 0)]
    pub n: u8,
}

/// `AT+IPR=<rate>` — зафиксировать скорость UART (для PPP автобод не годится).
#[derive(Clone, AtatCmd)]
#[at_cmd("+IPR", NoResponse, timeout_ms = 1000)]
pub struct SetBaudRate {
    #[at_arg(position = 0)]
    pub rate: u32,
}

/// `AT+CMUX=<mode>,<subset>,<port_speed>,<N1>` — перевести линию в
/// мультиплексный режим 3GPP TS 27.010.
///
/// После `OK` обычный AT-обмен по этому UART заканчивается: дальше всё, включая
/// сами AT-команды, ходит кадрами — см. [`crate::cmux`].
///
/// `mode` 0 — basic option, `subset` 0 — только UIH, `port_speed` 5 — 115200
/// бод (кодировка из 3GPP TS 27.007), `n1` — максимальный размер поля данных.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CMUX", NoResponse, timeout_ms = 5_000)]
pub struct SetCmuxMode {
    #[at_arg(position = 0)]
    pub mode: u8,
    #[at_arg(position = 1)]
    pub subset: u8,
    #[at_arg(position = 2)]
    pub port_speed: u8,
    #[at_arg(position = 3)]
    pub n1: u16,
}

/// `AT&W` — сохранить текущие настройки в профиль модуля.
///
/// Без этого `ATE0` и `AT+IPR` живут только до следующего сброса: при провале
/// питания SIM800L возвращается к автоопределению скорости, и его ответы
/// приходят на чужой скорости — в логе это видно как `Framing error`.
///
/// Пишет в энергонезависимую память с ограниченным ресурсом записи, поэтому
/// вызывать в каждом цикле переподключения нельзя.
#[derive(Clone, AtatCmd)]
#[at_cmd("&W", NoResponse, timeout_ms = 5_000)]
pub struct SaveSettings;

/// `AT+CFUN=<fun>` — 1 = полная функциональность, 0 = минимальная, 4 = airplane.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CFUN", NoResponse, timeout_ms = 15_000)]
pub struct SetFunctionality {
    #[at_arg(position = 0)]
    pub fun: u8,
}

/// `AT+CPIN?`
#[derive(Clone, AtatCmd)]
#[at_cmd("+CPIN?", PinStatus, timeout_ms = 5_000, attempts = 3)]
pub struct GetPinStatus;

/// `AT+CSQ`
#[derive(Clone, AtatCmd)]
#[at_cmd("+CSQ", SignalQuality, timeout_ms = 2_000)]
pub struct GetSignalQuality;

/// `AT+CREG?` — регистрация в GSM.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CREG?", RegistrationStatus, timeout_ms = 2_000)]
pub struct GetNetworkRegistration;

/// `AT+CGREG?` — регистрация в GPRS.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CGREG?", RegistrationStatus, timeout_ms = 2_000)]
pub struct GetGprsRegistration;

/// `AT+CIMI` — IMSI абонента.
///
/// Ответ приходит голым числом, **без** префикса `+CIMI:`, поэтому берём его
/// как есть. По первым цифрам видно домашнюю сеть — а значит, свой это
/// оператор отказывает в регистрации или чужой в роуминге.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CIMI", RawLine, parse = parse_raw_line, timeout_ms = 5_000)]
pub struct GetImsi;

/// `AT+CCID` — ICCID, серийный номер самой SIM-карты. Тоже без префикса.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CCID", RawLine, parse = parse_raw_line, timeout_ms = 5_000)]
pub struct GetIccid;

/// MCC — первые три цифры IMSI (код страны). `"???"`, если строка короче.
pub fn imsi_mcc(imsi: &str) -> &str {
    imsi.get(..3).unwrap_or("???")
}

/// MNC — код оператора, следующий за MCC.
///
/// Длина MNC не фиксирована: в Европе он двузначный, в Северной Америке
/// (MCC 3xx) и ещё нескольких планах — трёхзначный. Разбираем по MCC, а для
/// неизвестного формата возвращаем две цифры как наиболее частый случай.
pub fn imsi_mnc(imsi: &str) -> &str {
    let three_digit_mnc = matches!(
        imsi_mcc(imsi),
        "310" | "311" | "312" | "313" | "316" | "302"
    );
    let end = if three_digit_mnc { 6 } else { 5 };
    imsi.get(3..end).unwrap_or("??")
}

/// `+CMTI: "<storage>",<index>` — пришло новое SMS.
#[derive(Clone, Debug, AtatResp)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct NewMessageIndex {
    #[at_arg(position = 0)]
    pub storage: String<8>,
    #[at_arg(position = 1)]
    pub index: u32,
}

/// `AT+CMGF=<mode>` — 1 = текстовый режим SMS вместо PDU.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CMGF", NoResponse, timeout_ms = 5_000)]
pub struct SetSmsTextMode {
    #[at_arg(position = 0)]
    pub mode: u8,
}

/// `AT+CNMI=<mode>,<mt>,<bm>,<ds>,<bfr>` — как извещать о входящих SMS.
///
/// Нас интересует `2,1,0,0,0`: модем сохраняет сообщение и присылает `+CMTI`
/// с индексом. Вариант `2,2,...`, при котором текст приходит прямо в URC,
/// не годится: тело идёт второй строкой, а разборщик URC в `atat` читает
/// только до первого перевода строки и тело потеряет.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CNMI", NoResponse, timeout_ms = 5_000)]
pub struct SetSmsIndication {
    #[at_arg(position = 0)]
    pub mode: u8,
    #[at_arg(position = 1)]
    pub mt: u8,
    #[at_arg(position = 2)]
    pub bm: u8,
    #[at_arg(position = 3)]
    pub ds: u8,
    #[at_arg(position = 4)]
    pub bfr: u8,
}

/// `AT+CLTS=<mode>` — принимать время от сети (NITZ).
///
/// Настройка вступает в силу **только после перезапуска модуля**, поэтому
/// её имеет смысл сохранять в профиль через `AT&W`: реальное время появится
/// со следующего включения.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CLTS", NoResponse, timeout_ms = 5_000)]
pub struct SetNetworkTime {
    #[at_arg(position = 0)]
    pub mode: u8,
}

/// `AT+CCLK?` — часы модуля.
///
/// Ответ `+CCLK: "yy/MM/dd,hh:mm:ss±zz"` разбирается в [`crate::clock`], а не
/// здесь: разбор чистый и проверяется на хосте.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CCLK?", RawLine, parse = parse_raw_line, timeout_ms = 5_000)]
pub struct GetClock;

/// `AT+CPMS?` — занятость памяти под SMS.
///
/// Ответ вида `+CPMS: "SM",3,30,...`: использовано и всего, по три поля на
/// каждое из трёх хранилищ. Если память заполнена, модем перестаёт принимать
/// сообщения и `+CMTI` не приходит вовсе — по одной этой строке видно, в этом
/// дело или нет.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CPMS?", RawLine, parse = parse_raw_line, timeout_ms = 5_000)]
pub struct GetSmsStorage;

/// `AT+CMGR=<index>` — прочитать сообщение из памяти.
///
/// Ответ — две строки: заголовок `+CMGR: "REC UNREAD","<номер>",...` и текст.
/// Разбираем как есть: формат заголовка у операторов гуляет, а нам нужен
/// прежде всего текст.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CMGR", RawLine, parse = parse_raw_line, timeout_ms = 10_000)]
pub struct ReadSms {
    #[at_arg(position = 0)]
    pub index: u32,
}

/// `AT+CMGD=<index>` — удалить сообщение.
///
/// Без удаления память SIM забьётся, и новые SMS перестанут приходить.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CMGD", NoResponse, timeout_ms = 25_000)]
pub struct DeleteSms {
    #[at_arg(position = 0)]
    pub index: u32,
}

/// `AT+COPS?` — на какой сети мы сейчас.
///
/// Ответ разбирается «как есть»: у незарегистрированного модуля полей меньше.
#[derive(Clone, AtatCmd)]
#[at_cmd("+COPS?", RawLine, parse = parse_raw_line, timeout_ms = 10_000)]
pub struct GetOperator;

/// `AT+COPS=?` — поиск всех видимых сетей.
///
/// Тяжёлая команда: модуль сканирует эфир и молчит до минуты, иногда дольше.
/// Зато это единственный прямой ответ на вопрос «а 2G тут вообще есть».
#[derive(Clone, AtatCmd)]
#[at_cmd("+COPS=?", RawLine, parse = parse_raw_line, timeout_ms = 180_000)]
pub struct ScanOperators;

/// `AT+CGDCONT=<cid>,"<pdp_type>","<apn>"` — определить PDP-контекст.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CGDCONT", NoResponse, timeout_ms = 5_000)]
pub struct SetPdpContext<'a> {
    #[at_arg(position = 0)]
    pub cid: u8,
    #[at_arg(position = 1, len = 8)]
    pub pdp_type: &'a str,
    #[at_arg(position = 2, len = 64)]
    pub apn: &'a str,
}

/// `AT+CGATT=<state>` — прицепиться к GPRS.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CGATT", NoResponse, timeout_ms = 75_000)]
pub struct SetGprsAttach {
    #[at_arg(position = 0)]
    pub state: u8,
}

/// `AT+CGATT?`
#[derive(Clone, AtatCmd)]
#[at_cmd("+CGATT?", GprsAttachState, timeout_ms = 5_000)]
pub struct GetGprsAttach;

/// `AT+CIPSHUT` — закрыть встроенный TCP/IP-стек модема.
///
/// Обязательно перед `ATD*99#`: пока «свой» стек SIM800L активен, модем
/// откажется уходить в PPP.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CIPSHUT", NoResponse, timeout_ms = 65_000)]
pub struct ShutIpStack;

/// Распознаёт `SHUT OK` — нестандартный успешный ответ на `AT+CIPSHUT`.
///
/// Дайджестер `atat` знает только `OK` и `CONNECT`. Строка `\r\nSHUT OK\r\n`
/// не содержит `\r\nOK\r\n` как подстроку, поэтому без этого парсера она
/// остаётся в приёмном буфере: команда отваливается по таймауту (а он у
/// `AT+CIPSHUT` — 65 с), после чего «хвост» приклеивается к ответу на
/// следующую команду и ломает её разбор с `Error::Parse`.
///
/// Тело ответа возвращаем пустым: важен только сам факт успеха, а `NoResponse`
/// ничего другого и не ждёт. Требуем совпадения строго с начала буфера, чтобы
/// не проглотить чужой ответ, стоящий перед токеном.
pub fn parse_shut_ok(buf: &[u8]) -> Result<(&[u8], usize), atat::digest::ParseError> {
    const TOKEN: &[u8] = b"\r\nSHUT OK\r\n";
    if buf.starts_with(TOKEN) {
        Ok((&buf[..0], TOKEN.len()))
    } else {
        Err(atat::digest::ParseError::NoMatch)
    }
}

/// `ATD*99***1#` — уйти в data-режим. Успешный ответ — `CONNECT`,
/// его дайджестер `atat` трактует как успех наравне с `OK`.
///
/// `quote_escape_strings = false` обязателен: по умолчанию `atat` заключает
/// строковые аргументы в кавычки, а `ATD"*99***1#"` модем не понимает.
#[derive(Clone, AtatCmd)]
#[at_cmd(
    "D",
    NoResponse,
    value_sep = false,
    quote_escape_strings = false,
    timeout_ms = 30_000
)]
pub struct DialPpp<'a> {
    #[at_arg(position = 0, len = 24)]
    pub number: &'a str,
}

/// `ATH` — положить трубку (после выхода из data-режима через `+++`).
#[derive(Clone, AtatCmd)]
#[at_cmd("H", NoResponse, value_sep = false, timeout_ms = 10_000)]
pub struct Hangup;

// ---------------------------------------------------------------------------
// URC (незапрошенные сообщения)
// ---------------------------------------------------------------------------

/// Только те теги, которые никогда не приходят ответом на наши команды.
#[derive(Clone, Debug, AtatUrc)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub enum Urc {
    /// Модуль загрузился.
    #[at_urc(b"RDY")]
    Ready,
    /// Голосовая часть готова.
    #[at_urc(b"Call Ready")]
    CallReady,
    /// SMS-часть готова.
    #[at_urc(b"SMS Ready")]
    SmsReady,
    /// Модуль выключается (обычно — просадка питания).
    #[at_urc(b"NORMAL POWER DOWN")]
    PowerDown,
    /// Сеть деактивировала PDP-контекст.
    #[at_urc(b"+PDP")]
    PdpDeactivated,
    /// Пришло SMS: модем сообщает, куда его положил.
    #[at_urc(b"+CMTI")]
    NewMessage(NewMessageIndex),
}

// --- Отправка SMS ---------------------------------------------------------
//
// Отправка — единственная команда во всём наборе, идущая в два приёма.
// Сначала `AT+CMGS="номер"`, модем отвечает приглашением `> `, и только потом
// уходит текст, завершённый Ctrl-Z. Между приглашением и текстом модем ждёт и
// ничего больше не принимает.

/// Максимальная длина текста SMS в одном сообщении.
///
/// 160 символов семибитного алфавита GSM. Длиннее — это уже составное
/// сообщение с заголовком UDH, которого текстовый режим не умеет.
pub const SMS_MAX_LEN: usize = 160;

/// Приглашение к вводу текста.
///
/// Штатный дайджестер про него не знает: `> ` не заканчивается переводом
/// строки и на обычный ответ не похож. Ставится через
/// `DefaultDigester::with_custom_prompt`, как `SHUT OK` — через
/// `with_custom_success`.
pub fn parse_sms_prompt(buf: &[u8]) -> Result<(u8, usize), atat::digest::ParseError> {
    let trimmed = buf.strip_prefix(b"\r\n").unwrap_or(buf);
    if trimmed.starts_with(b"> ") {
        Ok((b'>', buf.len() - trimmed.len() + 2))
    } else if b"> ".starts_with(trimmed) && !trimmed.is_empty() {
        Err(atat::digest::ParseError::Incomplete)
    } else {
        Err(atat::digest::ParseError::NoMatch)
    }
}

/// Годится ли номер для отправки.
///
/// Только цифры, от 5 до 20 знаков. Ведущий `+` не принимается намеренно: в
/// имени топика MQTT он запрещён — это подстановочный знак, — поэтому номер
/// всюду ходит цифрами, а плюс подставляется перед отправкой.
pub fn valid_phone(number: &str) -> bool {
    (5..=20).contains(&number.len()) && number.bytes().all(|b| b.is_ascii_digit())
}

/// Пригоден ли текст к отправке в семибитном алфавите GSM.
///
/// Кириллица требует `AT+CSCS="UCS2"` и перекодировки всего, включая номер, —
/// это отдельная работа, и делать её молча, отправляя абракадабру, хуже, чем
/// честно отказаться.
pub fn sms_text_is_sendable(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= SMS_MAX_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii() && b != CTRL_Z && b != ESC)
}

/// Завершает текст сообщения и запускает отправку.
const CTRL_Z: u8 = 0x1A;
/// Отменяет ввод. В тексте недопустим — иначе сообщение молча не уйдёт.
const ESC: u8 = 0x1B;

/// `AT+CMGS="<номер>"` — первая половина отправки.
///
/// Ответом служит приглашение `> `, а не `OK`: `Response::Prompt` доезжает до
/// клиента как успех с пустым телом, поэтому тип ответа здесь `NoResponse`.
#[derive(Clone, AtatCmd)]
#[at_cmd("+CMGS", NoResponse, timeout_ms = 10_000)]
pub struct SendSmsHeader<'a> {
    #[at_arg(position = 0, len = 24)]
    pub number: &'a str,
}

/// Текст сообщения с завершающим Ctrl-Z — вторая половина отправки.
///
/// Написана вручную, а не выведена макросом: у неё нет ни префикса `AT`, ни
/// завершающего возврата каретки, ни разделителя `=`. Всё, что выводит
/// макрос, здесь помешало бы.
#[derive(Clone)]
pub struct SmsBody<'a> {
    pub text: &'a str,
}

impl atat::AtatCmd for SmsBody<'_> {
    type Response = RawLine;

    const MAX_LEN: usize = SMS_MAX_LEN + 1;

    /// Отправка по сети занимает секунды, а на слабом сигнале — десятки.
    const MAX_TIMEOUT_MS: u32 = 60_000;

    fn write(&self, buf: &mut [u8]) -> usize {
        let text = self.text.as_bytes();
        let len = text.len().min(SMS_MAX_LEN);
        buf[..len].copy_from_slice(&text[..len]);
        buf[len] = CTRL_Z;
        len + 1
    }

    fn parse(&self, resp: Result<&[u8], atat::InternalError>) -> Result<RawLine, atat::Error> {
        let resp = resp.map_err(atat::Error::from)?;
        parse_raw_line(resp).map_err(|_| atat::Error::Parse)
    }
}
