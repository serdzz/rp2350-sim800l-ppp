//! Телеметрия по MQTT поверх поднятого PPP-канала.
//!
//! Раз в [`config::MQTT_PUBLISH_SECS`] публикуется уровень сигнала в
//! [`config::MQTT_TOPIC_CSQ`]. Зелёный светодиод управляется извне: команда
//! `ON`, `OFF`, `BLINK` или `TOGGLE` в [`config::MQTT_TOPIC_LED`].
//!
//! Отправка SMS: номер последним сегментом топика
//! [`config::MQTT_TOPIC_SMS_SEND`], текст телом сообщения.
//!
//! Сам светодиод живёт в [`crate::led`] — здесь только разбор команды. Режим
//! задаётся исключительно командами и переживает переподключение к брокеру:
//! гасить светодиод при обрыве значило бы врать о состоянии, которое никто
//! не менял.
//!
//! # Про retain
//!
//! Обе публикации идут с флагом `retain`: брокер хранит последнее сообщение
//! в топике и отдаёт его любому новому подписчику. Без этого увидеть значение
//! может только тот, кто был подписан ровно в момент публикации, — а
//! телеметрия раз в минуту означает минуту пустого экрана.
//!
//! Обратная сторона, о которой стоит помнить: сохранённое SMS висит на брокере
//! до перезаписи и на публичном брокере доступно кому угодно. Для настоящего
//! применения это довод либо за свой брокер, либо за отключение retain на
//! топике SMS.
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

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_futures::select::{Either4, select4};
use embassy_net::Stack;
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use rust_mqtt::buffer::BumpBuffer;
use rust_mqtt::client::Client;
use rust_mqtt::client::event::{Event, Publish};
use rust_mqtt::client::options::{
    ConnectOptions, PublicationOptions, SubscriptionOptions, TopicReference,
};
use rust_mqtt::config::{KeepAlive, SessionExpiryInterval};
use rust_mqtt::types::{MqttString, TopicFilter, TopicName};

use crate::coin_io;
use crate::config;
use crate::led;
use crate::modem;

/// Принятое SMS, ожидающее публикации.
pub type SmsText = heapless::String<384>;

/// Очередь SMS от AT-канала к брокеру.
///
/// Очередь, а не общая ячейка: сообщения нельзя терять и нельзя заставлять
/// модем ждать, пока GPRS отдаст публикацию. Отправитель кладёт без блокировки
/// и роняет сообщение, если очередь переполнена — лучше потерять одно SMS,
/// чем застопорить разбор AT-канала.
pub static SMS_QUEUE: Channel<CriticalSectionRawMutex, SmsText, 4> = Channel::new();

/// Событие монетоприёмника, ожидающее публикации.
pub type CoinText = heapless::String<64>;

/// Очередь событий от монетоприёмника к брокеру.
///
/// Отдельная от SMS: монеты идут чаще и терять их обиднее, а смешивать
/// очереди значило бы, что одно длинное SMS задержит выдачу сдачи.
pub static COIN_QUEUE: Channel<CriticalSectionRawMutex, CoinText, 8> = Channel::new();

/// SMS, которое нужно отправить: номер цифрами и текст.
pub struct OutgoingSms {
    pub number: heapless::String<24>,
    pub text: heapless::String<{ modem::SMS_MAX_LEN }>,
}

/// Очередь на отправку — от брокера к тому, кто держит AT-канал.
///
/// Зеркало [`SMS_QUEUE`], только в обратную сторону. Отправка занимает
/// секунды и требует модема, поэтому делать её прямо в разборе публикации
/// нельзя: MQTT-клиент за это время потеряет соединение по keepalive.
pub static SMS_SEND_QUEUE: Channel<CriticalSectionRawMutex, OutgoingSms, 4> = Channel::new();

/// Держится ли сейчас соединение с брокером. Читает экран.
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

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
pub async fn mqtt_task(stack: Stack<'static>) -> ! {
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];
    let mut storage = [0u8; MQTT_BUFFER];

    loop {
        stack.wait_config_up().await;

        let outcome = session(stack, &mut rx_buffer, &mut tx_buffer, &mut storage).await;
        CONNECTED.store(false, Ordering::Relaxed);
        if let Err(()) = outcome {
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
    // Первый параметр — сколько SUBSCRIBE может быть в полёте одновременно.
    // Их четыре: светодиод, две блокировки монетоприёмника и отправка SMS.
    // `subscribe()` не ждёт SUBACK, он только отправляет пакет, поэтому с
    // запасом в единицу вторая подписка отвалилась бы с `SessionBuffer`.
    let mut client = Client::<'_, _, _, 4, 1, 1, 1>::new(&mut buffer);

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
    CONNECTED.store(true, Ordering::Relaxed);

    for name in [
        config::MQTT_TOPIC_LED,
        config::MQTT_TOPIC_COIN_BLOCK,
        config::MQTT_TOPIC_COIN_TOTAL,
        config::MQTT_TOPIC_SMS_SEND_FILTER,
    ] {
        let filter = topic_filter(name)?;
        client
            .subscribe(filter.as_borrowed(), SubscriptionOptions::new())
            .await
            .map_err(|e| warn!("MQTT: подписка на {} не удалась: {:?}", name, e))?;
        info!("MQTT: подписаны на {}", name);
        // SAFETY: заимствований из буфера после подписки нет.
        unsafe { client.buffer_mut().reset() };
    }

    let topic = topic_name(config::MQTT_TOPIC_CSQ)?;

    // Дедлайн, а не пересоздаваемый таймер: событие от poll() не должно
    // сдвигать момент следующей публикации.
    let mut deadline = Instant::now();

    let sms_topic = topic_name(config::MQTT_TOPIC_SMS)?;
    let coin_topic = topic_name(config::MQTT_TOPIC_COIN)?;

    loop {
        match select4(
            Timer::at(deadline),
            client.poll(),
            SMS_QUEUE.receive(),
            COIN_QUEUE.receive(),
        )
        .await
        {
            Either4::First(()) => {
                let csq = LAST_CSQ.load(Ordering::Relaxed);
                let mut payload = heapless::String::<8>::new();
                let _ = core::fmt::Write::write_fmt(&mut payload, format_args!("{csq}"));

                client
                    .publish(
                        // QoS 0 — значение по умолчанию: телеметрию не жалко
                        // потерять, и никакого состояния для повторной доставки.
                        // retain — чтобы подписчик увидел текущее значение
                        // сразу, а не через минуту.
                        &PublicationOptions::new(TopicReference::Name(topic.as_borrowed()))
                            .retain(),
                        payload.as_bytes().into(),
                    )
                    .await
                    .map_err(|e| warn!("MQTT: публикация не удалась: {:?}", e))?;

                info!("MQTT: {} <- CSQ {}", config::MQTT_TOPIC_CSQ, csq);
                // SAFETY: заимствований из буфера после публикации нет.
                unsafe { client.buffer_mut().reset() };

                deadline = Instant::now() + Duration::from_secs(config::MQTT_PUBLISH_SECS);
            }
            Either4::Second(event) => {
                let event = event.map_err(|e| warn!("MQTT: соединение потеряно: {:?}", e))?;
                match event {
                    Event::Publish(publication) => dispatch(&publication),
                    other => debug!("MQTT: событие {:?}", other),
                }
            }
            Either4::Third(sms) => {
                client
                    .publish(
                        &PublicationOptions::new(TopicReference::Name(sms_topic.as_borrowed()))
                            .retain(),
                        sms.as_bytes().into(),
                    )
                    .await
                    .map_err(|e| warn!("MQTT: SMS не опубликовано: {:?}", e))?;
                info!(
                    "MQTT: {} <- SMS ({} байт)",
                    config::MQTT_TOPIC_SMS,
                    sms.len()
                );
                // SAFETY: заимствований из буфера после публикации нет.
                unsafe { client.buffer_mut().reset() };
            }
            Either4::Fourth(coin) => {
                client
                    .publish(
                        // Без retain: событие монеты имеет смысл в момент
                        // наступления, а не как последнее известное состояние.
                        &PublicationOptions::new(TopicReference::Name(coin_topic.as_borrowed())),
                        coin.as_bytes().into(),
                    )
                    .await
                    .map_err(|e| warn!("MQTT: событие монеты не опубликовано: {:?}", e))?;
                info!("MQTT: {} <- {}", config::MQTT_TOPIC_COIN, coin.as_str());
                // SAFETY: заимствований из буфера после публикации нет.
                unsafe { client.buffer_mut().reset() };
            }
        }
    }
}

/// Собрать имя топика из строки настроек.
///
/// Ошибка возможна только при опечатке в `config` — длиннее 65535 байт или с
/// подстановочным знаком, — но проверить дешевле, чем ловить это на стенде.
fn topic_name(name: &str) -> Result<TopicName<'_>, ()> {
    TopicName::new(
        MqttString::try_from(name).map_err(|_| warn!("MQTT: слишком длинный топик {}", name))?,
    )
    .ok_or_else(|| warn!("MQTT: недопустимый топик {}", name))
}

/// Собрать фильтр подписки.
///
/// Отдельно от [`topic_name`], и это не педантизм: имя топика и фильтр —
/// разные типы с разными правилами. В имени подстановочные знаки запрещены,
/// поэтому `TopicName` отвергает `sms/send/+`, а без фильтра на этот топик не
/// подписаться вовсе.
fn topic_filter(name: &str) -> Result<TopicFilter<'_>, ()> {
    TopicFilter::new(
        MqttString::try_from(name).map_err(|_| warn!("MQTT: слишком длинный фильтр {}", name))?,
    )
    .ok_or_else(|| warn!("MQTT: недопустимый фильтр подписки {}", name))
}

/// Разослать входящую публикацию по её топику.
///
/// Подписок несколько, а событие приходит одно — различаем по точному имени
/// топика. Подстановочных знаков в подписках нет, поэтому сравнение строк
/// здесь и есть полное сопоставление.
fn dispatch<const N: usize>(publication: &Publish<'_, N>) {
    let topic: &str = publication.topic.as_ref().as_ref();

    match topic {
        config::MQTT_TOPIC_LED => apply_led_command(&publication.message),
        config::MQTT_TOPIC_COIN_BLOCK => coin_io::apply_block_command(&publication.message),
        config::MQTT_TOPIC_COIN_TOTAL => coin_io::apply_total_command(&publication.message),
        other if other.starts_with(config::MQTT_TOPIC_SMS_SEND) => {
            queue_sms(&other[config::MQTT_TOPIC_SMS_SEND.len()..], &publication.message)
        }
        other => warn!("MQTT: публикация в неожиданном топике {}", other),
    }
}

/// Поставить SMS в очередь на отправку.
///
/// Номер приходит последним сегментом топика, текст — телом. Проверяем оба
/// здесь, чтобы отказ был виден в логе рядом с командой, а не через минуту в
/// глубине AT-канала.
fn queue_sms(number: &str, payload: &[u8]) {
    let Ok(text) = core::str::from_utf8(payload) else {
        warn!("SMS: текст не в UTF-8");
        return;
    };
    if !modem::valid_phone(number) {
        warn!("SMS: номер в топике не годится, ожидаются только цифры без плюса");
        return;
    }
    if !modem::sms_text_is_sendable(text) {
        warn!(
            "SMS: текст не отправить ({} байт): нужен ASCII не длиннее {}",
            text.len(),
            modem::SMS_MAX_LEN
        );
        return;
    }

    let (Ok(number), Ok(text)) = (
        heapless::String::try_from(number),
        heapless::String::try_from(text),
    ) else {
        warn!("SMS: не поместилось в буфер");
        return;
    };

    // Не блокируемся: очередь переполняется только когда модем занят подъёмом
    // канала, а держать из-за этого MQTT-клиента нельзя — он отвалится по
    // keepalive.
    if SMS_SEND_QUEUE.try_send(OutgoingSms { number, text }).is_err() {
        warn!("SMS: очередь на отправку переполнена, сообщение потеряно");
    }
}

/// Разобрать команду светодиода: `ON`, `OFF`, `BLINK` или `TOGGLE`.
///
/// Регистр и обрамляющие пробелы игнорируются — клиенты и брокеры шлют
/// по-разному, и договориться об этом дешевле один раз здесь.
///
/// `TOGGLE` стоит особняком: остальные команды задают состояние, а эта зависит
/// от текущего, поэтому и смена делается в [`led::toggle`] одним действием.
/// Иначе две команды, пришедшие подряд, могли бы схлопнуться в одну.
fn apply_led_command(payload: &[u8]) {
    let trimmed = payload.trim_ascii();

    let mode = if trimmed.eq_ignore_ascii_case(b"ON") {
        led::Mode::On
    } else if trimmed.eq_ignore_ascii_case(b"OFF") {
        led::Mode::Off
    } else if trimmed.eq_ignore_ascii_case(b"BLINK") {
        led::Mode::Blink
    } else if trimmed.eq_ignore_ascii_case(b"TOGGLE") {
        let mode = led::toggle();
        info!("MQTT: светодиод -> {:?} (переключение)", mode);
        return;
    } else {
        warn!(
            "MQTT: непонятная команда светодиода ({} байт), ожидается ON, OFF, BLINK или TOGGLE",
            trimmed.len()
        );
        return;
    };

    led::set_mode(mode);
    info!("MQTT: светодиод -> {:?}", mode);
}
