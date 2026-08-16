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
mod battery;
mod clock;
mod cmux;
mod cmux_transport;
mod config;
mod display;
mod io_compat;
mod led;
mod lipo;
mod modem;
mod mqtt;
mod sim800l;
#[cfg(feature = "log-usb")]
mod usb_logger;

use atat::asynch::{AtatClient, Client};
use atat::{AtatIngress, DefaultDigester, Ingress, ResponseSlot, UrcChannel, UrcSubscription};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_net::{Config as NetConfig, ConfigV4, Ipv4Cidr, StackResources, StaticConfigV4};
use embassy_rp::adc::{
    Adc, Channel as AdcChannel, Config as AdcConfig, InterruptHandler as AdcInterruptHandler,
};
use embassy_rp::bind_interrupts;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::i2c::{Config as I2cConfig, I2c, InterruptHandler as I2cInterruptHandler};
use embassy_rp::peripherals::{I2C0, UART0};
use embassy_rp::uart::{BufferedInterruptHandler, BufferedUart, Config as UartConfig};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::pipe::{DynamicReader, DynamicWriter, Pipe};
use embassy_time::{Duration, Instant, Timer};
use static_cell::StaticCell;

#[cfg(feature = "log-rtt")]
use defmt_rtt as _;
use panic_probe as _;

use crate::io_compat::Compat;
use crate::modem::Urc;

// --- Размеры буферов -------------------------------------------------------

/// Максимальный размер одного ответа AT.
///
/// 1 КиБ, а не 512 Б, из-за `AT+COPS=?`: список видимых сетей легко
/// перерастает полкилобайта, а переполнение ingress-буфера привело бы к
/// потере всего ответа.
const INGRESS_BUF_SIZE: usize = 1024;
/// Глубина очереди URC.
const URC_CAPACITY: usize = 8;
/// Сколько задач могут читать URC: логирование, детектор перезагрузок и
/// приём SMS.
const URC_SUBSCRIBERS: usize = 3;
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

/// Подписка на URC, которую держит логика инициализации модема.
///
/// Вторая после `urc_task`; ровно поэтому [`URC_SUBSCRIBERS`] равен двум.
pub type UrcSub = UrcSubscription<'static, Urc, URC_CAPACITY, URC_SUBSCRIBERS>;

/// Разборщик ответов модема со всеми размерами буферов, зафиксированными выше.
type ModemIngress =
    Ingress<'static, DefaultDigester<Urc>, Urc, INGRESS_BUF_SIZE, URC_CAPACITY, URC_SUBSCRIBERS>;

/// Размер приёмного буфера декодера CMUX: кадр PPP плюс запас.
const CMUX_DECODE_BUF: usize = 1600;
/// Труба AT-канала.
const CMUX_AT_PIPE: usize = 512;
/// Труба PPP-канала — должна вмещать хотя бы кадр PPP целиком.
const CMUX_PPP_PIPE: usize = 2048;

/// Сколько перезагрузок модуля подряд считать признаком проблем с питанием.
const RESET_STREAK_HINT: u32 = 3;

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    ADC_IRQ_FIFO => AdcInterruptHandler;
    I2C0_IRQ => I2cInterruptHandler<I2C0>;
});

/// Фоновая задача сетевого стека `embassy-net`.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, embassy_net_ppp::Device<'static>>) -> ! {
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

/// Периодический замер питания.
///
/// GP29 = VSYS/3, GP24 = сенсор VBUS, GP23 = MODE/SYNC у MP28164 —
/// подробности и ограничения см. в `battery.rs`.
#[embassy_executor::task]
async fn battery_task(mut monitor: battery::PowerMonitor<'static>) -> ! {
    loop {
        match monitor.read().await {
            Ok(r) => match (r.vbat_mv, r.percent) {
                (Some(mv), Some(pct)) => info!(
                    "PWR: {:?}, батарея {} мВ ({} %), VSYS {} мВ",
                    r.source, mv, pct, r.vsys_mv
                ),
                _ => info!(
                    "PWR: {:?}, VSYS {} мВ (батарея не измеряется)",
                    r.source, r.vsys_mv
                ),
            },
            Err(e) => warn!("PWR: ошибка АЦП: {:?}", e),
        }
        Timer::after(Duration::from_secs(60)).await;
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

    // --- Мониторинг питания -----------------------------------------------
    // PIN_23/24/29 разведены на плате и на гребёнку не выходят.
    let monitor = battery::PowerMonitor::new(
        Adc::new(p.ADC, Irqs, AdcConfig::default()),
        AdcChannel::new_pin(p.PIN_29, Pull::None),
        Input::new(p.PIN_24, Pull::None),
        Output::new(p.PIN_23, Level::Low),
    );
    spawner.spawn(unwrap!(battery_task(monitor)));

    // --- UART на GP0 (TX) / GP1 (RX) --------------------------------------
    static UART_TX_BUF: StaticCell<[u8; UART_TX_BUF_SIZE]> = StaticCell::new();
    static UART_RX_BUF: StaticCell<[u8; UART_RX_BUF_SIZE]> = StaticCell::new();

    let mut uart_config = UartConfig::default();
    uart_config.baudrate = config::UART_BAUDRATE;

    let uart = BufferedUart::new(
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
    let (device, ppp_runner) = embassy_net_ppp::new(PPP_STATE.init(embassy_net_ppp::State::new()));

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
    // Экран SSD1306 на GP16 (SDA) / GP17 (SCL) — это выводы I2C0.
    spawner.spawn(unwrap!(display::display_task(I2c::new_async(
        p.I2C0,
        p.PIN_17,
        p.PIN_16,
        Irqs,
        I2cConfig::default(),
    ))));

    // Зелёный светодиод на GP25 — через R19 470R на землю, активен высоким.
    spawner.spawn(unwrap!(led::led_task(Output::new(p.PIN_25, Level::Low))));
    spawner.spawn(unwrap!(mqtt::mqtt_task(stack)));

    // --- atat: ingress + буфер команд -------------------------------------
    static INGRESS_BUF: StaticCell<[u8; INGRESS_BUF_SIZE]> = StaticCell::new();
    static CMD_BUF: StaticCell<[u8; CMD_BUF_SIZE]> = StaticCell::new();

    // with_custom_success: SIM800L отвечает на AT+CIPSHUT строкой `SHUT OK`,
    // которую штатный дайджестер не считает успехом — см. modem::parse_shut_ok.
    let ingress = Ingress::new(
        DefaultDigester::<Urc>::new().with_custom_success(modem::parse_shut_ok),
        INGRESS_BUF.init([0; INGRESS_BUF_SIZE]),
        &RES_SLOT,
        &URC_CHANNEL,
    );
    let cmd_buf = CMD_BUF.init([0; CMD_BUF_SIZE]);

    let ppp_config = embassy_net_ppp::Config {
        username: config::PPP_USERNAME,
        password: config::PPP_PASSWORD,
    };

    // Отдельная подписка для bring_up: по `RDY` он поймёт, что модуль ушёл
    // в перезагрузку, и не будет две минуты опрашивать мёртвую железку.
    let mut bring_up_urc: UrcSub = unwrap!(URC_CHANNEL.subscribe().ok());

    // Модуль включаем один раз; дальше при обрывах переподнимаем только сессию.
    sim800l::power_on(&mut pwrkey).await;

    if config::USE_CMUX {
        run_multiplexed(
            uart,
            ppp_runner,
            stack,
            ingress,
            cmd_buf,
            &mut bring_up_urc,
            ppp_config,
        )
        .await
    } else {
        run_plain(
            uart,
            ppp_runner,
            stack,
            ingress,
            cmd_buf,
            &mut bring_up_urc,
            ppp_config,
        )
        .await
    }
}

/// Применить IPv4-конфигурацию, полученную по IPCP.
fn apply_ipv4(stack: embassy_net::Stack<'static>, ipv4: embassy_net_ppp::Ipv4Status) {
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
}

/// Проверенный путь: UART принадлежит то `atat`, то PPP.
///
/// Пока канал поднят, модем недоступен для команд — ради снятия этого
/// ограничения и делается [`run_multiplexed`].
async fn run_plain(
    mut uart: BufferedUart,
    mut ppp_runner: embassy_net_ppp::Runner<'static>,
    stack: embassy_net::Stack<'static>,
    mut ingress: ModemIngress,
    cmd_buf: &'static mut [u8; CMD_BUF_SIZE],
    bring_up_urc: &mut UrcSub,
    ppp_config: embassy_net_ppp::Config<'static>,
) -> ! {
    // Сколько раз подряд модуль перезагрузился посреди инициализации.
    let mut reset_streak = 0u32;

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
            let setup_fut = sim800l::bring_up(&mut client, config::APN, bring_up_urc);

            match select(ingress_fut, setup_fut).await {
                Either::First(()) => unreachable!(),
                Either::Second(result) => result,
            }
        };

        if let Err(e) = bring_up {
            report_bring_up_error(&e, &mut reset_streak);
            // Вернуть модем в вменяемое состояние и попробовать снова.
            sim800l::escape_data_mode(&mut uart).await;
            Timer::after(Duration::from_secs(config::RECONNECT_DELAY_SECS)).await;
            continue;
        }

        reset_streak = 0;

        // ---------- фаза 2: data-режим (PPP владеет UART) ----------
        let result = ppp_runner
            .run(&mut uart, ppp_config.clone(), |ipv4| {
                apply_ipv4(stack, ipv4)
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

fn report_bring_up_error(e: &sim800l::BringUpError, reset_streak: &mut u32) {
    if matches!(e, sim800l::BringUpError::ModemReset) {
        *reset_streak += 1;
        warn!(
            "Модуль перезагрузился во время инициализации (подряд: {})",
            reset_streak
        );
        if *reset_streak >= RESET_STREAK_HINT {
            error!(
                "SIM800L перезагружается циклически. Регистрация идёт на полной \
                 мощности передатчика (до 2 А импульсами) — проверьте питание: \
                 электролит 1000 мкФ прямо на VCC/GND модуля, отдельные толстые \
                 провода от аккумулятора мимо макетки, заряд батареи."
            );
        }
    } else {
        *reset_streak = 0;
        error!("Инициализация модема не удалась: {:?}", e);
    }
}

/// Путь с мультиплексором 27.010: AT-команды и PPP живут одновременно.
///
/// Порядок важен. Всё, что проще сделать простыми AT-командами — связь, SIM,
/// регистрация, PDP-контекст — делается до `AT+CMUX`, потому что после него
/// обычный AT-обмен по этому UART заканчивается.
async fn run_multiplexed(
    mut uart: BufferedUart,
    mut ppp_runner: embassy_net_ppp::Runner<'static>,
    stack: embassy_net::Stack<'static>,
    mut ingress: ModemIngress,
    cmd_buf: &'static mut [u8; CMD_BUF_SIZE],
    bring_up_urc: &mut UrcSub,
    ppp_config: embassy_net_ppp::Config<'static>,
) -> ! {
    let mut reset_streak = 0u32;

    loop {
        // ---------- фаза A: обычный AT, до входа в мультиплексор ----------
        ingress.clear();

        let prepared = {
            let (uart_tx, uart_rx) = uart.split_ref();
            let mut client = Client::new(
                Compat(uart_tx),
                &RES_SLOT,
                &mut cmd_buf[..],
                atat::Config::new(),
            );
            let ingress_fut = async {
                ingress.read_from(Compat(uart_rx)).await;
            };
            let setup_fut = async {
                sim800l::prepare(&mut client, config::APN, bring_up_urc).await?;
                sim800l::enter_cmux(&mut client, config::CMUX_MAX_PAYLOAD).await
            };

            match select(ingress_fut, setup_fut).await {
                Either::First(()) => unreachable!(),
                Either::Second(result) => result,
            }
        };

        if let Err(e) = prepared {
            report_bring_up_error(&e, &mut reset_streak);
            sim800l::escape_data_mode(&mut uart).await;
            Timer::after(Duration::from_secs(config::RECONNECT_DELAY_SECS)).await;
            continue;
        }
        reset_streak = 0;

        // ---------- фаза B: мультиплексный режим ----------
        ingress.clear();
        multiplexed_session(
            &mut uart,
            &mut ppp_runner,
            stack,
            &mut ingress,
            cmd_buf,
            ppp_config.clone(),
        )
        .await;

        stack.set_config_v4(ConfigV4::None);
        // Просим модем вернуться в обычный AT-режим; если он уже там, вреда нет.
        sim800l::escape_data_mode(&mut uart).await;
        Timer::after(Duration::from_secs(config::RECONNECT_DELAY_SECS)).await;
    }
}

/// Один сеанс работы через мультиплексор. Возвращается, когда что-то развалилось.
async fn multiplexed_session(
    uart: &mut BufferedUart,
    ppp_runner: &mut embassy_net_ppp::Runner<'static>,
    stack: embassy_net::Stack<'static>,
    ingress: &mut ModemIngress,
    cmd_buf: &mut [u8; CMD_BUF_SIZE],
    ppp_config: embassy_net_ppp::Config<'static>,
) {
    let n1 = config::CMUX_MAX_PAYLOAD as usize;
    let (uart_tx, mut uart_rx) = uart.split_ref();

    let shared_tx: cmux_transport::SharedTx<_> = Mutex::new(uart_tx);
    let session: cmux_transport::SharedSession = Mutex::new(cmux::Session::new());
    let mut decoder = cmux::Decoder::<CMUX_DECODE_BUF>::new();

    let mut at_pipe: Pipe<CriticalSectionRawMutex, CMUX_AT_PIPE> = Pipe::new();
    let mut ppp_pipe: Pipe<CriticalSectionRawMutex, CMUX_PPP_PIPE> = Pipe::new();
    let (at_reader, at_writer) = at_pipe.split();
    let (ppp_reader, ppp_writer) = ppp_pipe.split();

    let mut routes = [
        cmux_transport::Route {
            dlci: config::CMUX_AT_DLCI,
            sink: DynamicWriter::from(at_writer),
        },
        cmux_transport::Route {
            dlci: config::CMUX_PPP_DLCI,
            sink: DynamicWriter::from(ppp_writer),
        },
    ];

    // Насос обязан крутиться всё время: подтверждения открытия каналов и
    // входящие данные идут только через него.
    let pump_fut = async {
        cmux_transport::pump(
            &mut uart_rx,
            &shared_tx,
            &mut decoder,
            &session,
            &mut routes,
        )
        .await;
    };

    let app_fut = async {
        if let Err(e) = cmux_transport::bring_up(
            &shared_tx,
            &session,
            cmux_transport::Channels {
                at: config::CMUX_AT_DLCI,
                ppp: config::CMUX_PPP_DLCI,
                attempts: config::CMUX_OPEN_ATTEMPTS,
                timeout: Duration::from_secs(2),
            },
        )
        .await
        {
            error!("CMUX: мультиплексор не поднялся: {:?}", e);
            return;
        }

        let (at_rx, at_tx) = cmux_transport::Channel::new(
            config::CMUX_AT_DLCI,
            DynamicReader::from(at_reader),
            &shared_tx,
            n1,
        )
        .split();
        let mut ppp_channel = cmux_transport::Channel::new(
            config::CMUX_PPP_DLCI,
            DynamicReader::from(ppp_reader),
            &shared_tx,
            n1,
        );

        // Дозвон идёт сырыми байтами прямо в PPP-канал: второй экземпляр
        // atat ради одной команды не нужен.
        if let Err(e) = sim800l::dial_on_stream(
            &mut ppp_channel,
            config::DIAL_STRING,
            Duration::from_secs(30),
        )
        .await
        {
            error!("CMUX: дозвон не удался: {:?}", e);
            return;
        }

        let mut client = Client::new(Compat(at_tx), &RES_SLOT, cmd_buf, atat::Config::new());

        // Настройки SMS задаём именно здесь, на AT-канале: в мультиплексном
        // режиме каждый DLCI — свой AT-интерфейс, и заданное до AT+CMUX сюда
        // не переносится.
        sim800l::configure_sms(&mut client).await;

        let ingress_fut = async {
            ingress.read_from(Compat(at_rx)).await;
        };
        let ppp_fut = ppp_runner.run(&mut ppp_channel, ppp_config, |ipv4| apply_ipv4(stack, ipv4));
        // Ради этого всё и затевалось: работа с модемом, пока канал поднят.
        // Опрос CSQ по таймеру и приём SMS по URC — оба невозможны без
        // мультиплексора, там модем занят PPP.
        let mut sms_urc: UrcSub = unwrap!(URC_CHANNEL.subscribe().ok());
        let at_fut = async {
            let mut csq_deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match select(Timer::at(csq_deadline), sms_urc.next_message_pure()).await {
                    Either::First(()) => {
                        match client.send(&modem::GetSignalQuality).await {
                            Ok(csq) => {
                                info!("CMUX: CSQ {} при поднятом PPP", csq.rssi);
                                mqtt::LAST_CSQ
                                    .store(csq.rssi, core::sync::atomic::Ordering::Relaxed);
                            }
                            Err(e) => warn!("CMUX: опрос CSQ не удался: {:?}", e),
                        }
                        read_network_time(&mut client).await;
                        csq_deadline = Instant::now() + Duration::from_secs(30);
                    }
                    Either::Second(Urc::NewMessage(notice)) => {
                        forward_sms(&mut client, notice.index).await;
                    }
                    Either::Second(_) => {}
                }
            }
        };

        match select3(ingress_fut, ppp_fut, at_fut).await {
            Either3::Second(Err(e)) => warn!("CMUX: PPP-сессия завершена: {:?}", e),
            _ => warn!("CMUX: сеанс прерван"),
        }
    };

    select(pump_fut, app_fut).await;
}

/// Прочитать пришедшее SMS и отдать его в очередь на публикацию.
///
/// Удаляем только после успешного чтения: иначе потерянное сообщение исчезнет
/// безвозвратно. Обратная сторона — при устойчивой ошибке чтения память SIM
/// заполнится и новые SMS приходить перестанут; это видно по логу.
async fn forward_sms<A: atat::asynch::AtatClient>(client: &mut A, index: u32) {
    let sms = match client.send(&modem::ReadSms { index }).await {
        Ok(sms) => sms,
        Err(e) => {
            warn!("SMS: индекс {} не прочитан: {:?}", index, e);
            return;
        }
    };

    // atat живёт на heapless 0.8, остальное дерево — на 0.9; перекладываем
    // через строковый срез.
    match mqtt::SmsText::try_from(sms.text.as_str()) {
        Ok(text) => {
            info!("SMS: индекс {}, {} байт", index, text.len());
            if mqtt::SMS_QUEUE.try_send(text).is_err() {
                warn!("SMS: очередь на публикацию переполнена, сообщение потеряно");
            }
        }
        Err(_) => warn!("SMS: индекс {} не поместился в буфер", index),
    }

    if let Err(e) = client.send(&modem::DeleteSms { index }).await {
        warn!("SMS: индекс {} не удалён: {:?}", index, e);
    }
}

/// Спросить у модема время сети и запомнить его для дисплея.
///
/// Часы модуля идут сами, но синхронизируются сетью только при регистрации,
/// поэтому перечитываем их вместе с уровнем сигнала. Заводскую дату (модем
/// отдаёт 2004 год, пока NITZ не пришёл) не сохраняем — иначе на экране
/// появилось бы правдоподобно выглядящее враньё.
async fn read_network_time<A: atat::asynch::AtatClient>(client: &mut A) {
    match client.send(&modem::GetClock).await {
        Ok(response) => match clock::parse_cclk(response.text.as_str()) {
            Some(now) if now.is_plausible() => clock::store(now),
            Some(_) => debug!("CLOCK: сеть время ещё не прислала"),
            None => warn!("CLOCK: не разобрал {}", response.text.as_str()),
        },
        Err(e) => warn!("CLOCK: AT+CCLK? не ответил: {:?}", e),
    }
}
