//! Phase 0 spike: connect to an RDP server with IronRDP (TLS + NLA/CredSSP), let the
//! desktop render until updates settle, and save the framebuffer as a PNG.
//!
//! ```shell
//! RDESK_PASSWORD=... cargo run -p rdp-screenshot -- --host win11.lan -u alice -o out.png
//! ```
//!
//! Based on IronRDP's `screenshot` example, with the password read from the environment or a
//! prompt (never argv), the server certificate fingerprint printed (input for the future trust
//! dialog), and capture stopping once the screen has been quiet for `--settle` seconds.

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::Parser;
use ironrdp::connector::{self, ConnectionResult, Credentials};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput};
use sha2::{Digest as _, Sha256};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use tokio_rustls::rustls;
use tracing::{debug, info, warn};

#[derive(Parser, Debug)]
#[command(about = "Connect to an RDP server and save a screenshot of the remote desktop")]
struct Args {
    /// Server hostname or IP address.
    #[arg(long)]
    host: String,
    #[arg(long, default_value_t = 3389)]
    port: u16,
    /// Username, optionally as DOMAIN\user.
    #[arg(short, long)]
    username: String,
    #[arg(short, long)]
    domain: Option<String>,
    #[arg(short, long, default_value = "out.png")]
    output: PathBuf,
    #[arg(long, default_value_t = 1920)]
    width: u16,
    #[arg(long, default_value_t = 1080)]
    height: u16,
    /// Use plain TLS security instead of NLA (CredSSP).
    #[arg(long)]
    no_nla: bool,
    /// Stop after this many seconds without graphics updates.
    #[arg(long, default_value_t = 3)]
    settle: u64,
    /// Hard limit for the whole capture, in seconds.
    #[arg(long, default_value_t = 30)]
    timeout: u64,
}

type TlsFramed = ironrdp_blocking::Framed<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("RDESK_LOG").unwrap_or_else(|_| "info,sspi=warn".into()),
        )
        .compact()
        .init();

    let args = Args::parse();
    // Never taken from argv, where other local users could read it via /proc.
    let password = match std::env::var("RDESK_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password(format!("Password for {}: ", args.username))?,
    };
    let (username, domain) = match args.username.split_once('\\') {
        Some((d, u)) => (u.to_owned(), Some(d.to_owned())),
        None => (args.username.clone(), args.domain.clone()),
    };

    let config = build_config(&args, username, password, domain);
    let started = Instant::now();
    let (result, mut framed) = connect(config, &args.host, args.port).context("connect")?;
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        width = result.desktop_size.width,
        height = result.desktop_size.height,
        compression = ?result.compression_type,
        "Connected"
    );

    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    );
    let updates = capture(
        result,
        &mut framed,
        &mut image,
        Duration::from_secs(args.settle),
        Duration::from_secs(args.timeout),
    )
    .context("active stage")?;

    let png: image::RgbaImage = image::ImageBuffer::from_raw(
        u32::from(image.width()),
        u32::from(image.height()),
        image.data().to_vec(),
    )
    .context("framebuffer size mismatch")?;
    png.save(&args.output).context("save PNG")?;
    info!(updates, output = %args.output.display(), total_ms = started.elapsed().as_millis(), "Saved screenshot");
    Ok(())
}

fn build_config(args: &Args, username: String, password: String, domain: Option<String>) -> connector::Config {
    connector::Config {
        credentials: Credentials::UsernamePassword { username, password },
        domain,
        // With NLA, CredSSP runs over TLS (PROTOCOL_HYBRID); without it, request PROTOCOL_SSL.
        enable_tls: args.no_nla,
        enable_credssp: !args.no_nla,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: args.width,
            height: args.height,
        },
        bitmap: None,
        client_build: 0,
        client_name: "rdesk-spike".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        // Log on with the supplied credentials instead of showing the server login screen (mstsc does the same).
        autologon: true,
        enable_audio_playback: false,
        compression_type: None,
        pointer_software_rendering: true,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

fn connect(config: connector::Config, host: &str, port: u16) -> anyhow::Result<(ConnectionResult, TlsFramed)> {
    let addr: SocketAddr = (host, port).to_socket_addrs()?.next().context("no address for host")?;
    debug!(%addr, "Resolved");

    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).context("TCP connect")?;
    tcp.set_nodelay(true)?;
    let client_addr = tcp.local_addr()?;

    let mut framed = ironrdp_blocking::Framed::new(tcp);
    let mut connector = connector::ClientConnector::new(config, client_addr);
    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut connector).context("negotiation")?;

    let (tls, server_public_key) = tls_upgrade(framed.into_inner_no_leftover(), host).context("TLS upgrade")?;
    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = ironrdp_blocking::Framed::new(tls);

    let result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut ReqwestNetworkClient,
        host.to_owned().into(),
        server_public_key,
        None,
    )
    .context("finalize (CredSSP / licensing / capabilities)")?;

    // Short read timeout so the capture loop can notice when the screen has gone quiet.
    framed
        .get_inner()
        .0
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok((result, framed))
}

/// Runs the active stage until no graphics update arrives for `settle`, or `limit` elapses.
/// Returns the number of graphics updates applied.
fn capture(
    result: ConnectionResult,
    framed: &mut TlsFramed,
    image: &mut DecodedImage,
    settle: Duration,
    limit: Duration,
) -> anyhow::Result<usize> {
    let mut stage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();

    let start = Instant::now();
    let mut last_update = start;
    let mut updates = 0usize;

    loop {
        if start.elapsed() >= limit {
            warn!("Capture time limit reached");
            break;
        }
        if updates > 0 && last_update.elapsed() >= settle {
            break;
        }

        let (action, payload) = match framed.read_pdu() {
            Ok(frame) => frame,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("read PDU")),
        };

        for out in stage.process(image, action, &payload)? {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).context("write response")?,
                ActiveStageOutput::GraphicsUpdate(_) => {
                    updates += 1;
                    last_update = Instant::now();
                }
                ActiveStageOutput::Terminate(reason) => {
                    info!(?reason, "Server ended the session");
                    return Ok(updates);
                }
                ActiveStageOutput::DeactivateAll => {
                    // Reactivation (e.g. server-side resize) isn't handled in the spike.
                    warn!("Server sent Deactivate All; stopping capture");
                    return Ok(updates);
                }
                other => debug!(?other, "Unhandled output"),
            }
        }
    }

    if let Ok(outputs) = stage.graceful_shutdown() {
        for out in outputs {
            if let ActiveStageOutput::ResponseFrame(frame) = out {
                let _ = framed.write_all(&frame);
            }
        }
    }
    Ok(updates)
}

fn tls_upgrade(
    stream: TcpStream,
    host: &str,
) -> anyhow::Result<(rustls::StreamOwned<rustls::ClientConnection, TcpStream>, Vec<u8>)> {
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::AcceptAnyCertificate))
        .with_no_client_auth();
    // CredSSP does not support TLS session resumption (MS-CSSP 3.1.5).
    config.resumption = rustls::client::Resumption::disabled();
    config.key_log = Arc::new(rustls::KeyLogFile::new());

    let conn = rustls::ClientConnection::new(Arc::new(config), host.to_owned().try_into()?)?;
    let mut tls = rustls::StreamOwned::new(conn, stream);
    tls.flush()?; // drive the handshake so the peer certificate is available

    let cert = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .context("server sent no certificate")?;
    let public_key = inspect_certificate(cert)?;
    Ok((tls, public_key))
}

/// Logs what the trust dialog will show and returns the subject public key CredSSP binds to.
fn inspect_certificate(der: &[u8]) -> anyhow::Result<Vec<u8>> {
    use x509_cert::der::Decode as _;

    let cert = x509_cert::Certificate::from_der(der)?;
    let tbs = &cert.tbs_certificate;
    let fingerprint = Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    info!(
        subject = %tbs.subject,
        issuer = %tbs.issuer,
        not_before = %tbs.validity.not_before,
        not_after = %tbs.validity.not_after,
        sha256 = %fingerprint,
        "Server certificate (NOT verified; spike accepts any certificate)"
    );

    Ok(tbs
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .context("subject public key BIT STRING is not aligned")?
        .to_owned())
}

mod danger {
    use tokio_rustls::rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use tokio_rustls::rustls::crypto::{self, CryptoProvider};
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme, pki_types};

    /// Accepts any server certificate but still checks handshake signatures.
    /// The real client replaces this with a verifier backed by the trust dialog + fingerprint store.
    #[derive(Debug)]
    pub(super) struct AcceptAnyCertificate;

    fn provider() -> std::sync::Arc<CryptoProvider> {
        CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| std::sync::Arc::new(crypto::aws_lc_rs::default_provider()))
    }

    impl ServerCertVerifier for AcceptAnyCertificate {
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &pki_types::CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            crypto::verify_tls12_signature(message, cert, dss, &provider().signature_verification_algorithms)
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &pki_types::CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            crypto::verify_tls13_signature(message, cert, dss, &provider().signature_verification_algorithms)
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            provider().signature_verification_algorithms.supported_schemes()
        }
    }
}
