//! Key combinations the local desktop would otherwise intercept: a "Send Keys" menu, and system
//! shortcut inhibition in full screen (like mstsc's default) so Super, Alt+Tab etc. reach the PC.

use std::rc::Rc;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use rdesk_core::input::{Operation, Scancode};
use tracing::debug;

use super::State;

const CTRL: (u8, bool) = (0x1D, false);
const ALT: (u8, bool) = (0x38, false);
const SHIFT: (u8, bool) = (0x2A, false);
const WIN: (u8, bool) = (0x5B, true);

struct Combination {
    /// Action target.
    id: &'static str,
    label: &'static str,
    /// Pressed in order, released in reverse.
    keys: &'static [(u8, bool)],
}

const COMBINATIONS: &[Combination] = &[
    Combination {
        id: "ctrl-alt-del",
        label: "Ctrl+Alt+Del",
        keys: &[CTRL, ALT, (0x53, true)],
    },
    Combination {
        id: "ctrl-shift-esc",
        label: "Task Manager (Ctrl+Shift+Esc)",
        keys: &[CTRL, SHIFT, (0x01, false)],
    },
    Combination {
        id: "win",
        label: "Start Menu (Windows)",
        keys: &[WIN],
    },
    Combination {
        id: "alt-tab",
        label: "Switch Window (Alt+Tab)",
        keys: &[ALT, (0x0F, false)],
    },
    Combination {
        id: "alt-f4",
        label: "Close Window (Alt+F4)",
        keys: &[ALT, (0x3E, false)],
    },
    Combination {
        id: "win-l",
        label: "Lock (Windows+L)",
        keys: &[WIN, (0x26, false)],
    },
    Combination {
        id: "print",
        label: "Print Screen",
        keys: &[(0x37, true)],
    },
];

/// The "Send Keys" header bar button.
pub(super) fn menu_button() -> gtk::MenuButton {
    let menu = gio::Menu::new();
    for combination in COMBINATIONS {
        menu.append(
            Some(combination.label),
            Some(&format!("win.send-keys::{}", combination.id)),
        );
    }
    gtk::MenuButton::builder()
        .icon_name("input-keyboard-symbolic")
        .tooltip_text("Send Keys")
        .menu_model(&menu)
        .build()
}

pub(super) fn attach(state: &State) {
    let action = gio::SimpleAction::new("send-keys", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(state);
    action.connect_activate(move |_, target| {
        let (Some(state), Some(target)) = (weak.upgrade(), target.and_then(|t| t.str())) else {
            return;
        };
        if let Some(combination) = COMBINATIONS.iter().find(|c| c.id == target) {
            send_combination(&state, combination.keys);
        }
    });
    state.window.add_action(&action);
    sync_send_keys(state);

    let weak = Rc::downgrade(state);
    state.window.connect_fullscreened_notify(move |_| {
        if let Some(state) = weak.upgrade() {
            sync_shortcut_inhibition(&state);
        }
    });
}

/// Called when the connection state changes.
pub(super) fn connection_changed(state: &State) {
    sync_send_keys(state);
    sync_shortcut_inhibition(state);
}

fn sync_send_keys(state: &State) {
    if let Some(action) = state
        .window
        .lookup_action("send-keys")
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(state.connected.get());
    }
}

/// Inhibits the desktop's own shortcuts while connected in full screen. On Wayland the desktop
/// may ask the user to allow this once.
fn sync_shortcut_inhibition(state: &State) {
    let Some(toplevel) = state.window.surface().and_downcast::<gdk::Toplevel>() else {
        return;
    };
    let inhibit = state.connected.get() && state.window.is_fullscreen();
    if inhibit {
        debug!("Inhibiting system shortcuts");
        toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
    } else {
        toplevel.restore_system_shortcuts();
    }
}

fn send_combination(state: &State, keys: &[(u8, bool)]) {
    let scancode = |&(code, extended): &(u8, bool)| Scancode::from_u8(extended, code);
    let press = keys.iter().map(|k| Operation::KeyPressed(scancode(k)));
    let release = keys.iter().rev().map(|k| Operation::KeyReleased(scancode(k)));
    if let Some(session) = state.session.borrow().as_ref() {
        session.send_input(press.chain(release));
    }
    state.picture.grab_focus();
}
