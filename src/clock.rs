//! Дата и время, полученные от сотовой сети.
//!
//! Модуль намеренно обходится одним `core`: разбор ответа `AT+CCLK?` и
//! упаковка значения проверяются на хосте в `at-tests/` тем же файлом, что
//! уходит в прошивку.
//!
//! # Откуда берётся время
//!
//! Модем узнаёт его от сети по NITZ — оператор передаёт время при регистрации.
//! Чтобы SIM800L это принимал, нужен `AT+CLTS=1`, причём **настройка вступает
//! в силу только после перезапуска модуля**: включаем и сохраняем в профиль,
//! а реальное время появляется со следующего цикла.
//!
//! Пока сеть время не прислала, модем отвечает заводским `04/01/01,00:00:00`.
//! Такое значение считается недействительным — см. [`DateTime::is_plausible`].

use core::sync::atomic::{AtomicU32, Ordering};

/// Дата и время без часового пояса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct DateTime {
    /// Полный год, например 2026.
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl DateTime {
    /// Похоже ли значение на настоящее время из сети.
    ///
    /// До прихода NITZ модем отдаёт `04/01/01,00:00:00`; всё, что раньше 2020
    /// года, считаем заводской заглушкой.
    pub const fn is_plausible(&self) -> bool {
        self.year >= 2020
            && self.month >= 1
            && self.month <= 12
            && self.day >= 1
            && self.day <= 31
            && self.hour < 24
            && self.minute < 60
            && self.second < 60
    }

    /// Упаковать в 32 бита, чтобы хранить в атомике.
    ///
    /// Так модуль обходится одним `core`: мьютекс потребовал бы `embassy-sync`,
    /// и файл перестал бы собираться в хостовых тестах. Шире взять нельзя —
    /// 64-битных атомиков у Cortex-M33 нет.
    ///
    /// Поля укладываются ровно в 32 бита, если считать год от 2020-го:
    /// `6 + 4 + 5 + 5 + 6 + 6`. Отсюда предел 2083 год — для этого устройства
    /// с запасом. Значение вне диапазона даёт 0, то есть «времени нет».
    pub const fn pack(&self) -> u32 {
        if self.year < YEAR_BASE || self.year > YEAR_BASE + 63 {
            return 0;
        }
        ((self.year - YEAR_BASE) as u32) << 26
            | (self.month as u32) << 22
            | (self.day as u32) << 17
            | (self.hour as u32) << 12
            | (self.minute as u32) << 6
            | self.second as u32
    }

    pub const fn unpack(packed: u32) -> Self {
        Self {
            year: YEAR_BASE + (packed >> 26) as u16,
            month: (packed >> 22) as u8 & 0x0F,
            day: (packed >> 17) as u8 & 0x1F,
            hour: (packed >> 12) as u8 & 0x1F,
            minute: (packed >> 6) as u8 & 0x3F,
            second: packed as u8 & 0x3F,
        }
    }
}

/// Начало отсчёта лет в упакованном представлении.
const YEAR_BASE: u16 = 2020;

/// Последнее время, полученное от сети. 0 — ещё не получали.
static LAST: AtomicU32 = AtomicU32::new(0);

/// Запомнить время, пришедшее от модема.
pub fn store(now: DateTime) {
    LAST.store(now.pack(), Ordering::Relaxed);
}

/// Последнее известное время. `None`, если сеть его ещё не присылала.
pub fn last() -> Option<DateTime> {
    match LAST.load(Ordering::Relaxed) {
        0 => None,
        packed => Some(DateTime::unpack(packed)),
    }
}

/// Разобрать ответ `AT+CCLK?`.
///
/// Формат — `+CCLK: "yy/MM/dd,hh:mm:ss±zz"`, где `zz` — часовой пояс в
/// четвертях часа. Пояс отбрасываем: модем и так отдаёт местное время, а
/// показывать на экране смещение незачем.
///
/// Разбираем от кавычки, а не от начала строки: у некоторых прошивок между
/// префиксом и значением встречается лишний пробел.
pub fn parse_cclk(response: &str) -> Option<DateTime> {
    let quoted = response.split('"').nth(1)?;
    let (date, time) = quoted.split_once(',')?;

    let mut date = date.split('/');
    let year: u16 = date.next()?.parse().ok()?;
    let month: u8 = date.next()?.parse().ok()?;
    let day: u8 = date.next()?.parse().ok()?;
    if date.next().is_some() {
        return None;
    }

    // Часовой пояс отрезаем вместе со знаком.
    let time = time
        .split_once(['+', '-'])
        .map(|(before, _)| before)
        .unwrap_or(time);

    let mut time = time.split(':');
    let hour: u8 = time.next()?.parse().ok()?;
    let minute: u8 = time.next()?.parse().ok()?;
    let second: u8 = time.next()?.parse().ok()?;
    if time.next().is_some() {
        return None;
    }

    Some(DateTime {
        // Модем отдаёт две цифры года.
        year: 2000 + year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

/// Число делений шкалы сигнала (0..=5) по `<rssi>` из `+CSQ`.
///
/// `rssi` 0 соответствует −113 дБм, 31 — −52 дБм, 99 — «не измерено».
/// Границы выбраны так, чтобы шкала вела себя как на телефоне: одно деление
/// уже около −105 дБм, все пять — от −70 дБм и выше.
pub const fn signal_bars(rssi: u8) -> u8 {
    match rssi {
        99 => 0,      // не измерено
        0..=3 => 0,   // ≤ −107 дБм
        4..=7 => 1,   // ≤ −99 дБм
        8..=13 => 2,  // ≤ −87 дБм
        14..=18 => 3, // ≤ −77 дБм
        19..=24 => 4, // ≤ −65 дБм
        _ => 5,
    }
}

/// `<rssi>` в дБм. Для 99 («не измерено») отдаём 0.
pub const fn rssi_dbm(rssi: u8) -> i16 {
    if rssi >= 99 {
        0
    } else {
        -113 + 2 * rssi as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cclk_response() {
        let dt = parse_cclk("+CCLK: \"26/08/16,12:34:56+12\"").unwrap();
        assert_eq!(
            dt,
            DateTime {
                year: 2026,
                month: 8,
                day: 16,
                hour: 12,
                minute: 34,
                second: 56,
            }
        );
        assert!(dt.is_plausible());
    }

    #[test]
    fn handles_timezone_variants() {
        // Отрицательное смещение.
        let dt = parse_cclk("+CCLK: \"26/08/16,12:34:56-20\"").unwrap();
        assert_eq!((dt.hour, dt.minute, dt.second), (12, 34, 56));
        // Без смещения вовсе.
        let dt = parse_cclk("+CCLK: \"26/08/16,12:34:56\"").unwrap();
        assert_eq!(dt.second, 56);
        // Лишний пробел после префикса.
        assert!(parse_cclk("+CCLK:  \"26/08/16,12:34:56+12\"").is_some());
    }

    /// До прихода NITZ модем отдаёт заводскую дату — показывать её нельзя.
    #[test]
    fn factory_default_is_not_plausible() {
        let dt = parse_cclk("+CCLK: \"04/01/01,00:00:00+00\"").unwrap();
        assert_eq!(dt.year, 2004);
        assert!(!dt.is_plausible());
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "+CCLK: 26/08/16,12:34:56",        // без кавычек
            "+CCLK: \"26/08/16 12:34:56\"",    // без запятой
            "+CCLK: \"26/08,12:34:56\"",       // мало полей даты
            "+CCLK: \"26/08/16,12:34\"",       // мало полей времени
            "+CCLK: \"26/08/16/01,12:34:56\"", // лишнее поле даты
            "+CCLK: \"xx/08/16,12:34:56\"",    // не число
            "",
        ] {
            assert!(parse_cclk(bad).is_none(), "разобралось: {bad}");
        }
    }

    #[test]
    fn packing_round_trips() {
        // Границы диапазона каждого поля.
        for dt in [
            DateTime {
                year: 2026,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                second: 58,
            },
            DateTime {
                year: 2020,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 1,
            },
            DateTime {
                year: 2083,
                month: 12,
                day: 31,
                hour: 23,
                minute: 59,
                second: 59,
            },
        ] {
            assert_eq!(DateTime::unpack(dt.pack()), dt, "{dt:?}");
            assert_ne!(dt.pack(), 0, "ноль зарезервирован под «времени нет»");
        }

        // Вне диапазона — заводская дата и слишком далёкое будущее.
        for year in [2004u16, 2019, 2084] {
            let dt = DateTime {
                year,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            };
            assert_eq!(dt.pack(), 0, "год {year} должен считаться отсутствующим");
        }
    }

    #[test]
    fn signal_scale_is_monotonic_and_bounded() {
        let mut previous = 0;
        for rssi in 0..=31u8 {
            let bars = signal_bars(rssi);
            assert!(bars <= 5);
            assert!(bars >= previous, "шкала пошла вниз на rssi={rssi}");
            previous = bars;
        }
        assert_eq!(signal_bars(31), 5);
        assert_eq!(signal_bars(99), 0, "«не измерено» — пустая шкала");
        assert_eq!(rssi_dbm(20), -73);
        assert_eq!(rssi_dbm(99), 0);
    }
}
