//! Телеметрия по MQTT поверх поднятого PPP-канала.
//!
//! Раз в [`config::MQTT_PUBLISH_SECS`] публикуется уровень сигнала в
//! [`config::MQTT_TOPIC_CSQ`]. Зелёный светодиод на GP25 управляется извне:
//! команда `ON` или `OFF` в [`config::MQTT_TOPIC_LED`].
//!
//! Состояние светодиода задаётся только командами и переживает переподключение
//! к брокеру: гасить его при обрыве значило бы врать о состоянии, которое
//! никто не менял.
//!
//! # Откуда берётся CSQ
//!
//! Опрос модема идёт **не отсюда**. Уровень сигнала кладёт в [`LAST_CSQ`] тот,
//! кто держит AT-канал, а эта задача только читает последнее значение.
//!
//! Это не украшательство, а следствие устройства связи: `AT+CSQ` требует
//! доступа к модему, а на пути без мультиплексора модем занят PPP и недоступен,
//! пока канал поднят. Осмысленные значения здесь появляются только со
//! включённым `USE_CMUX` — ровно ради этого мультиплексор и делался. Без него
//! в MQTT будет уходить 99, то есть «не измерено».
//!
//! # Про клиента
//!
//! `rust-mqtt` намеренно не управляет соединением: ни переподключений, ни
//! keepalive-цикла в нём нет. Всё это здесь, во внешнем цикле.
//!
//! Клиент обязан регулярно вызывать `poll()` — иначе протокол не движется и
//! брокер разорвёт соединение по keepalive. Поэтому публикация и опрос сидят
//! в одном `select`.

use core::sync::atomic::{AtomicU8, Ordering};

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_rp::gpio::Output;
use embassy_time::{Duration, Instant, Timer};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::event::Event;
use rust_mqtt::client::options::{
    ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference,
};
use rust_mqtt::config::{KeepAlive, SessionExpiryInterval};
use rust_mqtt::types::{MqttString, TopicName};

use crate::config;

/// Последний измеренный уровень сигнала. 99 — «не измерено», как в `+CSQ`.
///
/// Пишет владелец AT-канала, читает [`mqtt_task`].
pub static LAST_CSQ: AtomicU8 = AtomicU8::new(99);

/// Размер bump-буфера клиента. Хватает на CONNECT с идентификатором и
/// короткие публикации.
const MQTT_BUFFER: usize = 1024;

/// Пауза перед повторной попыткой после обрыва.
const RETRY_DELAY: Duration = Duration::from_secs(10);

/// Keepalive. Брокер разорвёт соединение, если мы столько молчим; `poll()`
/// в цикле шлёт PINGREQ сам.
const KEEP_ALIVE_SECS: u16 = 120;

/// Публикация телеметрии и индикация связи светодиодом.
#[embassy_executor::task]
pub async fn mqtt_task(stack: Stack<'static>, mut led: Output<'static>) -> ! {
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];
    let mut storage = [0u8; MQTT_BUFFER];

    loop {
        stack.wait_config_up().await;

        if let Err(()) = session(
            stack,
            &mut led,
            &mut rx_buffer,
            &mut tx_buffer,
            &mut storage,
        )
        .await
        {
            Timer::after(RETRY_DELAY).await;
        }
    }
}

/// Один сеанс: подключиться, публиковать, вернуться при первой ошибке.
///
/// Ошибки здесь не различаются по типу: реакция на любую одна — погасить
/// светодиод и переподключиться, а подробности уходят в лог.
async fn session(
    stack: Stack<'static>,
    led: &mut Output<'static>,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
    storage: &mut [u8],
) -> Result<(), ()> {
    let addrs = stack
        .dns_query(config::MQTT_HOST, DnsQueryType::A)
        .await
        .map_err(|e| warn!("MQTT: DNS не ответил: {:?}", e))?;
    let addr = *addrs
        .first()
        .ok_or_else(|| warn!("MQTT: пустой DNS-ответ"))?;

    let mut socket = TcpSocket::new(stack, rx_buffer, tx_buffer);
    // GPRS отвечает сотнями миллисекунд; таймаут должен быть щедрым.
    socket.set_timeout(Some(Duration::from_secs(30)));
    socket
        .connect((addr, config::MQTT_PORT))
        .await
        .map_err(|e| warn!("MQTT: TCP до {} не подключился: {:?}", addr, e))?;
    info!("MQTT: TCP до {} установлен", config::MQTT_HOST);

    let mut buffer = BumpBuffer::new(storage);
    let mut client = Client::<'_, _, _, 1, 1, 1, 1>::new(&mut buffer);

    let client_id = MqttString::try_from(config::MQTT_CLIENT_ID)
        .map_err(|_| warn!("MQTT: слишком длинный client id"))?;
    let options = ConnectOptions::new()
        .clean_start()
        .session_expiry_interval(SessionExpiryInterval::Seconds(0))
        .keep_alive(KeepAlive::Seconds(
            core::num::NonZero::new(KEEP_ALIVE_SECS).unwrap(),
        ));

    client
        .connect(socket, &options, Some(client_id))
        .await
        .map_err(|e| warn!("MQTT: брокер отказал: {:?}", e))?;

    // Буфер под CONNECT больше не нужен — освобождаем под публикации.
    // SAFETY: сброс допустим, пока не живы заимствования из буфера; сразу
    // после connect их нет.
    unsafe { client.buffer_mut().reset() };

    info!("MQTT: подключены к {}", config::MQTT_HOST);

    let led_topic = TopicName::new(
        MqttString::try_from(config::MQTT_TOPIC_LED)
            .map_err(|_| warn!("MQTT: слишком длинный топик светодиода"))?,
    )
    .ok_or_else(|| warn!("MQTT: недопустимый топик светодиода"))?;

    client
        .subscribe(led_topic.as_borrowed().into(), SubscriptionOptions::new())
        .await
        .map_err(|e| warn!("MQTT: подписка не удалась: {:?}", e))?;
    info!("MQTT: подписаны на {}", config::MQTT_TOPIC_LED);
    // SAFETY: заимствований из буфера после подписки нет.
    unsafe { client.buffer_mut().reset() };

    let topic = TopicName::new(
        MqttString::try_from(config::MQTT_TOPIC_CSQ)
            .map_err(|_| warn!("MQTT: слишком длинный топик"))?,
    )
    .ok_or_else(|| warn!("MQTT: недопустимый топик"))?;

    // Дедлайн, а не пересоздаваемый таймер: событие от poll() не должно
    // сдвигать момент следующей публикации.
    let mut deadline = Instant::now();

    loop {
        match select(Timer::at(deadline), client.poll()).await {
            Either::First(()) => {
                let csq = LAST_CSQ.load(Ordering::Relaxed);
                let mut payload = heapless::String::<8>::new();
                let _ = core::fmt::Write::write_fmt(&mut payload, format_args!("{csq}"));

                client
                    .publish(
                        // QoS 0 — значение по умолчанию: телеметрию не жалко
                        // потерять, и никакого состояния для повторной доставки.
                        &PublicationOptions::new(TopicReference::Name(topic.as_borrowed())),
                        payload.as_bytes().into(),
                    )
                    .await
                    .map_err(|e| warn!("MQTT: публикация не удалась: {:?}", e))?;

                info!("MQTT: {} <- CSQ {}", config::MQTT_TOPIC_CSQ, csq);
                // SAFETY: заимствований из буфера после публикации нет.
                unsafe { client.buffer_mut().reset() };

                deadline = Instant::now() + Duration::from_secs(config::MQTT_PUBLISH_SECS);
            }
            Either::Second(event) => {
                let event = event.map_err(|e| warn!("MQTT: соединение потеряно: {:?}", e))?;
                match event {
                    Event::Publish(publication) => apply_led_command(led, &publication.message),
                    other => debug!("MQTT: событие {:?}", other),
                }
            }
        }
    }
}

/// Исполнить команду светодиода: `ON` или `OFF`.
///
/// Регистр и обрамляющие пробелы игнорируются — брокеры и клиенты шлют
/// по-разному, а спорить об этом дешевле один раз здесь.
fn apply_led_command(led: &mut Output<'static>, payload: &[u8]) {
    let trimmed = payload.trim_ascii();

    if trimmed.eq_ignore_ascii_case(b"ON") {
        led.set_high();
        info!("MQTT: светодиод включён");
    } else if trimmed.eq_ignore_ascii_case(b"OFF") {
        led.set_low();
        info!("MQTT: светодиод выключен");
    } else {
        warn!(
            "MQTT: непонятная команда светодиода ({} байт), ожидается ON или OFF",
            trimmed.len()
        );
    }
}
