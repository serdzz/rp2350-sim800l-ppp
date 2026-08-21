//! Счётчики отказов связи: сколько раз она разваливалась с момента включения.
//!
//! Модуль обходится одним `core` — по тому же принципу, что [`crate::coin`]:
//! логика проверяется на хосте тем же файлом, что уходит в прошивку.
//!
//! # Зачем это на экране
//!
//! Разница между «связь есть» и «связь есть прямо сейчас, а до этого пропадала
//! двадцать раз» видна только по счётчику. Мгновенное состояние её скрывает:
//! канал, разваливающийся каждые полминуты, выглядит на экране точно так же,
//! как исправный, если посмотреть в удачный момент.
//!
//! Считаем два уровня по отдельности, потому что ломаются они порознь:
//!
//! * **канал** — PPP развалился, придётся поднимать заново вместе с модемом;
//! * **MQTT** — канал цел, а сессия с брокером потеряна.
//!
//! Счётчики обнуляются при перезагрузке — в том числе по сторожевому таймеру.
//! Это не недосмотр: они отвечают на вопрос «как ведёт себя связь в текущем
//! сеансе», а не «сколько всего было отказов за всё время».

use core::sync::atomic::{AtomicU32, Ordering};

static LINK_DROPS: AtomicU32 = AtomicU32::new(0);
static MQTT_DROPS: AtomicU32 = AtomicU32::new(0);

/// Насыщение вместо переполнения.
///
/// Переполнившийся счётчик показал бы ноль, то есть «всё хорошо», — ровно
/// противоположное правде. Лучше застрять на пределе.
fn bump(counter: &AtomicU32) -> u32 {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_add(1))
        })
        .unwrap_or(0)
        .saturating_add(1)
}

/// Развалился поднятый канал: PPP-сессия завершилась.
pub fn link_dropped() -> u32 {
    bump(&LINK_DROPS)
}

/// Потеряна установленная сессия с брокером.
pub fn mqtt_dropped() -> u32 {
    bump(&MQTT_DROPS)
}

pub fn link_drops() -> u32 {
    LINK_DROPS.load(Ordering::Relaxed)
}

pub fn mqtt_drops() -> u32 {
    MQTT_DROPS.load(Ordering::Relaxed)
}

/// Счётчик, годный для тесной строки экрана: двузначный, дальше «>99».
///
/// Обрезка по ширине показала бы часть числа — «100» превратилось бы в «10»,
/// то есть в неправду. Явная отметка «больше» честнее: точное значение всё
/// равно есть в логе, а на экране важен порядок величины.
///
/// Тип, а не строка: буфер потребовал бы `heapless`, а этот модуль намеренно
/// обходится одним `core` — иначе его не подключить к тестам на хосте.
pub struct Short(u32);

impl core::fmt::Display for Short {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0 > 99 {
            f.write_str(">99")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

pub fn short(count: u32) -> Short {
    Short(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_independent_and_saturate() {
        assert_eq!((link_drops(), mqtt_drops()), (0, 0));

        assert_eq!(link_dropped(), 1);
        assert_eq!(link_dropped(), 2);
        assert_eq!(mqtt_dropped(), 1);

        // Уровни считаются порознь: они и ломаются порознь.
        assert_eq!((link_drops(), mqtt_drops()), (2, 1));

        // Переполнение не должно превращаться в «отказов не было».
        LINK_DROPS.store(u32::MAX, Ordering::Relaxed);
        assert_eq!(link_dropped(), u32::MAX);
        assert_eq!(link_drops(), u32::MAX);
    }

    /// В строку экрана влезают два знака. Обрезка показала бы «100» как «10»,
    /// поэтому дальше — явная отметка.
    #[test]
    fn short_never_lies_about_magnitude() {
        assert_eq!(format!("{}", short(0)), "0");
        assert_eq!(format!("{}", short(7)), "7");
        assert_eq!(format!("{}", short(99)), "99");
        assert_eq!(format!("{}", short(100)), ">99");
        assert_eq!(format!("{}", short(u32::MAX)), ">99");
        // Что бы ни пришло, шире трёх знаков не станет.
        for n in [0, 9, 10, 99, 100, 1234, u32::MAX] {
            assert!(format!("{}", short(n)).len() <= 3, "{n}");
        }
    }
}
