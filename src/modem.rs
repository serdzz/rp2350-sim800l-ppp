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
}
