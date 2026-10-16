//! The header bar in full screen: hidden, sliding down over the remote desktop when the pointer
//! touches the top edge, and hiding again once the pointer has left it for a moment.

use std::rc::Rc;
use std::time::Duration;

use gtk4::prelude::*;
use gtk4::{self as gtk, glib};
use libadwaita as adw;

use super::State;

/// How long the pointer must stay away from the bar before it hides.
const HIDE_DELAY: Duration = Duration::from_millis(700);
/// The pointer this close to the top edge reveals the bar.
const EDGE: f64 = 2.0;

pub(super) fn attach(state: &State) {
    // Watch the whole window: the bar itself must keep itself revealed while the pointer is on it.
    let motion = gtk::EventControllerMotion::new();
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(state);
    motion.connect_motion(move |_, _, y| {
        if let Some(state) = weak.upgrade() {
            pointer_moved(&state, y);
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

/// Full screen changed: in full screen the bar overlays the desktop (so showing it doesn't resize
/// the remote desktop) and starts hidden.
pub(super) fn fullscreen_changed(state: &State) {
    let fullscreen = state.window.is_fullscreen();
    cancel_hide(state);
    state.toolbar.set_extend_content_to_top_edge(fullscreen);
    state.toolbar.set_reveal_top_bars(!fullscreen);
    // Over the desktop a flat bar would be see-through.
    state.toolbar.set_top_bar_style(if fullscreen {
        adw::ToolbarStyle::Raised
    } else {
        adw::ToolbarStyle::Flat
    });
    state.fullscreen_button.set_icon_name(if fullscreen {
        "view-restore-symbolic"
    } else {
        "view-fullscreen-symbolic"
    });
    state.fullscreen_button.set_tooltip_text(Some(if fullscreen {
        "Leave Full Screen (Ctrl+Alt+Enter)"
    } else {
        "Full Screen (Ctrl+Alt+Enter)"
    }));
    if fullscreen {
        state.toasts.add_toast(
            adw::Toast::builder()
                .title("Move the pointer to the top edge or press Ctrl+Alt+Enter to leave full screen")
                .timeout(4)
                .build(),
        );
    }
}

/// The header bar's full-screen button.
pub(super) fn toggle_fullscreen(state: &State) {
    if state.window.is_fullscreen() {
        state.window.unfullscreen();
    } else {
        state.window.fullscreen();
    }
}

fn pointer_moved(state: &State, y: f64) {
    if !state.window.is_fullscreen() {
        return;
    }
    let revealed = state.toolbar.reveals_top_bars();
    if y <= EDGE {
        cancel_hide(state);
        state.toolbar.set_reveal_top_bars(true);
    } else if revealed {
        // Keep the bar while the pointer is on it (or on a menu it opened); hide once below it.
        let bar = f64::from(state.toolbar.top_bar_height());
        if y <= bar || state.toolbar_menu_open.get() {
            cancel_hide(state);
        } else {
            schedule_hide(state);
        }
    }
}

fn schedule_hide(state: &State) {
    if !state.window.is_fullscreen() || state.hide_timer.borrow().is_some() || state.toolbar_menu_open.get() {
        return;
    }
    let weak = Rc::downgrade(state);
    let timer = glib::timeout_add_local_once(HIDE_DELAY, move || {
        let Some(state) = weak.upgrade() else { return };
        state.hide_timer.take();
        if state.window.is_fullscreen() && !state.toolbar_menu_open.get() {
            state.toolbar.set_reveal_top_bars(false);
            // Give the keyboard back to the remote desktop.
            state.picture.grab_focus();
        }
    });
    state.hide_timer.replace(Some(timer));
}

fn cancel_hide(state: &State) {
    if let Some(timer) = state.hide_timer.take() {
        timer.remove();
    }
}
