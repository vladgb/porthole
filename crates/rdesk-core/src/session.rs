use std::io;
use std::time::Duration;

use ironrdp::cliprdr::{CliprdrClient, CliprdrSvcMessages};
use ironrdp::connector::connection_activation::ConnectionActivationState;
use ironrdp::connector::sspi::credssp::NStatusCode;
use ironrdp::connector::{self, ClientConnector, ConnectionResult, ConnectorError, ConnectorErrorKind};
use ironrdp::core::WriteBuf;
use ironrdp::displaycontrol::client::DisplayControlClient;
use ironrdp::displaycontrol::pdu::MonitorLayoutEntry;
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::input::{Database, Operation};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::nego::FailureCode;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::rdpdr::Rdpdr;
use ironrdp::rdpdr::backend::noop::NoopRdpdrBackend;
use ironrdp::rdpsnd::client::Rdpsnd;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput, GracefulDisconnectReason, SessionError};
use ironrdp_tokio::{FramedWrite as _, TokioFramed};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_rustls::client::TlsStream;
use tracing::{debug, error, info, trace, warn};

use crate::clipboard::{self, Clipboard};
use crate::config::{Security, SessionConfig};
use crate::event::{AuthFailure, CertificatePrompt, DisconnectReason, PointerEvent, SessionEvent};
use crate::framebuffer::{Framebuffer, Rect};
use crate::tls;

/// How long to wait for the server to close the connection after we ask it to.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

enum Command {
    Input(Vec<Operation>),
    ReleaseAll,
    SyncLocks { caps: bool, num: bool, scroll: bool },
    Pause,
    ClipboardText(Option<String>),
    Resize { width: u32, height: u32, scale: u32 },
    Disconnect,
}

/// Controls a running session. Dropping it disconnects.
pub struct SessionHandle {
    commands: async_channel::Sender<Command>,
    framebuffer: Framebuffer,
}

impl SessionHandle {
    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    /// Queues keyboard/mouse operations. Ignored before the session is connected.
    pub fn send_input(&self, operations: impl IntoIterator<Item = Operation>) {
        let operations: Vec<_> = operations.into_iter().collect();
        if !operations.is_empty() {
            self.send(Command::Input(operations));
        }
    }

    /// Releases every key and button the remote side thinks is held (call on focus loss).
    pub fn release_all(&self) {
        self.send(Command::ReleaseAll);
    }

    pub fn synchronize_locks(&self, caps: bool, num: bool, scroll: bool) {
        self.send(Command::SyncLocks { caps, num, scroll });
    }

    /// Pause/Break has no make/break scancode of its own; it is sent as a special sequence.
    pub fn send_pause(&self) {
        self.send(Command::Pause);
    }

    /// Reports the local clipboard's text (`None`: it holds no text), offered to the remote PC.
    pub fn set_clipboard_text(&self, text: Option<String>) {
        self.send(Command::ClipboardText(text));
    }

    /// Asks the remote PC to change its desktop size (device pixels) and scale (percent, 0: keep).
    /// Takes effect once the display control channel is up; a `DesktopResized` event follows.
    /// Servers without dynamic resolution ignore it.
    pub fn resize(&self, width: u32, height: u32, scale: u32) {
        self.send(Command::Resize { width, height, scale });
    }

    /// Asks the server to end the session; a `Disconnected` event follows.
    pub fn disconnect(&self) {
        self.send(Command::Disconnect);
    }

    fn send(&self, command: Command) {
        // Unbounded: never fails while the session task is alive, and it's fine to drop input after.
        let _ = self.commands.try_send(command);
    }
}

/// Starts a session on `runtime` and returns immediately. Events arrive on the returned receiver;
/// the last one is always [`SessionEvent::Disconnected`], even if the session task panics.
pub fn connect(
    config: SessionConfig,
    runtime: &tokio::runtime::Handle,
) -> (SessionHandle, async_channel::Receiver<SessionEvent>) {
    let (command_tx, command_rx) = async_channel::unbounded();
    let (event_tx, event_rx) = async_channel::unbounded();
    let framebuffer = Framebuffer::default();

    let task = runtime.spawn(run(config, event_tx.clone(), command_rx, framebuffer.clone()));
    runtime.spawn(async move {
        let reason = match task.await {
            Ok(reason) => reason,
            Err(e) if e.is_panic() => {
                error!("Session task panicked");
                DisconnectReason::Internal("the session task crashed".to_owned())
            }
            Err(_) => DisconnectReason::Internal("the session task was cancelled".to_owned()),
        };
        info!(%reason, "Session ended");
        let _ = event_tx.send(SessionEvent::Disconnected(reason)).await;
    });

    (
        SessionHandle {
            commands: command_tx,
            framebuffer,
        },
        event_rx,
    )
}

type Events = async_channel::Sender<SessionEvent>;
type Commands = async_channel::Receiver<Command>;
type Framed = TokioFramed<TlsStream<TcpStream>>;

async fn run(config: SessionConfig, events: Events, commands: Commands, framebuffer: Framebuffer) -> DisconnectReason {
    info!(host = %config.host, port = config.port, security = ?config.security, "Connecting");
    let started = Instant::now();
    let established = tokio::select! {
        result = establish(&config, &events) => result,
        () = wait_for_disconnect(&commands) => return DisconnectReason::UserRequested,
    };
    let (result, framed, channels) = match established {
        Ok(established) => established,
        Err(reason) => return reason,
    };
    info!(
        elapsed_ms = started.elapsed().as_millis(),
        width = result.desktop_size.width,
        height = result.desktop_size.height,
        "Connected"
    );
    match active_session(result, framed, channels, &events, &commands, &framebuffer).await {
        Ok(reason) | Err(reason) => reason,
    }
}

async fn wait_for_disconnect(commands: &Commands) {
    while let Ok(command) = commands.recv().await {
        if matches!(command, Command::Disconnect) {
            return;
        }
    }
}

// ── Connecting ───────────────────────────────────────────────────────────────

type ClipboardSignals = Option<mpsc::UnboundedReceiver<clipboard::Signal>>;

/// Notifications from virtual channels, which IronRDP reports through callbacks.
struct Channels {
    clipboard: ClipboardSignals,
    /// Fires once the server accepts dynamic resolution changes.
    display_ready: mpsc::UnboundedReceiver<()>,
}

async fn establish(
    config: &SessionConfig,
    events: &Events,
) -> Result<(ConnectionResult, Framed, Channels), DisconnectReason> {
    let limit = config.connect_timeout;
    let nla = config.security == Security::Nla;

    let tcp = within(limit, "connecting", tcp_connect(&config.host, config.port)).await??;
    let client_addr = tcp
        .local_addr()
        .map_err(|e| DisconnectReason::Unreachable(describe_io(&e)))?;
    let mut framed = TokioFramed::new(tcp);
    let mut connector = ClientConnector::new(connector_config(config), client_addr);
    let clipboard_signals = config.clipboard.then(|| {
        let (backend, signals) = clipboard::Backend::new();
        connector.attach_static_channel(CliprdrClient::new(Box::new(backend)));
        signals
    });
    if let Some(audio) = &config.audio {
        connector.attach_static_channel(Rdpsnd::new(audio()));
        // Windows only starts audio once device redirection is set up, even with no devices.
        connector.attach_static_channel(Rdpdr::new(Box::new(NoopRdpdrBackend), config.client_name.clone()));
    }
    let (display_ready_tx, display_ready) = mpsc::unbounded_channel();
    connector.attach_static_channel(DrdynvcClient::new().with_dynamic_channel(DisplayControlClient::new(
        move |capabilities| {
            debug!(?capabilities, "Display control ready");
            let _ = display_ready_tx.send(());
            Ok(Vec::new())
        },
    )));
    let channels = Channels {
        clipboard: clipboard_signals,
        display_ready,
    };

    let should_upgrade = within(
        limit,
        "negotiating security",
        ironrdp_tokio::connect_begin(&mut framed, &mut connector),
    )
    .await?
    .map_err(|e| classify_connector_error(e, nla))?;

    let (tcp, leftover) = framed.into_inner();
    let upgraded = within(limit, "setting up encryption", tls::upgrade(tcp, &config.host))
        .await?
        .map_err(|e| DisconnectReason::Negotiation(format!("TLS handshake failed: {e}")))?;

    // Credentials are only sent after this point (CredSSP), so the user decides before they leave the machine.
    if let Some(problem) = upgraded.problem {
        let (reply, answer) = oneshot::channel();
        let prompt = CertificatePrompt {
            certificate: upgraded.certificate.clone(),
            problem,
            reply,
        };
        events
            .send(SessionEvent::CertificatePrompt(prompt))
            .await
            .map_err(|_| DisconnectReason::UserRequested)?;
        if !answer.await.unwrap_or(false) {
            return Err(DisconnectReason::CertificateRejected);
        }
        info!(sha256 = %upgraded.certificate.sha256_fingerprint, "Certificate accepted by user");
    }

    let marker = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = TokioFramed::new_with_leftover(upgraded.stream, leftover);
    let result = within(
        limit,
        "signing in",
        ironrdp_tokio::connect_finalize(
            marker,
            connector,
            &mut framed,
            &mut NoNetworkClient,
            config.host.clone().into(),
            upgraded.server_public_key,
            None,
        ),
    )
    .await?
    .map_err(|e| classify_connector_error(e, nla))?;

    Ok((result, framed, channels))
}

async fn within<T>(limit: Duration, phase: &'static str, f: impl Future<Output = T>) -> Result<T, DisconnectReason> {
    tokio::time::timeout(limit, f)
        .await
        .map_err(|_| DisconnectReason::Timeout(phase))
}

async fn tcp_connect(host: &str, port: u16) -> Result<TcpStream, DisconnectReason> {
    let addrs: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| {
            debug!(error = %e, "Name lookup failed");
            DisconnectReason::HostNotFound(host.to_owned())
        })?
        .collect();
    let mut last_error = None;
    for addr in addrs {
        debug!(%addr, "TCP connect");
        match TcpStream::connect(addr).await {
            Ok(stream) => {
                stream
                    .set_nodelay(true)
                    .map_err(|e| DisconnectReason::Unreachable(describe_io(&e)))?;
                return Ok(stream);
            }
            Err(e) => last_error = Some(e),
        }
    }
    Err(match last_error {
        Some(e) if e.kind() == io::ErrorKind::ConnectionRefused => DisconnectReason::Unreachable(format!(
            "nothing is accepting connections on port {port}. Remote Desktop might be turned off on the PC."
        )),
        Some(e) => DisconnectReason::Unreachable(describe_io(&e)),
        None => DisconnectReason::HostNotFound(host.to_owned()),
    })
}

fn connector_config(config: &SessionConfig) -> connector::Config {
    let nla = config.security == Security::Nla;
    connector::Config {
        credentials: connector::Credentials::UsernamePassword {
            username: config.credentials.username.clone(),
            password: config.credentials.password.clone(),
        },
        domain: config.credentials.domain.clone(),
        // With NLA, CredSSP runs over TLS (PROTOCOL_HYBRID); without it, request PROTOCOL_SSL.
        enable_tls: !nla,
        enable_credssp: nla,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: config.keyboard_layout,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: connector::DesktopSize {
            width: config.desktop_width,
            height: config.desktop_height,
        },
        bitmap: None,
        client_build: 0,
        client_name: config.client_name.clone(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        // Pointer shapes come as separate bitmaps so the UI can use a native cursor.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        request_data: None,
        // Sign in with the supplied credentials instead of showing the server's sign-in screen.
        autologon: true,
        enable_audio_playback: config.audio.is_some(),
        compression_type: None,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::default() | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION,
        desktop_scale_factor: config.desktop_scale_factor,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

/// Kerberos (which needs to reach a KDC) isn't supported yet; NTLM never calls this.
struct NoNetworkClient;

impl ironrdp_tokio::NetworkClient for NoNetworkClient {
    async fn send(
        &mut self,
        request: &ironrdp::connector::sspi::generator::NetworkRequest,
    ) -> connector::ConnectorResult<Vec<u8>> {
        warn!(url = %request.url, "Kerberos network request refused (not supported yet)");
        Err(connector::general_err!("Kerberos is not supported yet"))
    }
}

/// `nla`: whether NLA was requested (the only protocol offered then, so the server can't downgrade).
fn classify_connector_error(e: ConnectorError, nla: bool) -> DisconnectReason {
    warn!(error = %error_chain(&e), "Connection failed");
    // Without IronRDP's source locations, which mean nothing to users.
    let detail = match e.kind() {
        ConnectorErrorKind::Reason(description) => description.clone(),
        kind => error_chain(kind),
    };
    match e.kind() {
        // The server picked a protocol we didn't offer: it can't do NLA.
        ConnectorErrorKind::Reason(description) if nla && description.contains("but server selected") => {
            DisconnectReason::NlaNotSupported
        }
        ConnectorErrorKind::Negotiation(failure) if failure.code() == FailureCode::HYBRID_REQUIRED_BY_SERVER => {
            DisconnectReason::NlaRequired
        }
        ConnectorErrorKind::Credssp(sspi_error) => DisconnectReason::Authentication(match sspi_error.nstatus {
            Some(status) => auth_failure(status, &detail),
            None => AuthFailure::Other(sspi_error.description.clone()),
        }),
        ConnectorErrorKind::AccessDenied => {
            DisconnectReason::Authentication(AuthFailure::Other("access denied".to_owned()))
        }
        ConnectorErrorKind::Negotiation(failure) => DisconnectReason::Negotiation(failure.to_string()),
        _ => match find_io_error(&e) {
            Some(io) if io.kind() == io::ErrorKind::UnexpectedEof => {
                DisconnectReason::ConnectionLost("the PC closed the connection while connecting".to_owned())
            }
            Some(io) => DisconnectReason::ConnectionLost(describe_io(io)),
            None => DisconnectReason::Protocol(detail),
        },
    }
}

fn auth_failure(status: NStatusCode, detail: &str) -> AuthFailure {
    match status {
        NStatusCode::LOGON_FAILURE | NStatusCode::WRONG_PASSWORD | NStatusCode::NO_SUCH_USER => {
            AuthFailure::WrongCredentials
        }
        NStatusCode::ACCOUNT_LOCKED_OUT => AuthFailure::AccountLockedOut,
        NStatusCode::ACCOUNT_DISABLED => AuthFailure::AccountDisabled,
        NStatusCode::PASSWORD_EXPIRED => AuthFailure::PasswordExpired,
        NStatusCode::PASSWORD_MUST_CHANGE => AuthFailure::PasswordMustChange,
        NStatusCode::ACCOUNT_RESTRICTION | NStatusCode::INVALID_LOGON_HOURS | NStatusCode::INVALID_WORKSTATION => {
            AuthFailure::AccountRestriction
        }
        NStatusCode::LOGON_TYPE_NOT_GRANTED | NStatusCode::LOGON_NOT_GRANTED => AuthFailure::LogonNotGranted,
        _ => AuthFailure::Other(detail.to_owned()),
    }
}

fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        text.push_str(": ");
        text.push_str(&s.to_string());
        source = s.source();
    }
    text
}

fn find_io_error<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a io::Error> {
    let mut current = Some(e);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            return Some(io);
        }
        current = err.source();
    }
    None
}

fn describe_io(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::ConnectionReset => "the PC reset the connection".to_owned(),
        io::ErrorKind::ConnectionAborted => "the connection was aborted".to_owned(),
        io::ErrorKind::UnexpectedEof => "the PC closed the connection".to_owned(),
        io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => {
            "the PC isn't reachable from this network".to_owned()
        }
        io::ErrorKind::TimedOut => "the network connection timed out".to_owned(),
        _ => e.to_string(),
    }
}

// ── Active session ───────────────────────────────────────────────────────────

fn protocol_error(e: SessionError) -> DisconnectReason {
    if let Some(io) = find_io_error(&e) {
        return DisconnectReason::ConnectionLost(describe_io(io));
    }
    let detail = error_chain(&e);
    warn!(error = %detail, "Session failed");
    let detail = strip_locations(&detail);
    // MCS Disconnect Provider Ultimatum: the server dropped the connection deliberately.
    if detail.contains("disconnect provider ultimatum") {
        return DisconnectReason::ServerEnded("the PC closed the connection".to_owned());
    }
    DisconnectReason::Protocol(detail)
}

/// Removes IronRDP's `[context @ file:line]` markers, which mean nothing to users.
fn strip_locations(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        match rest[start..].find(']') {
            Some(len) if rest[start..start + len].contains(" @ ") => {
                out.push_str(&rest[..start]);
                rest = rest[start + len + 1..].trim_start();
            }
            _ => {
                out.push_str(&rest[..=start]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn lost(e: io::Error) -> DisconnectReason {
    DisconnectReason::ConnectionLost(describe_io(&e))
}

/// Runs until the session ends. Both arms carry the reason; `Err` is used for `?` on failures.
async fn active_session(
    result: ConnectionResult,
    framed: Framed,
    channels: Channels,
    events: &Events,
    commands: &Commands,
    framebuffer: &Framebuffer,
) -> Result<DisconnectReason, DisconnectReason> {
    let (mut reader, mut writer) = ironrdp_tokio::split_tokio_framed(framed);
    let size = result.desktop_size;
    let mut image = DecodedImage::new(PixelFormat::BgrX32, size.width, size.height);
    framebuffer.resize(u32::from(size.width), u32::from(size.height));
    let _ = events
        .send(SessionEvent::Connected {
            width: size.width,
            height: size.height,
        })
        .await;

    let activation_factory = result.activation_factory;
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
    let mut input = Database::new();
    let mut shutdown_deadline: Option<Instant> = None;
    let mut dirty: Vec<Rect> = Vec::new();
    let mut clipboard = Clipboard::default();
    let Channels {
        clipboard: mut clipboard_signals,
        mut display_ready,
    } = channels;
    let mut display = DisplayControl::default();

    loop {
        let outputs = tokio::select! {
            frame = reader.read_pdu() => match frame {
                Ok((action, payload)) => {
                    trace!(?action, len = payload.len(), "PDU");
                    stage.process(&mut image, action, &payload).map_err(protocol_error)?
                }
                // The server closes the connection in response to our shutdown request.
                Err(_) if shutdown_deadline.is_some() => return Ok(DisconnectReason::UserRequested),
                Err(e) => return Err(lost(e)),
            },
            command = commands.recv(), if shutdown_deadline.is_none() => match command {
                Ok(Command::Input(operations)) => {
                    let fastpath = input.apply(operations);
                    stage.process_fastpath_input(&mut image, &fastpath).map_err(protocol_error)?
                }
                Ok(Command::ReleaseAll) => {
                    let fastpath = input.release_all();
                    stage.process_fastpath_input(&mut image, &fastpath).map_err(protocol_error)?
                }
                Ok(Command::SyncLocks { caps, num, scroll }) => {
                    let event = ironrdp::input::synchronize_event(scroll, num, caps, false);
                    stage.process_fastpath_input(&mut image, &[event]).map_err(protocol_error)?
                }
                Ok(Command::Pause) => stage.process_fastpath_input(&mut image, &pause_sequence()).map_err(protocol_error)?,
                Ok(Command::Resize { width, height, scale }) => {
                    display.request(width, height, scale, &image);
                    display.send(&mut stage)?
                }
                Ok(Command::ClipboardText(text)) => {
                    let Some(channel) = stage.get_svc_processor_mut::<CliprdrClient>() else { continue };
                    match clipboard.set_local(text, channel) {
                        Ok(Some(messages)) => clipboard_frames(&stage, messages)?,
                        Ok(None) => Vec::new(),
                        Err(e) => {
                            warn!(error = %e, "Clipboard announcement failed");
                            Vec::new()
                        }
                    }
                }
                // An explicit request, or the UI dropped the handle.
                Ok(Command::Disconnect) | Err(_) => {
                    debug!("Requesting shutdown");
                    shutdown_deadline = Some(Instant::now() + SHUTDOWN_GRACE);
                    stage.graceful_shutdown().map_err(protocol_error)?
                }
            },
            Some(()) = display_ready.recv(), if !display.ready => {
                display.ready = true;
                display.send(&mut stage)?
            }
            Some(signal) = recv(&mut clipboard_signals) => {
                let Some(channel) = stage.get_svc_processor_mut::<CliprdrClient>() else { continue };
                match clipboard.handle(signal, channel) {
                    Ok(outcome) => {
                        if let Some(text) = outcome.remote_text {
                            let _ = events.send(SessionEvent::ClipboardText(text)).await;
                        }
                        match outcome.messages {
                            Some(messages) => clipboard_frames(&stage, messages)?,
                            None => Vec::new(),
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "Clipboard error");
                        Vec::new()
                    }
                }
            }
            () = sleep_until(shutdown_deadline) => {
                debug!("Server did not close the connection in time");
                return Ok(DisconnectReason::UserRequested);
            }
        };

        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => writer.write_all(&frame).await.map_err(lost)?,
                ActiveStageOutput::GraphicsUpdate(region) => dirty.push(to_rect(&region)),
                ActiveStageOutput::PointerDefault => pointer(events, PointerEvent::Default).await,
                ActiveStageOutput::PointerHidden => pointer(events, PointerEvent::Hidden).await,
                ActiveStageOutput::PointerPosition { x, y } => pointer(events, PointerEvent::Position { x, y }).await,
                ActiveStageOutput::PointerBitmap(bitmap) => pointer(events, PointerEvent::Bitmap(bitmap)).await,
                ActiveStageOutput::Terminate(reason) => {
                    debug!(?reason, "Server terminated the session");
                    return Ok(match reason {
                        _ if shutdown_deadline.is_some() => DisconnectReason::UserRequested,
                        GracefulDisconnectReason::UserInitiated => DisconnectReason::UserRequested,
                        GracefulDisconnectReason::ServerInitiated => {
                            DisconnectReason::ServerEnded("the session was ended on the PC".to_owned())
                        }
                        GracefulDisconnectReason::Other(description) => DisconnectReason::ServerEnded(description),
                    });
                }
                ActiveStageOutput::DeactivateAll => {
                    publish(framebuffer, events, &image, &mut dirty);
                    let (width, height) = reactivate(&mut stage, &activation_factory, &mut reader, &mut writer).await?;
                    image = DecodedImage::new(PixelFormat::BgrX32, width, height);
                    framebuffer.resize(u32::from(width), u32::from(height));
                    let _ = events.send(SessionEvent::DesktopResized { width, height }).await;
                    if framebuffer.mark_full_damage() {
                        let _ = events.try_send(SessionEvent::FrameReady);
                    }
                }
                ActiveStageOutput::MultitransportRequest(pdu) => {
                    debug!(request_id = pdu.request_id, "Multitransport (UDP) request ignored");
                }
                ActiveStageOutput::AutoDetect(request) => trace!(?request, "Auto-detect request"),
            }
        }
        publish(framebuffer, events, &image, &mut dirty);
    }
}

fn publish(framebuffer: &Framebuffer, events: &Events, image: &DecodedImage, dirty: &mut Vec<Rect>) {
    if !dirty.is_empty() && framebuffer.publish(image.data(), dirty) {
        let _ = events.try_send(SessionEvent::FrameReady);
    }
    dirty.clear();
}

async fn pointer(events: &Events, event: PointerEvent) {
    let _ = events.send(SessionEvent::Pointer(event)).await;
}

/// Dynamic resolution (MS-RDPEDISP): the latest requested size, sent once the channel is ready.
#[derive(Default)]
struct DisplayControl {
    ready: bool,
    pending: Option<(u32, u32, u32)>,
    /// The last size we asked for, so repeated identical requests are dropped.
    requested: Option<(u32, u32, u32)>,
}

impl DisplayControl {
    fn request(&mut self, width: u32, height: u32, scale: u32, image: &DecodedImage) {
        let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
        let current = (u32::from(image.width()), u32::from(image.height()));
        let request = (width, height, scale);
        if self.requested == Some(request) || (self.requested.is_none() && current == (width, height)) {
            return;
        }
        self.pending = Some(request);
    }

    fn send(&mut self, stage: &mut ActiveStage) -> Result<Vec<ActiveStageOutput>, DisconnectReason> {
        if !self.ready {
            return Ok(Vec::new());
        }
        let Some((width, height, scale)) = self.pending.take() else {
            return Ok(Vec::new());
        };
        debug!(width, height, scale, "Requesting a desktop resize");
        let scale_factor = (scale >= 100).then_some(scale);
        match stage.encode_resize(width, height, scale_factor, None) {
            Some(frame) => {
                self.requested = Some((width, height, scale));
                Ok(vec![ActiveStageOutput::ResponseFrame(frame.map_err(protocol_error)?)])
            }
            None => Ok(Vec::new()),
        }
    }
}

fn clipboard_frames(
    stage: &ActiveStage,
    messages: CliprdrSvcMessages<ironrdp::cliprdr::Client>,
) -> Result<Vec<ActiveStageOutput>, DisconnectReason> {
    let frame = stage.process_svc_processor_messages(messages).map_err(protocol_error)?;
    Ok(vec![ActiveStageOutput::ResponseFrame(frame)])
}

/// Waits for a clipboard signal; never resolves when the clipboard is off.
async fn recv(signals: &mut ClipboardSignals) -> Option<clipboard::Signal> {
    match signals {
        Some(signals) => signals.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn to_rect(r: &InclusiveRectangle) -> Rect {
    Rect {
        x: u32::from(r.left),
        y: u32::from(r.top),
        width: u32::from(r.right.saturating_sub(r.left)) + 1,
        height: u32::from(r.bottom.saturating_sub(r.top)) + 1,
    }
}

/// Pause has no break code: it is sent as E1 1D 45 (Ctrl with the EXTENDED1 flag, then NumLock).
fn pause_sequence() -> [FastPathInputEvent; 2] {
    [
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1, 0x1D),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x45),
    ]
}

/// Deactivation-Reactivation Sequence (MS-RDPBCGR 1.3.1.3), e.g. after a server-side resolution change.
async fn reactivate<R, W>(
    stage: &mut ActiveStage,
    factory: &connector::connection_activation::ConnectionActivationFactory,
    reader: &mut ironrdp_tokio::Framed<R>,
    writer: &mut ironrdp_tokio::Framed<W>,
) -> Result<(u16, u16), DisconnectReason>
where
    R: ironrdp_tokio::FramedRead,
    W: ironrdp_tokio::FramedWrite,
{
    debug!("Deactivation-Reactivation Sequence");
    let mut activation = factory.create();
    let mut buf = WriteBuf::new();
    loop {
        let written = ironrdp_tokio::single_sequence_step_read(reader, &mut activation, &mut buf)
            .await
            .map_err(|e| classify_connector_error(e, false))?;
        if written.size().is_some() {
            writer.write_all(buf.filled()).await.map_err(lost)?;
        }
        if let ConnectionActivationState::Finalized {
            desktop_size,
            share_id,
            enable_server_pointer,
            pointer_software_rendering,
        } = activation.connection_activation_state()
        {
            debug!(?desktop_size, "Reactivated");
            stage.set_fastpath_processor(
                ironrdp::session::fast_path::ProcessorBuilder {
                    io_channel_id: activation.io_channel_id(),
                    user_channel_id: activation.user_channel_id(),
                    share_id,
                    enable_server_pointer,
                    pointer_software_rendering,
                    bulk_decompressor: None,
                }
                .build(),
            );
            stage.set_share_id(share_id);
            stage.set_enable_server_pointer(enable_server_pointer);
            return Ok((desktop_size.width, desktop_size.height));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusive_rect_conversion() {
        let r = to_rect(&InclusiveRectangle {
            left: 10,
            top: 20,
            right: 10,
            bottom: 29,
        });
        assert_eq!((r.x, r.y, r.width, r.height), (10, 20, 1, 10));
    }

    #[test]
    fn maps_logon_statuses() {
        assert_eq!(
            auth_failure(NStatusCode::LOGON_FAILURE, ""),
            AuthFailure::WrongCredentials
        );
        assert_eq!(
            auth_failure(NStatusCode::ACCOUNT_LOCKED_OUT, ""),
            AuthFailure::AccountLockedOut
        );
        assert!(matches!(
            auth_failure(NStatusCode(0xC000_1234), "x"),
            AuthFailure::Other(_)
        ));
    }

    #[test]
    fn explains_nla_mismatches() {
        let downgrade = || {
            ConnectorError::new(
                "Initiation",
                ConnectorErrorKind::Reason(
                    "client advertised HYBRID | HYBRID_EX, but server selected STANDARD_RDP_SECURITY".to_owned(),
                ),
            )
        };
        assert_eq!(
            classify_connector_error(downgrade(), true),
            DisconnectReason::NlaNotSupported
        );
        assert!(matches!(
            classify_connector_error(downgrade(), false),
            DisconnectReason::Protocol(detail) if !detail.contains(".rs:")
        ));
        let required = ConnectorError::new(
            "Initiation",
            ConnectorErrorKind::Negotiation(FailureCode::HYBRID_REQUIRED_BY_SERVER.into()),
        );
        assert_eq!(classify_connector_error(required, false), DisconnectReason::NlaRequired);
    }

    #[test]
    fn strips_source_locations() {
        assert_eq!(
            strip_locations("[decode @ /a/b.rs:1] decode error: [x @ c.rs:2] other (received disconnect)"),
            "decode error: other (received disconnect)"
        );
        assert_eq!(strip_locations("keep [this] text"), "keep [this] text");
    }

    #[test]
    fn only_transient_failures_may_retry() {
        assert!(!DisconnectReason::Authentication(AuthFailure::WrongCredentials).may_retry());
        assert!(!DisconnectReason::CertificateRejected.may_retry());
        assert!(DisconnectReason::ConnectionLost(String::new()).may_retry());
    }
}
