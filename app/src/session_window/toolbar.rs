//! Full screen: the header bar is hidden, and a small leave-full-screen button slides down when
//! the pointer touches the top edge near the right corner, hiding again once the pointer has left
//! it for a moment.

use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{self as gtk, glib};
use libadwaita as adw;

use super::State;

/// How long the pointer must stay away from the button before it hides.
const HIDE_DELAY: Duration = Duration::from_millis(700);
/// The pointer this close to the top edge...
const EDGE: f64 = 2.0;
/// ...and this close to the right edge reveals the button.
const HOT_CORNER: f64 = 24.0;
/// While shown, the button stays as long as the pointer is this close to the corner.
const KEEP_AREA: f64 = 64.0;

/// The leave-full-screen button in the top-right corner, shown only on demand. Its margin keeps
/// the very corner clickable for the remote desktop (a maximized window's close button).
pub(super) fn corner_button() -> (gtk::Revealer, gtk::Button) {
    let button = gtk::Button::builder()
        .icon_name("view-restore-symbolic")
        .tooltip_text("Leave Full Screen (Ctrl+Alt+Enter)")
        .focus_on_click(false)
        .css_classes(["osd", "circular"])
        .build();
    let revealer = gtk::Revealer::builder()
        .child(&button)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(8)
        .margin_end(8)
        .build();
    (revealer, button)
}

pub(super) fn attach(state: &State) {
    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(state);
    motion.connect_motion(move |_, x, y| {
        if let Some(state) = weak.upgrade() {
            pointer_moved(&state, x, y);
        }
    });
    let weak = Rc::downgrade(state);
    motion.connect_leave(move |_| {
        if let Some(state) = weak.upgrade() {
            schedule_hide(&state);
        }
    });
    state.window.add_controller(motion);
}

/// Full screen changed: the header bar goes away in full screen and comes back after.
pub(super) fn fullscreen_changed(state: &State) {
    let fullscreen = state.window.is_fullscreen();
    cancel_hide(state);
    state.toolbar.set_reveal_top_bars(!fullscreen);
    state.corner.set_reveal_child(false);
    if fullscreen {
        state.toasts.add_toast(
            adw::Toast::builder()
                .title("Move the pointer to the top-right corner or press Ctrl+Alt+Enter to leave full screen")
                .timeout(4)
                .build(),
        );
    }
}

/// The header bar's full-screen button and the corner button.
pub(super) fn toggle_fullscreen(state: &State) {
    if state.window.is_fullscreen() {
        state.window.unfullscreen();
    } else {
        state.window.fullscreen();
    }
}

fn pointer_moved(state: &State, x: f64, y: f64) {
    if !state.window.is_fullscreen() {
        return;
    }
    let from_right = f64::from(state.window.width()) - x;
    if y <= EDGE && from_right <= HOT_CORNER {
        cancel_hide(state);
        state.corner.set_reveal_child(true);
    } else if state.corner.reveals_child() {
        if y <= KEEP_AREA && from_right <= KEEP_AREA {
            cancel_hide(state);
        } else {
            schedule_hide(state);
        }
    }
}

fn schedule_hide(state: &State) {
    if !state.corner.reveals_child() || state.hide_timer.borrow().is_some() {
        return;
    }
    let weak = Rc::downgrade(state);
    let timer = glib::timeout_add_local_once(HIDE_DELAY, move || {
        let Some(state) = weak.upgrade() else { return };
        state.hide_timer.take();
        state.corner.set_reveal_child(false);
    });
    state.hide_timer.replace(Some(timer));
}

fn cancel_hide(state: &State) {
    if let Some(timer) = state.hide_timer.take() {
        timer.remove();
    }
}
