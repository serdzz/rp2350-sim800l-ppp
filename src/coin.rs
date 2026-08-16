//! Учёт монет и разбор команд блокировки.
//!
//! Модуль обходится одним `core` — по тому же принципу, что [`crate::cmux`]:
//! логика проверяется на хосте тем же файлом, что уходит в прошивку, а всё,
//! что трогает выводы, лежит в [`crate::coin_io`].

use core::sync::atomic::{AtomicU32, Ordering};

/// Сколько выходных линий у монетоприёмника NRI G-13.6000.
pub const LINES: usize = 6;

/// Накопленный кредит в наименьших единицах валюты.
static CREDIT: AtomicU32 = AtomicU32::new(0);

/// Текущий кредит.
///
/// Списывать его будет логика выдачи, когда появится: для этого нужен
/// `CREDIT.swap(0, ..)` одним действием, иначе монета, попавшая между чтением
/// и сбросом, пропала бы бесследно.
pub fn credit() -> u32 {
    CREDIT.load(Ordering::Relaxed)
}

/// Прибавить монету к кредиту и вернуть новую сумму.
///
/// Насыщение вместо переполнения: потерять точность на невероятной сумме
/// лучше, чем внезапно обнулить кредит.
pub fn add(value: u32) -> u32 {
    let previous = CREDIT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
            Some(c.saturating_add(value))
        })
        .unwrap_or(0);
    previous.saturating_add(value)
}

/// Заблокирована ли линия текущей маской. Нумерация в маске с нуля.
pub const fn is_blocked(mask: u8, line: usize) -> bool {
    line < LINES && mask & (1 << line) != 0
}

/// Разобрать команду блокировки: список номеров линий через запятую.
///
/// Номера человеческие, от 1 до 6 — те же, что в документации на приёмник.
/// Голую битовую маску в команде принимать не стал: «5» как маска означает
/// первую и третью линии, а как список — только пятую, и перепутать это
/// слишком легко.
///
/// | Команда | Смысл |
/// |---|---|
/// | `none`, `0`, пусто | ничего не блокировать |
/// | `all` | заблокировать все шесть |
/// | `1,3` или `1 3` | заблокировать первую и третью |
///
/// `None`, если во вводе мусор или номер вне диапазона: молча проглотить
/// опечатку значило бы оставить автомат в неожиданном состоянии.
pub fn parse_block_mask(command: &str) -> Option<u8> {
    let command = command.trim();

    if command.is_empty() || command.eq_ignore_ascii_case("none") || command == "0" {
        return Some(0);
    }
    if command.eq_ignore_ascii_case("all") {
        return Some((1 << LINES) - 1);
    }

    let mut mask = 0u8;
    for token in command.split([',', ' ']) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let line: usize = token.parse().ok()?;
        if !(1..=LINES).contains(&line) {
            return None;
        }
        mask |= 1 << (line - 1);
    }
    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_block_commands() {
        assert_eq!(parse_block_mask(""), Some(0));
        assert_eq!(parse_block_mask("   "), Some(0));
        assert_eq!(parse_block_mask("none"), Some(0));
        assert_eq!(parse_block_mask("NONE"), Some(0));
        assert_eq!(parse_block_mask("0"), Some(0));
        assert_eq!(parse_block_mask("all"), Some(0b111111));
        assert_eq!(parse_block_mask("ALL"), Some(0b111111));

        assert_eq!(parse_block_mask("1"), Some(0b000001));
        assert_eq!(parse_block_mask("6"), Some(0b100000));
        assert_eq!(parse_block_mask("1,3"), Some(0b000101));
        assert_eq!(parse_block_mask("1 3"), Some(0b000101));
        assert_eq!(parse_block_mask(" 2 , 4 "), Some(0b001010));
        // Повтор безвреден.
        assert_eq!(parse_block_mask("2,2"), Some(0b000010));
    }

    /// Опечатку лучше отвергнуть, чем оставить автомат в неожиданном
    /// состоянии.
    #[test]
    fn rejects_bad_commands() {
        for bad in ["7", "0,1", "-1", "1,x", "первая", "1..3", "1;3"] {
            assert!(parse_block_mask(bad).is_none(), "принято: {bad}");
        }
    }

    #[test]
    fn mask_addresses_lines_from_zero() {
        let mask = parse_block_mask("1,6").unwrap();
        assert!(is_blocked(mask, 0), "линия 1 — нулевой бит");
        assert!(is_blocked(mask, 5), "линия 6 — пятый бит");
        assert!(!is_blocked(mask, 1));
        // Выход за число линий блокировкой не считается.
        assert!(!is_blocked(0xFF, LINES));
    }

    #[test]
    fn credit_accumulates_and_saturates() {
        assert_eq!(credit(), 0);
        assert_eq!(add(150), 150);
        assert_eq!(add(50), 200);
        assert_eq!(credit(), 200);

        // Переполнение не должно обнулять накопленное.
        assert_eq!(add(u32::MAX), u32::MAX);
        assert_eq!(add(1), u32::MAX);
    }
}
