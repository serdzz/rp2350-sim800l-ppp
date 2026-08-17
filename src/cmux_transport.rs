//! Раздача каналов CMUX поверх одного UART.
//!
//! Логика протокола лежит в [`crate::cmux`] и проверяется на хосте. Здесь —
//! только обвязка под embassy: кто владеет портом и как байты попадают из
//! линии в нужный канал.
//!
//! ```text
//!                    ┌──────────────┐  UIH(DLCI 1)   ┌─────────────┐
//!   UART RX ─► pump ─┤ cmux::Decoder├───────────────►│ Pipe → atat │
//!                    │ cmux::Session│  UIH(DLCI 2)   ├─────────────┤
//!                    └──────────────┘───────────────►│ Pipe → PPP  │
//!                                                    └─────────────┘
//!   UART TX ◄──── Mutex ◄──── Channel::write (обрамляет в UIH)
//! ```
//!
//! # Два инварианта, на которых всё держится
//!
//! 1. **Передатчик под мьютексом.** Кадр уходит в линию тремя записями, и если
//!    между ними вклинится другой канал, оба кадра погибнут. Мьютекс держится
//!    на все три записи — см. [`Channel::write`].
//! 2. **Запись не длиннее `max_payload`.** [`Channel::write`] возвращает число
//!    принятых байт, и вызывающая сторона обязана смотреть на него. Это
//!    штатное поведение `embedded_io_async::Write`, а `write_all` разложит
//!    длинную запись на несколько кадров сам.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::pipe::{DynamicReader, DynamicWriter};
use embedded_io::ErrorKind;
use embedded_io_async::{BufRead, ErrorType, Read, Write};

use embassy_time::{Duration, Instant, Timer};

use crate::cmux::{
    CONTROL_DLCI, ChannelState, Decoder, Event, Frame, Session, UihFraming, control,
};

/// Передатчик, общий на все каналы.
pub type SharedTx<T> = Mutex<CriticalSectionRawMutex, T>;
/// Состояние мультиплексора, общее для насоса и того, кто открывает каналы.
pub type SharedSession = Mutex<CriticalSectionRawMutex, Session>;

/// Ошибка канала.
///
/// Тип ошибки нижележащего UART стирается: потребителям ([`atat`], PPP) важен
/// лишь факт сбоя, а обобщать по нему все структуры — лишний шум.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub enum Error {
    /// Не удалось записать в UART.
    Write,
    /// Кадр не собрался: DLCI или длина вне допустимого.
    Framing,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Write => f.write_str("cmux: ошибка записи в UART"),
            Self::Framing => f.write_str("cmux: кадр не собрался"),
        }
    }
}

// `embedded_io::Error` в 0.7 требует `core::error::Error`, а тот — `Display`.
impl core::error::Error for Error {}

impl embedded_io::Error for Error {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

/// Читающая половина канала.
pub struct ChannelRx<'a> {
    rx: DynamicReader<'a>,
}

/// Пишущая половина канала: обрамляет данные в UIH и отдаёт в общий UART.
pub struct ChannelTx<'a, T: Write> {
    dlci: u8,
    tx: &'a SharedTx<T>,
    max_payload: usize,
}

/// Логический канал целиком.
///
/// Реализует `Read`, `BufRead` и `Write` из `embedded-io-async`, поэтому
/// отдаётся `embassy_net_ppp::Runner::run` напрямую. `atat` же хочет чтение и
/// запись отдельными объектами — для него канал делится [`Channel::split`].
pub struct Channel<'a, T: Write> {
    rx: ChannelRx<'a>,
    tx: ChannelTx<'a, T>,
}

impl<'a, T: Write> Channel<'a, T> {
    /// `rx` — читающая половина канальной трубы, которую наполняет [`pump`].
    /// `max_payload` — значение N1 из `AT+CMUX`.
    pub fn new(dlci: u8, rx: DynamicReader<'a>, tx: &'a SharedTx<T>, max_payload: usize) -> Self {
        Self {
            rx: ChannelRx { rx },
            tx: ChannelTx {
                dlci,
                tx,
                max_payload,
            },
        }
    }

    #[allow(dead_code)] // пригодится при отладке маршрутизации
    pub fn dlci(&self) -> u8 {
        self.tx.dlci
    }

    /// Разделить на половины — так канал подходит `atat`.
    pub fn split(self) -> (ChannelRx<'a>, ChannelTx<'a, T>) {
        (self.rx, self.tx)
    }
}

impl ErrorType for ChannelRx<'_> {
    type Error = Error;
}

impl Read for ChannelRx<'_> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(self.rx.read(buf).await)
    }
}

impl BufRead for ChannelRx<'_> {
    async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        Ok(self.rx.fill_buf().await)
    }

    fn consume(&mut self, amt: usize) {
        self.rx.consume(amt);
    }
}

impl<T: Write> ErrorType for ChannelTx<'_, T> {
    type Error = Error;
}

impl<T: Write> Write for ChannelTx<'_, T> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        let n = buf.len().min(self.max_payload);
        let framing = UihFraming::new(self.dlci, n).map_err(|_| Error::Framing)?;

        // Мьютекс держим на все три записи: кадр в линии обязан быть цельным.
        let mut tx = self.tx.lock().await;
        tx.write_all(framing.header())
            .await
            .map_err(|_| Error::Write)?;
        tx.write_all(&buf[..n]).await.map_err(|_| Error::Write)?;
        tx.write_all(framing.trailer())
            .await
            .map_err(|_| Error::Write)?;
        Ok(n)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        let mut tx = self.tx.lock().await;
        tx.flush().await.map_err(|_| Error::Write)
    }
}

impl<T: Write> ErrorType for Channel<'_, T> {
    type Error = Error;
}

impl<T: Write> Read for Channel<'_, T> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.rx.read(buf).await
    }
}

impl<T: Write> BufRead for Channel<'_, T> {
    async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        self.rx.fill_buf().await
    }

    fn consume(&mut self, amt: usize) {
        self.rx.consume(amt);
    }
}

impl<T: Write> Write for Channel<'_, T> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.tx.write(buf).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.tx.flush().await
    }
}

/// Куда складывать данные конкретного DLCI.
pub struct Route<'a> {
    pub dlci: u8,
    pub sink: DynamicWriter<'a>,
}

/// Отправить служебный кадр (SABM, DISC, UA, DM).
///
/// У таких кадров поле данных пустое, поэтому буфера на восемь байт хватает
/// с запасом.
pub async fn send_frame<T: Write>(tx: &SharedTx<T>, frame: &Frame<'_>) -> Result<(), Error> {
    let mut buf = [0u8; 8];
    let n = frame.encode(&mut buf).map_err(|_| Error::Framing)?;
    let mut tx = tx.lock().await;
    tx.write_all(&buf[..n]).await.map_err(|_| Error::Write)
}

/// Отправить сообщение управляющего канала: UIH на DLCI 0 с телом
/// «тип — длина — значение».
pub async fn send_control_message<T: Write>(
    tx: &SharedTx<T>,
    bits: u8,
    command: bool,
    value: &[u8],
) -> Result<(), Error> {
    let mut message = [0u8; 8];
    let len = control::encode(bits, command, value, &mut message).map_err(|_| Error::Framing)?;

    let mut buf = [0u8; 16];
    let n = Frame::uih(CONTROL_DLCI, &message[..len])
        .encode(&mut buf)
        .map_err(|_| Error::Framing)?;

    let mut tx = tx.lock().await;
    tx.write_all(&buf[..n]).await.map_err(|_| Error::Write)
}

/// Сообщить модему, что канал готов к обмену (MSC с сигналами V.24).
///
/// Спецификация (§5.4.6.3.7) требует слать это до любых пользовательских
/// данных на только что созданном канале.
pub async fn announce_channel<T: Write>(tx: &SharedTx<T>, dlci: u8) -> Result<(), Error> {
    // Значение MSC: октет DLCI (бит 2 всегда единичный) и октет сигналов.
    let value = [(dlci << 2) | 0b11, control::V24Signals::DTE_READY.encode()];
    send_control_message(tx, control::MSC, true, &value).await
}

/// Попросить модем выйти из мультиплексного режима (CLD, §5.4.6.3.3).
///
/// Пока не вызывается: сеанс завершается сбросом модема через `+++`/`ATH`,
/// что надёжнее в ситуации, когда мультиплексор уже развалился.
#[allow(dead_code)]
pub async fn close_multiplexer<T: Write>(tx: &SharedTx<T>) -> Result<(), Error> {
    send_control_message(tx, control::CLD, true, &[]).await
}

/// Разобрать и обслужить сообщение, пришедшее по управляющему каналу.
///
/// По §5.4.6.2 сообщения ходят парами команда-ответ, и **ответ несёт те же
/// биты типа, что и команда**. Поэтому отвечаем, не полагаясь на таблицу
/// известных типов: даже незнакомую команду можно корректно отбить.
///
/// Управление потоком (`FCon`/`FCoff`) пока только показывается в логе, но не
/// исполняется: остановка передачи по всем каналам, кроме нулевого, — это
/// отдельная работа в [`Channel::write`].
async fn handle_control_message<T: Write>(tx: &SharedTx<T>, payload: &[u8]) {
    let Some((bits, command, value)) = control::decode(payload) else {
        warn!("CMUX: неразбираемое сообщение управляющего канала");
        return;
    };

    if !command {
        debug!("CMUX: ответ управляющего канала, тип 0x{:02x}", bits);
        return;
    }

    let reply = match bits {
        control::MSC => {
            // §5.4.6.3.7: в ответе возвращаются те же сигналы, что пришли.
            if let Some(&dlci_octet) = value.first() {
                debug!("CMUX: MSC для DLCI {}", dlci_octet >> 2);
            }
            Some(value)
        }
        control::CLD => {
            warn!("CMUX: модем закрывает мультиплексор (CLD)");
            Some(&[][..])
        }
        control::FCON => {
            info!("CMUX: модем снял стоп потока (FCon)");
            Some(&[][..])
        }
        control::FCOFF => {
            warn!("CMUX: модем просит остановить поток (FCoff) — не исполняется");
            Some(&[][..])
        }
        other => {
            warn!("CMUX: неизвестная управляющая команда 0x{:02x}", other);
            // Отвечаем NSC, вложив октет типа непонятой команды (§5.4.6.3.8).
            let unsupported = [control::type_octet(other, true)];
            if send_control_message(tx, control::NSC, false, &unsupported)
                .await
                .is_err()
            {
                warn!("CMUX: не удалось отправить NSC");
            }
            None
        }
    };

    if let Some(value) = reply
        && send_control_message(tx, bits, false, value).await.is_err()
    {
        warn!("CMUX: не удалось ответить по управляющему каналу");
    }
}

/// Что помешало поднять мультиплексор.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub enum BringUpError {
    /// Не удалось записать в UART.
    Io(Error),
    /// Модем не подтвердил открытие канала за отведённое время.
    Timeout(u8),
    /// Канал оказался в неожиданном состоянии.
    State(u8),
}

impl From<Error> for BringUpError {
    fn from(e: Error) -> Self {
        Self::Io(e)
    }
}

/// Как часто опрашивать состояние канала, ожидая подтверждения.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Пауза после неудачного чтения UART.
///
/// Существует не ради экономии, а чтобы не заморозить всю прошивку. Ошибка
/// приёма (Break, Framing, Overrun) возвращается **сразу**, без ожидания, и
/// `.await` на готовом будущем исполнителю не уступает. Если линия залипла —
/// модем обесточился и держит её в нуле, — ошибки идут непрерывно, и цикл без
/// паузы съедает процессор целиком: перестают идти PPP, MQTT, USB-лог и
/// перерисовка экрана, хотя сама задача выглядит живой.
///
/// С паузой голодания нет, и обрыв замечают те, кому положено: PPP-сессия
/// падает по своему таймауту, [`crate::main`] поднимает связь заново.
const READ_ERROR_BACKOFF: Duration = Duration::from_millis(5);

/// Дождаться, пока канал перейдёт в нужное состояние.
///
/// Подтверждение приходит в [`pump`], в другой задаче, поэтому смотрим на
/// общее состояние. Опрос, а не сигнал: это происходит один раз при подъёме,
/// и лишний примитив синхронизации ради него не окупается.
async fn wait_state(
    session: &SharedSession,
    dlci: u8,
    want: ChannelState,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if session.lock().await.state(dlci) == want {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        Timer::after(POLL_INTERVAL).await;
    }
}

/// Открыть канал: SABM и ожидание UA, с повторами.
///
/// Повторы обязательны: при входе в мультиплексный режим модем нередко
/// пропускает первый SABM, пока переключает разбор входного потока.
pub async fn open_channel<T: Write>(
    tx: &SharedTx<T>,
    session: &SharedSession,
    dlci: u8,
    attempts: u32,
    timeout: Duration,
) -> Result<(), BringUpError> {
    for attempt in 1..=attempts {
        let sabm = {
            let mut session = session.lock().await;
            if session.state(dlci) == ChannelState::Open {
                return Ok(());
            }
            // Предыдущая попытка могла оставить канал в Opening.
            session.force_closed(dlci);
            session.open(dlci).map_err(|_| BringUpError::State(dlci))?
        };

        send_frame(tx, &sabm).await?;

        if wait_state(session, dlci, ChannelState::Open, timeout).await {
            info!("CMUX: канал {} открыт (попытка {})", dlci, attempt);
            return Ok(());
        }
        warn!("CMUX: канал {} не подтверждён, попытка {}", dlci, attempt);
    }

    Err(BringUpError::Timeout(dlci))
}

/// Какие каналы поднимать.
#[derive(Debug, Clone, Copy)]
pub struct Channels {
    /// Канал под AT-команды.
    pub at: u8,
    /// Канал под PPP.
    pub ppp: u8,
    pub attempts: u32,
    pub timeout: Duration,
}

/// Поднять мультиплексор: управляющий канал, затем каналы данных.
///
/// Вызывать уже после того, как модем принял `AT+CMUX` и [`pump`] запущен:
/// подтверждения приходят только через насос.
pub async fn bring_up<T: Write>(
    tx: &SharedTx<T>,
    session: &SharedSession,
    channels: Channels,
) -> Result<(), BringUpError> {
    // Управляющий канал первым — через него идут MSC и CLD (§5.4.6).
    open_channel(
        tx,
        session,
        CONTROL_DLCI,
        channels.attempts,
        channels.timeout,
    )
    .await?;

    for dlci in [channels.at, channels.ppp] {
        open_channel(tx, session, dlci, channels.attempts, channels.timeout).await?;
        // Сигналы V.24 до первых данных, как требует спецификация.
        announce_channel(tx, dlci).await?;
    }

    info!("CMUX: мультиплексор поднят");
    Ok(())
}

/// Насос: читает UART, разбирает кадры и разводит их по каналам.
///
/// Не возвращается. Ошибки чтения не фатальны — декодер пересинхронизируется
/// на следующем флаге, поэтому насос переживает и мусор в линии, и
/// перезагрузку модема.
pub async fn pump<R, T, const RX: usize>(
    uart_rx: &mut R,
    tx: &SharedTx<T>,
    decoder: &mut Decoder<RX>,
    session: &SharedSession,
    routes: &mut [Route<'_>],
) -> !
where
    R: Read,
    T: Write,
{
    let mut chunk = [0u8; 64];
    let mut error_burst = 0u32;
    loop {
        let read = match uart_rx.read(&mut chunk).await {
            // Пустое чтение возвращается сразу, не дожидаясь байтов, поэтому
            // `continue` без паузы был бы холостым циклом — см. ниже.
            Ok(0) => {
                Timer::after(READ_ERROR_BACKOFF).await;
                continue;
            }
            Ok(n) => {
                if error_burst > 0 {
                    warn!(
                        "CMUX: линия восстановилась, ошибок чтения подряд: {}",
                        error_burst
                    );
                    error_burst = 0;
                }
                n
            }
            Err(_) => {
                // Первую ошибку показываем, остальные копим: при залипшей
                // линии их тысячи в секунду, и лог станет бесполезен.
                if error_burst == 0 {
                    warn!("CMUX: ошибка чтения UART");
                }
                error_burst = error_burst.saturating_add(1);
                decoder.reset();
                Timer::after(READ_ERROR_BACKOFF).await;
                continue;
            }
        };

        for &byte in &chunk[..read] {
            let Some(result) = decoder.push(byte) else {
                continue;
            };

            let frame = match result {
                Ok(frame) => frame,
                Err(e) => {
                    debug!("CMUX: кадр отброшен: {:?}", e);
                    continue;
                }
            };

            // Событие считаем под замком, а отправку ответа делаем уже без
            // него: иначе блокировка на записи в UART задержала бы тех, кто
            // всего лишь спрашивает состояние канала.
            let event = {
                let mut session = session.lock().await;
                session.on_frame(&frame)
            };

            match event {
                // Нулевой канал служебный: его содержимое — не данные
                // потребителя, а сообщения самого мультиплексора.
                Event::Data { dlci, payload } if dlci == CONTROL_DLCI => {
                    handle_control_message(tx, payload).await
                }
                Event::Data { dlci, payload } => match routes.iter().find(|r| r.dlci == dlci) {
                    Some(route) => write_all(&route.sink, payload).await,
                    None => debug!("CMUX: данные для незанятого DLCI {}", dlci),
                },
                // Об успешном открытии докладывает open_channel — здесь только
                // отладочный след, иначе строка двоится.
                Event::Opened(dlci) => debug!("CMUX: подтверждение открытия канала {}", dlci),
                Event::Closed(dlci) => info!("CMUX: канал {} закрыт", dlci),
                Event::RemoteOpen { dlci, reply } | Event::RemoteDisconnect { dlci, reply } => {
                    info!("CMUX: модем изменил состояние канала {}", dlci);
                    if send_frame(tx, &reply).await.is_err() {
                        warn!("CMUX: не удалось ответить по каналу {}", dlci);
                    }
                }
                Event::Ignored => {}
            }
        }
    }
}

/// Дописать всё в трубу канала.
///
/// `DynamicWriter` за раз принимает столько, сколько влезло, поэтому крутим
/// цикл. Если потребитель канала не читает, здесь мы и будем ждать — это
/// естественный обратный напор: насос перестаёт разбирать линию, UART
/// упирается в аппаратный буфер, и модем притормаживает.
async fn write_all(sink: &DynamicWriter<'_>, mut data: &[u8]) {
    while !data.is_empty() {
        let n = sink.write(data).await;
        data = &data[n..];
    }
}
