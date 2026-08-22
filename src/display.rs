//! Графический экран SSD1306 по I2C: время из сети и уровень сигнала.
//!
//! ```text
//!   RP2350
//!   GP16 ── SDA ──┐
//!   GP17 ── SCL ──┤ SSD1306 128x64, адрес 0x3C или 0x3D
//!   3V3  ── VCC ──┤
//!   GND  ── GND ──┘
//! ```
//!
//! Что на экране:
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ 12:34        85%    ▁▃▅▇█    │  время, заряд батареи, шкала сигнала
//! │ 2026-08-16                   │
//! │ Кредит 1.50                  │  накоплено монетоприёмником
//! │ -73dBm MQTT + 0/2            │  обрывы: MQTT / канала
//! └──────────────────────────────┘
//! ```
//!
//! Два счётчика в нижней строке — не украшение. Мгновенное состояние скрывает
//! главное: связь, разваливающаяся каждые полминуты, выглядит здесь ровно как
//! исправная, если посмотреть в удачную секунду. Считаются они от включения и
//! обнуляются перезагрузкой, в том числе по сторожевому таймеру.
//!
//! Время приходит от сотовой сети — см. [`crate::clock`]. Пока сеть его не
//! прислала, вместо часов выводится «--:--»: показывать заводскую дату модема
//! (2004 год) значило бы врать правдоподобно выглядящими цифрами.
//!
//! Заряд приходит от топливомера MAX17048 — см. [`crate::fuel`]. Шина у них
//! общая, поэтому она за мьютексом. Если топливомера нет, вместо процентов
//! стоит прочерк: пустое место читалось бы как «показа заряда тут не
//! предусмотрено».
//!
//! Экран необязателен. Если по шине никто не отозвался, задача сообщает об
//! этом в лог и засыпает: связь и телеметрия от наличия дисплея не зависят.

use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_rp::i2c::{Async, I2c};
use embassy_rp::peripherals::I2C0;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Timer};
use embedded_graphics::mono_font::MonoTextStyle;
// Кириллический набор: шрифты `ascii` вместо русских букв рисуют заглушку.
use embedded_graphics::mono_font::iso_8859_5::{FONT_6X10, FONT_10X20};
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;
use ssd1306::mode::BufferedGraphicsModeAsync;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306Async};

use crate::clock;
use crate::coin;
use crate::coin_io;
use crate::fuel;
use crate::health;
use crate::mqtt;

/// Адреса, на которых встречаются эти модули. `0x3C` — заводской, `0x3D`
/// получается перепайкой перемычки на плате.
const CANDIDATE_ADDRESSES: [u8; 2] = [0x3C, 0x3D];

/// Как часто перерисовывать. Раз в секунду достаточно для часов и не грузит
/// шину.
const REFRESH: Duration = Duration::from_secs(1);

// Разметка шкалы сигнала: пять столбиков в правом верхнем углу.
const BAR_COUNT: i32 = 5;
const BAR_WIDTH: i32 = 5;
const BAR_GAP: i32 = 2;
const BAR_BASELINE: i32 = 20;
const BAR_RIGHT: i32 = 126;

/// Где начинается заряд батареи: правее часов, левее шкалы сигнала.
const BATTERY_X: i32 = 58;

/// Шина у экрана общая с топливомером, отсюда обёртка вместо самой шины.
type SharedI2c = I2cDevice<'static, CriticalSectionRawMutex, I2c<'static, I2C0, Async>>;

type Display = Ssd1306Async<
    I2CInterface<SharedI2c>,
    DisplaySize128x64,
    BufferedGraphicsModeAsync<DisplaySize128x64>,
>;

/// Найти экран на шине.
///
/// Пробуем оба адреса: заводской и переставленный перемычкой. Возвращаем
/// `None`, если не отозвался никто.
async fn probe(mut i2c: SharedI2c) -> Option<Display> {
    let mut found = None;
    for address in CANDIDATE_ADDRESSES {
        // Байт 0x00 — префикс потока команд; сам по себе он безвреден и годится
        // как проверка присутствия. Обращаемся через трейт: шина теперь за
        // общей обёрткой, и собственных методов embassy-rp у неё нет.
        if embedded_hal_async::i2c::I2c::write(&mut i2c, address, &[0x00u8])
            .await
            .is_ok()
        {
            info!("OLED: найден по адресу 0x{:02x}", address);
            found = Some(address);
            break;
        }
    }

    let address = found?;
    let interface = I2CDisplayInterface::new_custom_address(i2c, address);
    let mut display = Ssd1306Async::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();

    if display.init().await.is_err() {
        warn!("OLED: инициализация не удалась");
        return None;
    }
    Some(display)
}

/// Нарисовать шкалу сигнала: заполненные деления сплошные, остальные контуром.
///
/// Контур вместо пустоты — чтобы было видно, сколько делений всего, и слабый
/// сигнал не выглядел как отсутствие экрана.
fn draw_signal(display: &mut Display, bars: u8) {
    let filled = PrimitiveStyle::with_fill(BinaryColor::On);
    let outline = PrimitiveStyle::with_stroke(BinaryColor::On, 1);
    let total_width = BAR_COUNT * BAR_WIDTH + (BAR_COUNT - 1) * BAR_GAP;
    let left = BAR_RIGHT - total_width;

    for index in 0..BAR_COUNT {
        let height = 4 + index * 3;
        let rect = Rectangle::new(
            Point::new(left + index * (BAR_WIDTH + BAR_GAP), BAR_BASELINE - height),
            Size::new(BAR_WIDTH as u32, height as u32),
        );
        let style = if index < bars as i32 { filled } else { outline };
        let _ = rect.into_styled(style).draw(display);
    }
}

/// Перерисовать экран целиком.
async fn render(display: &mut Display) {
    let big = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);

    display.clear(BinaryColor::Off).ok();

    // Часы и дата.
    let mut time = heapless::String::<8>::new();
    let mut date = heapless::String::<16>::new();
    match clock::last() {
        Some(now) if now.is_plausible() => {
            let _ = core::fmt::Write::write_fmt(
                &mut time,
                format_args!("{:02}:{:02}", now.hour, now.minute),
            );
            let _ = core::fmt::Write::write_fmt(
                &mut date,
                format_args!("{:04}-{:02}-{:02}", now.year, now.month, now.day),
            );
        }
        _ => {
            let _ = core::fmt::Write::write_str(&mut time, "--:--");
            let _ = core::fmt::Write::write_str(&mut date, "ждём время сети");
        }
    }
    let _ = Text::new(&time, Point::new(2, 18), big).draw(display);
    let _ = Text::new(&date, Point::new(2, 32), small).draw(display);

    // Заряд батареи — в просвет между часами и шкалой сигнала: единственное
    // свободное место, все четыре строки заняты.
    //
    // Показываем проценты, а не вольты: оператору нужно решение «менять или
    // нет», а не показание. Напряжение при этом идёт в лог — там оно как раз
    // диагностическое, и по нему видно, врёт ли модель разряда.
    let mut charge = heapless::String::<8>::new();
    match fuel::percent() {
        Some(pct) => {
            let _ = core::fmt::Write::write_fmt(&mut charge, format_args!("{pct}%"));
        }
        // Прочерк, а не пустое место: так видно, что топливомер опрашивается и
        // не отвечает, а не что показа заряда тут вовсе не предусмотрено.
        None => {
            let _ = core::fmt::Write::write_str(&mut charge, "--%");
        }
    }
    let _ = Text::new(&charge, Point::new(BATTERY_X, 18), small).draw(display);

    // Уровень сигнала: шкала справа вверху.
    let rssi = mqtt::LAST_CSQ.load(core::sync::atomic::Ordering::Relaxed);
    draw_signal(display, clock::signal_bars(rssi));

    // Кредит — главное для автомата, поэтому отдельной строкой.
    //
    // Рядом с ним признак полной блокировки: закрытый приёмник, никак себя не
    // выдающий, выглядит как сломанный, и разбираться с этим будут отвёрткой.
    let mut credit = heapless::String::<24>::new();
    let total = coin::credit();
    let _ = core::fmt::Write::write_fmt(
        &mut credit,
        format_args!("Кредит {}.{:02}", total / 100, total % 100),
    );
    if coin_io::total_blocked() {
        let _ = core::fmt::Write::write_str(&mut credit, " ЗАКРЫТО");
    }
    let _ = Text::new(&credit, Point::new(2, 46), small).draw(display);

    // Нижняя строка сжата: уровень в дБм, состояние канала до брокера и два
    // счётчика обрывов — сперва MQTT, затем самого канала.
    //
    // Счётчики важнее, чем кажется: мгновенное состояние скрывает главное.
    // Связь, разваливающаяся каждые полминуты, выглядит здесь ровно как
    // исправная, если посмотреть на экран в удачную секунду.
    let mut status = heapless::String::<32>::new();
    let dbm = clock::rssi_dbm(rssi);
    let mqtt_state = if mqtt::CONNECTED.load(core::sync::atomic::Ordering::Relaxed) {
        "MQTT +"
    } else {
        "MQTT -"
    };
    let (mqtt_drops, link_drops) = (
        health::short(health::mqtt_drops()),
        health::short(health::link_drops()),
    );
    if dbm == 0 {
        let _ = core::fmt::Write::write_fmt(
            &mut status,
            format_args!("--dBm {mqtt_state} {mqtt_drops}/{link_drops}"),
        );
    } else {
        let _ = core::fmt::Write::write_fmt(
            &mut status,
            format_args!("{dbm}dBm {mqtt_state} {mqtt_drops}/{link_drops}"),
        );
    }
    let _ = Text::new(&status, Point::new(2, 60), small).draw(display);

    if display.flush().await.is_err() {
        warn!("OLED: не удалось обновить экран");
    }
}

/// Обновляет экран раз в секунду.
#[embassy_executor::task]
pub async fn display_task(i2c: SharedI2c) -> ! {
    let Some(mut display) = probe(i2c).await else {
        warn!("OLED: не отвечает ни 0x3C, ни 0x3D — экран не подключён?");
        // Задача обязана жить: возврат из неё освободит слот исполнителя лишь
        // до перезагрузки, а пользы от этого никакой.
        loop {
            Timer::after(Duration::from_secs(60)).await;
        }
    };

    loop {
        render(&mut display).await;
        Timer::after(REFRESH).await;
    }
}
