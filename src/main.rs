//! RP2350-Plus (Waveshare) + SIM800L: выход в интернет через PPP.
//!
//! Схема работы:
//!
//! ```text
//!   ┌───────── командный режим ─────────┐   ┌──── data-режим ────┐
//!   BufferedUart ──► atat::Ingress ──► ResponseSlot/UrcChannel
//!        ▲                                  BufferedUart ──► embassy-net-ppp
//!        └── atat::Client (AT+...)                              │
//!                                                        embassy-net (TCP/UDP/DNS)
//! ```
//!
//! Ключевая деталь: UART отдаётся то `atat`, то PPP — не одновременно.
//! Пока идёт настройка модема, задача ingress живёт внутри `select!` и
//! автоматически снимается, как только `bring_up` вернул `CONNECT`.
//! После этого тот же UART целиком уходит в `embassy_net_ppp::Runner::run`.

#![no_std]
#![no_main]

// Объявлен первым: `#![macro_use]` внутри fmt.rs делает info!/warn!/unwrap!
// видимыми во всех модулях ниже.
mod fmt;

mod app;
mod config;
mod io_compat;
mod modem;
mod sim800l;
#[cfg(feature = "log-usb")]
mod usb_logger;

use atat::asynch::Client;
use atat::{AtatIngress, DefaultDigester, Ingress, ResponseSlot, UrcChannel, UrcSubscription};
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::{Config as NetConfig, ConfigV4, Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::UART0;
use embassy_rp::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig};
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use panic_probe as _;
#[cfg(feature = "log-rtt")]
use defmt_rtt as _;

use crate::io_compat::Compat;
use crate::modem::Urc;

// --- Размеры буферов -------------------------------------------------------

/// Максимальный размер одного ответа AT.
const INGRESS_BUF_SIZE: usize = 512;
/// Глубина очереди URC.
const URC_CAPACITY: usize = 8;
/// Сколько задач могут читать URC.
const URC_SUBSCRIBERS: usize = 2;
/// Буфер сериализации исходящей AT-команды.
const CMD_BUF_SIZE: usize = 256;

/// TX-буфер UART. PPP шлёт кадры до ~1500 байт.
const UART_TX_BUF_SIZE: usize = 1024;
/// RX-буфер UART. Без аппаратного flow control это единственная защита от
/// переполнения на 115200 бод — меньше 2 КиБ ставить не стоит.
const UART_RX_BUF_SIZE: usize = 2048;

// --- Глобальное состояние atat --------------------------------------------

static RES_SLOT: ResponseSlot<INGRESS_BUF_SIZE> = ResponseSlot::new();
static URC_CHANNEL: UrcChannel<Urc, URC_CAPACITY, URC_SUBSCRIBERS> = UrcChannel::new();

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
});

/// Фоновая задача сетевого стека `embassy-net`.
#[embassy_executor::task]
async fn net_task(
    mut runner: embassy_net::Runner<'static, embassy_net_ppp::Device<'static>>,
) -> ! {
    runner.run().await
}

/// Логирование незапрошенных сообщений модема.
#[embassy_executor::task]
async fn urc_task(mut sub: UrcSubscription<'static, Urc, URC_CAPACITY, URC_SUBSCRIBERS>) -> ! {
    loop {
        let urc = sub.next_message_pure().await;
        info!("URC: {:?}", urc);
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // USB-логгер поднимаем первым делом, чтобы поймать как можно больше старта.
    // Хост всё равно enumerate'ит порт ~1 с, поэтому самые первые строки
    // осядут в буфере логгера и уйдут в терминал позже.
    #[cfg(feature = "log-usb")]
    spawner.spawn(unwrap!(usb_logger::logger_task(usb_logger::driver(p.USB))));

    info!("RP2350-Plus + SIM800L: старт");

    // GP2 -> PWRKEY модуля. Если PWRKEY не разведён (модуль стартует сам),
    // пин просто останется неиспользуемым выходом.
    let mut pwrkey = Output::new(p.PIN_2, Level::High);

    // --- UART на GP0 (TX) / GP1 (RX) --------------------------------------
    static UART_TX_BUF: StaticCell<[u8; UART_TX_BUF_SIZE]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; UART_RX_BUF_SIZE]> = StaticCell::new();

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = config::UART_BAUDRATE;

    let mut uart = BufferedUart::new(
        p.UART0,
        p.PIN_0,
        p.PIN_1,
        Irqs,
        UART_TX_BUF.init([0; UART_TX_BUF_SIZE]),
        UART_RX_BUF.init([0; UART_RX_BUF_SIZE]),
        uart_config,
    );

    // --- PPP-драйвер как embassy-net Device -------------------------------
    static PPP_STATE: StaticCell<embassy_net_ppp::State<4, 4>> = StaticCell::new();
    let (device, mut ppp_runner) = embassy_net_ppp::new(PPP_STATE.init(embassy_net_ppp::State::new()));

    // --- Сетевой стек ------------------------------------------------------
    // IP-конфигурацию не задаём: её принесёт IPCP при подъёме PPP.
    let seed = RoscRng.next_u64();
    static RESOURCES: StaticCell<StackResources<4>> = StaticCell::new();
    let (stack, net_runner) = embassy_net::new(
        device,
        NetConfig::default(),
        RESOURCES.init(StackResources::new()),
        seed,
    );

    spawner.spawn(unwrap!(net_task(net_runner)));
    spawner.spawn(unwrap!(urc_task(unwrap!(URC_CHANNEL.subscribe().ok()))));
    spawner.spawn(unwrap!(app::demo_task(stack)));

    // --- atat: ingress + буфер команд -------------------------------------
    static INGRESS_BUF: StaticCell<[u8; INGRESS_BUF_SIZE]> = StaticCell::new();
    static CMD_BUF: StaticCell<[u8; CMD_BUF_SIZE]> = StaticCell::new();

    let mut ingress = Ingress::new(
        DefaultDigester::<Urc>::default(),
        INGRESS_BUF.init([0; INGRESS_BUF_SIZE]),
        &RES_SLOT,
        &URC_CHANNEL,
    );
    let cmd_buf = CMD_BUF.init([0; CMD_BUF_SIZE]);

    let ppp_config = embassy_net_ppp::Config {
        username: config::PPP_USERNAME,
        password: config::PPP_PASSWORD,
    };

    // Модуль включаем один раз; дальше при обрывах переподнимаем только сессию.
    sim800l::power_on(&mut pwrkey).await;

    loop {
        // ---------- фаза 1: командный режим (atat владеет UART) ----------
        ingress.clear();

        let bring_up = {
            let (uart_tx, uart_rx) = uart.split_ref();
            // Compat(..) переносит UART из embedded-io-async 0.7 в 0.6 — см. io_compat.
            let mut client = Client::new(
                Compat(uart_tx),
                &RES_SLOT,
                &mut cmd_buf[..],
                atat::Config::new(),
            );

            // read_from() никогда не возвращается; `;` приводит `!` к `()`.
            let ingress_fut = async {
                ingress.read_from(Compat(uart_rx)).await;
            };
            let setup_fut = sim800l::bring_up(&mut client, config::APN);

            match select(ingress_fut, setup_fut).await {
                Either::First(()) => unreachable!(),
                Either::Second(result) => result,
            }
        };

        if let Err(e) = bring_up {
            error!("Инициализация модема не удалась: {:?}", e);
            // Вернуть модем в вменяемое состояние и попробовать снова.
            sim800l::escape_data_mode(&mut uart).await;
            Timer::after(Duration::from_secs(config::RECONNECT_DELAY_SECS)).await;
            continue;
        }

        // ---------- фаза 2: data-режим (PPP владеет UART) ----------
        let result = ppp_runner
            .run(&mut uart, ppp_config.clone(), |ipv4| {
                let Some(address) = ipv4.address else {
                    warn!("PPP: пир не выдал IPv4-адрес");
                    return;
                };

                let mut dns_servers = heapless::Vec::new();
                for server in ipv4.dns_servers.iter().flatten() {
                    let _ = dns_servers.push(*server);
                }

                info!("PPP: адрес {:?}, пир {:?}", address, ipv4.peer_address);

                // Маска /0 + отсутствие шлюза — стандартная конфигурация для
                // point-to-point линка: весь трафик уходит в PPP-интерфейс.
                stack.set_config_v4(ConfigV4::Static(StaticConfigV4 {
                    address: Ipv4Cidr::new(address, 0),
                    gateway: None,
                    dns_servers,
                }));
            })
            .await;

        // Ok-вариант — Infallible: run() возвращается только с ошибкой.
        match result {
            Err(e) => warn!("PPP-сессия завершена: {:?}", e),
        }

        // Снимаем протухшую конфигурацию, чтобы wait_config_up() снова блокировал.
        stack.set_config_v4(ConfigV4::None);

        // ---------- фаза 3: возврат в командный режим ----------
        sim800l::escape_data_mode(&mut uart).await;
        Timer::after(Duration::from_secs(config::RECONNECT_DELAY_SECS)).await;
    }
}
