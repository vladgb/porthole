//! Text clipboard: remote copies land on the local clipboard; local text is offered to the remote.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use tracing::debug;

use super::State;

pub(super) fn attach(state: &State) {
    let clipboard = state.window.clipboard();
    let weak = Rc::downgrade(state);
    let handler = clipboard.connect_changed(move |_| {
        if let Some(state) = weak.upgrade() {
            share_local(&state);
        }
    });
    // The clipboard outlives the window.
    let handler = std::cell::Cell::new(Some(handler));
    state.window.connect_destroy(move |window| {
        if let Some(handler) = handler.take() {
            window.clipboard().disconnect(handler);
        }
    });

    let weak = Rc::downgrade(state);
    state.window.connect_is_active_notify(move |window| {
        // On Wayland, clipboard changes made elsewhere are only seen once we have focus again.
        if window.is_active()
            && let Some(state) = weak.upgrade()
        {
            share_local(&state);
        }
    });
}

/// Text the remote PC copied.
pub(super) fn set_remote(state: &State, text: String) {
    state.window.clipboard().set_text(&text);
    state.clipboard_from_remote.replace(Some(text));
}

/// Reads the local clipboard and offers its text to the remote PC.
pub(super) fn share_local(state: &State) {
    if !state.clipboard || !state.connected.get() {
        return;
    }
    let clipboard = state.window.clipboard();
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let text = read_text(&clipboard).await;
        let Some(state) = weak.upgrade() else { return };
        // Our own copy of the remote's text: the remote has it already.
        if text.is_some() && *state.clipboard_from_remote.borrow() == text {
            return;
        }
        if let Some(session) = state.session.borrow().as_ref() {
            session.set_clipboard_text(text);
        }
    });
}

async fn read_text(clipboard: &gdk::Clipboard) -> Option<String> {
    // Fails (harmlessly) when the clipboard holds no text, e.g. an image.
    match clipboard.read_text_future().await {
        Ok(text) => text.map(String::from),
        Err(e) => {
            debug!(error = %e, "Couldn't read the clipboard");
            None
        }
    }
}
