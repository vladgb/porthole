//! RemoteApp spike: connect in RAIL mode, start one program, log the `rail` channel and the
//! windowing orders, then save each remote window as a PNG cut out of the desktop image.
//!
//! ```shell
//! PORTHOLE_PASSWORD=... cargo run -p rdp-remoteapp -- --host win11.lan -u alice --app notepad.exe
//! ```
//!
//! The PC must allow unlisted programs: set `fAllowUnlistedRemotePrograms` (DWORD 1) under
//! `HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services`, or publish the program and
//! pass its alias as `--app '||alias'`.

mod orders;
mod rail;
mod wire;

use std::io::Write as _;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::Parser;
use ironrdp::connector::{self, ConnectionResult, Credentials};
use ironrdp::core::{ReadCursor, decode_cursor};
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::Action;
use ironrdp::pdu::fast_path::{FastPathHeader, FastPathUpdatePdu, UpdateCode};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::input::fast_path::FastPathInputEvent;
use ironrdp::pdu::input::{MousePdu, mouse::PointerFlags};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput};
use sha2::{Digest as _, Sha256};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use tokio_rustls::rustls;
use tracing::{debug, info, warn};

use crate::orders::Windows;
use crate::rail::RailChannel;

#[derive(Parser, Debug)]
#[command(about = "Start one program over RemoteApp and save each of its windows as a PNG")]
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
    /// Program to start: a path, a name on the PC's PATH, or `||alias` for a published app.
    /// Empty to start nothing and only list the windows the session already has.
    #[arg(long, default_value = "notepad.exe")]
    app: String,
    #[arg(long, default_value = "")]
    args: String,
    #[arg(long, default_value = "")]
    working_dir: String,
    /// Folder for desktop.png and one PNG per window.
    #[arg(short, long, default_value = "remoteapp-out")]
    out_dir: PathBuf,
    /// Size of the virtual desktop the windows live on (the local screen, for a real client).
    #[arg(long, default_value_t = 1920)]
    width: u16,
    #[arg(long, default_value_t = 1080)]
    height: u16,
    /// Use plain TLS security instead of NLA (CredSSP).
    #[arg(long)]
    no_nla: bool,
    /// Once a window exists, stop after this many seconds without updates.
    #[arg(long, default_value_t = 4)]
    settle: u64,
    /// Click here (desktop pixels) if the screen goes quiet before RemoteApp starts, e.g. to
    /// answer Yes on "Another user is signed in" when the PC's console is in use.
    #[arg(long, value_parser = parse_point)]
    click: Option<(u16, u16)>,
    /// Hard limit for the whole run, in seconds.
    #[arg(long, default_value_t = 45)]
    timeout: u64,
}

fn parse_point(s: &str) -> Result<(u16, u16), String> {
    let (x, y) = s.split_once(',').ok_or("expected X,Y")?;
    Ok((
        x.trim().parse().map_err(|e| format!("{e}"))?,
        y.trim().parse().map_err(|e| format!("{e}"))?,
    ))
}

type TlsFramed = ironrdp_blocking::Framed<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PORTHOLE_LOG")
                // The session warns about every Orders update it skips; those are ours.
                .unwrap_or_else(|_| "info,sspi=warn,ironrdp_session::fast_path=error".into()),
        )
        .compact()
        .init();

    let args = Args::parse();
    // Never taken from argv, where other local users could read it via /proc.
    let password = match std::env::var("PORTHOLE_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password(format!("Password for {}: ", args.username))?,
    };
    let (username, domain) = match args.username.split_once('\\') {
        Some((d, u)) => (u.to_owned(), Some(d.to_owned())),
        None => (args.username.clone(), args.domain.clone()),
    };

    let config = build_config(&args, username, password, domain);
    let rail = RailChannel::new(
        args.app.clone(),
        args.args.clone(),
        args.working_dir.clone(),
        args.width,
        args.height,
    );
    let started = Instant::now();
    let (result, mut framed) = connect(config, rail, &args.host, args.port).context("connect")?;
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        width = result.desktop_size.width,
        height = result.desktop_size.height,
        "Connected in RemoteApp mode"
    );

    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    );
    let mut windows = Windows::default();
    run(
        result,
        &mut framed,
        &mut image,
        &mut windows,
        Duration::from_secs(args.settle),
        Duration::from_secs(args.timeout),
        args.click,
    )
    .context("active stage")?;

    save(&args.out_dir, &image, &windows)?;
    info!(
        total_ms = started.elapsed().as_millis(),
        orders = windows.orders,
        "Done"
    );
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
        client_name: "porthole-spike".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        // Log on with the supplied credentials instead of showing the server login screen (mstsc does the same).
        autologon: true,
        enable_audio_playback: false,
        enable_gfx: false,
        enable_audio_capture: false,
        enable_rail: true,
        // Uncompressed, so the windowing orders can be read straight off the wire.
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

fn connect(
    config: connector::Config,
    rail: RailChannel,
    host: &str,
    port: u16,
) -> anyhow::Result<(ConnectionResult, TlsFramed)> {
    let addr: SocketAddr = (host, port).to_socket_addrs()?.next().context("no address for host")?;
    debug!(%addr, "Resolved");

    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10)).context("TCP connect")?;
    tcp.set_nodelay(true)?;
    let client_addr = tcp.local_addr()?;

    let mut framed = ironrdp_blocking::Framed::new(tcp);
    let mut connector = connector::ClientConnector::new(config, client_addr).with_static_channel(rail);
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

    // Short read timeout so the loop can notice when things have gone quiet.
    framed
        .get_inner()
        .0
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))?;
    Ok((result, framed))
}

/// Runs the active stage until the windows have settled or `limit` elapses.
fn run(
    result: ConnectionResult,
    framed: &mut TlsFramed,
    image: &mut DecodedImage,
    windows: &mut Windows,
    settle: Duration,
    limit: Duration,
    mut click: Option<(u16, u16)>,
) -> anyhow::Result<()> {
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

    loop {
        if start.elapsed() >= limit {
            warn!(windows = windows.windows.len(), "Time limit reached");
            break;
        }
        if !windows.windows.is_empty() && last_update.elapsed() >= settle {
            break;
        }

        let rail_started = stage
            .get_svc_processor::<RailChannel>()
            .is_some_and(RailChannel::started);
        if !rail_started
            && last_update.elapsed() >= Duration::from_secs(2)
            && let Some((x, y)) = click.take()
        {
            info!(x, y, "Screen is quiet and RemoteApp hasn't started; clicking");
            let mouse = |flags| {
                FastPathInputEvent::MouseEvent(MousePdu {
                    flags,
                    number_of_wheel_rotation_units: 0,
                    x_position: x,
                    y_position: y,
                })
            };
            let events = [
                mouse(PointerFlags::MOVE),
                mouse(PointerFlags::LEFT_BUTTON | PointerFlags::DOWN),
                mouse(PointerFlags::LEFT_BUTTON),
            ];
            for out in stage.process_fastpath_input(image, &events)? {
                if let ActiveStageOutput::ResponseFrame(frame) = out {
                    framed.write_all(&frame).context("write input")?;
                }
            }
            last_update = Instant::now();
        }

        let (action, payload) = match framed.read_pdu() {
            Ok(frame) => frame,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("read PDU")),
        };

        if action == Action::FastPath {
            let before = windows.orders;
            peek_orders(&payload, windows);
            if windows.orders != before {
                last_update = Instant::now();
            }
        }

        let outputs = match stage.process(image, action, &payload) {
            Ok(outputs) => outputs,
            Err(e) => {
                // E.g. a Logon Errors Info whose type is an NTSTATUS, which IronRDP can't decode.
                let tail: String = payload.iter().map(|b| format!("{b:02x}")).collect();
                warn!(error = %e, ?action, len = payload.len(), %tail, "Skipping a PDU the session couldn't process");
                continue;
            }
        };
        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).context("write response")?,
                ActiveStageOutput::GraphicsUpdate(_) => last_update = Instant::now(),
                ActiveStageOutput::Terminate(reason) => {
                    info!(?reason, "Server ended the session");
                    return Ok(());
                }
                ActiveStageOutput::DeactivateAll => {
                    warn!("Server sent Deactivate All; stopping");
                    return Ok(());
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
    Ok(())
}

/// Pulls the Orders updates out of a fast-path frame before the session (which skips them) sees it.
fn peek_orders(frame: &[u8], windows: &mut Windows) {
    let mut cursor = ReadCursor::new(frame);
    if decode_cursor::<FastPathHeader>(&mut cursor).is_err() {
        return;
    }
    while !cursor.remaining().is_empty() {
        let Ok(update) = decode_cursor::<FastPathUpdatePdu<'_>>(&mut cursor) else {
            return;
        };
        if update.update_code != UpdateCode::Orders {
            continue;
        }
        if update.compression_flags.is_some_and(|f| !f.is_empty()) {
            warn!("Compressed Orders update; the spike doesn't decompress");
            continue;
        }
        windows.fast_path_orders(update.data, update.fragmentation);
    }
}

/// Writes the whole desktop image and, for each shown window, the part of it the window covers.
fn save(dir: &Path, image: &DecodedImage, windows: &Windows) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let desktop: image::RgbaImage = image::ImageBuffer::from_raw(
        u32::from(image.width()),
        u32::from(image.height()),
        image.data().to_vec(),
    )
    .context("framebuffer size mismatch")?;
    desktop.save(dir.join("desktop.png")).context("save desktop.png")?;

    for (id, w) in &windows.windows {
        let x0 = w.offset.0.clamp(0, i32::from(image.width()));
        let y0 = w.offset.1.clamp(0, i32::from(image.height()));
        let x1 = (i64::from(w.offset.0) + i64::from(w.size.0)).clamp(0, i64::from(image.width()));
        let y1 = (i64::from(w.offset.1) + i64::from(w.size.1)).clamp(0, i64::from(image.height()));
        let (x0, y0) = (u32::try_from(x0)?, u32::try_from(y0)?);
        let (x1, y1) = (u32::try_from(x1)?, u32::try_from(y1)?);
        info!(
            window = format!("{id:#x}"),
            kind = w.kind(),
            title = %w.title,
            show = w.show,
            rect = format!("{}x{}+{}+{}", w.size.0, w.size.1, w.offset.0, w.offset.1),
            client = format!("{}x{}+{}+{}", w.client_size.0, w.client_size.1, w.client_offset.0, w.client_offset.1),
            visible_rects = ?w.visible_rects,
            "Final window"
        );
        // SW_HIDE, or entirely off the desktop.
        if w.show == 0 || x1 <= x0 || y1 <= y0 {
            continue;
        }
        let crop = image::imageops::crop_imm(&desktop, x0, y0, x1 - x0, y1 - y0).to_image();
        let path = dir.join(format!("window-{id:08x}.png"));
        crop.save(&path).with_context(|| format!("save {}", path.display()))?;
        info!(path = %path.display(), "Saved window");
    }
    Ok(())
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
    let tbs = cert.tbs_certificate();
    let fingerprint = Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    info!(
        subject = %tbs.subject(),
        issuer = %tbs.issuer(),
        not_before = %tbs.validity().not_before,
        not_after = %tbs.validity().not_after,
        sha256 = %fingerprint,
        "Server certificate (NOT verified; spike accepts any certificate)"
    );

    Ok(tbs
        .subject_public_key_info()
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
