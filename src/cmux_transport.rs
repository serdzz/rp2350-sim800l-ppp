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

// Пока никто не вызывается: рабочий путь в main.rs идёт без мультиплексора.
// Снять, когда транспорт будет подключён.
#![allow(dead_code)]

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::pipe::{DynamicReader, DynamicWriter};
use embedded_io::ErrorKind;
use embedded_io_async::{BufRead, ErrorType, Read, Write};

use crate::cmux::{Decoder, Event, Frame, Session, UihFraming};

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

/// Логический канал: выглядит как обычный поток байт.
///
/// Реализует `Read`, `BufRead` и `Write` из `embedded-io-async`, поэтому
/// отдаётся `embassy_net_ppp::Runner::run` напрямую. Для `atat`, который сидит
/// на версии 0.6, понадобится обёртка `Compat` из [`crate::io_compat`].
pub struct Channel<'a, T: Write> {
    dlci: u8,
    rx: DynamicReader<'a>,
    tx: &'a SharedTx<T>,
    max_payload: usize,
}

impl<'a, T: Write> Channel<'a, T> {
    /// `rx` — читающая половина канальной трубы, которую наполняет [`pump`].
    /// `max_payload` — значение N1 из `AT+CMUX`.
    pub fn new(dlci: u8, rx: DynamicReader<'a>, tx: &'a SharedTx<T>, max_payload: usize) -> Self {
        Self {
            dlci,
            rx,
            tx,
            max_payload,
        }
    }

    pub fn dlci(&self) -> u8 {
        self.dlci
    }
}

impl<T: Write> ErrorType for Channel<'_, T> {
    type Error = Error;
}

impl<T: Write> Read for Channel<'_, T> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        Ok(self.rx.read(buf).await)
    }
}

impl<T: Write> BufRead for Channel<'_, T> {
    async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        Ok(self.rx.fill_buf().await)
    }

    fn consume(&mut self, amt: usize) {
        self.rx.consume(amt);
    }
}

impl<T: Write> Write for Channel<'_, T> {
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
    loop {
        let read = match uart_rx.read(&mut chunk).await {
            Ok(0) => continue,
            Ok(n) => n,
            Err(_) => {
                warn!("CMUX: ошибка чтения UART");
                decoder.reset();
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
                Event::Data { dlci, payload } => match routes.iter().find(|r| r.dlci == dlci) {
                    Some(route) => write_all(&route.sink, payload).await,
                    None => debug!("CMUX: данные для незанятого DLCI {}", dlci),
                },
                Event::Opened(dlci) => info!("CMUX: канал {} открыт", dlci),
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
