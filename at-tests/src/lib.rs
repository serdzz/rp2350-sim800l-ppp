//! Проверка формата AT-команд, разбора ответов и работы дайджестера.
//!
//! Запуск: `./run-at-tests.sh` из корня проекта.

/// Тот же самый файл, что уходит в прошивку.
///
/// Feature `_defmt` в этом крейте не объявлен, поэтому `#[cfg_attr]` в
/// modem.rs не разворачивает `derive(defmt::Format)` — заглушки логгера
/// на хосте не нужны.
#[path = "../../src/modem.rs"]
pub mod modem;

/// Кривая заряда Li-Po — тоже ровно тот файл, что уходит в прошивку.
#[path = "../../src/lipo.rs"]
pub mod lipo;

/// Разбор времени сети и шкала сигнала — тесты внутри модуля.
#[path = "../../src/clock.rs"]
pub mod clock;

/// Фрейминг CMUX. Тесты живут внутри самого модуля.
#[path = "../../src/cmux.rs"]
pub mod cmux;

/// Учёт монет и разбор команд блокировки — тесты внутри модуля.
#[path = "../../src/coin.rs"]
pub mod coin;

#[cfg(test)]
mod tests {
    use super::modem::*;
    use atat::{AtatCmd, AtatUrc, DigestResult, Digester};

    fn ser<C: AtatCmd>(c: &C) -> String {
        let mut buf = vec![0u8; 512];
        let n = c.write(&mut buf);
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    #[test]
    fn commands_serialize_as_expected() {
        assert_eq!(ser(&At), "AT\r");
        assert_eq!(ser(&DisableEcho), "ATE0\r");
        assert_eq!(ser(&SetVerboseErrors { n: 2 }), "AT+CMEE=2\r");
        assert_eq!(ser(&SetBaudRate { rate: 115200 }), "AT+IPR=115200\r");
        assert_eq!(ser(&SetFunctionality { fun: 1 }), "AT+CFUN=1\r");
        assert_eq!(ser(&GetPinStatus), "AT+CPIN?\r");
        assert_eq!(ser(&GetSignalQuality), "AT+CSQ\r");
        assert_eq!(ser(&GetNetworkRegistration), "AT+CREG?\r");
        assert_eq!(ser(&GetGprsRegistration), "AT+CGREG?\r");
        assert_eq!(ser(&GetGprsAttach), "AT+CGATT?\r");
        assert_eq!(ser(&SetGprsAttach { state: 1 }), "AT+CGATT=1\r");
        assert_eq!(ser(&ShutIpStack), "AT+CIPSHUT\r");
        // Префикс "AT" + "&W", без знака "=" — полей у команды нет.
        assert_eq!(ser(&SaveSettings), "AT&W\r");
        // basic option, только UIH, 115200 бод, поле данных до 127 байт.
        assert_eq!(
            ser(&SetCmuxMode {
                mode: 0,
                subset: 0,
                port_speed: 5,
                n1: 127
            }),
            "AT+CMUX=0,0,5,127\r"
        );
        assert_eq!(
            ser(&SetPdpContext {
                cid: 1,
                pdp_type: "IP",
                apn: "internet"
            }),
            "AT+CGDCONT=1,\"IP\",\"internet\"\r"
        );
        // Строка дозвона обязана уйти БЕЗ кавычек.
        assert_eq!(ser(&DialPpp { number: "*99***1#" }), "ATD*99***1#\r");
        assert_eq!(ser(&DialPpp { number: "*99#" }), "ATD*99#\r");
        assert_eq!(ser(&Hangup), "ATH\r");
    }

    #[test]
    fn responses_parse() {
        let r = GetPinStatus.parse(Ok(b"+CPIN: READY")).unwrap();
        assert_eq!(r.code.as_str(), "READY");

        let r = GetSignalQuality.parse(Ok(b"+CSQ: 24,0")).unwrap();
        assert_eq!((r.rssi, r.ber), (24, 0));

        let r = GetNetworkRegistration.parse(Ok(b"+CREG: 0,1")).unwrap();
        assert!(r.is_registered());

        let r = GetGprsRegistration.parse(Ok(b"+CGREG: 0,5")).unwrap();
        assert!(r.is_registered());

        let r = GetNetworkRegistration.parse(Ok(b"+CREG: 0,2")).unwrap();
        assert!(!r.is_registered(), "2 = идёт поиск сети, не регистрация");

        let r = GetGprsAttach.parse(Ok(b"+CGATT: 1")).unwrap();
        assert_eq!(r.state, 1);

        // Пустое тело: ответом был только код результата (OK / CONNECT).
        assert!(At.parse(Ok(b"")).is_ok());
        assert!(DialPpp { number: "*99#" }.parse(Ok(b"")).is_ok());
    }

    /// `AT+COPS?` меняет число полей в зависимости от состояния регистрации,
    /// поэтому разбирается «как есть». Структура с фиксированными полями
    /// развалилась бы на первом же варианте.
    #[test]
    fn cops_parses_both_shapes() {
        assert_eq!(ser(&GetOperator), "AT+COPS?\r");
        assert_eq!(ser(&ScanOperators), "AT+COPS=?\r");

        // Не зарегистрирован — одно поле.
        let r = GetOperator.parse(Ok(b"+COPS: 0")).unwrap();
        assert_eq!(r.text.as_str(), "+COPS: 0");

        // Зарегистрирован — три поля.
        let r = GetOperator.parse(Ok(b"+COPS: 0,0,\"MegaFon\"")).unwrap();
        assert_eq!(r.text.as_str(), "+COPS: 0,0,\"MegaFon\"");

        // Список сетей от AT+COPS=?
        let r = ScanOperators
            .parse(Ok(b"+COPS: (2,\"MegaFon\",\"MegaFon\",\"25002\"),(1,\"MTS\",\"MTS\",\"25001\")"))
            .unwrap();
        assert!(r.text.as_str().contains("25002"));
        assert!(r.text.as_str().contains("MTS"));
    }

    /// `AT+CIMI` и `AT+CCID` отвечают голым числом без префикса — обычный
    /// разбор по полям на таком ответе не работает.
    #[test]
    fn sim_identity_commands() {
        assert_eq!(ser(&GetImsi), "AT+CIMI\r");
        assert_eq!(ser(&GetIccid), "AT+CCID\r");

        let r = GetImsi.parse(Ok(b"247010123456789")).unwrap();
        assert_eq!(r.text.as_str(), "247010123456789");

        let r = GetIccid.parse(Ok(b"8937101234567890123")).unwrap();
        assert_eq!(r.text.as_str(), "8937101234567890123");
    }

    #[test]
    fn imsi_splits_into_mcc_and_mnc() {
        // Латвия, Tele2 (247-02) — MNC двузначный.
        assert_eq!(imsi_mcc("247020123456789"), "247");
        assert_eq!(imsi_mnc("247020123456789"), "02");

        // США (310-260, T-Mobile) — MNC трёхзначный.
        assert_eq!(imsi_mcc("310260123456789"), "310");
        assert_eq!(imsi_mnc("310260123456789"), "260");

        // Мусор на входе не должен паниковать.
        assert_eq!(imsi_mcc(""), "???");
        assert_eq!(imsi_mnc(""), "??");
        assert_eq!(imsi_mnc("24"), "??");
    }

    /// Команды и URC для приёма SMS.
    #[test]
    fn sms_commands_and_notification() {
        assert_eq!(ser(&SetSmsTextMode { mode: 1 }), "AT+CMGF=1\r");
        assert_eq!(
            ser(&SetSmsIndication {
                mode: 2,
                mt: 1,
                bm: 0,
                ds: 0,
                bfr: 0
            }),
            "AT+CNMI=2,1,0,0,0\r"
        );
        assert_eq!(ser(&GetSmsStorage), "AT+CPMS?\r");
        assert_eq!(ser(&ReadSms { index: 3 }), "AT+CMGR=3\r");

        // Занятость памяти разбирается как есть: три хранилища по три поля.
        let r = GetSmsStorage
            .parse(Ok(b"+CPMS: \"SM\",3,30,\"SM\",3,30,\"SM\",3,30"))
            .unwrap();
        assert!(r.text.as_str().contains("3,30"));
        assert_eq!(ser(&DeleteSms { index: 3 }), "AT+CMGD=3\r");

        // Извещение о новом сообщении: хранилище и индекс.
        let urc = Urc::parse(b"+CMTI: \"SM\",3").unwrap();
        match urc {
            Urc::NewMessage(n) => {
                assert_eq!(n.storage.as_str(), "SM");
                assert_eq!(n.index, 3);
            }
            other => panic!("{:?}", other),
        }

        // Ответ на AT+CMGR — две строки; берём как есть, включая текст.
        let body = b"+CMGR: \"REC UNREAD\",\"+37120000000\",\"\",\"26/08/15,12:00:00+12\"\r\nHello";
        let r = ReadSms { index: 3 }.parse(Ok(body)).unwrap();
        assert!(r.text.as_str().contains("+37120000000"));
        assert!(r.text.as_str().ends_with("Hello"));
    }

    /// Отправка идёт в два приёма, и на проводе это должно выглядеть ровно
    /// так: команда с номером в кавычках, потом голый текст с Ctrl-Z и без
    /// возврата каретки.
    #[test]
    fn sms_send_is_two_stage() {
        assert_eq!(
            ser(&SendSmsHeader {
                number: "+37120000000"
            }),
            "AT+CMGS=\"+37120000000\"\r"
        );

        let mut buf = vec![0u8; 256];
        let n = SmsBody { text: "Hello" }.write(&mut buf);
        assert_eq!(&buf[..n], b"Hello\x1a", "ни AT, ни \\r — только текст и Ctrl-Z");

        // Ответ на текст — номер отправленного сообщения.
        let r = SmsBody { text: "Hello" }.parse(Ok(b"+CMGS: 42")).unwrap();
        assert_eq!(r.text.as_str(), "+CMGS: 42");
    }

    /// Приглашение `> ` не заканчивается переводом строки, и штатный
    /// дайджестер его не узнаёт. Без своего разборщика отправка встала бы на
    /// первой же половине.
    #[test]
    fn prompt_is_recognised() {
        let mut d = atat::DefaultDigester::<Urc>::new().with_custom_prompt(parse_sms_prompt);

        let (res, used) = d.digest(b"\r\n> ");
        assert!(matches!(res, DigestResult::Prompt(b'>')), "{:?}", res);
        assert_eq!(used, 4);

        // Приглашение без предваряющего перевода строки тоже встречается.
        let (res, _) = d.digest(b"> ");
        assert!(matches!(res, DigestResult::Prompt(b'>')), "{:?}", res);

        // Обычные ответы не сломались.
        let (res, _) = d.digest(b"\r\n+CMGS: 42\r\n\r\nOK\r\n");
        match res {
            DigestResult::Response(Ok(body)) => assert_eq!(body, b"+CMGS: 42"),
            other => panic!("{:?}", other),
        }
    }

    /// Номер ходит цифрами: в имени топика MQTT плюс запрещён, это
    /// подстановочный знак.
    #[test]
    fn phone_numbers_are_digits_only() {
        assert!(valid_phone("37120000000"));
        assert!(valid_phone("12345"));
        for bad in ["+37120000000", "371 2000", "1234", "", "371-20000000", "abcde"] {
            assert!(!valid_phone(bad), "принят: {bad}");
        }
        // Двадцать знаков — предел, двадцать один уже нет.
        assert!(valid_phone(&"1".repeat(20)));
        assert!(!valid_phone(&"1".repeat(21)));
    }

    /// Кириллица требует UCS2 и перекодировки всего сообщения. Отправить её
    /// как есть — значит получить абракадабру, поэтому отказываемся честно.
    #[test]
    fn only_plain_ascii_is_sendable() {
        assert!(sms_text_is_sendable("Coin jam on line 3"));
        assert!(sms_text_is_sendable(&"x".repeat(160)));

        assert!(!sms_text_is_sendable(""), "пустое отправлять незачем");
        assert!(!sms_text_is_sendable(&"x".repeat(161)), "не влезает в одно");
        assert!(!sms_text_is_sendable("Замятие монеты"), "кириллица");
        assert!(!sms_text_is_sendable("текст\u{1a}хвост"), "Ctrl-Z оборвёт");
        assert!(!sms_text_is_sendable("a\u{1b}b"), "Esc отменит отправку");
    }

    /// `+CMTI` не должен затенять ответы на наши команды.
    #[test]
    fn cmti_does_not_shadow_cmgr() {
        let mut d = atat::DefaultDigester::<Urc>::new().with_custom_success(parse_shut_ok);

        let (res, _) = d.digest(b"\r\n+CMTI: \"SM\",3\r\n");
        assert!(matches!(res, DigestResult::Urc(_)), "{:?}", res);

        let (res, _) = d.digest(b"\r\n+CMGR: \"REC UNREAD\",\"+371\",\"\",\"x\"\r\nHi\r\n\r\nOK\r\n");
        assert!(matches!(res, DigestResult::Response(Ok(_))), "{:?}", res);
    }

    /// Длинный список сетей обязан обрезаться, а не паниковать.
    #[test]
    fn raw_line_truncates_instead_of_panicking() {
        let long = vec![b'x'; 4096];
        let r = ScanOperators.parse(Ok(&long)).unwrap();
        assert_eq!(r.text.len(), 384, "должно обрезаться по вместимости");
    }

    /// Обрезка не должна разрубать многобайтный символ пополам.
    #[test]
    fn raw_line_truncation_keeps_utf8_intact() {
        let long = "ю".repeat(4096);
        let r = ScanOperators.parse(Ok(long.as_bytes())).unwrap();
        // 384-байтный буфер, символ по 2 байта -> 192 символа, 384 байта.
        assert_eq!(r.text.chars().count(), 192);
        assert_eq!(r.text.len(), 384);
    }

    /// Ровно тот баг, что уронил дозвон на живом стенде: `AT+CIPSHUT`
    /// отвечает `SHUT OK`, штатный дайджестер такой ответ не признаёт,
    /// строка остаётся в буфере и приклеивается к следующей команде.
    #[test]
    fn shut_ok_is_recognised_as_success() {
        let mut d = atat::DefaultDigester::<Urc>::new().with_custom_success(parse_shut_ok);

        // Сам по себе SHUT OK — успех с пустым телом.
        let (res, used) = d.digest(b"\r\nSHUT OK\r\n");
        match res {
            DigestResult::Response(Ok(body)) => assert!(body.is_empty()),
            other => panic!("{:?}", other),
        }
        assert_eq!(used, 11, "токен должен быть съеден целиком");

        // NoResponse обязан разобраться из пустого тела.
        assert!(ShutIpStack.parse(Ok(b"")).is_ok());

        // Без парсера тот же ввод не распознаётся вовсе — это и есть причина
        // залипания в буфере.
        let mut plain = atat::DefaultDigester::<Urc>::new();
        let (res, used) = plain.digest(b"\r\nSHUT OK\r\n");
        assert!(matches!(res, DigestResult::None), "{:?}", res);
        assert_eq!(used, 0, "байты остались бы в буфере");

        // Обычные ответы после добавления парсера не сломались.
        let (res, _) = d.digest(b"\r\n+CSQ: 24,0\r\n\r\nOK\r\n");
        match res {
            DigestResult::Response(Ok(body)) => assert_eq!(body, b"+CSQ: 24,0"),
            other => panic!("{:?}", other),
        }
        let (res, _) = d.digest(b"\r\nCONNECT\r\n");
        assert!(matches!(res, DigestResult::Response(Ok(_))), "{:?}", res);
    }

    /// Воспроизведение сбоя: хвост `SHUT OK` перед ответом на дозвон.
    #[test]
    fn stale_shut_ok_would_break_the_next_command() {
        // Без парсера тело ответа на дозвон приезжает как "SHUT OK" -> Error::Parse.
        let mut plain = atat::DefaultDigester::<Urc>::new();
        let (res, _) = plain.digest(b"\r\nSHUT OK\r\n\r\nCONNECT\r\n");
        match res {
            DigestResult::Response(Ok(body)) => {
                assert_eq!(body, b"SHUT OK");
                assert!(
                    DialPpp { number: "*99#" }.parse(Ok(body)).is_err(),
                    "именно так дозвон и падал с Parse"
                );
            }
            other => panic!("{:?}", other),
        }

        // С парсером SHUT OK съедается отдельно, дозвон получает чистый CONNECT.
        let mut d = atat::DefaultDigester::<Urc>::new().with_custom_success(parse_shut_ok);
        let (_, used) = d.digest(b"\r\nSHUT OK\r\n\r\nCONNECT\r\n");
        let (res, _) = d.digest(&b"\r\nSHUT OK\r\n\r\nCONNECT\r\n"[used..]);
        match res {
            DigestResult::Response(Ok(body)) => {
                assert!(body.is_empty());
                assert!(DialPpp { number: "*99#" }.parse(Ok(body)).is_ok());
            }
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn digester_handles_connect_and_urcs() {
        let mut d = atat::DefaultDigester::<Urc>::default();

        // CONNECT — успех наравне с OK. Без этого дозвон падал бы по таймауту.
        let (res, used) = d.digest(b"\r\nCONNECT\r\n");
        assert!(matches!(res, DigestResult::Response(Ok(_))), "{:?}", res);
        assert_eq!(used, 11);

        let (res, _) = d.digest(b"\r\n+CSQ: 24,0\r\n\r\nOK\r\n");
        match res {
            DigestResult::Response(Ok(b)) => assert_eq!(b, b"+CSQ: 24,0"),
            other => panic!("{:?}", other),
        }

        let (res, _) = d.digest(b"\r\nERROR\r\n");
        assert!(matches!(res, DigestResult::Response(Err(_))), "{:?}", res);

        let (res, _) = d.digest(b"\r\nRDY\r\n");
        assert!(matches!(res, DigestResult::Urc(b"RDY")), "{:?}", res);

        let (res, _) = d.digest(b"\r\n+PDP: DEACT\r\n");
        assert!(matches!(res, DigestResult::Urc(_)), "{:?}", res);
    }

    /// Главная ловушка atat: URC-теги разбираются ДО ответов на команды.
    /// Если бы `+CREG` был объявлен в `Urc`, ответ на `AT+CREG?` ушёл бы
    /// в URC-канал, а `send()` завис бы до таймаута.
    #[test]
    fn urc_tags_do_not_shadow_command_responses() {
        let mut d = atat::DefaultDigester::<Urc>::default();
        for probe in [
            &b"\r\n+CREG: 0,1\r\n\r\nOK\r\n"[..],
            &b"\r\n+CGREG: 0,1\r\n\r\nOK\r\n"[..],
            &b"\r\n+CSQ: 24,0\r\n\r\nOK\r\n"[..],
            &b"\r\n+CPIN: READY\r\n\r\nOK\r\n"[..],
            &b"\r\n+CGATT: 1\r\n\r\nOK\r\n"[..],
        ] {
            let (res, _) = d.digest(probe);
            assert!(
                matches!(res, DigestResult::Response(Ok(_))),
                "ответ разобран как URC: {:?}",
                res
            );
        }
    }

    #[test]
    fn urc_enum_maps_tags() {
        assert!(matches!(Urc::parse(b"RDY"), Some(Urc::Ready)));
        assert!(matches!(Urc::parse(b"Call Ready"), Some(Urc::CallReady)));
        assert!(matches!(Urc::parse(b"SMS Ready"), Some(Urc::SmsReady)));
        assert!(matches!(
            Urc::parse(b"NORMAL POWER DOWN"),
            Some(Urc::PowerDown)
        ));
        assert!(matches!(
            Urc::parse(b"+PDP: DEACT"),
            Some(Urc::PdpDeactivated)
        ));
        assert!(Urc::parse(b"+CSQ: 24,0").is_none());
    }
}
