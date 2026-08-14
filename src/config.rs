//! Всё, что нужно править под свою плату / оператора — собрано здесь.

/// APN оператора. Примеры: "internet" (МТС/Билайн), "internet.tele2.ru",
/// "internet.beeline.ru", "iot.1nce.net".
pub const APN: &str = "internet";

/// Логин/пароль PAP для PPP. У большинства российских операторов пустые;
/// у Билайна исторически "beeline"/"beeline", у МТС "mts"/"mts".
pub const PPP_USERNAME: &[u8] = b"";
pub const PPP_PASSWORD: &[u8] = b"";

/// Строка дозвона в PPP-режим. `*99***1#` использует PDP-контекст №1,
/// который мы настраиваем через AT+CGDCONT. Некоторые прошивки понимают
/// только короткое `*99#`.
pub const DIAL_STRING: &str = "*99***1#";

/// Скорость UART модема. SIM800L автоопределяет скорость по первым "AT",
/// но для PPP её нужно зафиксировать — см. `modem::SetBaudRate`.
pub const UART_BAUDRATE: u32 = 115_200;

/// Сколько ждать регистрации в сети (шаг опроса — 2 с).
pub const REGISTRATION_ATTEMPTS: u32 = 60;

/// Запускать `AT+COPS=?` (поиск всех видимых сетей), если регистрация не
/// удалась. Команда занимает до нескольких минут и удлиняет цикл повтора,
/// зато прямо отвечает, виден ли вообще 2G. Для отладки — да, в поле — нет.
pub const SCAN_OPERATORS_ON_FAILURE: bool = true;

/// Пауза перед повторной попыткой после развала PPP-сессии.
pub const RECONNECT_DELAY_SECS: u64 = 10;

/// Хост для демо-запроса после подъёма IP.
pub const DEMO_HOST: &str = "example.com";
pub const DEMO_PORT: u16 = 80;
