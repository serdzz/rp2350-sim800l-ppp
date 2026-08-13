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
