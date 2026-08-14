//! Фрейминг мультиплексора 3GPP TS 27.010, basic option.
//!
//! Позволяет гонять по одному UART несколько логических каналов: PPP на
//! одном DLCI, AT-команды на другом. Сейчас в проекте UART принадлежит либо
//! `atat`, либо PPP — см. шапку `main.rs`; CMUX снимает это ограничение.
//!
//! Модуль намеренно не зависит ни от embassy, ни от `atat` — за счёт этого он
//! подключается в `at-tests/` через `#[path]` и проверяется на хосте.
//!
//! # Формат кадра
//!
//! ```text
//! ┌──────┬─────────┬─────────┬───────────┬─────────────┬─────┬──────┐
//! │ Flag │ Address │ Control │  Length   │ Information │ FCS │ Flag │
//! │ 0xF9 │  1 окт  │  1 окт  │ 1 или 2   │   0..N окт  │ 1   │ 0xF9 │
//! └──────┴─────────┴─────────┴───────────┴─────────────┴─────┴──────┘
//! ```
//!
//! Байт-стаффинг в basic option не нужен: длина передаётся явно, поэтому
//! значение `0xF9` внутри поля данных не ломает разбор.
//!
//! # Две ловушки спецификации
//!
//! 1. **Порядок бит.** По TS 27.010 §5.2.2 поля передаются младшим битом
//!    вперёд, из-за чего значения в спецификации записаны в обратном к
//!    привычному порядке. Приложение же видит обычные октеты. Annex B.2 прямо
//!    советует не переворачивать биты руками, а взять отражённую таблицу CRC —
//!    так и сделано в [`CRC`].
//! 2. **FCS у UIH считается только по заголовку** (§5.2.1.6): адрес, управление
//!    и длина. Поле данных не защищено. У остальных типов кадров FCS покрывает
//!    всё. Перепутать эти два случая — значит получить кадры, которые молча
//!    отбраковываются на одной стороне.

#![allow(dead_code)]

/// Флаг начала и конца кадра (§5.2.1.1).
pub const FLAG: u8 = 0xF9;

/// Значение накопителя CRC после прогона заголовка вместе с принятым FCS.
///
/// Спецификация (§5.2.1.6) записывает его как `1111 0011` в порядке передачи;
/// в обычном представлении октета это `0xCF`.
pub const FCS_GOOD: u8 = 0xCF;

/// Предел длины при однобайтном поле длины.
pub const MAX_LEN_SHORT: usize = 127;
/// Предел длины при двухбайтном поле длины (15 бит).
pub const MAX_LEN_LONG: usize = 32_767;

/// Бит P/F в поле управления.
const PF: u8 = 0x10;

/// Отражённая таблица CRC-8, полином `x^8 + x^2 + x + 1`.
///
/// Строится на этапе компиляции; [`crc_table_matches_specification`] сверяет её
/// с значениями из Annex B спецификации.
///
/// [`crc_table_matches_specification`]: tests::crc_table_matches_specification
const CRC: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u8;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xE0 } else { c >> 1 };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// Прогоняет байты через CRC, продолжая с накопителя `acc`.
fn crc_update(mut acc: u8, data: &[u8]) -> u8 {
    let mut i = 0;
    while i < data.len() {
        acc = CRC[(acc ^ data[i]) as usize];
        i += 1;
    }
    acc
}

/// FCS для набора байт: единичное дополнение остатка (§5.2.1.6).
pub fn fcs(data: &[u8]) -> u8 {
    !crc_update(0xFF, data)
}

/// Тип кадра, таблица 2 спецификации. Бит P/F хранится отдельно.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Установить режим — открывает канал.
    Sabm,
    /// Подтверждение SABM или DISC.
    Ua,
    /// Канал закрыт.
    Dm,
    /// Закрыть канал.
    Disc,
    /// Данные; FCS считается только по заголовку.
    Uih,
    /// Данные; FCS покрывает и поле данных.
    Ui,
}

impl FrameKind {
    /// Код поля управления без бита P/F.
    pub const fn code(self) -> u8 {
        match self {
            Self::Sabm => 0x2F,
            Self::Ua => 0x63,
            Self::Dm => 0x0F,
            Self::Disc => 0x43,
            Self::Uih => 0xEF,
            Self::Ui => 0x03,
        }
    }

    /// Разбор поля управления; бит P/F игнорируется.
    pub fn from_code(control: u8) -> Option<Self> {
        match control & !PF {
            0x2F => Some(Self::Sabm),
            0x63 => Some(Self::Ua),
            0x0F => Some(Self::Dm),
            0x43 => Some(Self::Disc),
            0xEF => Some(Self::Uih),
            0x03 => Some(Self::Ui),
            _ => None,
        }
    }

    /// Входит ли поле данных в расчёт FCS.
    ///
    /// У UIH — нет: спецификация защищает только доставку в правильный DLCI.
    pub const fn fcs_covers_information(self) -> bool {
        !matches!(self, Self::Uih)
    }
}

/// Адресное поле: идентификатор канала и признак команды.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    /// Номер логического канала, 0..=63. Канал 0 — управляющий.
    pub dlci: u8,
    /// Бит C/R: `true` — команда, `false` — ответ.
    pub command: bool,
}

impl Address {
    /// Собрать октет: `DLCI | C/R | EA`, где EA всегда 1.
    pub const fn encode(self) -> u8 {
        (self.dlci << 2) | ((self.command as u8) << 1) | 1
    }

    /// Разобрать октет. `None`, если EA = 0 — расширенный адрес спецификацией
    /// зарезервирован, но не определён, и мы его не поддерживаем.
    pub fn decode(octet: u8) -> Option<Self> {
        if octet & 1 == 0 {
            return None;
        }
        Some(Self {
            dlci: octet >> 2,
            command: octet & 0b10 != 0,
        })
    }
}

/// Разобранный или собираемый кадр.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub address: Address,
    pub kind: FrameKind,
    /// Бит Poll/Final.
    pub poll_final: bool,
    /// Поле данных; пусто у всех типов, кроме UI и UIH.
    pub information: &'a [u8],
}

/// Почему не удалось собрать кадр.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    /// Выходной буфер меньше [`Frame::encoded_len`].
    BufferTooSmall,
    /// Поле данных длиннее [`MAX_LEN_LONG`].
    InformationTooLong,
    /// DLCI не помещается в шесть бит.
    DlciOutOfRange,
}

/// Почему кадр отброшен при разборе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Контрольная сумма не сошлась.
    Fcs,
    /// EA = 0: многооктетный адрес не поддерживается.
    ExtendedAddress,
    /// Неизвестный тип кадра.
    UnknownControl,
    /// Поле данных не помещается в буфер декодера.
    TooLong,
    /// После FCS не оказалось закрывающего флага.
    MissingClosingFlag,
}

impl<'a> Frame<'a> {
    /// Сколько байт займёт кадр целиком, включая оба флага.
    pub fn encoded_len(&self) -> usize {
        let length_octets = if self.information.len() > MAX_LEN_SHORT {
            2
        } else {
            1
        };
        // флаг + адрес + управление + длина + данные + FCS + флаг
        1 + 1 + 1 + length_octets + self.information.len() + 1 + 1
    }

    /// Сериализовать кадр в `out`, вернуть число записанных байт.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        if self.address.dlci > 0x3F {
            return Err(EncodeError::DlciOutOfRange);
        }
        let len = self.information.len();
        if len > MAX_LEN_LONG {
            return Err(EncodeError::InformationTooLong);
        }
        let total = self.encoded_len();
        if out.len() < total {
            return Err(EncodeError::BufferTooSmall);
        }

        let control = self.kind.code() | if self.poll_final { PF } else { 0 };

        let mut n = 0;
        out[n] = FLAG;
        n += 1;

        let header_start = n;
        out[n] = self.address.encode();
        n += 1;
        out[n] = control;
        n += 1;

        if len > MAX_LEN_SHORT {
            // EA = 0 в первом октете: следом идёт второй.
            out[n] = ((len as u16 & 0x7F) << 1) as u8;
            n += 1;
            out[n] = (len >> 7) as u8;
            n += 1;
        } else {
            out[n] = ((len as u8) << 1) | 1;
            n += 1;
        }
        let header_end = n;

        out[n..n + len].copy_from_slice(self.information);
        n += len;

        let covered_end = if self.kind.fcs_covers_information() {
            n
        } else {
            header_end
        };
        out[n] = fcs(&out[header_start..covered_end]);
        n += 1;

        out[n] = FLAG;
        n += 1;

        Ok(n)
    }
}

/// Состояние конечного автомата разбора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Ищем открывающий флаг.
    Sync,
    /// Ждём адрес; лишние флаги здесь — межкадровое заполнение.
    Address,
    Control,
    Length1,
    Length2,
    Information,
    Fcs,
    /// Ждём закрывающий флаг.
    ClosingFlag,
}

/// Потоковый разборщик кадров.
///
/// `N` — предельный размер поля данных. Для PPP нужно не меньше 1500 плюс
/// запас на заголовки convergence layer.
pub struct Decoder<const N: usize> {
    state: State,
    address: Address,
    kind: FrameKind,
    poll_final: bool,
    /// Младшие семь бит длины, пока ждём второй октет.
    length_low: u8,
    length: usize,
    received: usize,
    buffer: [u8; N],
    /// CRC по заголовку — им проверяются кадры UIH.
    crc_header: u8,
    /// CRC по заголовку и данным — им проверяются все остальные.
    crc_full: u8,
}

impl<const N: usize> Default for Decoder<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Decoder<N> {
    pub const fn new() -> Self {
        Self {
            state: State::Sync,
            address: Address {
                dlci: 0,
                command: false,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            length_low: 0,
            length: 0,
            received: 0,
            buffer: [0; N],
            crc_header: 0xFF,
            crc_full: 0xFF,
        }
    }

    /// Сбросить разбор и снова искать открывающий флаг.
    pub fn reset(&mut self) {
        self.state = State::Sync;
    }

    /// Подать очередной байт.
    ///
    /// Возвращает `Some`, когда кадр собран либо отброшен. Ссылка на данные
    /// живёт до следующего вызова.
    pub fn push(&mut self, byte: u8) -> Option<Result<Frame<'_>, DecodeError>> {
        match self.state {
            State::Sync => {
                if byte == FLAG {
                    self.state = State::Address;
                }
                None
            }

            State::Address => {
                // Подряд идущие флаги — межкадровое заполнение (§5.2.5).
                if byte == FLAG {
                    return None;
                }
                match Address::decode(byte) {
                    Some(address) => {
                        self.address = address;
                        self.crc_header = crc_update(0xFF, &[byte]);
                        self.state = State::Control;
                        None
                    }
                    None => {
                        self.state = State::Sync;
                        Some(Err(DecodeError::ExtendedAddress))
                    }
                }
            }

            State::Control => match FrameKind::from_code(byte) {
                Some(kind) => {
                    self.kind = kind;
                    self.poll_final = byte & PF != 0;
                    self.crc_header = crc_update(self.crc_header, &[byte]);
                    self.state = State::Length1;
                    None
                }
                None => {
                    self.state = State::Sync;
                    Some(Err(DecodeError::UnknownControl))
                }
            },

            State::Length1 => {
                self.crc_header = crc_update(self.crc_header, &[byte]);
                if byte & 1 != 0 {
                    self.length = (byte >> 1) as usize;
                    self.begin_information()
                } else {
                    self.length_low = byte >> 1;
                    self.state = State::Length2;
                    None
                }
            }

            State::Length2 => {
                self.crc_header = crc_update(self.crc_header, &[byte]);
                self.length = self.length_low as usize | ((byte as usize) << 7);
                self.begin_information()
            }

            State::Information => {
                self.buffer[self.received] = byte;
                self.received += 1;
                self.crc_full = crc_update(self.crc_full, &[byte]);
                if self.received == self.length {
                    self.state = State::Fcs;
                }
                None
            }

            State::Fcs => {
                let accumulated = if self.kind.fcs_covers_information() {
                    self.crc_full
                } else {
                    self.crc_header
                };
                if CRC[(accumulated ^ byte) as usize] == FCS_GOOD {
                    self.state = State::ClosingFlag;
                    None
                } else {
                    self.state = State::Sync;
                    Some(Err(DecodeError::Fcs))
                }
            }

            State::ClosingFlag => {
                if byte == FLAG {
                    // Закрывающий флаг может быть и открывающим для следующего
                    // кадра (§5.2.1.1), поэтому ждём сразу адрес.
                    self.state = State::Address;
                    Some(Ok(Frame {
                        address: self.address,
                        kind: self.kind,
                        poll_final: self.poll_final,
                        information: &self.buffer[..self.length],
                    }))
                } else {
                    self.state = State::Sync;
                    Some(Err(DecodeError::MissingClosingFlag))
                }
            }
        }
    }

    /// Общий хвост разбора длины: решить, ждать данные или сразу FCS.
    fn begin_information(&mut self) -> Option<Result<Frame<'_>, DecodeError>> {
        if self.length > N {
            self.state = State::Sync;
            return Some(Err(DecodeError::TooLong));
        }
        self.received = 0;
        self.crc_full = self.crc_header;
        self.state = if self.length == 0 {
            State::Fcs
        } else {
            State::Information
        };
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собранный кадр в владеющем виде — `Frame` заимствует декодер и в `Vec`
    /// его не положить.
    type Owned = (Address, FrameKind, bool, Vec<u8>);

    fn feed<const N: usize>(
        decoder: &mut Decoder<N>,
        bytes: &[u8],
    ) -> Vec<Result<Owned, DecodeError>> {
        let mut out = Vec::new();
        for &b in bytes {
            if let Some(result) = decoder.push(b) {
                out.push(result.map(|f| (f.address, f.kind, f.poll_final, f.information.to_vec())));
            }
        }
        out
    }

    fn encode(frame: &Frame<'_>) -> Vec<u8> {
        let mut buf = vec![0u8; frame.encoded_len()];
        let n = frame.encode(&mut buf).unwrap();
        assert_eq!(n, buf.len(), "encoded_len должен совпадать с записанным");
        buf
    }

    /// Значения из Annex B спецификации. Таблица строится на этапе
    /// компиляции, и опечатка в полиноме тихо сломала бы весь фрейминг.
    #[test]
    fn crc_table_matches_specification() {
        let head: [u8; 32] = [
            0x00, 0x91, 0xE3, 0x72, 0x07, 0x96, 0xE4, 0x75, 0x0E, 0x9F, 0xED, 0x7C, 0x09, 0x98,
            0xEA, 0x7B, 0x1C, 0x8D, 0xFF, 0x6E, 0x1B, 0x8A, 0xF8, 0x69, 0x12, 0x83, 0xF1, 0x60,
            0x15, 0x84, 0xF6, 0x67,
        ];
        assert_eq!(&CRC[..32], &head[..]);
        assert_eq!(CRC[255], 0xCF, "последнее значение таблицы из Annex B");
    }

    /// Канонический кадр открытия управляющего канала: его байты одинаковы
    /// во всех реализациях CMUX, поэтому годятся как эталон.
    #[test]
    fn canonical_sabm_on_control_channel() {
        let frame = Frame {
            address: Address {
                dlci: 0,
                command: true,
            },
            kind: FrameKind::Sabm,
            poll_final: true,
            information: &[],
        };
        assert_eq!(encode(&frame), vec![0xF9, 0x03, 0x3F, 0x01, 0x1C, 0xF9]);
    }

    /// Приёмная проверка из §5.2.1.6: прогон защищённых полей вместе с
    /// принятым FCS даёт фиксированную константу.
    #[test]
    fn receiver_check_constant() {
        let header = [0x03u8, 0x3F, 0x01];
        let f = fcs(&header);
        assert_eq!(f, 0x1C);
        assert_eq!(CRC[(crc_update(0xFF, &header) ^ f) as usize], FCS_GOOD);
    }

    #[test]
    fn address_round_trip() {
        for dlci in 0..=63u8 {
            for command in [false, true] {
                let a = Address { dlci, command };
                let octet = a.encode();
                assert_eq!(octet & 1, 1, "EA всегда 1");
                assert_eq!(Address::decode(octet), Some(a));
            }
        }
        // EA = 0 — расширенный адрес, не поддерживаем.
        assert_eq!(Address::decode(0x02), None);
    }

    #[test]
    fn control_round_trip() {
        let kinds = [
            FrameKind::Sabm,
            FrameKind::Ua,
            FrameKind::Dm,
            FrameKind::Disc,
            FrameKind::Uih,
            FrameKind::Ui,
        ];
        for kind in kinds {
            for pf in [false, true] {
                let code = kind.code() | if pf { PF } else { 0 };
                assert_eq!(FrameKind::from_code(code), Some(kind));
                assert_eq!(code & PF != 0, pf);
            }
        }
        assert_eq!(FrameKind::from_code(0x00), None);
    }

    #[test]
    fn round_trip_through_decoder() {
        let payload = b"AT+CSQ\r";
        let frame = Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: payload,
        };
        let bytes = encode(&frame);

        let mut d = Decoder::<256>::new();
        let out = feed(&mut d, &bytes);
        assert_eq!(out.len(), 1);
        let (address, kind, pf, info) = out[0].clone().unwrap();
        assert_eq!(address, frame.address);
        assert_eq!(kind, FrameKind::Uih);
        assert!(!pf);
        assert_eq!(info, payload);
    }

    /// Пустое поле данных — длина присутствует всегда (§5.2.1.5).
    #[test]
    fn empty_information_still_has_length_octet() {
        let frame = Frame {
            address: Address {
                dlci: 5,
                command: false,
            },
            kind: FrameKind::Ua,
            poll_final: true,
            information: &[],
        };
        let bytes = encode(&frame);
        assert_eq!(bytes.len(), 6);
        assert_eq!(bytes[3], 0x01, "длина 0 с установленным EA");

        let mut d = Decoder::<64>::new();
        let out = feed(&mut d, &bytes);
        assert_eq!(out.len(), 1);
        assert!(out[0].clone().unwrap().3.is_empty());
    }

    /// Больше 127 байт — поле длины становится двухоктетным, EA первого = 0.
    #[test]
    fn two_octet_length_for_long_payload() {
        let payload: Vec<u8> = (0..1500u16).map(|i| i as u8).collect();
        let frame = Frame {
            address: Address {
                dlci: 2,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: &payload,
        };
        let bytes = encode(&frame);
        assert_eq!(bytes[3] & 1, 0, "EA=0: следом второй октет длины");
        assert_eq!(
            (bytes[3] >> 1) as usize | ((bytes[4] as usize) << 7),
            payload.len()
        );

        let mut d = Decoder::<2048>::new();
        let out = feed(&mut d, &bytes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].clone().unwrap().3, payload);
    }

    /// Граница между одно- и двухоктетной длиной.
    #[test]
    fn length_boundary_at_127() {
        for len in [126usize, 127, 128, 129] {
            let payload = vec![0xA5u8; len];
            let frame = Frame {
                address: Address {
                    dlci: 3,
                    command: true,
                },
                kind: FrameKind::Ui,
                poll_final: false,
                information: &payload,
            };
            let bytes = encode(&frame);
            let expected_octets = if len > 127 { 2 } else { 1 };
            assert_eq!(bytes.len(), 5 + expected_octets + len, "длина {len}");

            let mut d = Decoder::<512>::new();
            let out = feed(&mut d, &bytes);
            assert_eq!(out.len(), 1, "длина {len}");
            assert_eq!(out[0].clone().unwrap().3.len(), len);
        }
    }

    /// Главная тонкость спецификации: у UIH контрольная сумма не покрывает
    /// поле данных, у UI — покрывает.
    #[test]
    fn uih_fcs_covers_header_only() {
        let payload = b"payload";

        let mut uih = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: payload,
        });
        let mut ui = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Ui,
            poll_final: false,
            information: payload,
        });
        assert_ne!(
            uih[uih.len() - 2],
            ui[ui.len() - 2],
            "FCS обязан отличаться: у UI в него входят данные"
        );

        // Портим байт данных.
        let idx = 5;
        uih[idx] ^= 0xFF;
        ui[idx] ^= 0xFF;

        // UIH: заголовок цел, кадр принимается — данные не защищены.
        let mut d = Decoder::<64>::new();
        assert!(feed(&mut d, &uih)[0].is_ok());

        // UI: та же порча ломает контрольную сумму.
        let mut d = Decoder::<64>::new();
        assert_eq!(feed(&mut d, &ui)[0], Err(DecodeError::Fcs));
    }

    /// Порча заголовка отбраковывается у любого типа кадра.
    #[test]
    fn corrupted_header_is_rejected() {
        let mut bytes = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: b"data",
        });
        bytes[1] = Address {
            dlci: 2,
            command: true,
        }
        .encode();

        let mut d = Decoder::<64>::new();
        assert_eq!(feed(&mut d, &bytes)[0], Err(DecodeError::Fcs));
    }

    #[test]
    fn rejects_unknown_control_and_extended_address() {
        let mut d = Decoder::<64>::new();
        assert_eq!(
            feed(&mut d, &[FLAG, 0x03, 0x00]),
            vec![Err(DecodeError::UnknownControl)]
        );

        let mut d = Decoder::<64>::new();
        assert_eq!(
            feed(&mut d, &[FLAG, 0x02]),
            vec![Err(DecodeError::ExtendedAddress)]
        );
    }

    /// Кадр длиннее буфера декодера отбраковывается, а не переполняет его.
    #[test]
    fn rejects_frame_longer_than_buffer() {
        let payload = vec![0u8; 200];
        let bytes = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: &payload,
        });
        let mut d = Decoder::<64>::new();
        assert_eq!(feed(&mut d, &bytes)[0], Err(DecodeError::TooLong));
    }

    /// После мусора в линии разбор обязан восстановиться на следующем кадре.
    #[test]
    fn resynchronises_after_garbage() {
        let good = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: b"ok",
        });

        let mut stream = vec![0x00, 0xAB, 0xCD, 0xEF];
        stream.extend_from_slice(&good);

        let mut d = Decoder::<64>::new();
        let out = feed(&mut d, &stream);
        let frames: Vec<_> = out.iter().filter(|r| r.is_ok()).collect();
        assert_eq!(frames.len(), 1);
        assert_eq!(out.last().unwrap().clone().unwrap().3, b"ok");
    }

    /// Закрывающий флаг служит открывающим для следующего кадра.
    #[test]
    fn shared_flag_between_frames() {
        let mk = |dlci: u8, info: &'static [u8]| {
            encode(&Frame {
                address: Address {
                    dlci,
                    command: true,
                },
                kind: FrameKind::Uih,
                poll_final: false,
                information: info,
            })
        };
        let first = mk(1, b"one");
        let second = mk(2, b"two");

        // Склеиваем, выбрасывая дублирующийся флаг на стыке.
        let mut stream = first.clone();
        stream.extend_from_slice(&second[1..]);

        let mut d = Decoder::<64>::new();
        let out = feed(&mut d, &stream);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].clone().unwrap().0.dlci, 1);
        assert_eq!(out[0].clone().unwrap().3, b"one");
        assert_eq!(out[1].clone().unwrap().0.dlci, 2);
        assert_eq!(out[1].clone().unwrap().3, b"two");
    }

    /// Межкадровое заполнение — повторяющиеся флаги (§5.2.5).
    #[test]
    fn tolerates_inter_frame_fill() {
        let frame = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: b"x",
        });
        let mut stream = vec![FLAG; 5];
        stream.extend_from_slice(&frame);
        stream.extend_from_slice(&[FLAG, FLAG]);

        let mut d = Decoder::<64>::new();
        let out = feed(&mut d, &stream);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].clone().unwrap().3, b"x");
    }

    /// Байт `0xF9` внутри данных не должен обрываться разбор: в basic option
    /// длина известна заранее и стаффинг не применяется.
    #[test]
    fn flag_byte_inside_payload_is_transparent() {
        let payload = [FLAG, 0x00, FLAG, FLAG, 0x42];
        let bytes = encode(&Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: &payload,
        });
        let mut d = Decoder::<64>::new();
        let out = feed(&mut d, &bytes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].clone().unwrap().3, payload);
    }

    #[test]
    fn encode_reports_buffer_and_range_errors() {
        let frame = Frame {
            address: Address {
                dlci: 1,
                command: true,
            },
            kind: FrameKind::Uih,
            poll_final: false,
            information: b"data",
        };
        let mut small = [0u8; 4];
        assert_eq!(frame.encode(&mut small), Err(EncodeError::BufferTooSmall));

        let bad = Frame {
            address: Address {
                dlci: 64,
                command: true,
            },
            ..frame
        };
        let mut buf = [0u8; 32];
        assert_eq!(bad.encode(&mut buf), Err(EncodeError::DlciOutOfRange));
    }
}
