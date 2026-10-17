//! Using all local monitors in full screen: the remote desktop gets one monitor per local monitor,
//! and each local monitor shows its part in a full-screen window of its own (the session window
//! on its monitor, extra windows on the others).

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use porthole_core::MonitorRect;
use tracing::{debug, info};

use super::State;

/// A part of the remote desktop, in its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Area {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A full-screen window showing the part of the desktop on another local monitor.
pub(super) struct ExtraView {
    pub window: gtk::Window,
    pub picture: gtk::Picture,
    pub area: Area,
    pub texture: RefCell<Option<gdk::Texture>>,
}

/// One local monitor's place in the remote layout.
#[derive(Debug, Clone, PartialEq)]
struct Placement {
    monitor: MonitorRect,
    area: Area,
}

/// The remote layout for local monitors given as (x, y, width, height) in device pixels; the
/// first is the primary. Remote positions are relative to the primary; areas are relative to the
/// desktop's top-left corner.
fn plan(monitors: &[(i32, i32, u32, u32)]) -> Vec<Placement> {
    let Some(&(px, py, _, _)) = monitors.first() else {
        return Vec::new();
    };
    let rects: Vec<MonitorRect> = monitors
        .iter()
        .map(|&(x, y, width, height)| MonitorRect {
            left: x - px,
            top: y - py,
            // The protocol wants even widths.
            width: width & !1,
            height,
        })
        .collect();
    let left = rects.iter().map(|r| r.left).min().unwrap_or(0);
    let top = rects.iter().map(|r| r.top).min().unwrap_or(0);
    rects
        .into_iter()
        .map(|monitor| Placement {
            area: Area {
                x: (monitor.left - left) as u32,
                y: (monitor.top - top) as u32,
                width: monitor.width,
                height: monitor.height,
            },
            monitor,
        })
        .collect()
}

/// Called when the session window enters or leaves full screen.
pub(super) fn fullscreen_changed(state: &State) {
    if state.window.is_fullscreen() && state.all_monitors {
        enter(state);
    } else {
        leave(state);
    }
}

fn enter(state: &State) {
    if state.spanning.get() {
        return;
    }
    let Some(display) = gdk::Display::default() else { return };
    let Some(surface) = state.window.surface() else { return };
    let monitors: Vec<gdk::Monitor> = display
        .monitors()
        .iter::<gdk::Monitor>()
        .filter_map(Result::ok)
        .collect();
    if monitors.len() < 2 {
        return;
    }
    // The session window's monitor is the primary; the others follow.
    let primary = display
        .monitor_at_surface(&surface)
        .unwrap_or_else(|| monitors[0].clone());
    let mut ordered = vec![primary.clone()];
    ordered.extend(monitors.into_iter().filter(|m| *m != primary));
    let device = |m: &gdk::Monitor| {
        let g = m.geometry();
        let s = m.scale();
        let px = |v: i32| (f64::from(v) * s).round();
        (
            px(g.x()) as i32,
            px(g.y()) as i32,
            px(g.width()) as u32,
            px(g.height()) as u32,
        )
    };
    let placements = plan(&ordered.iter().map(device).collect::<Vec<_>>());
    info!(monitors = placements.len(), "Using all monitors");
    let scale = (primary.scale() * 100.0).round() as u32;

    state.spanning.set(true);
    state.main_area.set(Some(placements[0].area));
    let app = state.window.application();
    for (monitor, placement) in ordered.iter().zip(&placements).skip(1) {
        let view = extra_window(state, app.as_ref(), monitor, placement.area);
        state.extra_views.borrow_mut().push(view);
    }
    state
        .span_layout
        .replace(Some((placements.iter().map(|p| p.monitor).collect(), scale)));
    send_layout(state);
    // Every view needs the whole picture again.
    super::refresh_views(state);
}

fn extra_window(state: &State, app: Option<&gtk::Application>, monitor: &gdk::Monitor, area: Area) -> Rc<ExtraView> {
    let picture = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .can_shrink(true)
        .focusable(true)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["porthole-display"])
        .build();
    let window = gtk::Window::builder()
        .title(state.window.title().unwrap_or_default())
        .child(&picture)
        .decorated(false)
        .build();
    window.set_application(app);
    let view = Rc::new(ExtraView {
        window: window.clone(),
        picture,
        area,
        texture: RefCell::default(),
    });
    super::input::attach_extra(state, &view);
    // Closing any of the windows ends spanning (Alt+F4 on a secondary monitor, say).
    let weak = Rc::downgrade(state);
    window.connect_close_request(move |_| {
        if let Some(state) = weak.upgrade() {
            glib::idle_add_local_once(move || state.window.unfullscreen());
        }
        glib::Propagation::Proceed
    });
    window.fullscreen_on_monitor(monitor);
    window.present();
    debug!(monitor = ?monitor.geometry(), ?area, "Window for another monitor");
    view
}

/// Sends the all-monitor layout (again, e.g. after reconnecting).
pub(super) fn send_layout(state: &State) {
    if let (Some((monitors, scale)), Some(session)) = (&*state.span_layout.borrow(), &*state.session.borrow()) {
        session.set_monitors(monitors.clone(), *scale);
    }
}

/// Back to one window following its size.
pub(super) fn leave(state: &State) {
    if !state.spanning.replace(false) {
        return;
    }
    debug!("Leaving all-monitor full screen");
    state.main_area.set(None);
    state.span_layout.replace(None);
    for view in state.extra_views.take() {
        view.window.destroy();
    }
    super::refresh_views(state);
    super::resize::schedule(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_monitors_relative_to_the_primary() {
        // Primary on the right of a taller monitor that sits higher up.
        let placements = plan(&[(1920, 0, 1920, 1080), (0, -200, 1281, 1440)]);
        assert_eq!(
            placements[0].monitor,
            MonitorRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(
            placements[1].monitor,
            MonitorRect {
                left: -1920,
                top: -200,
                width: 1280,
                height: 1440
            }
        );
        assert_eq!(
            placements[0].area,
            Area {
                x: 1920,
                y: 200,
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(
            placements[1].area,
            Area {
                x: 0,
                y: 0,
                width: 1280,
                height: 1440
            }
        );
    }
}
