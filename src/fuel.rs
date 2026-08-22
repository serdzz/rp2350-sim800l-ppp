//! Топливомер MAX17048 (SparkFun Qwiic Fuel Gauge) на общей шине I2C.
//!
//! ```text
//!   RP2350            Qwiic
//!   GP16 ── SDA ──┬── MAX17048 (0x36)
//!   GP17 ── SCL ──┤
//!   3V3  ── VCC ──┤   и SSD1306 (0x3C) на той же шине
//!   GND  ── GND ──┘
//! ```
//!
//! Аккумулятор подключается к самому топливомеру, а не к плате: он меряет
//! напряжение прямо на банке.
//!
//! # Зачем он, если есть `battery.rs`
//!
//! Затем, что тот меряет не банку, а VSYS, и при воткнутом USB о батарее не
//! говорит ничего: VBUS через диод задаёт VSYS независимо от её состояния.
//! Мы на это уже натыкались — «батарея не измеряется» в каждой строке лога.
//!
//! MAX17048 такого ограничения не имеет. Заодно он считает заряд не по
//! напряжению, а по своей модели разряда: под нагрузкой напряжение проседает,
//! и пересчёт «вольты → проценты» врёт тем сильнее, чем больше ток. У модема
//! этот ток доходит до двух ампер.
//!
//! # Про общую шину
//!
//! Экран работает с шиной асинхронно, а `max170xx` — блокирующий крейт. Свести
//! их удалось потому, что `embassy_rp::I2c` реализует оба трейта сразу: экран
//! ходит через асинхронную обёртку, а здесь мы берём мьютекс и обращаемся к
//! шине напрямую. Транзакция короткая — пара байт, — так что блокировка внутри
//! асинхронной задачи здесь безобидна.

use core::sync::atomic::{AtomicU16, Ordering};

use embassy_rp::i2c::{Async, I2c};
use embassy_rp::peripherals::I2C0;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use max170xx::Max17048;

/// Шина, разделяемая экраном и топливомером.
pub type I2cBus = Mutex<CriticalSectionRawMutex, I2c<'static, I2C0, Async>>;

/// Как часто опрашивать. Заряд меняется медленно, чаще незачем.
const POLL: Duration = Duration::from_secs(10);

/// Признак «показаний нет».
///
/// Ноль не годится: разряженная в ноль банка — вполне возможное показание, и
/// спутать его с отсутствующим датчиком нельзя.
const NO_READING: u16 = u16::MAX;

/// Напряжение наружу не отдаётся: показывать его негде, а в лог оно уходит
/// там же, где читается. Держим отдельным полем только затем, чтобы не
/// печатать устаревшее значение при потере датчика.
static PERMILLE: AtomicU16 = AtomicU16::new(NO_READING);

/// Заряд в промилле (0…1000). `None`, если топливомера нет.
///
/// Промилле, а не проценты: датчик отдаёт дробное значение, и округлять его до
/// целых процентов прямо при чтении — терять то, что он посчитал.
pub fn permille() -> Option<u16> {
    match PERMILLE.load(Ordering::Relaxed) {
        NO_READING => None,
        pm => Some(pm),
    }
}

/// Заряд в процентах, для показа человеку.
pub fn percent() -> Option<u8> {
    permille().map(|pm| ((pm + 5) / 10).min(100) as u8)
}

/// Опрашивает топливомер и складывает показания в атомики.
#[embassy_executor::task]
pub async fn fuel_task(bus: &'static I2cBus) -> ! {
    let mut announced = false;

    loop {
        // Мьютекс держим только на время обмена: экран рисует раз в секунду и
        // ждать нас не должен.
        let reading = {
            let mut i2c = bus.lock().await;
            let mut gauge = Max17048::new(&mut *i2c);
            match (gauge.voltage(), gauge.soc()) {
                (Ok(v), Ok(soc)) => Some((v, soc)),
                _ => None,
            }
        };

        match reading {
            Some((volts, soc)) => {
                if !announced {
                    info!("FUEL: MAX17048 отвечает");
                    announced = true;
                }
                PERMILLE.store((soc * 10.0).clamp(0.0, 1000.0) as u16, Ordering::Relaxed);
                info!(
                    "FUEL: батарея {} мВ, заряд {} %",
                    (volts * 1000.0) as u16,
                    (soc + 0.5) as u16
                );
            }
            None => {
                // Показание стираем, а не оставляем последнее: устаревшие
                // проценты на экране хуже честного прочерка.
                PERMILLE.store(NO_READING, Ordering::Relaxed);
                if announced {
                    warn!("FUEL: MAX17048 перестал отвечать");
                    announced = false;
                }
            }
        }

        Timer::after(POLL).await;
    }
}
