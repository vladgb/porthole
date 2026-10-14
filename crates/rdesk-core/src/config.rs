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
    fn debug_hides_password() {
        let c = Credentials::parse("alice", "hunter2");
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
