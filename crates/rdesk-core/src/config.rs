use std::fmt;
use std::time::Duration;

/// Everything needed to open one session.
#[derive(Clone)]
pub struct SessionConfig {
    /// Hostname or IP address (no port).
    pub host: String,
    pub port: u16,
    pub credentials: Credentials,
    pub security: Security,
    /// Requested desktop size in pixels. The server may pick a different one.
    pub desktop_width: u16,
    pub desktop_height: u16,
    /// Windows keyboard layout id (KLID), e.g. `0x0409` for US English.
    pub keyboard_layout: u32,
    /// Shown on the server as the client name (truncated to 15 characters by the protocol).
    pub client_name: String,
    /// Limit for each network phase of connecting (TCP, TLS, sign-in, activation).
    /// Time spent waiting for the user to answer a certificate prompt is not counted.
    pub connect_timeout: Duration,
}

impl SessionConfig {
    pub fn new(host: impl Into<String>, credentials: Credentials) -> Self {
        Self {
            host: host.into(),
            port: 3389,
            credentials,
            security: Security::Nla,
            desktop_width: 1920,
            desktop_height: 1080,
            keyboard_layout: 0x0409,
            client_name: default_client_name(),
            connect_timeout: Duration::from_secs(20),
        }
    }
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("credentials", &self.credentials)
            .field("security", &self.security)
            .field("desktop", &(self.desktop_width, self.desktop_height))
            .field("keyboard_layout", &format_args!("{:#06x}", self.keyboard_layout))
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Security {
    /// Network Level Authentication (CredSSP over TLS). Credentials are checked before a session is created.
    #[default]
    Nla,
    /// TLS only; the server shows its own sign-in screen if credentials are missing or wrong.
    Tls,
}

#[derive(Clone, Default)]
pub struct Credentials {
    pub username: String,
    pub domain: Option<String>,
    pub password: String,
}

impl Credentials {
    /// Splits `DOMAIN\user` into its parts. `user@domain` (UPN) is passed through as the username.
    pub fn parse(user: &str, password: impl Into<String>) -> Self {
        let (username, domain) = match user.split_once('\\') {
            Some((domain, user)) if !domain.is_empty() => (user.to_owned(), Some(domain.to_owned())),
            Some((_, user)) => (user.to_owned(), None),
            None => (user.to_owned(), None),
        };
        Self {
            username,
            domain,
            password: password.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("domain", &self.domain)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Splits `host`, `host:port`, `[v6]:port` or a bare IPv6 address. The default port is 3389.
pub fn parse_address(input: &str) -> Result<(String, u16), AddressError> {
    let input = input.trim();
    let port = |p: &str| {
        p.parse::<u16>()
            .ok()
            .filter(|&p| p != 0)
            .ok_or(AddressError::InvalidPort)
    };
    let (host, port) = if let Some(rest) = input.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or(AddressError::Invalid)?;
        match after.strip_prefix(':') {
            Some(p) => (host, port(p)?),
            None if after.is_empty() => (host, 3389),
            None => return Err(AddressError::Invalid),
        }
    } else {
        match input.rsplit_once(':') {
            // A bare IPv6 address has several colons and no port.
            Some((host, p)) if !host.contains(':') => (host, port(p)?),
            _ => (input, 3389),
        }
    };
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err(AddressError::Invalid);
    }
    Ok((host.to_owned(), port))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("Enter a PC name or IP address, optionally followed by :port")]
    Invalid,
    #[error("The port must be a number from 1 to 65535")]
    InvalidPort,
}

fn default_client_name() -> String {
    let name = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rdesk".to_owned());
    name.chars().take(15).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_domain_user() {
        let c = Credentials::parse(r"CONTOSO\alice", "pw");
        assert_eq!(c.username, "alice");
        assert_eq!(c.domain.as_deref(), Some("CONTOSO"));
    }

    #[test]
    fn keeps_upn_and_plain_names() {
        assert_eq!(
            Credentials::parse("alice@contoso.com", "").username,
            "alice@contoso.com"
        );
        let c = Credentials::parse(r"\alice", "");
        assert_eq!((c.username.as_str(), c.domain), ("alice", None));
    }

    #[test]
    fn parses_addresses() {
        assert_eq!(parse_address(" pc.lan "), Ok(("pc.lan".to_owned(), 3389)));
        assert_eq!(parse_address("10.0.0.5:3390"), Ok(("10.0.0.5".to_owned(), 3390)));
        assert_eq!(parse_address("[fe80::1]:3390"), Ok(("fe80::1".to_owned(), 3390)));
        assert_eq!(parse_address("[fe80::1]"), Ok(("fe80::1".to_owned(), 3389)));
        assert_eq!(parse_address("fe80::1"), Ok(("fe80::1".to_owned(), 3389)));
        assert_eq!(parse_address("pc:0"), Err(AddressError::InvalidPort));
        assert_eq!(parse_address("pc:x"), Err(AddressError::InvalidPort));
        assert_eq!(parse_address(""), Err(AddressError::Invalid));
        assert_eq!(parse_address("my pc"), Err(AddressError::Invalid));
    }

    #[test]
    fn debug_hides_password() {
        let c = Credentials::parse("alice", "hunter2");
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
