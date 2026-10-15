//! Dynamic resolution: the remote desktop follows the size of the display area.
//!
//! Requests wait until the size has settled, so dragging the window edge doesn't make the remote
//! PC switch resolution many times a second. Until the new desktop arrives, the old one is scaled.

use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use super::{Desktop, State};

const SETTLE: Duration = Duration::from_millis(400);

pub(super) fn attach(state: &State) {
    let weak = Rc::downgrade(state);
    state.window.connect_realize(move |window| {
        let Some(surface) = window.surface() else { return };
        let on_layout = weak.clone();
        surface.connect_layout(move |_, _, _| {
            if let Some(state) = on_layout.upgrade() {
                schedule(&state);
            }
        });
        let on_scale = weak.clone();
        surface.connect_scale_notify(move |_| {
            if let Some(state) = on_scale.upgrade() {
                schedule(&state);
            }
        });
    });
    // Hiding the header bar in full screen changes the display area but not the window size.
    let weak = Rc::downgrade(state);
    state.window.connect_fullscreened_notify(move |_| {
        if let Some(state) = weak.upgrade() {
            schedule(&state);
        }
    });
}

/// Requests a desktop matching the display area once the size stops changing.
pub(super) fn schedule(state: &State) {
    if let Some(pending) = state.resize_timeout.take() {
        pending.remove();
    }
    let weak = Rc::downgrade(state);
    let source = glib::timeout_add_local_once(SETTLE, move || {
        let Some(state) = weak.upgrade() else { return };
        state.resize_timeout.take();
        request(&state);
    });
    state.resize_timeout.replace(Some(source));
}

fn request(state: &State) {
    if !state.connected.get() {
        return;
    }
    let (width, height) = (state.picture.width(), state.picture.height());
    let Some(surface) = state.window.surface() else { return };
    if width <= 0 || height <= 0 {
        return;
    }
    let scale = surface.scale();
    let device = |logical: i32| (f64::from(logical) * scale).round() as u32;
    let desktop = Desktop::new(device(width), device(height), scale);
    if let Some(session) = state.session.borrow().as_ref() {
        session.resize(u32::from(desktop.width), u32::from(desktop.height), desktop.scale);
    }
}
