//! Explicit URL authentication for async-nats versions that do not apply userinfo.
//! Strip credentials before addresses can enter SDK diagnostics/reconnect logs.
use async_nats::{ConnectOptions, ServerAddr, ToServerAddrs};
use common::error::PlatformError;

#[derive(PartialEq, Eq)]
enum Authentication {
    None,
    Token(String),
    UserPassword(String, String),
}
fn invalid() -> PlatformError {
    PlatformError::messaging("invalid NATS URL authentication")
}
fn decode(value: &str) -> Result<String, PlatformError> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut input = value.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        bytes.push(if byte == b'%' {
            let hi = input
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or_else(invalid)?;
            let lo = input
                .next()
                .and_then(|b| (b as char).to_digit(16))
                .ok_or_else(invalid)?;
            ((hi << 4) | lo) as u8
        } else {
            byte
        });
    }
    let value = String::from_utf8(bytes).map_err(|_| invalid())?;
    if value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(value)
}
fn parse_address(
    value: &str,
    credentials_file: bool,
) -> Result<(ServerAddr, Authentication), PlatformError> {
    if value.len() > 16384 || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    let mut addresses = value.to_server_addrs().map_err(|_| invalid())?;
    let address = addresses.next().ok_or_else(invalid)?;
    if addresses.next().is_some() {
        return Err(invalid());
    }
    let authentication = match (address.username(), address.password()) {
        (None, None) => {
            let authority = value
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(value)
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("");
            if authority.contains('@') {
                return Err(invalid());
            }
            Authentication::None
        }
        (Some(_), _) if credentials_file => {
            return Err(PlatformError::messaging(
                "NATS URL authentication and credentials file cannot be combined",
            ))
        }
        (Some(user), Some(password)) => {
            Authentication::UserPassword(decode(user)?, decode(password)?)
        }
        (Some(token), None) => Authentication::Token(decode(token)?),
        (None, Some(_)) => return Err(invalid()),
    };
    let mut url = address.into_inner();
    url.set_password(None).map_err(|_| invalid())?;
    url.set_username("").map_err(|_| invalid())?;
    let address = ServerAddr::from_url(url).map_err(|_| invalid())?;
    Ok((address, authentication))
}
fn parse(
    value: &str,
    credentials_file: bool,
) -> Result<(Vec<ServerAddr>, Authentication), PlatformError> {
    if value.len() > 16384 || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    let mut addresses = Vec::new();
    let mut authentication = None;
    for entry in value.split(',') {
        if addresses.len() == 16 || entry.trim().is_empty() {
            return Err(invalid());
        }
        let (address, auth) = parse_address(entry.trim(), credentials_file)?;
        if authentication
            .as_ref()
            .is_some_and(|existing| existing != &auth)
        {
            return Err(PlatformError::messaging(
                "NATS server addresses must use identical authentication",
            ));
        }
        if authentication.is_none() {
            authentication = Some(auth);
        }
        addresses.push(address);
    }
    Ok((addresses, authentication.ok_or_else(invalid)?))
}
pub(super) fn options(
    value: &str,
    credentials_file: bool,
) -> Result<(Vec<ServerAddr>, ConnectOptions), PlatformError> {
    let (address, auth) = parse(value, credentials_file)?;
    let options = match auth {
        Authentication::None => ConnectOptions::new(),
        Authentication::Token(token) => ConnectOptions::new().token(token),
        Authentication::UserPassword(user, password) => {
            ConnectOptions::new().user_and_password(user, password)
        }
    };
    Ok((address, options))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn url_credentials_are_decoded_once_and_removed_from_sdk_addresses() {
        let (address, auth) = parse("tls://u%40ser:p%3Ass%25+word@[::1]:4223", false).unwrap();
        assert_eq!(address[0].as_url_str(), "tls://[::1]:4223");
        assert!(address[0].tls_required());
        match auth {
            Authentication::UserPassword(user, password) => {
                assert_eq!(user, "u@ser");
                assert_eq!(password, "p:ss%+word");
            }
            _ => panic!("expected user/password"),
        }
        let (address, auth) = parse("nats://tok%40en%2B@localhost:4222", false).unwrap();
        assert!(address[0].username().is_none() && address[0].password().is_none());
        assert!(matches!(auth,Authentication::Token(token) if token=="tok@en+"));
        let (address, auth) = parse("ws://localhost:8080/nats", true).unwrap();
        assert_eq!(address[0].as_url_str(), "ws://localhost:8080/nats");
        assert!(matches!(auth, Authentication::None));
    }
    #[test]
    fn malformed_credentials_and_ambiguous_auth_fail_without_echoing_inputs() {
        for url in [
            "nats://u:private%GG@localhost",
            "nats://private%FF@localhost",
            "nats://private%0A@localhost",
            "nats://:private@localhost",
            "nats://u:private@host1:4222,nats://host2:4222",
        ] {
            let error = match parse(url, false) {
                Ok(_) => panic!("invalid URL accepted"),
                Err(error) => error,
            };
            assert!(!error.to_string().contains("private"));
        }
        for url in ["nats://user:private@localhost", "tls://private@localhost"] {
            assert!(parse(url, true).is_err());
        }
        assert!(options("localhost:4222", true).is_ok());
    }

    #[test]
    fn sdk_diagnostics_receive_no_url_credentials() {
        for input in [
            "nats://user-canary:password-canary@localhost:4222",
            "tls://token-canary@localhost:4222",
        ] {
            let (address, options) = options(input, false).unwrap();
            let diagnostics = format!("{address:?} {options:?}");
            for secret in ["user-canary", "password-canary", "token-canary"] {
                assert!(!diagnostics.contains(secret));
            }
        }
    }

    #[test]
    fn bounded_csv_preserves_every_server_and_requires_one_common_authentication() {
        let (addresses, auth) = parse(
            "nats://u:p%2Css@localhost:4222, tls://u:p%2Css@[::1]:4223",
            false,
        )
        .unwrap();
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].port(), 4222);
        assert_eq!(addresses[1].port(), 4223);
        assert!(!addresses[0].tls_required());
        assert!(addresses[1].tls_required());
        assert!(addresses
            .iter()
            .all(|a| a.username().is_none() && a.password().is_none()));
        assert!(
            matches!(auth,Authentication::UserPassword(user,password) if user=="u"&&password=="p,ss")
        );
        assert!(parse(
            "nats://token@localhost:4222,tls://token@localhost:4223",
            false
        )
        .is_ok());
        assert!(parse("localhost:4222,tls://localhost:4223", true).is_ok());
        for value in [
            "nats://u:p@localhost:4222,nats://localhost:4223",
            "nats://localhost:4222,nats://token@localhost:4223",
            "nats://u:p@localhost:4222,nats://u:other@localhost:4223",
            "nats://token@localhost:4222,nats://other@localhost:4223",
            "nats://u:p@localhost:4222,",
            "nats://localhost:4222,,nats://localhost:4223",
            "nats://localhost:4222,invalid://localhost:4223",
        ] {
            assert!(parse(value, false).is_err());
        }
        assert!(parse(&["nats://localhost:4222"; 16].join(","), false).is_ok());
        assert!(parse(&["nats://localhost:4222"; 17].join(","), false).is_err());
        assert!(parse("nats://u:p@localhost:4222,nats://u:p@localhost:4223", true).is_err());
    }
}
