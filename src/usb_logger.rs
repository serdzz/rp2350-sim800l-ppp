//! Текстовый лог поверх USB CDC-ACM.
//!
//! Плата поднимается как виртуальный COM-порт; отладчик не нужен:
//!
//! ```text
//! screen /dev/tty.usbmodem*  115200      # macOS
//! picocom /dev/ttyACM0 -b 115200         # Linux
//! ```
//!
//! Скорость порта роли не играет — CDC-ACM её игнорирует.
//!
//! Сообщения буферизуются в пайп на [`LOG_BUF_SIZE`] байт. Пока хост не открыл
//! порт, буфер заполняется и старые строки теряются: первые секунды после
//! подачи питания в терминале не видны. Чтобы поймать самый старт, держите
//! порт открытым и передёргивайте плату через RUN.

use core::fmt::Write as _;

use embassy_futures::join::join;
use embassy_rp::bind_interrupts;
use embassy_rp::peripherals::USB;
use embassy_rp::usb::{Driver, InterruptHandler};
use embassy_time::Instant;
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::{Builder, Config};
use embassy_usb_logger::{DummyHandler, UsbLogger, Writer, MAX_PACKET_SIZE};
use log::{LevelFilter, Level, Record};
use static_cell::StaticCell;

/// Размер кольцевого буфера сообщений.
const LOG_BUF_SIZE: usize = 2048;

/// Порог логирования. `Trace` добавит внутренние сообщения embassy-net/PPP.
const LEVEL: LevelFilter = LevelFilter::Debug;

bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => InterruptHandler<USB>;
});

static LOGGER: UsbLogger<LOG_BUF_SIZE, DummyHandler> = UsbLogger::with_custom_style(style);

/// Формат строки: `[INFO ] 4.312 текст`.
///
/// Отметка времени повторяет `defmt-timestamp-uptime` из RTT-сборки, чтобы
/// логи двух режимов можно было сравнивать.
fn style(record: &Record, writer: &mut Writer<'_, LOG_BUF_SIZE>) {
    let level = match record.level() {
        Level::Error => "ERROR",
        Level::Warn => "WARN ",
        Level::Info => "INFO ",
        Level::Debug => "DEBUG",
        Level::Trace => "TRACE",
    };
    let now = Instant::now();
    let _ = write!(
        writer,
        "[{}] {}.{:03} {}\r\n",
        level,
        now.as_secs(),
        now.as_millis() % 1000,
        record.args()
    );
}

/// Создаёт USB-драйвер. Вызывать до `spawn(logger_task(..))`.
pub fn driver(usb: embassy_rp::Peri<'static, USB>) -> Driver<'static, USB> {
    Driver::new(usb, Irqs)
}

/// Фоновая задача: USB-устройство + перекачка буфера в CDC-класс.
#[embassy_executor::task]
pub async fn logger_task(driver: Driver<'static, USB>) -> ! {
    // set_logger_racy безопасен здесь: задача стартует раньше любых логов
    // приложения и вызывается ровно один раз.
    unsafe {
        let _ = log::set_logger_racy(&LOGGER).map(|()| log::set_max_level_racy(LEVEL));
    }

    // 0xc0de:0xcafe — тестовая пара VID/PID из примеров Embassy.
    // Для серийного изделия нужен собственный VID/PID.
    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Waveshare");
    config.product = Some("RP2350-Plus SIM800L logger");
    config.serial_number = None;
    config.max_power = 100;
    config.max_packet_size_0 = MAX_PACKET_SIZE;

    static CONFIG_DESC: StaticCell<[u8; 128]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 16]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static CDC_STATE: StaticCell<State> = StaticCell::new();

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 128]),
        BOS_DESC.init([0; 16]),
        MSOS_DESC.init([0; 256]),
        CONTROL_BUF.init([0; 64]),
    );

    let class = CdcAcmClass::new(
        &mut builder,
        CDC_STATE.init(State::new()),
        MAX_PACKET_SIZE as u16,
    );
    let mut device = builder.build();

    join(device.run(), LOGGER.create_future_from_class(class)).await;
    core::unreachable!()
}
