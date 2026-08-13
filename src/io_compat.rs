//! Мост между `embedded-io-async` 0.7 (её использует `embassy-rp`) и 0.6
//! (её использует `atat` 0.24).
//!
//! Обе версии в графе зависимостей живут одновременно, но их трейты — разные
//! типы, поэтому `BufferedUartTx` не подходит под `atat::asynch::Client`
//! напрямую. [`Compat`] — прозрачная обёртка, которая переносит `Read`/`Write`
//! из 0.7 в 0.6.
//!
//! Как только `atat` обновится на `embedded-io-async` 0.7, этот модуль можно
//! удалить, а `Compat(x)` заменить на `x`.

#![allow(async_fn_in_trait)]

use embedded_io::{Error as _, ErrorKind as Kind07};
use embedded_io_06::{Error as Error06, ErrorKind as Kind06, ErrorType as ErrorType06};
use embedded_io_async::{Read as Read07, Write as Write07};
use embedded_io_async_06::{Read as Read06, Write as Write06};

/// Обёртка над транспортом `embedded-io-async` 0.7.
pub struct Compat<T>(pub T);

/// Ошибка, приведённая к системе типов `embedded-io` 0.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "_defmt", derive(defmt::Format))]
pub struct CompatError(Kind07);

impl Error06 for CompatError {
    fn kind(&self) -> Kind06 {
        match self.0 {
            Kind07::NotFound => Kind06::NotFound,
            Kind07::PermissionDenied => Kind06::PermissionDenied,
            Kind07::ConnectionRefused => Kind06::ConnectionRefused,
            Kind07::ConnectionReset => Kind06::ConnectionReset,
            Kind07::ConnectionAborted => Kind06::ConnectionAborted,
            Kind07::NotConnected => Kind06::NotConnected,
            Kind07::AddrInUse => Kind06::AddrInUse,
            Kind07::AddrNotAvailable => Kind06::AddrNotAvailable,
            Kind07::BrokenPipe => Kind06::BrokenPipe,
            Kind07::AlreadyExists => Kind06::AlreadyExists,
            Kind07::InvalidInput => Kind06::InvalidInput,
            Kind07::InvalidData => Kind06::InvalidData,
            Kind07::TimedOut => Kind06::TimedOut,
            Kind07::Interrupted => Kind06::Interrupted,
            Kind07::Unsupported => Kind06::Unsupported,
            Kind07::OutOfMemory => Kind06::OutOfMemory,
            Kind07::WriteZero => Kind06::WriteZero,
            _ => Kind06::Other,
        }
    }
}

impl<T> ErrorType06 for Compat<T>
where
    T: embedded_io::ErrorType,
{
    type Error = CompatError;
}

impl<T> Read06 for Compat<T>
where
    T: Read07,
{
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.0.read(buf).await.map_err(|e| CompatError(e.kind()))
    }
}

impl<T> Write06 for Compat<T>
where
    T: Write07,
{
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.0.write(buf).await.map_err(|e| CompatError(e.kind()))
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.0.flush().await.map_err(|e| CompatError(e.kind()))
    }
}
