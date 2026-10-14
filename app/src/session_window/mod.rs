//! One window per session: remote display, input, full screen, disconnect handling.

mod input;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{cairo, gdk, gio, glib};
use libadwaita as adw;
use rdesk_core::{Credentials, DisconnectReason, PointerEvent, SessionConfig, SessionEvent, SessionHandle};
use rdesk_input::ScrollAccumulator;
use tracing::{debug, warn};

use crate::dialogs;

pub struct SessionWindow;

struct Inner {
    window: adw::ApplicationWindow,
    toolbar: adw::ToolbarView,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    picture: gtk::Picture,
    ended: adw::StatusPage,
    reconnect: gtk::Button,

    host: String,
    port: u16,
    desktop: (u16, u16),
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
}

type State = Rc<Inner>;

impl SessionWindow {
    pub fn open(
        app: &adw::Application,
        parent: Option<&impl IsA<gtk::Window>>,
        host: String,
        port: u16,
        credentials: Credentials,
    ) {
        let desktop = desktop_size(parent.map(|p| p.upcast_ref()));
        let state = build(app, host, port, desktop, credentials);
        state.window.maximize();
        state.window.present();
        start(&state);
    }
}

fn build(app: &adw::Application, host: String, port: u16, desktop: (u16, u16), credentials: Credentials) -> State {
    let title = adw::WindowTitle::new(&host, "Connecting…");
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

    let connecting = adw::StatusPage::builder()
        .title(format!("Connecting to {host}"))
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
    picture.add_css_class("rdesk-display");

    let reconnect = gtk::Button::builder()
        .label("Reconnect")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let ended = adw::StatusPage::builder()
        .icon_name("network-offline-symbolic")
        .title("Disconnected")
        .child(&reconnect)
        .build();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    stack.add_named(&connecting, Some("connecting"));
    stack.add_named(&picture, Some("display"));
    stack.add_named(&ended, Some("ended"));

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&host)
        .default_width(1280)
        .default_height(800)
        .content(&toolbar)
        .build();
    // A black background around the scaled desktop instead of the window colour.
    let css = gtk::CssProvider::new();
    css.load_from_string(".rdesk-display { background: black; }");
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&window),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let state = Rc::new(Inner {
        window,
        toolbar,
        title,
        toasts,
        stack,
        picture,
        ended,
        reconnect,
        host,
        port,
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
    });

    disconnect.connect_clicked(glib::clone!(
        #[weak(rename_to = window)]
        state.window,
        move |_| window.close()
    ));
    fullscreen.connect_clicked(glib::clone!(
        #[weak(rename_to = window)]
        state.window,
        move |_| window.fullscreen()
    ));
    state.window.connect_fullscreened_notify(glib::clone!(
        #[weak(rename_to = toolbar)]
        state.toolbar,
        #[weak(rename_to = toasts)]
        state.toasts,
        move |window| {
            let fullscreen = window.is_fullscreen();
            toolbar.set_reveal_top_bars(!fullscreen);
            if fullscreen {
                toasts.add_toast(adw::Toast::new("Press Ctrl+Alt+Enter to leave full screen"));
            }
        }
    ));

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

    input::attach(&state);
    state
}

fn start(state: &State) {
    let mut config = SessionConfig::new(state.host.clone(), state.credentials.borrow().clone());
    config.port = state.port;
    (config.desktop_width, config.desktop_height) = state.desktop;

    let (session, events) = rdesk_core::connect(config, crate::runtime().handle());
    state.session.replace(Some(session));
    state.texture.replace(None);
    state.picture.set_paintable(None::<&gdk::Paintable>);
    state.title.set_subtitle("Connecting…");
    state.stack.set_visible_child_name("connecting");
    glib::spawn_future_local(run_events(state.clone(), events));
}

async fn reconnect_session(state: State) {
    let reason = state.last_reason.borrow().clone();
    if let Some(DisconnectReason::Authentication(failure)) = reason {
        let username = state.credentials.borrow().username.clone();
        let Some(credentials) =
            dialogs::ask_credentials(&state.window, &state.host, Some(&username), Some(&failure.to_string())).await
        else {
            return;
        };
        state.credentials.replace(credentials);
    }
    start(&state);
}

async fn run_events(state: State, events: async_channel::Receiver<SessionEvent>) {
    while let Ok(event) = events.recv().await {
        match event {
            SessionEvent::CertificatePrompt(prompt) => {
                let trusted = dialogs::ask_certificate_trust(&state.window, &prompt.certificate, &prompt.problem).await;
                if trusted { prompt.accept() } else { prompt.reject() }
            }
            SessionEvent::Connected { width, height } => {
                state.connected.set(true);
                state.title.set_subtitle(&format!("{width} × {height}"));
                state.stack.set_visible_child_name("display");
                state.picture.grab_focus();
                input::synchronize_locks(&state);
            }
            SessionEvent::DesktopResized { width, height } => {
                state.title.set_subtitle(&format!("{width} × {height}"));
            }
            SessionEvent::FrameReady => schedule_upload(&state),
            SessionEvent::Pointer(pointer) => set_pointer(&state, pointer),
            SessionEvent::Disconnected(reason) => {
                session_ended(&state, reason);
                return;
            }
        }
    }
}

fn session_ended(state: &State, reason: DisconnectReason) {
    debug!(%reason, "Session ended");
    state.connected.set(false);
    state.session.replace(None);
    state.picture.set_cursor(None);
    if state.closing.take().is_some() || reason == DisconnectReason::UserRequested {
        state.window.destroy();
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
    let previous = state.texture.borrow().clone();
    let texture = session.framebuffer().take_damage(|damage| {
        if damage.width == 0 || damage.height == 0 {
            return None;
        }
        let (width, height) = (damage.width as i32, damage.height as i32);
        // GdkMemoryTexture keeps a reference to its bytes, so every texture needs its own copy.
        let bytes = glib::Bytes::from(damage.data);
        let mut builder = gdk::MemoryTextureBuilder::new()
            .set_bytes(Some(&bytes))
            .set_width(width)
            .set_height(height)
            .set_stride(damage.stride)
            .set_format(gdk::MemoryFormat::B8g8r8x8);
        if !damage.full
            && let Some(previous) = &previous
            && previous.width() == width
            && previous.height() == height
        {
            let rects: Vec<_> = damage
                .rects
                .iter()
                .map(|r| cairo::RectangleInt::new(r.x as i32, r.y as i32, r.width as i32, r.height as i32))
                .collect();
            builder = builder
                .set_update_texture(Some(previous))
                .set_update_region(Some(&cairo::Region::create_rectangles(&rects)));
        }
        Some(builder.build())
    });
    if let Some(texture) = texture {
        state.picture.set_paintable(Some(&texture));
        state.texture.replace(Some(texture));
    }
}

fn set_pointer(state: &State, pointer: PointerEvent) {
    match pointer {
        PointerEvent::Default => state.picture.set_cursor_from_name(Some("default")),
        PointerEvent::Hidden => state.picture.set_cursor_from_name(Some("none")),
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
            state.picture.set_cursor(Some(&cursor));
        }
    }
}

/// The remote desktop size: the logical size of the monitor the parent window is on, so the
/// session is pixel-exact in full screen. (Dynamic resolution comes in Phase 2.)
fn desktop_size(parent: Option<&gtk::Window>) -> (u16, u16) {
    let fallback = (1920, 1080);
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
    let clamp = |v: i32, min: i32| u16::try_from(v.clamp(min, 8192)).unwrap_or(1024);
    // Some servers reject odd widths.
    (clamp(geometry.width() & !1, 640), clamp(geometry.height(), 480))
}
