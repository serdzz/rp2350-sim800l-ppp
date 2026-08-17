//! Учёт монет и разбор команд блокировки.
//!
//! Модуль обходится одним `core` — по тому же принципу, что [`crate::cmux`]:
//! логика проверяется на хосте тем же файлом, что уходит в прошивку, а всё,
//! что трогает выводы, лежит в [`crate::coin_io`].

use core::sync::atomic::{AtomicU32, Ordering};

/// Сколько выходных линий у монетоприёмника NRI G-13.
pub const LINES: usize = 6;

/// Монета, опознаваемая по коду на выходных линиях.
///
/// Код — **битовая маска по шести пинам**, а не номер линии. Документация
/// допускает это прямо: «possible to transmit coin signals in a binary code or
/// assign more than one line to a type of coin». На шильдике код записан
/// десятичным числом: `5` означает `0b000101`, то есть пины 1 и 3 **вместе**.
///
/// Частный случай — один бит на монету (`1`, `2`, `4`, `8`, `16`, `32`), и он
/// сюда укладывается без оговорок. Поэтому распознавание по маске годится в
/// обоих вариантах, и гадать, какой у вас, не нужно: неизвестный код прошивка
/// напечатает в лог, останется вписать его сюда.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coin {
    /// Код на линиях. Бит 0 — линия 1.
    pub mask: u8,
    /// Номинал в наименьших единицах валюты.
    pub value: u32,
    /// Как называть в логе.
    pub name: &'static str,
}

/// Найти монету по коду, снятому с линий.
///
/// Точное совпадение, а не «содержит биты»: при двоичном кодировании коды
/// пересекаются (`0b101` — это евро, а не «лат вместе с гривной»), и
/// поразрядное сравнение засчитывало бы одну монету как другую.
pub fn lookup(coins: &[Coin], mask: u8) -> Option<&Coin> {
    coins.iter().find(|coin| coin.mask == mask)
}

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

/// Разобрать команду полной блокировки: `block` или `accept`.
///
/// Не `on`/`off`, хотя так короче: «ON» одинаково правдоподобно читается и как
/// «блокировка включена», и как «приём включён». Спутать эти два смысла на
/// работающем автомате — значит либо глотать монеты без кредита, либо
/// принимать их там, где принимать не собирались.
pub fn parse_total_block(command: &str) -> Option<bool> {
    let command = command.trim();

    if command.eq_ignore_ascii_case("block") {
        Some(true)
    } else if command.eq_ignore_ascii_case("accept") {
        Some(false)
    } else {
        None
    }
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
    fn parses_total_block_commands() {
        assert_eq!(parse_total_block("block"), Some(true));
        assert_eq!(parse_total_block("BLOCK"), Some(true));
        assert_eq!(parse_total_block(" accept "), Some(false));
        assert_eq!(parse_total_block("ACCEPT"), Some(false));

        // Двусмысленное намеренно не принимается — см. документацию функции.
        for bad in ["on", "off", "1", "0", "", "all", "none", "blocked"] {
            assert!(parse_total_block(bad).is_none(), "принято: {bad}");
        }
    }

    /// Коды при двоичном кодировании пересекаются, поэтому сравнение обязано
    /// быть точным. Поразрядное «содержит биты» засчитало бы евро (`0b101`)
    /// как лат (`0b001`) — и то, и другое зажигает первый пин.
    #[test]
    fn lookup_matches_whole_code() {
        const COINS: &[Coin] = &[
            Coin {
                mask: 0b000001,
                value: 142,
                name: "LVL 1",
            },
            Coin {
                mask: 0b000101,
                value: 100,
                name: "EUR 1",
            },
            Coin {
                mask: 0b000110,
                value: 100,
                name: "жетон",
            },
        ];

        assert_eq!(lookup(COINS, 0b000001).unwrap().name, "LVL 1");
        assert_eq!(lookup(COINS, 0b000101).unwrap().name, "EUR 1");
        assert_eq!(lookup(COINS, 0b000110).unwrap().value, 100);

        // Ни один бит не должен «подойти частично».
        assert!(lookup(COINS, 0b000100).is_none(), "половина кода — не монета");
        assert!(lookup(COINS, 0b000111).is_none());
        assert!(lookup(COINS, 0).is_none(), "пустой код монетой не является");
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
