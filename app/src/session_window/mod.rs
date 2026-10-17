//! One window per session: remote display, input, full screen, disconnect handling.

mod clipboard;
mod input;
mod keys;
mod monitors;
mod remote_files;
mod resize;
mod toolbar;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{cairo, gdk, gio, glib};
use libadwaita as adw;
use porthole_core::{
    CertificateInfo, CertificateProblem, Credentials, DisconnectReason, PointerEvent, Security, SessionConfig,
    SessionEvent, SessionHandle,
};
use porthole_input::ScrollAccumulator;
use porthole_store::{CertificateTrust, Uuid, keyring};
use tracing::{debug, warn};

use crate::dialogs::{self, CertificateDecision};

pub struct SessionWindow;

/// Where to connect.
pub struct Target {
    pub host: String,
    pub port: u16,
    /// Window title; the PC's display name.
    pub name: String,
    /// The saved PC, if connecting to one.
    pub pc: Option<Uuid>,
    pub nla: bool,
    pub clipboard: bool,
    pub audio: bool,
    pub microphone: bool,
    pub camera: bool,
    pub all_monitors: bool,
    /// Lossless picture instead of H.264.
    pub best_quality: bool,
    pub shared_folders: Vec<std::path::PathBuf>,
    /// Save the password in the keyring once connected (a saved PC's "Remember password").
    pub save_password: bool,
    /// The password came from the keyring.
    pub password_saved: bool,
}

struct Inner {
    window: adw::ApplicationWindow,
    toolbar: adw::ToolbarView,
    /// The leave-full-screen button shown in full screen (see `toolbar`).
    corner: gtk::Revealer,
    /// Hides `corner` once the pointer has left it.
    hide_timer: RefCell<Option<glib::SourceId>>,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    picture: gtk::Picture,
    ended: adw::StatusPage,
    details: gtk::Expander,
    details_label: gtk::Label,
    reconnect: gtk::Button,

    host: String,
    port: u16,
    pc: Option<Uuid>,
    nla: bool,
    clipboard: bool,
    audio: bool,
    microphone: bool,
    camera: bool,
    all_monitors: bool,
    best_quality: bool,
    /// Full screen over every monitor is active (see `monitors`).
    spanning: Cell<bool>,
    /// The part of the desktop the session window shows while spanning (`None`: all of it).
    main_area: Cell<Option<monitors::Area>>,
    extra_views: RefCell<Vec<Rc<monitors::ExtraView>>>,
    /// The monitors asked for while spanning, and the scale.
    span_layout: RefCell<Option<(Vec<porthole_core::MonitorRect>, u32)>>,
    shared_folders: Vec<std::path::PathBuf>,
    save_password: Cell<bool>,
    password_saved: Cell<bool>,
    desktop: Desktop,
    credentials: RefCell<Credentials>,

    session: RefCell<Option<SessionHandle>>,
    /// The texture on screen; the base for incremental updates.
    texture: RefCell<Option<gdk::Texture>>,
    upload_scheduled: Cell<bool>,
    connected: Cell<bool>,
    last_reason: RefCell<Option<DisconnectReason>>,
    /// The window was closed while connected; it is destroyed once the session has shut down.
    closing: RefCell<Option<gio::ApplicationHoldGuard>>,
    scroll_vertical: RefCell<ScrollAccumulator>,
    scroll_horizontal: RefCell<ScrollAccumulator>,
    /// The local clipboard holds what the remote copied (so it isn't offered back).
    clipboard_from_remote: Cell<bool>,
    /// We're putting the remote's copy on the clipboard; the change that follows is ours.
    clipboard_setting: Cell<bool>,
    /// A pending dynamic resolution request, waiting for the window size to settle.
    resize_timeout: RefCell<Option<glib::SourceId>>,
    /// A session in this window got connected; transient failures after that are retried.
    was_connected: Cell<bool>,
    /// Automatic reconnection: attempts made so far and the timer for the next one.
    reconnect_attempts: Cell<u32>,
    reconnect_timer: RefCell<Option<glib::SourceId>>,
    /// A certificate the user accepted for this window, accepted again when reconnecting.
    accepted_certificate: RefCell<Option<String>>,
}

type State = Rc<Inner>;

impl SessionWindow {
    pub fn open(
        app: &adw::Application,
        parent: Option<&impl IsA<gtk::Window>>,
        target: Target,
        credentials: Credentials,
    ) {
        let desktop = desktop_size(parent.map(|p| p.upcast_ref()));
        let state = build(app, target, desktop, credentials);
        state.window.maximize();
        state.window.present();
        start(&state);
    }
}

fn build(app: &adw::Application, target: Target, desktop: Desktop, credentials: Credentials) -> State {
    let Target {
        host,
        port,
        name,
        pc,
        nla,
        clipboard,
        audio,
        microphone,
        camera,
        all_monitors,
        best_quality,
        shared_folders,
        save_password,
        password_saved,
    } = target;
    let title = adw::WindowTitle::new(&name, "Connecting…");
    let header = adw::HeaderBar::builder().title_widget(&title).build();
    let disconnect = gtk::Button::builder()
        .icon_name("network-offline-symbolic")
        .tooltip_text("Disconnect")
        .build();
    let fullscreen = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text("Full Screen (Ctrl+Alt+Enter)")
        .build();
    header.pack_start(&disconnect);
    header.pack_end(&fullscreen);
    let keys_menu = keys::menu_button();
    header.pack_end(&keys_menu);

    let connecting = adw::StatusPage::builder()
        .title(format!("Connecting to {name}"))
        .child(
            &gtk::Spinner::builder()
                .spinning(true)
                .width_request(32)
                .height_request(32)
                .build(),
        )
        .build();

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    picture.add_css_class("porthole-display");

    let reconnect = gtk::Button::builder()
        .label("Reconnect")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    // Technical detail behind a disconnection, folded away: useful for reporting a problem.
    let details_label = gtk::Label::builder()
        .selectable(true)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(["caption", "monospace", "dim-label"])
        .build();
    let details = gtk::Expander::builder()
        .label("Details")
        .child(&details_label)
        .visible(false)
        .build();
    let ended_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .halign(gtk::Align::Center)
        .width_request(360)
        .build();
    ended_box.append(&reconnect);
    ended_box.append(&details);
    let ended = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("Disconnected")
        .child(&ended_box)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&connecting, Some("connecting"));
    stack.add_named(&picture, Some("display"));
    stack.add_named(&ended, Some("ended"));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let (corner, corner_button) = toolbar::corner_button();
    let overlay = gtk::Overlay::builder().child(&toasts).build();
    overlay.add_overlay(&corner);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&overlay));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&name)
        .default_width(1280)
        .default_height(800)
        .content(&toolbar)
        .build();
    // A black background around the scaled desktop instead of the window colour.
    let css = gtk::CssProvider::new();
    css.load_from_string(".porthole-display { background: black; }");
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let state = Rc::new(Inner {
        window,
        toolbar,
        corner,
        hide_timer: RefCell::default(),
        title,
        toasts,
        stack,
        picture,
        ended,
        details,
        details_label,
        reconnect,
        host,
        port,
        pc,
        nla,
        clipboard,
        audio,
        microphone,
        camera,
        all_monitors,
        best_quality,
        spanning: Cell::new(false),
        main_area: Cell::new(None),
        extra_views: RefCell::default(),
        span_layout: RefCell::default(),
        shared_folders,
        save_password: Cell::new(save_password),
        password_saved: Cell::new(password_saved),
        desktop,
        credentials: RefCell::new(credentials),
        session: RefCell::default(),
        texture: RefCell::default(),
        upload_scheduled: Cell::new(false),
        connected: Cell::new(false),
        last_reason: RefCell::default(),
        closing: RefCell::default(),
        scroll_vertical: RefCell::default(),
        scroll_horizontal: RefCell::default(),
        clipboard_from_remote: Cell::new(false),
        clipboard_setting: Cell::new(false),
        resize_timeout: RefCell::default(),
        was_connected: Cell::new(false),
        reconnect_attempts: Cell::new(0),
        reconnect_timer: RefCell::default(),
        accepted_certificate: RefCell::default(),
    });

    disconnect.connect_clicked(glib::clone!(
        #[weak(rename_to = window)]
        state.window,
        move |_| window.close()
    ));
    let weak = Rc::downgrade(&state);
    fullscreen.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            toolbar::toggle_fullscreen(&state);
        }
    });
    let weak = Rc::downgrade(&state);
    state.window.connect_fullscreened_notify(move |_| {
        if let Some(state) = weak.upgrade() {
            toolbar::fullscreen_changed(&state);
            monitors::fullscreen_changed(&state);
        }
    });
    let weak = Rc::downgrade(&state);
    corner_button.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            toolbar::toggle_fullscreen(&state);
        }
    });
    toolbar::attach(&state);

    let weak = Rc::downgrade(&state);
    state.reconnect.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            glib::spawn_future_local(reconnect_session(state));
        }
    });

    let weak = Rc::downgrade(&state);
    state.window.connect_close_request(move |window| {
        let Some(state) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        cancel_auto_reconnect(&state);
        // Disconnect politely; the window goes away now and is destroyed when the session ends.
        match state.session.borrow().as_ref() {
            Some(session) => session.disconnect(),
            None => return glib::Propagation::Proceed,
        }
        if let Some(app) = window.application() {
            state.closing.replace(Some(app.hold()));
        }
        window.set_visible(false);
        glib::Propagation::Stop
    });

    // Handlers only hold weak references; the window keeps the state alive until it's destroyed.
    let keep_alive = RefCell::new(Some(state.clone()));
    state.window.connect_destroy(move |_| drop(keep_alive.take()));

    input::attach(&state);
    clipboard::attach(&state);
    resize::attach(&state);
    keys::attach(&state);
    state
}

fn start(state: &State) {
    let mut config = SessionConfig::new(state.host.clone(), state.credentials.borrow().clone());
    config.port = state.port;
    config.security = if state.nla { Security::Nla } else { Security::Tls };
    config.desktop_width = state.desktop.width;
    config.desktop_height = state.desktop.height;
    config.desktop_scale_factor = state.desktop.scale;
    config.keyboard_layout = input::keyboard_layout();
    config.clipboard = state.clipboard;
    config.graphics_pipeline = !state.best_quality;
    if state.microphone {
        let microphone: porthole_core::Microphone = Arc::new(|format, sink| {
            porthole_audio::Capture::start(format.sample_rate, format.channels, sink)
                .map(|capture| Box::new(capture) as Box<dyn Send>)
                .map_err(|e| e.to_string())
        });
        config.microphone = Some(microphone);
    }
    if state.camera {
        config.camera = Some(Arc::new(porthole_camera::PipeWireCameras));
    }
    config.shared_folders = state
        .shared_folders
        .iter()
        .map(|path| porthole_core::SharedFolder {
            name: path
                .file_name()
                .map_or_else(|| "Folder".to_owned(), |n| n.to_string_lossy().into_owned()),
            path: path.clone(),
        })
        .collect();
    if state.audio {
        let output: porthole_core::AudioOutput = Arc::new(|| Box::new(porthole_audio::PipeWireOutput::new()));
        config.audio = Some(output);
    }

    let (session, events) = porthole_core::connect(config, crate::runtime().handle());
    state.session.replace(Some(session));
    state.texture.replace(None);
    state.picture.set_paintable(None::<&gdk::Paintable>);
    state.title.set_subtitle("Connecting…");
    state.stack.set_visible_child_name("connecting");
    glib::spawn_future_local(run_events(state.clone(), events));
}

async fn reconnect_session(state: State) {
    cancel_auto_reconnect(&state);
    let reason = state.last_reason.borrow().clone();
    if let Some(DisconnectReason::Authentication(failure)) = reason {
        let username = account_name(&state.credentials.borrow());
        let remember = state
            .pc
            .map(|_| state.password_saved.get() || state.save_password.get());
        let Some(sign_in) = dialogs::ask_credentials(
            &state.window,
            &state.title.title(),
            Some(&username),
            Some(&failure.to_string()),
            remember,
        )
        .await
        else {
            return;
        };
        state.credentials.replace(sign_in.credentials);
        state.save_password.set(sign_in.remember);
        if !sign_in.remember
            && state.password_saved.replace(false)
            && let Some(id) = state.pc
            && let Some(Err(e)) = crate::on_runtime(keyring::forget_password(id)).await
        {
            warn!(error = %e, "Couldn't remove the saved password");
        }
    }
    start(&state);
}

async fn run_events(state: State, events: async_channel::Receiver<SessionEvent>) {
    while let Ok(event) = events.recv().await {
        match event {
            SessionEvent::CertificatePrompt(prompt) => {
                if check_certificate(&state, &prompt.certificate, &prompt.problem).await {
                    prompt.accept()
                } else {
                    prompt.reject()
                }
            }
            SessionEvent::Connected { width, height } => {
                state.connected.set(true);
                state.was_connected.set(true);
                save_password(&state);
                state.reconnect_attempts.set(0);
                keys::connection_changed(&state);
                if let Some(id) = state.pc
                    && let Err(e) = crate::config().store.borrow_mut().mark_connected(id)
                {
                    warn!(error = %e, "Couldn't record the connection");
                }
                state.title.set_subtitle(&format!("{width} × {height}"));
                state.stack.set_visible_child_name("display");
                state.picture.grab_focus();
                input::synchronize_locks(&state);
                clipboard::share_local(&state);
                resize::schedule(&state);
                monitors::send_layout(&state);
            }
            SessionEvent::DesktopResized { width, height } => {
                state.title.set_subtitle(&format!("{width} × {height}"));
            }
            SessionEvent::FrameReady => schedule_upload(&state),
            SessionEvent::Pointer(pointer) => set_pointer(&state, pointer),
            SessionEvent::Clipboard(remote) => clipboard::set_remote(&state, remote),
            SessionEvent::Disconnected(reason) => {
                session_ended(&state, reason);
                return;
            }
        }
    }
}

/// The certificate failed verification: connect if the user trusted it before, otherwise ask.
async fn check_certificate(state: &State, certificate: &CertificateInfo, problem: &CertificateProblem) -> bool {
    let config = crate::config();
    let fingerprint = &certificate.sha256_fingerprint;
    let trust = config
        .store
        .borrow()
        .certificate_trust(&state.host, state.port, fingerprint);
    if trust == CertificateTrust::Trusted
        || state.accepted_certificate.borrow().as_deref() == Some(fingerprint.as_str())
    {
        debug!(host = %state.host, "Certificate trusted earlier");
        return true;
    }
    let changed = trust == CertificateTrust::Changed;
    match dialogs::ask_certificate_trust(&state.window, certificate, problem, changed).await {
        CertificateDecision::Reject => false,
        CertificateDecision::AcceptOnce => {
            state.accepted_certificate.replace(Some(fingerprint.clone()));
            true
        }
        CertificateDecision::AcceptAlways => {
            let saved = config
                .store
                .borrow_mut()
                .trust_certificate(&state.host, state.port, fingerprint);
            if let Err(e) = saved {
                warn!(error = %e, "Couldn't save the trusted certificate");
                state
                    .toasts
                    .add_toast(adw::Toast::new("Couldn't remember the certificate"));
            }
            true
        }
    }
}

/// Waits before each automatic reconnection attempt.
const RECONNECT_DELAYS: [u32; 5] = [2, 4, 8, 15, 30];

/// Retries a lost session after a delay, showing a countdown. Returns whether a retry is scheduled.
/// Only for sessions that were connected before, and never for sign-in failures (`may_retry`).
fn schedule_auto_reconnect(state: &State, reason: &DisconnectReason) -> bool {
    let attempt = state.reconnect_attempts.get();
    let Some(&delay) = RECONNECT_DELAYS.get(attempt as usize) else {
        return false;
    };
    if !state.was_connected.get() || !reason.may_retry() {
        return false;
    }
    state.reconnect_attempts.set(attempt + 1);
    state.ended.set_title("Reconnecting…");
    state.details.set_visible(false);
    state.reconnect.set_label("Reconnect Now");
    state.title.set_subtitle("Reconnecting…");
    state.stack.set_visible_child_name("ended");

    let reason = glib::markup_escape_text(&reason.to_string());
    let total = RECONNECT_DELAYS.len();
    let remaining = Cell::new(delay);
    let update = {
        let ended = state.ended.clone();
        move |seconds: u32| {
            ended.set_description(Some(&format!(
                "{reason}\n\nTrying again in {seconds} s (attempt {} of {total}).",
                attempt + 1
            )));
        }
    };
    update(delay);
    let weak = Rc::downgrade(state);
    let timer = glib::timeout_add_seconds_local(1, move || {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let left = remaining.get().saturating_sub(1);
        remaining.set(left);
        if left > 0 {
            update(left);
            return glib::ControlFlow::Continue;
        }
        state.reconnect_timer.take();
        debug!(attempt = attempt + 1, "Reconnecting automatically");
        start(&state);
        glib::ControlFlow::Break
    });
    cancel_auto_reconnect(state);
    state.reconnect_timer.replace(Some(timer));
    true
}

fn cancel_auto_reconnect(state: &State) {
    if let Some(timer) = state.reconnect_timer.take() {
        timer.remove();
    }
}

/// `DOMAIN\user` (or the plain user name / UPN), as typed.
fn account_name(credentials: &Credentials) -> String {
    match &credentials.domain {
        Some(domain) => format!("{domain}\\{}", credentials.username),
        None => credentials.username.clone(),
    }
}

/// Stores the password after a successful sign-in, if the user asked to remember it.
fn save_password(state: &State) {
    let Some(id) = state.pc.filter(|_| state.save_password.replace(false)) else {
        return;
    };
    let credentials = state.credentials.borrow();
    let username = account_name(&credentials);
    let password = zeroize::Zeroizing::new(credentials.password.clone());
    let label = format!("Porthole password for {}", state.title.title());
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let stored = crate::on_runtime(async move { keyring::store_password(id, &username, &label, &password).await });
        match stored.await {
            Some(Ok(())) => {
                if let Some(state) = weak.upgrade() {
                    state.password_saved.set(true);
                }
            }
            Some(Err(e)) => {
                warn!(error = %e, "Couldn't save the password");
                if let Some(state) = weak.upgrade() {
                    state
                        .toasts
                        .add_toast(adw::Toast::new("Couldn't save the password in the keyring"));
                }
            }
            None => {}
        }
    });
}

/// Keeps a small picture of the last frame for the saved PC's tile in the main window.
fn save_thumbnail(state: &State) {
    let (Some(id), Some(texture)) = (state.pc, state.texture.borrow().clone()) else {
        return;
    };
    let Some(png) = thumbnail_png(&state.window, &texture) else {
        return;
    };
    if let Err(e) = crate::config().store.borrow().save_thumbnail(id, &png) {
        warn!(error = %e, "Couldn't save the thumbnail");
        return;
    }
    crate::connection_center::refresh_saved();
}

/// Downscales on the GPU (or whatever renders the window) and encodes as PNG.
fn thumbnail_png(window: &impl IsA<gtk::Native>, texture: &gdk::Texture) -> Option<glib::Bytes> {
    const WIDTH: f32 = 480.0;
    let (width, height) = (texture.width() as f32, texture.height() as f32);
    let scale = (WIDTH / width).min(1.0);
    let bounds = gtk4::graphene::Rect::new(0.0, 0.0, (width * scale).round(), (height * scale).round());
    let snapshot = gtk::Snapshot::new();
    snapshot.append_scaled_texture(texture, gtk4::gsk::ScalingFilter::Trilinear, &bounds);
    let node = snapshot.to_node()?;
    let renderer = window.renderer()?;
    Some(renderer.render_texture(node, Some(&bounds)).save_to_png_bytes())
}

fn session_ended(state: &State, reason: DisconnectReason) {
    debug!(%reason, "Session ended");
    save_thumbnail(state);
    state.connected.set(false);
    keys::connection_changed(state);
    state.session.replace(None);
    state.picture.set_cursor(None);
    monitors::leave(state);
    if state.closing.take().is_some() || reason == DisconnectReason::UserRequested {
        state.window.destroy();
        return;
    }
    if schedule_auto_reconnect(state, &reason) {
        state.last_reason.replace(Some(reason));
        return;
    }
    if state.window.is_fullscreen() {
        state.window.unfullscreen();
    }
    let auth = matches!(reason, DisconnectReason::Authentication(_));
    state
        .ended
        .set_title(if auth { "Couldn't Sign In" } else { "Disconnected" });
    state
        .ended
        .set_description(Some(&glib::markup_escape_text(&reason.to_string())));
    state.details_label.set_label(reason.detail().unwrap_or_default());
    state.details.set_expanded(false);
    state.details.set_visible(reason.detail().is_some());
    state
        .reconnect
        .set_label(if auth { "Sign In Again" } else { "Reconnect" });
    state.title.set_subtitle("Disconnected");
    state.stack.set_visible_child_name("ended");
    state.last_reason.replace(Some(reason));
}

/// Uploads at most once per frame clock tick, however often the server sends updates.
fn schedule_upload(state: &State) {
    if state.upload_scheduled.replace(true) {
        return;
    }
    let state = state.clone();
    state.picture.clone().add_tick_callback(move |_, _| {
        state.upload_scheduled.set(false);
        upload(&state);
        glib::ControlFlow::Break
    });
}

fn upload(state: &State) {
    let session = state.session.borrow();
    let Some(session) = session.as_ref() else {
        return;
    };
    let main_area = state.main_area.get();
    let extras = state.extra_views.borrow().clone();
    let previous = state.texture.borrow().clone();
    let textures = session.framebuffer().take_damage(|damage| {
        if damage.width == 0 || damage.height == 0 {
            return None;
        }
        // GdkMemoryTexture keeps a reference to its bytes, so every frame needs its own copy;
        // the views share it.
        let bytes = glib::Bytes::from(damage.data);
        let main = view_texture(&damage, &bytes, main_area, previous.as_ref());
        let extra: Vec<_> = extras
            .iter()
            .map(|view| view_texture(&damage, &bytes, Some(view.area), view.texture.borrow().as_ref()))
            .collect();
        Some((main, extra))
    });
    let Some((main, extra)) = textures else { return };
    if let Some(texture) = main {
        state.picture.set_paintable(Some(&texture));
        state.texture.replace(Some(texture));
    }
    for (view, texture) in extras.iter().zip(extra) {
        if let Some(texture) = texture {
            view.picture.set_paintable(Some(&texture));
            view.texture.replace(Some(texture));
        }
    }
    debug_dump_views(state);
}

/// For tests on headless displays: `PORTHOLE_DEBUG_VIEWS_DIR` gets each window's picture as
/// `view-N.png` (N = 0 for the session window).
fn debug_dump_views(state: &State) {
    let Some(dir) = std::env::var_os("PORTHOLE_DEBUG_VIEWS_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let mut textures = vec![state.texture.borrow().clone()];
    textures.extend(state.extra_views.borrow().iter().map(|v| v.texture.borrow().clone()));
    for (i, texture) in textures.into_iter().enumerate() {
        if let Some(texture) = texture {
            let _ = texture.save_to_png(dir.join(format!("view-{i}.png")));
        }
    }
}

/// The texture for the part of the desktop a view shows (`None`: all of it). Shares the frame's
/// bytes; only the changed parts are uploaded when the view's previous texture is reusable.
/// `None` when the view has nothing new.
fn view_texture(
    damage: &porthole_core::Damage<'_>,
    bytes: &glib::Bytes,
    area: Option<monitors::Area>,
    previous: Option<&gdk::Texture>,
) -> Option<gdk::Texture> {
    let area = area.unwrap_or(monitors::Area {
        x: 0,
        y: 0,
        width: damage.width,
        height: damage.height,
    });
    // The server may not (yet) have the layout the view expects.
    let width = area.width.min(damage.width.saturating_sub(area.x));
    let height = area.height.min(damage.height.saturating_sub(area.y));
    if width == 0 || height == 0 {
        return None;
    }
    let offset = area.y as usize * damage.stride + area.x as usize * 4;
    let len = (height as usize - 1) * damage.stride + width as usize * 4;
    let slice = glib::Bytes::from_bytes(bytes, offset..offset + len);
    let mut builder = gdk::MemoryTextureBuilder::new()
        .set_bytes(Some(&slice))
        .set_width(width as i32)
        .set_height(height as i32)
        .set_stride(damage.stride)
        .set_format(gdk::MemoryFormat::B8g8r8x8);
    if !damage.full
        && let Some(previous) = previous
        && previous.width() == width as i32
        && previous.height() == height as i32
    {
        let rects: Vec<_> = damage
            .rects
            .iter()
            .filter_map(|r| {
                let x0 = r.x.max(area.x);
                let y0 = r.y.max(area.y);
                let x1 = (r.x + r.width).min(area.x + width);
                let y1 = (r.y + r.height).min(area.y + height);
                (x0 < x1 && y0 < y1).then(|| {
                    cairo::RectangleInt::new(
                        (x0 - area.x) as i32,
                        (y0 - area.y) as i32,
                        (x1 - x0) as i32,
                        (y1 - y0) as i32,
                    )
                })
            })
            .collect();
        if rects.is_empty() {
            return None;
        }
        builder = builder
            .set_update_texture(Some(previous))
            .set_update_region(Some(&cairo::Region::create_rectangles(&rects)));
    }
    Some(builder.build())
}

/// Repaints every view from scratch, e.g. after they were rearranged.
fn refresh_views(state: &State) {
    if let Some(session) = state.session.borrow().as_ref() {
        session.framebuffer().mark_full_damage();
    }
    schedule_upload(state);
}

fn set_pointer(state: &State, pointer: PointerEvent) {
    match pointer {
        PointerEvent::Default => set_cursor(state, gdk::Cursor::from_name("default", None).as_ref()),
        PointerEvent::Hidden => set_cursor(state, gdk::Cursor::from_name("none", None).as_ref()),
        // Wayland doesn't let clients warp the pointer.
        PointerEvent::Position { .. } => {}
        PointerEvent::Bitmap(bitmap) => {
            let (w, h) = (usize::from(bitmap.width), usize::from(bitmap.height));
            if w == 0 || h == 0 || bitmap.bitmap_data.len() < w * h * 4 {
                warn!(
                    w,
                    h,
                    len = bitmap.bitmap_data.len(),
                    "Ignoring malformed pointer bitmap"
                );
                return;
            }
            let bytes = glib::Bytes::from(&bitmap.bitmap_data[..w * h * 4]);
            let texture = gdk::MemoryTexture::new(w as i32, h as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, w * 4);
            let cursor =
                gdk::Cursor::from_texture(&texture, i32::from(bitmap.hotspot_x), i32::from(bitmap.hotspot_y), None);
            set_cursor(state, Some(&cursor));
        }
    }
}

fn set_cursor(state: &State, cursor: Option<&gdk::Cursor>) {
    state.picture.set_cursor(cursor);
    for view in state.extra_views.borrow().iter() {
        view.picture.set_cursor(cursor);
    }
}

/// The initial remote desktop: the full size of the monitor the parent window is on, in device
/// pixels, and its scale in percent. Once the window is laid out, the desktop follows its size.
fn desktop_size(parent: Option<&gtk::Window>) -> Desktop {
    let fallback = Desktop {
        width: 1920,
        height: 1080,
        scale: 100,
    };
    let Some(display) = gdk::Display::default() else {
        return fallback;
    };
    let monitor = parent
        .and_then(|p| p.surface())
        .and_then(|surface| display.monitor_at_surface(&surface))
        .or_else(|| display.monitors().item(0).and_downcast::<gdk::Monitor>());
    let Some(monitor) = monitor else {
        return fallback;
    };
    let geometry = monitor.geometry();
    let scale = monitor.scale();
    let device = |logical: i32| (f64::from(logical) * scale).round() as u32;
    Desktop::new(device(geometry.width()), device(geometry.height()), scale)
}

/// A remote desktop size in device pixels, and the scale in percent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Desktop {
    width: u16,
    height: u16,
    scale: u32,
}

impl Desktop {
    fn new(width: u32, height: u32, scale: f64) -> Self {
        // RDP limits; some servers also reject odd widths.
        let clamp = |v: u32, min: u32| u16::try_from(v.clamp(min, 8192)).unwrap_or(1024);
        Self {
            width: clamp(width & !1, 640),
            height: clamp(height, 480),
            scale: ((scale * 100.0).round() as u32).clamp(100, 500),
        }
    }
}
