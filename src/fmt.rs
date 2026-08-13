//! Единый фасад логирования: `trace!` / `debug!` / `info!` / `warn!` /
//! `error!` / `unwrap!` разворачиваются либо в `defmt`, либо в `log` —
//! в зависимости от выбранного feature-флага (см. `Cargo.toml`).
//!
//! Модуль объявлен с `#![macro_use]`, поэтому макросы видны во всём крейте
//! без импортов. `mod fmt;` в `main.rs` обязан идти **до** остальных модулей.
//!
//! Форматные строки должны быть совместимы с обоими бэкендами:
//! `{}` требует `Display` (log) / `Format` (defmt), `{:?}` — `Debug` (log) /
//! `Format` (defmt). Поэтому собственные типы получают `#[derive(Debug)]`
//! всегда и `defmt::Format` — через `#[cfg_attr(feature = "_defmt", ...)]`.

#![macro_use]
#![allow(unused_macros)]

#[cfg(all(feature = "_defmt", feature = "_log"))]
compile_error!("Выберите ровно один бэкенд логирования: log-rtt ИЛИ log-usb.");

#[cfg(not(any(feature = "_defmt", feature = "_log")))]
compile_error!("Не выбран бэкенд логирования: включите feature log-rtt или log-usb.");

macro_rules! trace {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "_defmt")]
            ::defmt::trace!($s $(, $x)*);
            #[cfg(feature = "_log")]
            ::log::trace!($s $(, $x)*);
        }
    };
}

macro_rules! debug {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "_defmt")]
            ::defmt::debug!($s $(, $x)*);
            #[cfg(feature = "_log")]
            ::log::debug!($s $(, $x)*);
        }
    };
}

macro_rules! info {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "_defmt")]
            ::defmt::info!($s $(, $x)*);
            #[cfg(feature = "_log")]
            ::log::info!($s $(, $x)*);
        }
    };
}

macro_rules! warn {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "_defmt")]
            ::defmt::warn!($s $(, $x)*);
            #[cfg(feature = "_log")]
            ::log::warn!($s $(, $x)*);
        }
    };
}

macro_rules! error {
    ($s:literal $(, $x:expr)* $(,)?) => {
        {
            #[cfg(feature = "_defmt")]
            ::defmt::error!($s $(, $x)*);
            #[cfg(feature = "_log")]
            ::log::error!($s $(, $x)*);
        }
    };
}

/// Аналог `defmt::unwrap!`, работающий в обоих режимах.
macro_rules! unwrap {
    ($e:expr $(,)?) => {
        match $crate::fmt::Try::into_result($e) {
            ::core::result::Result::Ok(value) => value,
            ::core::result::Result::Err(_) => {
                $crate::fmt::unwrap_failed(::core::concat!(
                    ::core::file!(),
                    ":",
                    ::core::line!()
                ))
            }
        }
    };
}

/// Вызывается из `unwrap!`; вынесено в функцию, чтобы макрос не тащил
/// форматирование в каждую точку вызова.
#[cold]
#[inline(never)]
pub fn unwrap_failed(location: &str) -> ! {
    #[cfg(feature = "_defmt")]
    ::defmt::panic!("unwrap failed at {}", location);
    #[cfg(not(feature = "_defmt"))]
    ::core::panic!("unwrap failed at {}", location);
}

/// Обобщение над `Option` и `Result` для `unwrap!`.
pub trait Try {
    type Ok;
    type Error;
    fn into_result(self) -> Result<Self::Ok, Self::Error>;
}

impl<T> Try for Option<T> {
    type Ok = T;
    type Error = ();

    #[inline]
    fn into_result(self) -> Result<T, ()> {
        self.ok_or(())
    }
}

impl<T, E> Try for Result<T, E> {
    type Ok = T;
    type Error = E;

    #[inline]
    fn into_result(self) -> Self {
        self
    }
}
