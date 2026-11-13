//! RemoteApp: each main window or dialog of the remote program gets a window of its own, showing
//! the part of the remote desktop it covers. The session window only shows progress, the
//! desktop when Windows needs an answer before the program can start (e.g. "Another user is
//! signed in"), and errors.
//!
//! Local windows are placed by the compositor; remote ones stay where Windows put them, so a
//! point in a local window maps to the remote window's area. Windows draws its own title bar
//! and frame; when the user drags them, the PC hands the move or resize over to us.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use porthole_core::input::{MouseButton, Operation};
use porthole_core::{MoveSizeKind, RemoteAppEvent, RemoteWindow, WindowCommand};
use tracing::{debug, info};

use super::State;
use super::monitors::{Area, ExtraView};

/// How long to wait for the program's first window before showing the desktop, which then
/// usually holds a question from Windows.
const SHOW_DESKTOP_AFTER: Duration = Duration::from_secs(6);
/// Once the program's last window is gone, how long before the session is closed: programs
/// may replace a window (a splash screen by the main window, say).
const CLOSE_AFTER: Duration = Duration::from_secs(5);
/// How long a local size has to stay put before the remote window is resized to match.
const RESIZE_SETTLE: Duration = Duration::from_millis(300);
/// How long input waits for the PC to activate the window it's meant for.
const ACTIVATE_WAIT: Duration = Duration::from_millis(250);
/// Width of the strip along a window's edges that resizes it here (logical pixels). Windows'
/// own resize border lies outside the window's image, so it can't be grabbed.
const RESIZE_EDGE: f64 = 4.0;

struct AppWindow {
    view: Rc<ExtraView>,
    info: RemoteWindow,
}

/// A menu, dropdown or tooltip of the program: a popover on one of its windows, so it can
/// extend past that window like the real thing (and is a popup to the compositor).
struct Popup {
    view: Rc<ExtraView>,
    popover: gtk::Popover,
    /// The app window it's attached to.
    parent: u32,
    info: RemoteWindow,
}

#[derive(Default)]
pub(super) struct Apps {
    windows: RefCell<HashMap<u32, AppWindow>>,
    popups: RefCell<HashMap<u32, Popup>>,
    /// The popups as presented, bottom to top (the compositor stacks the latest on top).
    popup_stack: RefCell<Vec<u32>>,
    /// The PC's windows, top to bottom.
    z_order: RefCell<Vec<u32>>,
    /// One of the program's windows was shown in this session.
    seen: Cell<bool>,
    /// The PC started the program (so it's past any sign-in questions).
    started: Cell<bool>,
    /// Enhanced RemoteApp: windows get images of their own and aren't on the desktop.
    window_images: Cell<bool>,
    /// Why the program couldn't start, shown when the session has ended.
    failure: RefCell<Option<String>>,
    show_desktop: RefCell<Option<glib::SourceId>>,
    close: RefCell<Option<glib::SourceId>>,
    /// The window with the focus on the PC.
    active: Cell<u32>,
    /// Input held back until the PC has activated this window (see `focus`).
    held: RefCell<Option<Held>>,
    /// The PC's pointer shape, restored when the pointer leaves a resize edge.
    cursor: RefCell<Option<gdk::Cursor>>,
    /// Windows' invisible window borders (left, top, right, bottom), learned from how a moved
    /// window comes back (see `move_remote`).
    frame: Cell<Option<(u32, u32, u32, u32)>>,
    /// Moves sent and not answered yet, by window.
    moves: RefCell<HashMap<u32, SentMove>>,
}

struct SentMove {
    /// The rectangle sent, borders included.
    rect: (i32, i32, u32, u32),
    /// The visible position and size wanted.
    origin: (i32, i32),
    size: (u32, u32),
    /// Sent again after learning the borders.
    retried: bool,
}

struct Held {
    window: u32,
    operations: Vec<Operation>,
    timeout: glib::SourceId,
}

pub(super) fn views(state: &State) -> Vec<Rc<ExtraView>> {
    let windows = state.apps.windows.borrow();
    let popups = state.apps.popups.borrow();
    windows
        .values()
        .map(|w| w.view.clone())
        .chain(popups.values().map(|p| p.view.clone()))
        .collect()
}

/// Every view with the remote window it shows.
fn shown(state: &State) -> Vec<(Rc<ExtraView>, RemoteWindow)> {
    let windows = state.apps.windows.borrow();
    let popups = state.apps.popups.borrow();
    windows
        .values()
        .map(|w| (w.view.clone(), w.info.clone()))
        .chain(popups.values().map(|p| (p.view.clone(), p.info.clone())))
        .collect()
}

/// Uploads the windows that have an image of their own (enhanced RemoteApp) and returns the
/// others, which show their part of the desktop.
pub(super) fn upload(state: &State, session: &porthole_core::SessionHandle) -> Vec<Rc<ExtraView>> {
    let mut from_desktop = Vec::new();
    for (view, info) in shown(state) {
        let Some(image) = session.window_image(info.id) else {
            // In enhanced mode the desktop doesn't have the window; its image comes later.
            if !state.apps.window_images.get() {
                view.origin.set(None);
                from_desktop.push(view.clone());
            }
            continue;
        };
        view.origin.set(Some((info.x, info.y)));
        let area = Area {
            x: 0,
            y: 0,
            width: image.width,
            height: image.height,
        };
        let previous = view.texture.borrow().clone();
        let texture = image.framebuffer.take_damage(|damage| {
            if damage.width == 0 || damage.height == 0 {
                return None;
            }
            let bytes = glib::Bytes::from(damage.data);
            super::view_texture(&damage, &bytes, Some(area), previous.as_ref(), true)
        });
        if let Some(texture) = texture {
            view.picture.set_paintable(Some(&texture));
            view.texture.replace(Some(texture));
        }
    }
    from_desktop
}

/// Makes the next upload repaint every window with an image of its own.
pub(super) fn mark_full_damage(state: &State, session: &porthole_core::SessionHandle) {
    for (_, info) in shown(state) {
        if let Some(image) = session.window_image(info.id) {
            image.framebuffer.mark_full_damage();
        }
    }
}

/// The session is up; the program's windows follow once Windows has started it.
pub(super) fn connected(state: &State) {
    let program = state.program.as_deref().unwrap_or_default();
    state.title.set_subtitle(&format!("Starting {program}…"));
    let weak = Rc::downgrade(state);
    let source = glib::timeout_add_local_once(SHOW_DESKTOP_AFTER, move || {
        let Some(state) = weak.upgrade() else { return };
        state.apps.show_desktop.take();
        if !state.apps.seen.get() && !state.apps.started.get() && state.connected.get() {
            debug!("No program window yet; showing the desktop");
            state.stack.set_visible_child_name("display");
            state.picture.grab_focus();
        }
    });
    replace_timer(&state.apps.show_desktop, Some(source));
}

pub(super) fn handle(state: &State, event: RemoteAppEvent) {
    match event {
        RemoteAppEvent::Started { window_images } => {
            state.apps.started.set(true);
            state.apps.window_images.set(window_images);
        }
        RemoteAppEvent::StartFailed(message) => {
            state.apps.failure.replace(Some(message));
            if let Some(session) = state.session.borrow().as_ref() {
                session.disconnect();
            }
        }
        RemoteAppEvent::Window(info) => window_changed(state, info),
        RemoteAppEvent::WindowRemoved(id) => remove(state, id),
        RemoteAppEvent::ActiveWindow(id) => {
            state.apps.active.set(id);
            let ready = state.apps.held.borrow().as_ref().is_some_and(|held| held.window == id);
            if ready {
                release_held(state, false);
            }
        }
        RemoteAppEvent::ZOrder(order) => {
            state.apps.z_order.replace(order);
            restack_popups(state);
        }
        RemoteAppEvent::MoveSize { window, start, kind } => move_size(state, window, start, kind),
    }
}

fn window_changed(state: &State, info: RemoteWindow) {
    if !info.is_app_window() {
        remove_window(state, info.id);
        if info.is_popup() {
            popup_changed(state, info);
        } else {
            remove_popup(state, info.id);
        }
        return;
    }
    remove_popup(state, info.id);
    let area = visible_area(state, &info);
    let mut windows = state.apps.windows.borrow_mut();
    if let Some(existing) = windows.get_mut(&info.id) {
        let view = existing.view.clone();
        let old = std::mem::replace(&mut existing.info, info.clone());
        drop(windows);
        if (info.x, info.y, info.width, info.height) != (old.x, old.y, old.width, old.height) {
            move_answered(state, &info);
        }
        view.area.set(area);
        view.window.set_title(Some(&info.title));
        if info.is_minimized() != old.is_minimized() {
            if info.is_minimized() {
                view.window.minimize();
            } else {
                view.window.unminimize();
            }
        }
        if info.is_maximized() != old.is_maximized() {
            if info.is_maximized() {
                view.window.maximize();
            } else {
                view.window.unmaximize();
            }
        }
        if (info.width, info.height) != (old.width, old.height) && !info.is_maximized() {
            let (width, height) = logical_size(state, info.width, info.height);
            view.window.set_default_size(width, height);
        }
        super::refresh_views(state);
        return;
    }
    let owner = windows.get(&info.owner).map(|w| w.view.window.clone());
    drop(windows);
    let view = open(state, &info, area, owner.as_ref());
    state
        .apps
        .windows
        .borrow_mut()
        .insert(info.id, AppWindow { view, info });
    first_window(state);
    replace_timer(&state.apps.close, None);
    super::refresh_views(state);
}

fn popup_changed(state: &State, info: RemoteWindow) {
    let Some((parent, parent_view, parent_info)) = popup_parent(state, &info) else {
        remove_popup(state, info.id);
        return;
    };
    let area = visible_area(state, &info);
    let (width, height) = logical_size(state, info.width, info.height);
    let scale = scale(state);
    let local = |v: i32, from: i32| (f64::from(v - from) / scale).round() as i32;
    let (x, y) = (local(info.x, parent_info.x), local(info.y, parent_info.y));
    // Placed below a 1-pixel rectangle at the popup's top-left corner. Wayland wants that
    // rectangle inside the parent window, so it's clamped there and the rest is an offset.
    let parent_width = parent_view.picture.width().max(1);
    let parent_height = parent_view.picture.height().max(1);
    let anchor = gdk::Rectangle::new(x.clamp(0, parent_width - 1), (y - 1).clamp(0, parent_height - 1), 1, 1);
    let offset = (x - anchor.x(), y - (anchor.y() + 1));
    let mut popups = state.apps.popups.borrow_mut();
    if let Some(popup) = popups.get_mut(&info.id)
        && popup.parent == parent
    {
        popup.info = info;
        popup.view.area.set(area);
        popup.view.picture.set_size_request(width, height);
        popup.popover.set_pointing_to(Some(&anchor));
        popup.popover.set_offset(offset.0, offset.1);
        drop(popups);
        super::refresh_views(state);
        return;
    }
    drop(popups);
    remove_popup(state, info.id);

    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Fill)
        .can_shrink(true)
        .width_request(width)
        .height_request(height)
        .build();
    let popover = gtk::Popover::builder()
        .child(&picture)
        .has_arrow(false)
        .autohide(false)
        .can_focus(false)
        .position(gtk::PositionType::Bottom)
        .halign(gtk::Align::Start)
        .css_classes(["porthole-popup"])
        .build();
    popover.set_parent(&parent_view.picture);
    popover.set_pointing_to(Some(&anchor));
    popover.set_offset(offset.0, offset.1);
    // The keyboard stays with the parent window, which sends keys to the PC (menu navigation).
    let view = Rc::new(ExtraView::new(parent_view.window.clone(), picture, area, Some(info.id)));
    super::input::attach_popup(state, &view);
    popover.popup();
    state.apps.popup_stack.borrow_mut().push(info.id);
    debug!(
        window = format_args!("{:#x}", info.id),
        parent = format_args!("{parent:#x}"),
        ?anchor,
        "Popup"
    );
    state.apps.popups.borrow_mut().insert(
        info.id,
        Popup {
            view,
            popover,
            parent,
            info,
        },
    );
    restack_popups(state);
    super::refresh_views(state);
}

/// Stacks the popups as they are on the PC (a dropdown's shadow under its list, say) by
/// presenting them again from the bottom up when the order differs.
fn restack_popups(state: &State) {
    let wanted = {
        let z_order = state.apps.z_order.borrow();
        let mut stack = state.apps.popup_stack.borrow().clone();
        // Bottom to top: further down the z-order first; unknown ones stay on top.
        stack.sort_by_key(|id| std::cmp::Reverse(z_order.iter().position(|w| w == id).unwrap_or(0)));
        stack
    };
    if *state.apps.popup_stack.borrow() == wanted {
        return;
    }
    debug!(?wanted, "Restacking popups");
    let popovers: Vec<gtk::Popover> = {
        let popups = state.apps.popups.borrow();
        wanted
            .iter()
            .filter_map(|id| popups.get(id).map(|p| p.popover.clone()))
            .collect()
    };
    for popover in &popovers {
        popover.popdown();
    }
    for popover in &popovers {
        popover.popup();
    }
    state.apps.popup_stack.replace(wanted);
}

/// The window a popup belongs to: its owner, else the active one (menus have no owner), else
/// any of the program's windows.
fn popup_parent(state: &State, info: &RemoteWindow) -> Option<(u32, Rc<ExtraView>, RemoteWindow)> {
    let windows = state.apps.windows.borrow();
    [info.owner, state.apps.active.get()]
        .into_iter()
        .find_map(|id| windows.get(&id).map(|w| (id, w)))
        .or_else(|| windows.iter().next().map(|(&id, w)| (id, w)))
        .map(|(id, w)| (id, w.view.clone(), w.info.clone()))
}

fn remove_popup(state: &State, id: u32) {
    let Some(popup) = state.apps.popups.borrow_mut().remove(&id) else {
        return;
    };
    state.apps.popup_stack.borrow_mut().retain(|&p| p != id);
    popup.popover.popdown();
    popup.popover.unparent();
}

/// The first of the program's windows is up: the session window steps aside.
fn first_window(state: &State) {
    if state.apps.seen.replace(true) {
        return;
    }
    info!(program = ?state.program, "The program's window is up");
    replace_timer(&state.apps.show_desktop, None);
    state.window.set_visible(false);
}

fn remove(state: &State, id: u32) {
    remove_popup(state, id);
    remove_window(state, id);
}

/// Closes the local window for an app window, with its popups.
fn remove_window(state: &State, id: u32) {
    let Some(window) = state.apps.windows.borrow_mut().remove(&id) else {
        return;
    };
    let orphans: Vec<u32> = state
        .apps
        .popups
        .borrow()
        .iter()
        .filter(|(_, p)| p.parent == id)
        .map(|(&popup, _)| popup)
        .collect();
    for popup in orphans {
        remove_popup(state, popup);
    }
    window.view.window.destroy();
    if state.apps.windows.borrow().is_empty() && state.apps.seen.get() {
        // The program has probably exited; Windows signs the session out by itself soon, but
        // there's no need to wait for that.
        let weak = Rc::downgrade(state);
        let source = glib::timeout_add_local_once(CLOSE_AFTER, move || {
            let Some(state) = weak.upgrade() else { return };
            state.apps.close.take();
            if state.apps.windows.borrow().is_empty()
                && let Some(session) = state.session.borrow().as_ref()
            {
                info!("The program has no windows left; disconnecting");
                session.disconnect();
            }
        });
        replace_timer(&state.apps.close, Some(source));
    }
}

fn open(state: &State, info: &RemoteWindow, area: Area, owner: Option<&gtk::Window>) -> Rc<ExtraView> {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["porthole-app-picture"])
        .build();
    let (width, height) = logical_size(state, info.width, info.height);
    // Windows draws the title bar and frame.
    let window = gtk::Window::builder()
        .title(&info.title)
        .child(&picture)
        .decorated(false)
        .css_classes(["porthole-app"])
        .default_width(width)
        .default_height(height)
        .build();
    window.set_application(state.window.application().as_ref());
    window.set_transient_for(owner);
    let view = Rc::new(ExtraView::new(window.clone(), picture, area, Some(info.id)));
    super::input::attach_extra(state, &view);
    attach_resize_edges(state, &view);

    let id = info.id;
    let weak = Rc::downgrade(state);
    window.connect_is_active_notify(move |window| {
        if window.is_active()
            && let Some(state) = weak.upgrade()
        {
            focus(&state, id);
            super::input::synchronize_locks(&state);
        }
    });
    // Closing asks the program, which may want to save first.
    let weak = Rc::downgrade(state);
    window.connect_close_request(move |_| {
        if let Some(state) = weak.upgrade()
            && let Some(session) = state.session.borrow().as_ref()
        {
            session.window_command(id, WindowCommand::Close);
        }
        glib::Propagation::Stop
    });
    follow_local_size(state, &view);
    if info.is_maximized() {
        window.maximize();
    }
    window.present();
    debug!(window = format_args!("{id:#x}"), title = %info.title, ?area, "Window for the program");
    view
}

/// Input is for `window`. Remote windows overlap on the PC's desktop even when the local ones
/// don't, and the PC sends clicks and keys to whatever is on top there, so the window is
/// activated first and input waits until the PC reports it active.
pub(super) fn focus(state: &State, window: u32) {
    // Clicks in menus and dropdowns go to whatever window has them, without activating anything.
    if state.apps.popups.borrow().contains_key(&window) {
        return;
    }
    if state.apps.active.get() == window || state.apps.held.borrow().as_ref().is_some_and(|h| h.window == window) {
        return;
    }
    match state.session.borrow().as_ref() {
        Some(session) => session.activate_window(window, true),
        None => return,
    }
    let weak = Rc::downgrade(state);
    let timeout = glib::timeout_add_local_once(ACTIVATE_WAIT, move || {
        let Some(state) = weak.upgrade() else { return };
        debug!(
            window = format_args!("{window:#x}"),
            "The PC didn't confirm the activation; sending input anyway"
        );
        release_held(&state, true);
    });
    // Input held for another window goes out first, in order.
    release_held(state, false);
    state.apps.held.replace(Some(Held {
        window,
        operations: Vec::new(),
        timeout,
    }));
}

/// Holds `operations` back if a window is being activated; returns whether it did.
pub(super) fn hold(state: &State, operations: &mut Vec<Operation>) -> bool {
    match state.apps.held.borrow_mut().as_mut() {
        Some(held) => {
            held.operations.append(operations);
            true
        }
        None => false,
    }
}

/// Sends the held input. `timed_out`: called from the hold's own timeout, which has fired.
fn release_held(state: &State, timed_out: bool) {
    let Some(held) = state.apps.held.take() else { return };
    if !timed_out {
        held.timeout.remove();
    }
    if let Some(session) = state.session.borrow().as_ref() {
        session.send_input(held.operations);
    }
}

/// The PC changed the pointer shape.
pub(super) fn cursor_changed(state: &State, cursor: Option<&gdk::Cursor>) {
    state.apps.cursor.replace(cursor.cloned());
}

/// Which edge or corner of the picture a point is on, if any.
fn edge_at(picture: &gtk::Picture, x: f64, y: f64) -> Option<gdk::SurfaceEdge> {
    edge_in(f64::from(picture.width()), f64::from(picture.height()), x, y)
}

/// Which edge or corner of a `width` × `height` area a point is on, if any.
fn edge_in(width: f64, height: f64, x: f64, y: f64) -> Option<gdk::SurfaceEdge> {
    let (left, right) = (x < RESIZE_EDGE, x >= width - RESIZE_EDGE);
    let (top, bottom) = (y < RESIZE_EDGE, y >= height - RESIZE_EDGE);
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => gdk::SurfaceEdge::NorthWest,
        (_, true, true, _) => gdk::SurfaceEdge::NorthEast,
        (true, _, _, true) => gdk::SurfaceEdge::SouthWest,
        (_, true, _, true) => gdk::SurfaceEdge::SouthEast,
        (true, ..) => gdk::SurfaceEdge::West,
        (_, true, ..) => gdk::SurfaceEdge::East,
        (_, _, true, _) => gdk::SurfaceEdge::North,
        (.., true) => gdk::SurfaceEdge::South,
        _ => return None,
    })
}

fn edge_cursor(edge: gdk::SurfaceEdge) -> &'static str {
    match edge {
        gdk::SurfaceEdge::NorthWest => "nw-resize",
        gdk::SurfaceEdge::NorthEast => "ne-resize",
        gdk::SurfaceEdge::SouthWest => "sw-resize",
        gdk::SurfaceEdge::SouthEast => "se-resize",
        gdk::SurfaceEdge::West => "w-resize",
        gdk::SurfaceEdge::East => "e-resize",
        gdk::SurfaceEdge::North => "n-resize",
        _ => "s-resize",
    }
}

/// A press on the window's edge resizes it here instead of going to the PC.
fn attach_resize_edges(state: &State, view: &Rc<ExtraView>) {
    let resizable = {
        let weak_state = Rc::downgrade(state);
        let weak_view = Rc::downgrade(view);
        move || -> bool {
            let (Some(state), Some(view)) = (weak_state.upgrade(), weak_view.upgrade()) else {
                return false;
            };
            !view.window.is_maximized()
                && !view.window.is_fullscreen()
                && view
                    .remote_window
                    .and_then(|id| state.apps.windows.borrow().get(&id).map(|w| !w.info.is_maximized()))
                    .unwrap_or(false)
        }
    };

    let motion = gtk::EventControllerMotion::new();
    let on_edge = Rc::new(Cell::new(false));
    let weak_state = Rc::downgrade(state);
    let can_resize = resizable.clone();
    motion.connect_motion(glib::clone!(
        #[strong]
        on_edge,
        move |controller, x, y| {
            let Some(picture) = controller.widget().and_downcast::<gtk::Picture>() else {
                return;
            };
            let edge = edge_at(&picture, x, y).filter(|_| can_resize());
            match edge {
                Some(edge) => {
                    on_edge.set(true);
                    picture.set_cursor(gdk::Cursor::from_name(edge_cursor(edge), None).as_ref());
                }
                None if on_edge.replace(false) => {
                    if let Some(state) = weak_state.upgrade() {
                        picture.set_cursor(state.apps.cursor.borrow().as_ref());
                    }
                }
                None => {}
            }
        }
    ));
    view.picture.add_controller(motion);

    // Capture phase: the press must not reach the PC.
    let press = gtk::EventControllerLegacy::new();
    press.set_propagation_phase(gtk::PropagationPhase::Capture);
    press.connect_event(move |controller, event| {
        if event.event_type() != gdk::EventType::ButtonPress {
            return glib::Propagation::Proceed;
        }
        let Some(button) = event.downcast_ref::<gdk::ButtonEvent>() else {
            return glib::Propagation::Proceed;
        };
        let Some(picture) = controller.widget().and_downcast::<gtk::Picture>() else {
            return glib::Propagation::Proceed;
        };
        let Some((x, y)) = event.position() else {
            return glib::Propagation::Proceed;
        };
        // Event positions are in surface coordinates; the picture fills the window.
        let Some(edge) = edge_at(&picture, x, y).filter(|_| button.button() == 1 && resizable()) else {
            return glib::Propagation::Proceed;
        };
        let (Some(toplevel), Some(device)) = (
            picture
                .native()
                .and_then(|n| n.surface())
                .and_downcast::<gdk::Toplevel>(),
            event.device(),
        ) else {
            return glib::Propagation::Proceed;
        };
        debug!(?edge, "Resizing a program window here");
        toplevel.begin_resize(edge, Some(&device), 1, x, y, event.time());
        glib::Propagation::Stop
    });
    view.picture.add_controller(press);
}

/// When the compositor resizes the window (tiling, say), the remote window follows once the
/// size settles.
fn follow_local_size(state: &State, view: &Rc<ExtraView>) {
    let weak_state = Rc::downgrade(state);
    let weak_view = Rc::downgrade(view);
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    view.window.connect_realize(move |window| {
        let Some(surface) = window.surface() else { return };
        let (weak_state, weak_view, pending) = (weak_state.clone(), weak_view.clone(), pending.clone());
        surface.connect_layout(move |_, _, _| {
            let (weak_state, weak_view, slot) = (weak_state.clone(), weak_view.clone(), pending.clone());
            let source = glib::timeout_add_local_once(RESIZE_SETTLE, move || {
                slot.take();
                if let (Some(state), Some(view)) = (weak_state.upgrade(), weak_view.upgrade())
                    && view.moving.get().is_none()
                {
                    sync_size(&state, &view);
                }
            });
            replace_timer(&pending, Some(source));
        });
    });
}

/// Resizes the remote window to the local one's size, unless they already match.
fn sync_size(state: &State, view: &ExtraView) {
    let Some(id) = view.remote_window else { return };
    let Some(info) = state.apps.windows.borrow().get(&id).map(|w| w.info.clone()) else {
        return;
    };
    if info.is_maximized() || info.is_minimized() {
        return;
    }
    let scale = scale(state);
    let width = (f64::from(view.picture.width()) * scale).round() as u32;
    let height = (f64::from(view.picture.height()) * scale).round() as u32;
    if width == 0 || height == 0 || (width.abs_diff(info.width) <= 1 && height.abs_diff(info.height) <= 1) {
        return;
    }
    debug!(
        window = format_args!("{id:#x}"),
        width, height, "Resizing the remote window to match"
    );
    move_remote(state, &info, width, height);
}

/// The PC hands a move or resize over: the compositor does it, with the button still held.
fn move_size(state: &State, id: u32, start: bool, kind: MoveSizeKind) {
    let Some(view) = state.apps.windows.borrow().get(&id).map(|w| w.view.clone()) else {
        return;
    };
    if !start {
        view.moving.set(None);
        return;
    }
    let press = view.last_press.borrow().clone();
    let toplevel = view.window.surface().and_downcast::<gdk::Toplevel>();
    let (Some(press), Some(toplevel), Some(device)) = (
        press.as_ref().and_then(|e| e.downcast_ref::<gdk::ButtonEvent>()),
        toplevel,
        press.as_ref().and_then(gdk::Event::device),
    ) else {
        // Keyboard moves and sizes (from the window menu) aren't supported: end them at once.
        debug!(?kind, "Can't move or resize locally; ending");
        view.moving.set(Some(kind));
        move_size_ended(state, &view);
        return;
    };
    let (x, y) = press.position().unwrap_or_default();
    let button = i32::try_from(press.button()).unwrap_or(1);
    let time = press.time();
    view.moving.set(Some(kind));
    let edge = match kind {
        MoveSizeKind::Left => Some(gdk::SurfaceEdge::West),
        MoveSizeKind::Right => Some(gdk::SurfaceEdge::East),
        MoveSizeKind::Top => Some(gdk::SurfaceEdge::North),
        MoveSizeKind::TopLeft => Some(gdk::SurfaceEdge::NorthWest),
        MoveSizeKind::TopRight => Some(gdk::SurfaceEdge::NorthEast),
        MoveSizeKind::Bottom => Some(gdk::SurfaceEdge::South),
        MoveSizeKind::BottomLeft => Some(gdk::SurfaceEdge::SouthWest),
        MoveSizeKind::BottomRight => Some(gdk::SurfaceEdge::SouthEast),
        MoveSizeKind::Move | MoveSizeKind::KeyMove | MoveSizeKind::KeySize => None,
    };
    debug!(?kind, x, y, button, "Local move/resize");
    match edge {
        Some(edge) => toplevel.begin_resize(edge, Some(&device), button, x, y, time),
        None => toplevel.begin_move(&device, button, x, y, time),
    }
}

/// The compositor is done: tell the PC the window's rectangle, which ends its move loop, and
/// release the button it still thinks is held.
pub(super) fn move_size_ended(state: &State, view: &ExtraView) {
    let Some(kind) = view.moving.take() else { return };
    let Some(id) = view.remote_window else { return };
    let Some(info) = state.apps.windows.borrow().get(&id).map(|w| w.info.clone()) else {
        return;
    };
    let (mut width, mut height) = (info.width, info.height);
    if kind != MoveSizeKind::Move {
        let scale = scale(state);
        width = (f64::from(view.picture.width()) * scale).round() as u32;
        height = (f64::from(view.picture.height()) * scale).round() as u32;
    }
    debug!(?kind, width, height, "Local move/resize ended");
    move_remote(state, &info, width.max(1), height.max(1));
    if let Some(session) = state.session.borrow().as_ref() {
        session.send_input([Operation::MouseButtonReleased(MouseButton::Left)]);
    }
}

/// Gives the remote window this visible size where it is. Moves are in window rectangles,
/// which include Windows' invisible borders; windowing orders give the visible frame.
fn move_remote(state: &State, info: &RemoteWindow, width: u32, height: u32) {
    let frame = match info.resize_margins {
        (0, 0, 0, 0) => state.apps.frame.get().unwrap_or_default(),
        margins => margins,
    };
    send_move(state, info.id, (info.x, info.y), (width, height), frame, false);
}

fn send_move(
    state: &State,
    window: u32,
    origin: (i32, i32),
    size: (u32, u32),
    frame: (u32, u32, u32, u32),
    retried: bool,
) {
    let (left, top, right, bottom) = frame;
    let grow = |v: u32| i32::try_from(v).unwrap_or(0);
    let rect = (
        origin.0 - grow(left),
        origin.1 - grow(top),
        size.0 + left + right,
        size.1 + top + bottom,
    );
    state.apps.moves.borrow_mut().insert(
        window,
        SentMove {
            rect,
            origin,
            size,
            retried,
        },
    );
    if let Some(session) = state.session.borrow().as_ref() {
        session.move_window(window, rect.0, rect.1, rect.2, rect.3);
    }
}

/// The PC moved or resized a window: if that answers our request, learn the invisible borders
/// from the difference, and ask again once if the size came out wrong.
fn move_answered(state: &State, info: &RemoteWindow) {
    let Some(sent) = state.apps.moves.borrow_mut().remove(&info.id) else {
        return;
    };
    if info.is_maximized() || info.is_minimized() {
        return;
    }
    let Some(frame) = frame_between(sent.rect, (info.x, info.y, info.width, info.height)) else {
        return;
    };
    if state.apps.frame.replace(Some(frame)) != Some(frame) {
        debug!(?frame, "Learned Windows' invisible window borders");
    }
    if ((info.x, info.y), (info.width, info.height)) != (sent.origin, sent.size) && !sent.retried {
        send_move(state, info.id, sent.origin, sent.size, frame, true);
    }
}

/// The borders (left, top, right, bottom) between a window rectangle sent and the visible
/// frame that came back, if they look like window borders.
fn frame_between(sent: (i32, i32, u32, u32), visible: (i32, i32, u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let border = |outer: i64, inner: i64| u32::try_from(outer - inner).ok().filter(|&b| b <= 32);
    let right = |(x, _, w, _): (i32, i32, u32, u32)| i64::from(x) + i64::from(w);
    let bottom = |(_, y, _, h): (i32, i32, u32, u32)| i64::from(y) + i64::from(h);
    Some((
        border(i64::from(visible.0), i64::from(sent.0))?,
        border(i64::from(visible.1), i64::from(sent.1))?,
        border(right(sent), right(visible))?,
        border(bottom(sent), bottom(visible))?,
    ))
}

/// The session ended. Returns whether that's the normal end of a RemoteApp session (the
/// program's windows were up and all of them have closed), which needs no message.
pub(super) fn session_ended(state: &State) -> bool {
    replace_timer(&state.apps.show_desktop, None);
    replace_timer(&state.apps.close, None);
    if let Some(held) = state.apps.held.take() {
        held.timeout.remove();
    }
    state.apps.active.set(0);
    let popups: Vec<u32> = state.apps.popups.borrow().keys().copied().collect();
    for popup in popups {
        remove_popup(state, popup);
    }
    let windows: Vec<_> = state.apps.windows.borrow_mut().drain().map(|(_, w)| w.view).collect();
    let finished = state.apps.seen.replace(false) && windows.is_empty();
    state.apps.window_images.set(false);
    state.apps.started.set(false);
    for view in windows {
        view.window.destroy();
    }
    if !finished {
        state.window.set_visible(true);
    }
    finished
}

/// Why the program couldn't start, once (see [`RemoteAppEvent::StartFailed`]).
pub(super) fn take_failure(state: &State) -> Option<String> {
    state.apps.failure.take()
}

/// The part of the remote window on the remote desktop: maximized windows hang over the edges.
fn visible_area(state: &State, info: &RemoteWindow) -> Area {
    let (desktop_width, desktop_height) = state
        .session
        .borrow()
        .as_ref()
        .map_or((u32::from(state.desktop.width), u32::from(state.desktop.height)), |s| {
            s.framebuffer().size()
        });
    let clamp = |v: i64, max: u32| v.clamp(0, i64::from(max)) as u32;
    let (x0, y0) = (i64::from(info.x), i64::from(info.y));
    let x = clamp(x0, desktop_width);
    let y = clamp(y0, desktop_height);
    let right = clamp(x0 + i64::from(info.width), desktop_width);
    let bottom = clamp(y0 + i64::from(info.height), desktop_height);
    Area {
        x,
        y,
        width: right.saturating_sub(x),
        height: bottom.saturating_sub(y),
    }
}

/// Remote pixels per local logical pixel: the PC renders at the local monitor's scale.
fn scale(state: &State) -> f64 {
    f64::from(state.desktop.scale) / 100.0
}

fn logical_size(state: &State, width: u32, height: u32) -> (i32, i32) {
    let scale = scale(state);
    let logical = |v: u32| ((f64::from(v) / scale).round() as i32).max(1);
    (logical(width), logical(height))
}

fn replace_timer(slot: &RefCell<Option<glib::SourceId>>, source: Option<glib::SourceId>) {
    if let Some(old) = slot.replace(source) {
        old.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_resize_edges() {
        assert_eq!(edge_in(100.0, 50.0, 1.0, 1.0), Some(gdk::SurfaceEdge::NorthWest));
        assert_eq!(edge_in(100.0, 50.0, 99.0, 25.0), Some(gdk::SurfaceEdge::East));
        assert_eq!(edge_in(100.0, 50.0, 50.0, 48.0), Some(gdk::SurfaceEdge::South));
        assert_eq!(edge_in(100.0, 50.0, 97.0, 47.0), Some(gdk::SurfaceEdge::SouthEast));
        assert_eq!(edge_in(100.0, 50.0, 50.0, 25.0), None);
    }

    #[test]
    fn learns_window_borders_from_a_move() {
        // Asked for 1121x696 at (41, 26); Windows 11 came back with 1111x691 at (46, 26).
        assert_eq!(
            frame_between((41, 26, 1121, 696), (46, 26, 1111, 691)),
            Some((5, 0, 5, 5))
        );
        // The window went somewhere else entirely: not borders.
        assert_eq!(frame_between((41, 26, 1121, 696), (300, 26, 1111, 691)), None);
        assert_eq!(frame_between((41, 26, 100, 100), (30, 26, 100, 100)), None);
    }
}
