//! Пересчёт напряжения одноэлементного Li-Po в проценты заряда.
//!
//! Модуль намеренно не зависит от embassy и HAL — за счёт этого он
//! подключается в `at-tests/` через `#[path]` и проверяется на хосте.

/// Разрядная кривая одноэлементного Li-Po при небольшом токе, мВ → %.
///
/// Под нагрузкой напряжение просаживается, и оценка занижается. Для SIM800L
/// это особенно заметно: во время передачи модуль берёт до 2 А импульсами,
/// поэтому замер имеет смысл делать между сеансами связи, а не во время них.
const CURVE: [(u16, u8); 11] = [
    (4200, 100),
    (4100, 90),
    (4000, 80),
    (3930, 70),
    (3870, 60),
    (3800, 50),
    (3750, 40),
    (3700, 30),
    (3650, 20),
    (3550, 10),
    (3400, 0),
];

/// Оценка заряда по напряжению с линейной интерполяцией между точками кривой.
pub fn percent(mv: u16) -> u8 {
    let (top_mv, top_pct) = CURVE[0];
    if mv >= top_mv {
        return top_pct;
    }
    let (bottom_mv, bottom_pct) = CURVE[CURVE.len() - 1];
    if mv <= bottom_mv {
        return bottom_pct;
    }

    for pair in CURVE.windows(2) {
        let (hi_mv, hi_pct) = pair[0];
        let (lo_mv, lo_pct) = pair[1];
        if mv <= hi_mv && mv >= lo_mv {
            let span = (hi_mv - lo_mv) as u32;
            let above = (mv - lo_mv) as u32;
            return (lo_pct as u32 + above * (hi_pct - lo_pct) as u32 / span) as u8;
        }
    }
    bottom_pct
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_outside_the_curve() {
        assert_eq!(percent(4300), 100);
        assert_eq!(percent(4200), 100);
        assert_eq!(percent(3400), 0);
        assert_eq!(percent(3000), 0);
    }

    #[test]
    fn hits_curve_points_exactly() {
        for (mv, pct) in CURVE {
            assert_eq!(percent(mv), pct, "точка кривой {mv} мВ");
        }
    }

    #[test]
    fn interpolates_between_points() {
        // Ровно посередине между 3800/50 % и 3750/40 %
        assert_eq!(percent(3775), 45);
        // Между 4200/100 % и 4100/90 %
        assert_eq!(percent(4150), 95);
    }

    #[test]
    fn is_monotonic() {
        let mut prev = 0;
        for mv in 3300..=4300 {
            let p = percent(mv);
            assert!(p >= prev, "не монотонно на {mv} мВ: {p} < {prev}");
            prev = p;
        }
    }
}
