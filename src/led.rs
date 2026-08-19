//! Зелёный светодиод на GP25 и его режимы.
//!
//! Вынесен в отдельную задачу, потому что `BLINK` — это поведение во времени,
//! а не установка уровня: пин должен переключаться сам, независимо от того,
//! чем занят MQTT-клиент. Команда лишь меняет режим, всё остальное делает
//! [`led_task`].
//!
//! Режим хранится в атомике, а не за мьютексом: это один байт, писателей
//! немного, и ждать тут нечего.

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_rp::gpio::Output;
use embassy_time::{Duration, Timer};

/// Полупериод мигания: столько горит и столько же не горит.
const BLINK_HALF_PERIOD: Duration = Duration::from_millis(400);

/// Как часто перечитывать режим, когда светодиод неподвижен.
///
/// Задаёт задержку реакции на команду. 100 мс на глаз незаметны, а будить
/// задачу сигналом ради этого — лишняя машинерия.
const IDLE_POLL: Duration = Duration::from_millis(100);

/// Что светодиод делает.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub enum Mode {
    Off,
    On,
    Blink,
}

impl Mode {
    const fn code(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Blink => 2,
        }
    }

    /// Неизвестный код трактуем как «выключено»: атомик пишем только мы сами,
    /// но молча зажигать светодиод из-за испорченного значения незачем.
    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::On,
            2 => Self::Blink,
            _ => Self::Off,
        }
    }
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Off.code());

/// Сменить режим. Задача подхватит его не позже [`IDLE_POLL`].
pub fn set_mode(mode: Mode) {
    MODE.store(mode.code(), Ordering::Relaxed);
}

pub fn mode() -> Mode {
    Mode::from_code(MODE.load(Ordering::Relaxed))
}

/// Переключить: горит или мигает — погасить, погашен — зажечь.
///
/// Возвращает новый режим, чтобы вызывающему не пришлось читать атомик
/// повторно и получить чужое значение.
///
/// Смена делается одним действием, а не чтением с последующей записью: команды
/// приходят из MQTT, и две подряд не должны схлопнуться в одну. `Blink`
/// считается включённым состоянием — гасить мигающий светодиод логичнее, чем
/// объявлять его выключенным.
pub fn toggle() -> Mode {
    let previous = MODE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |code| {
            Some(match Mode::from_code(code) {
                Mode::Off => Mode::On,
                _ => Mode::Off,
            }
            .code())
        })
        .unwrap_or(Mode::Off.code());

    match Mode::from_code(previous) {
        Mode::Off => Mode::On,
        _ => Mode::Off,
    }
}

/// Исполняет текущий режим.
#[embassy_executor::task]
pub async fn led_task(mut led: Output<'static>) -> ! {
    loop {
        match mode() {
            Mode::Off => {
                led.set_low();
                Timer::after(IDLE_POLL).await;
            }
            Mode::On => {
                led.set_high();
                Timer::after(IDLE_POLL).await;
            }
            Mode::Blink => {
                led.toggle();
                Timer::after(BLINK_HALF_PERIOD).await;
            }
        }
    }
}
