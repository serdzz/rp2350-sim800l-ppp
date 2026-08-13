//! Измерение питания RP2350-Plus: напряжение VSYS, источник (USB/батарея),
//! оценка заряда Li-Po.
//!
//! # Что на плате (по схеме RP2350-Plus)
//!
//! ```text
//!   VBUS ──┤>├── D1 ──┬── VSYS ──┬── R1 200K ──┬── Q1 SI2306 ── GP29/ADC3
//!   VBAT ──┤>├── D2 ──┘          │             ├── R7 100K ── GND
//!    (P3, PH1.25)                │             └── C7 100nF ── GND
//!                                └── MP28164 (buck-boost) ── 3V3
//! ```
//!
//! * **GP29 / ADC3 = VSYS / 3** — делитель R1 200K / R7 100K, ровно как на
//!   Raspberry Pi Pico. C7 100nF работает буфером заряда для АЦП.
//! * **Q1 (SI2306)** — N-канальный ключ между делителем и GP29, затвор жёстко
//!   на 3V3. Управлять им не нужно: пока плата запитана, он открыт. Когда 3V3
//!   снят (`3V3_EN` низкий), ключ закрывается и отвязывает GP29 от VSYS, чтобы
//!   делитель не питал ногу МК и не сажал батарею.
//! * **GP24** — сенсор VBUS через R8 100K: высокий уровень = воткнут USB.
//! * **GP23** — вход MODE/SYNC у MP28164. Низкий = авто PSM/PWM (экономично,
//!   но пульсации на VSYS больше), высокий = постоянная ШИМ. На время замера
//!   поднимаем — так же, как рекомендуют делать на Pico.
//!
//! # Чего эта плата НЕ умеет
//!
//! VBAT попадает на VSYS через диод Шоттки D2 (MBR230LSFT1G), и отдельной
//! линии измерения батареи нет. Отсюда два следствия:
//!
//! 1. **При воткнутом USB напряжение батареи не измеряется** — VSYS задаёт
//!    VBUS через D1 (≈4.7 В) независимо от состояния аккумулятора.
//! 2. На батарее `VBAT = VSYS + Vf(D2)`, и падение на диоде зависит от тока
//!    (≈0.2 В при десятках мА, ≈0.35 В при полуампере). См. [`SCHOTTKY_DROP_MV`]
//!    — эту константу стоит откалибровать вольтметром под свою нагрузку.

use embassy_rp::adc::{Adc, Async, Channel, Error as AdcError};
use embassy_rp::gpio::{Input, Output};
use embassy_time::{Duration, Timer};

/// Опорное напряжение АЦП (ADC_VREF на плате привязан к 3V3).
const ADC_REF_MV: u32 = 3300;
/// Разрядность АЦП RP2350.
const ADC_MAX: u32 = 4095;
/// Коэффициент делителя R1 200K / R7 100K.
const DIVIDER: u32 = 3;

/// Падение на D2 (MBR230LSFT1G) при токе порядка сотни мА.
///
/// Откалибруйте под свою нагрузку: измерьте вольтметром напряжение прямо на
/// аккумуляторе и сравните с `vsys_mv` из лога.
pub const SCHOTTKY_DROP_MV: u16 = 250;

/// Сколько выборок усредняем за один замер.
const SAMPLES: usize = 16;

/// Откуда сейчас питается плата.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub enum Source {
    /// Воткнут USB — VSYS держит VBUS, о батарее судить нельзя.
    Usb,
    /// Питание от аккумулятора.
    Battery,
}

/// Результат замера.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct Reading {
    /// Напряжение VSYS, мВ.
    pub vsys_mv: u16,
    /// Текущий источник питания.
    pub source: Source,
    /// Напряжение на аккумуляторе, мВ. `None` при питании от USB.
    pub vbat_mv: Option<u16>,
    /// Оценка заряда, %. `None` при питании от USB.
    pub percent: Option<u8>,
}

/// Владеет АЦП и служебными пинами питания.
pub struct PowerMonitor<'d> {
    adc: Adc<'d, Async>,
    vsys: Channel<'d>,
    vbus_sense: Input<'d>,
    smps_mode: Output<'d>,
}

impl<'d> PowerMonitor<'d> {
    /// `vsys` — канал на PIN_29, `vbus_sense` — вход на PIN_24,
    /// `smps_mode` — выход на PIN_23 (создавайте с `Level::Low`).
    pub fn new(
        adc: Adc<'d, Async>,
        vsys: Channel<'d>,
        vbus_sense: Input<'d>,
        smps_mode: Output<'d>,
    ) -> Self {
        Self {
            adc,
            vsys,
            vbus_sense,
            smps_mode,
        }
    }

    /// Плата питается от USB.
    pub fn usb_connected(&self) -> bool {
        self.vbus_sense.is_high()
    }

    /// Один замер с усреднением.
    ///
    /// На время измерения переводит MP28164 в режим постоянной ШИМ: в режиме
    /// энергосбережения пульсации VSYS дают разброс в десятки милливольт.
    pub async fn read(&mut self) -> Result<Reading, AdcError> {
        self.smps_mode.set_high();
        // Преобразователю нужно время на переход + успокоение C7 (100nF).
        Timer::after(Duration::from_millis(2)).await;

        let mut acc: u32 = 0;
        for _ in 0..SAMPLES {
            acc += self.adc.read(&mut self.vsys).await? as u32;
        }

        self.smps_mode.set_low();

        let raw = acc / SAMPLES as u32;
        let vsys_mv = (raw * ADC_REF_MV * DIVIDER / ADC_MAX) as u16;

        let (source, vbat_mv) = if self.usb_connected() {
            (Source::Usb, None)
        } else {
            (Source::Battery, Some(vsys_mv + SCHOTTKY_DROP_MV))
        };

        Ok(Reading {
            vsys_mv,
            source,
            vbat_mv,
            percent: vbat_mv.map(crate::lipo::percent),
        })
    }
}
