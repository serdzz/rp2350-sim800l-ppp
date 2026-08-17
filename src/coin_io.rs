//! Монетоприёмник NRI G-13.6000: чтение линий и блокировка каналов.
//!
//! # Подключение
//!
//! | Контакт | Сигнал | Куда |
//! |---|---|---|
//! | 1 | GND | общая земля с платой |
//! | 2 | UB +12 В | отдельный источник 10–16 В, 20 мА |
//! | 7 | output line 1 | GP3 |
//! | 8 | output line 2 | GP4 |
//! | 9 | output line 3 | GP5 |
//! | 10 | output line 4 | GP6 |
//! | 3 | output line 5 | GP7 |
//! | 4 | output line 6 | GP8 |
//! | 6 | total blocking | GP9 через ключ, см. ниже |
//!
//! Нумерация линий и контактов **не совпадает** — на этом легко ошибиться.
//!
//! # Полная блокировка
//!
//! Контакт 6 закрывает приём целиком, независимо от поканальной маски. По
//! схеме подключения он активен **высоким**: блокировка при `≥ 3.5 В`, приём
//! при `≤ 1 В`, выдержит до 35 В.
//!
//! Порог 3.5 В выше, чем выдаёт RP2350, поэтому напрямую вывод сюда заводить
//! нельзя — 3.3 В попадают под порог, и блокировка либо не сработает, либо
//! будет срабатывать через раз. Между ними ставится ключ на NPN, подтягивающий
//! линию к +12 В:
//!
//! ```text
//!                        +12 В
//!                          │
//!                         10к
//!                          │
//!   GP9 ──4.7к──┬── base   ├──────── контакт 6 (total blocking)
//!               │        ╱
//!              10к   NPN │
//!               │        ╲
//!              GND ───────┴──────── GND
//! ```
//!
//! Каскад инвертирует, и это выбрано намеренно: пока контроллер в сбросе и
//! `GP9` не управляется, резистор в базе держит транзистор закрытым, на линии
//! 12 В — приёмник **заблокирован**. Монета, брошенная в мёртвый автомат, не
//! будет проглочена без кредита. Полярность задаётся
//! [`config::COIN_TOTAL_BLOCK_INVERTED`].
//!
//! # Почему выводы двунаправленные
//!
//! По документации (стр. 21) у каждой линии двойное назначение:
//! `output line 1, act. low, low = blocking A1`. То есть по ней же приходит
//! импульс о монете, и она же, удерживаемая в нуле, **блокирует свой канал**.
//!
//! Отсюда [`Flex`] вместо простого входа:
//!
//! * канал открыт — вывод вход с подтяжкой, ловим импульсы;
//! * канал закрыт — вывод выход, притянутый к земле.
//!
//! Выдавать на линию высокий уровень нельзя **никогда**: с той стороны
//! открытый коллектор, и в момент импульса он потянет её к земле против
//! нашего выхода. Поэтому в закрытом состоянии только `set_low`, а «отпустить»
//! означает вернуть вывод во вход.
//!
//! Подтяжка нужна внешняя, 4.7–10 кОм к 3V3: встроенная в RP2350 слишком
//! слабая для длинных проводов в корпусе автомата. Открытый коллектор только
//! притягивает линию к земле и никогда не выдаёт 12 В, поэтому на ногу
//! контроллера попадает максимум 3.3 В.
//!
//! # Как считается монета
//!
//! Опознанная монета даёт импульс **100 мс**. Ждём спад, выдерживаем
//! [`DEBOUNCE`], проверяем, что уровень всё ещё низкий, и только тогда
//! засчитываем. Затем дожидаемся возврата линии в высокий уровень — иначе один
//! импульс был бы посчитан многократно.

use embassy_rp::Peri;
use embassy_rp::gpio::{Flex, Level, Output, Pull};
use embassy_rp::peripherals::PIN_9;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Timer};

use crate::coin;
use crate::config;
use crate::mqtt;

/// Сколько ждать после спада, прежде чем засчитать монету.
///
/// Импульс длится 100 мс, так что 20 мс отсекают помеху и не рискуют
/// пропустить настоящий сигнал.
const DEBOUNCE: Duration = Duration::from_millis(20);

/// Текущая маска заблокированных линий.
///
/// [`Watch`], а не атомик: задача линии висит на ожидании импульса, и её нужно
/// уметь разбудить при смене маски. По приёмнику на линию — отсюда параметр
/// [`coin::LINES`].
static BLOCK_MASK: Watch<CriticalSectionRawMutex, u8, { coin::LINES }> = Watch::new();

/// Выставить исходную маску блокировки. Зовётся до запуска задач линий.
///
/// Не полагаемся на «по умолчанию ничего не заблокировано»: линии без номинала
/// обязаны быть закрыты с первой секунды, иначе монета провалится в кассу, не
/// дав кредита. См. [`config::COIN_BLOCK_DEFAULT`].
pub fn init() {
    set_block_mask(config::COIN_BLOCK_DEFAULT);
}

/// Задать, какие линии заблокированы. Задачи подхватят изменение немедленно.
pub fn set_block_mask(mask: u8) {
    BLOCK_MASK.sender().send(mask);
    info!("COIN: маска блокировки 0b{:06b}", mask);
}

/// Полностью ли заблокирован приём. Отдельно от [`BLOCK_MASK`], потому что
/// это независимое состояние: закрытый автомат не отменяет того, какие монеты
/// в нём не принимаются, когда он открыт.
static TOTAL_BLOCK: Watch<CriticalSectionRawMutex, bool, 1> = Watch::new();

/// Закрыть или открыть приём целиком.
pub fn set_total_block(blocked: bool) {
    TOTAL_BLOCK.sender().send(blocked);
    info!(
        "COIN: приём {}",
        if blocked { "заблокирован" } else { "открыт" }
    );
}

/// Заблокирован ли приём целиком. Читает экран.
pub fn total_blocked() -> bool {
    TOTAL_BLOCK.try_get().unwrap_or(false)
}

/// Обработать команду полной блокировки, пришедшую по MQTT.
pub fn apply_total_command(payload: &[u8]) {
    let Ok(text) = core::str::from_utf8(payload) else {
        warn!("COIN: команда полной блокировки не в UTF-8");
        return;
    };

    match coin::parse_total_block(text) {
        Some(blocked) => set_total_block(blocked),
        None => warn!("COIN: непонятная команда, ожидается block или accept"),
    }
}

/// Уровень на `GP9`, соответствующий заданному состоянию.
///
/// Полярность задаётся [`config::COIN_TOTAL_BLOCK_INVERTED`] — см. там про
/// буферный каскад и порог 3.5 В.
const fn total_level(blocked: bool) -> Level {
    if blocked != config::COIN_TOTAL_BLOCK_INVERTED {
        Level::High
    } else {
        Level::Low
    }
}

/// Держит линию полной блокировки (контакт 6 приёмника).
///
/// Вывод создаётся здесь, а не в `main`, чтобы полярность знал ровно один
/// файл. Стартовый уровень — «заблокировано»: до первой команды автомат
/// закрыт, а не принимает монеты неизвестно на каком основании.
#[embassy_executor::task]
pub async fn total_block_task(pin: Peri<'static, PIN_9>) -> ! {
    let mut pin = Output::new(pin, total_level(true));
    let mut rx = unwrap!(TOTAL_BLOCK.receiver());

    // Первое состояние задаём сами: без него приёмник остался бы закрытым до
    // прихода команды, а работать он должен и без связи.
    set_total_block(false);

    loop {
        let blocked = rx.changed().await;
        pin.set_level(total_level(blocked));
    }
}

/// Обработать команду блокировки, пришедшую по MQTT.
pub fn apply_block_command(payload: &[u8]) {
    let Ok(text) = core::str::from_utf8(payload) else {
        warn!("COIN: команда блокировки не в UTF-8");
        return;
    };

    match coin::parse_block_mask(text) {
        Some(mask) => set_block_mask(mask),
        None => warn!("COIN: непонятная команда блокировки, ожидается none, all или «1,3»"),
    }
}

/// Засчитать монету и отправить событие в MQTT.
fn register(line: usize) {
    let value = config::COIN_VALUES.get(line).copied().unwrap_or(0);
    if value == 0 {
        warn!("COIN: линия {} без номинала, монета не засчитана", line + 1);
        return;
    }

    let total = coin::add(value);
    info!("COIN: линия {}, номинал {}, кредит {}", line + 1, value, total);

    let mut payload = mqtt::CoinText::new();
    let _ = core::fmt::Write::write_fmt(
        &mut payload,
        format_args!(
            "{{\"line\":{},\"value\":{},\"credit\":{}}}",
            line + 1,
            value,
            total
        ),
    );

    // Не блокируемся: очередь переполняется только когда стоит связь, а
    // задерживать из-за этого приём монет нельзя.
    if mqtt::COIN_QUEUE.try_send(payload).is_err() {
        warn!("COIN: очередь публикаций переполнена, событие потеряно");
    }
}

/// Следит за одной линией монетоприёмника.
///
/// Задача на линию, а не одна на все шесть: так каждая обрабатывается
/// независимо, и монета, пришедшая по второй линии во время выдержки по
/// первой, не теряется.
#[embassy_executor::task(pool_size = coin::LINES)]
pub async fn coin_line_task(mut pin: Flex<'static>, line: usize) -> ! {
    let mut mask_rx = unwrap!(BLOCK_MASK.receiver());
    let mut mask = mask_rx.try_get().unwrap_or(0);

    loop {
        if coin::is_blocked(mask, line) {
            // Удерживаем линию в нуле — для приёмника это и есть блокировка
            // канала. Высокий уровень не выдаём никогда: с той стороны
            // открытый коллектор.
            pin.set_low();
            pin.set_as_output();

            // Ждём, пока линию не разблокируют.
            while coin::is_blocked(mask, line) {
                mask = mask_rx.changed().await;
            }
            continue;
        }

        pin.set_as_input();
        pin.set_pull(Pull::Up);

        match embassy_futures::select::select(pin.wait_for_falling_edge(), mask_rx.changed()).await {
            embassy_futures::select::Either::First(()) => {
                Timer::after(DEBOUNCE).await;
                if pin.is_low() {
                    register(line);
                    // Пока линия не отпущена, новых монет по ней быть не может.
                    pin.wait_for_high().await;
                }
            }
            embassy_futures::select::Either::Second(updated) => mask = updated,
        }
    }
}
