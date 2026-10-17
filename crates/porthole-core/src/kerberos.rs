//! Kerberos for domain sign-in: CredSSP negotiates Kerberos (falling back to NTLM), and sspi's
//! requests to the KDC go out over TCP or UDP from here.
//!
//! The KDC is found from `SSPI_KDC_URL`, `/etc/krb5.conf`, or DNS SRV records for the realm.

use std::net::IpAddr;
use std::time::Duration;

use ironrdp::connector::sspi::generator::NetworkRequest;
use ironrdp::connector::sspi::network_client::NetworkProtocol;
use ironrdp::connector::{self, ConnectorResult};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UdpSocket};
use tracing::debug;

use crate::config::{Credentials, SessionConfig};

const KDC_TIMEOUT: Duration = Duration::from_secs(10);
/// KDC replies are small; anything bigger is refused.
const MAX_REPLY: usize = 1024 * 1024;

/// Kerberos needs a domain account and the PC's name (its service principal is TERMSRV/<name>);
/// local accounts and IP addresses stay on NTLM.
pub(crate) fn wanted(config: &SessionConfig) -> bool {
    config.kerberos && domain_account(&config.credentials) && config.host.parse::<IpAddr>().is_err()
}

fn domain_account(credentials: &Credentials) -> bool {
    credentials.domain.as_deref().is_some_and(|d| !d.is_empty() && d != ".") || credentials.username.contains('@')
}

/// Sends sspi's KDC requests.
pub(crate) struct KdcClient;

impl ironrdp_tokio::NetworkClient for KdcClient {
    async fn send(&mut self, request: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        debug!(url = %request.url, protocol = ?request.protocol, "KDC request");
        let exchange = async {
            match request.protocol {
                NetworkProtocol::Tcp => tcp(&request.url, &request.data).await,
                NetworkProtocol::Udp => udp(&request.url, &request.data).await,
                NetworkProtocol::Http | NetworkProtocol::Https => {
                    Err("KDC proxies over HTTPS aren't supported yet".to_owned())
                }
            }
        };
        match tokio::time::timeout(KDC_TIMEOUT, exchange).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(e)) => Err(connector::custom_err!("KDC", std::io::Error::other(e))),
            Err(_) => Err(connector::custom_err!(
                "KDC",
                std::io::Error::new(std::io::ErrorKind::TimedOut, "the domain controller didn't answer")
            )),
        }
    }
}

fn address(url: &url::Url) -> Result<String, String> {
    let host = url.host_str().ok_or("KDC URL without a host")?;
    Ok(format!("{host}:{}", url.port().unwrap_or(88)))
}

/// Kerberos over TCP: each message is prefixed with its length (RFC 4120 7.2.2). sspi's request
/// already carries the prefix; the reply's is kept, as sspi expects.
async fn tcp(url: &url::Url, data: &[u8]) -> Result<Vec<u8>, String> {
    let mut stream = TcpStream::connect(address(url)?).await.map_err(|e| e.to_string())?;
    stream.write_all(data).await.map_err(|e| e.to_string())?;
    let mut len = [0; 4];
    stream.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let size = u32::from_be_bytes(len) as usize;
    if size > MAX_REPLY {
        return Err("KDC reply too large".to_owned());
    }
    let mut reply = vec![0; 4 + size];
    reply[..4].copy_from_slice(&len);
    stream.read_exact(&mut reply[4..]).await.map_err(|e| e.to_string())?;
    Ok(reply)
}

/// Kerberos over UDP: one datagram each way.
async fn udp(url: &url::Url, data: &[u8]) -> Result<Vec<u8>, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
    socket.connect(address(url)?).await.map_err(|e| e.to_string())?;
    socket.send(data).await.map_err(|e| e.to_string())?;
    let mut buffer = vec![0; 65_535];
    let n = socket.recv(&mut buffer).await.map_err(|e| e.to_string())?;
    // sspi expects the TCP-style length prefix on replies.
    let mut reply = (n as u32).to_be_bytes().to_vec();
    reply.extend_from_slice(&buffer[..n]);
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(host: &str, user: &str) -> SessionConfig {
        SessionConfig::new(host, Credentials::parse(user, "x"))
    }

    #[test]
    fn kerberos_only_for_domain_accounts_on_named_pcs() {
        assert!(wanted(&config("pc.corp.example", r"CORP\me")));
        assert!(wanted(&config("pc", "me@corp.example")));
        assert!(!wanted(&config("pc.corp.example", "me")));
        assert!(!wanted(&config("10.0.0.5", r"CORP\me")));
        assert!(!wanted(&config("fe80::1", r"CORP\me")));
        let mut off = config("pc.corp.example", r"CORP\me");
        off.kerberos = false;
        assert!(!wanted(&off));
    }
}
