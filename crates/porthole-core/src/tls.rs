//! TLS upgrade with certificate verification that reports problems instead of failing.
//!
//! RDP servers almost always use self-signed certificates, so a failed verification has to be
//! shown to the user rather than abort the handshake. The verifier runs the normal WebPKI checks
//! against the system roots, records the outcome, and lets the handshake finish; the session
//! then asks the UI before any credentials are sent (CredSSP starts after this).

use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    self, CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::{debug, warn};

use crate::event::{CertificateInfo, CertificateProblem};

pub(crate) struct Upgraded {
    pub stream: TlsStream<TcpStream>,
    pub certificate: CertificateInfo,
    /// `None` when the certificate chain is valid for the host.
    pub problem: Option<CertificateProblem>,
    /// The subject public key, which CredSSP binds the authentication to.
    pub server_public_key: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TlsError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Rustls(#[from] rustls::Error),
    #[error("invalid server name: {0}")]
    ServerName(String),
    #[error("server sent no certificate")]
    NoCertificate,
    #[error("unreadable server certificate: {0}")]
    BadCertificate(String),
}

pub(crate) async fn upgrade(tcp: TcpStream, host: &str) -> Result<Upgraded, TlsError> {
    let verifier = Arc::new(RecordingVerifier {
        inner: webpki_verifier(),
        provider: provider(),
        outcome: Mutex::new(None),
    });
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_no_client_auth();
    // CredSSP does not support TLS session resumption (MS-CSSP 3.1.5).
    config.resumption = rustls::client::Resumption::disabled();

    let name = ServerName::try_from(host.to_owned()).map_err(|e| TlsError::ServerName(e.to_string()))?;
    let stream = TlsConnector::from(Arc::new(config)).connect(name, tcp).await?;

    let der = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or(TlsError::NoCertificate)?
        .to_vec();
    let (certificate, server_public_key) = inspect(host, der)?;
    let problem = match verifier.outcome.lock().unwrap_or_else(|e| e.into_inner()).take() {
        Some(Ok(())) => None,
        Some(Err(e)) => Some(classify(&e)),
        None => Some(CertificateProblem::Other("the certificate was not checked".to_owned())),
    };
    debug!(sha256 = %certificate.sha256_fingerprint, ?problem, "Server certificate");
    Ok(Upgraded {
        stream,
        certificate,
        problem,
        server_public_key,
    })
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// The system trust store, loaded once per process.
fn system_roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let loaded = rustls_native_certs::load_native_certs();
            for e in &loaded.errors {
                warn!(error = %e, "Problem loading system certificates");
            }
            let mut roots = RootCertStore::empty();
            let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
            debug!(added, ignored, "Loaded system root certificates");
            Arc::new(roots)
        })
        .clone()
}

/// `None` when the system has no root certificates (WebPKI refuses an empty store); every
/// certificate is then reported as untrusted.
fn webpki_verifier() -> Option<Arc<WebPkiServerVerifier>> {
    let roots = system_roots();
    if roots.is_empty() {
        return None;
    }
    WebPkiServerVerifier::builder_with_provider(roots, provider())
        .build()
        .inspect_err(|e| warn!(error = %e, "Certificate verifier unavailable"))
        .ok()
}

#[derive(Debug)]
struct RecordingVerifier {
    inner: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
    outcome: Mutex<Option<Result<(), rustls::Error>>>,
}

impl ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let result = match &self.inner {
            Some(inner) => inner
                .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
                .map(|_| ()),
            None => Err(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer)),
        };
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        // Let the handshake complete; the session decides (with the user) before sending credentials.
        Ok(ServerCertVerified::assertion())
    }

    // Handshake signatures are always checked: they prove the server holds the certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn classify(e: &rustls::Error) -> CertificateProblem {
    match e {
        rustls::Error::InvalidCertificate(c) => match c {
            CertificateError::UnknownIssuer | CertificateError::BadSignature => CertificateProblem::UntrustedIssuer,
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                CertificateProblem::NameMismatch
            }
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => CertificateProblem::Expired,
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                CertificateProblem::NotYetValid
            }
            CertificateError::Revoked => CertificateProblem::Revoked,
            other => CertificateProblem::Other(format!("{other:?}")),
        },
        other => CertificateProblem::Other(other.to_string()),
    }
}

fn inspect(host: &str, der: Vec<u8>) -> Result<(CertificateInfo, Vec<u8>), TlsError> {
    use x509_cert::der::Decode as _;

    let cert = x509_cert::Certificate::from_der(&der).map_err(|e| TlsError::BadCertificate(e.to_string()))?;
    let tbs = &cert.tbs_certificate;
    let public_key = tbs
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| TlsError::BadCertificate("subject public key is not byte aligned".to_owned()))?
        .to_owned();
    let info = CertificateInfo {
        host: host.to_owned(),
        subject: tbs.subject.to_string(),
        issuer: tbs.issuer.to_string(),
        not_before: tbs.validity.not_before.to_string(),
        not_after: tbs.validity.not_after.to_string(),
        sha256_fingerprint: fingerprint(&der),
        der,
    };
    Ok((info, public_key))
}

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}
