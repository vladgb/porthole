//! Clipboard: what the remote copies lands on the local clipboard; local text and images are
//! offered to the remote.

use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{gdk, glib};
use libadwaita as adw;
use rdesk_core::{ClipboardContent, RemoteClipboard};
use tracing::{debug, warn};

use super::{State, remote_files};

pub(super) fn attach(state: &State) {
    let clipboard = state.window.clipboard();
    let weak = Rc::downgrade(state);
    // `formats`, not `changed`: on X11 another app's formats arrive after the change.
    let handler = clipboard.connect_formats_notify(move |_| {
        let Some(state) = weak.upgrade() else { return };
        // Our own copy of what the remote sent: the remote has it already.
        let ours = state.clipboard_setting.take();
        state.clipboard_from_remote.set(ours);
        if !ours {
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

/// What the remote PC copied.
pub(super) fn set_remote(state: &State, remote: RemoteClipboard) {
    let clipboard = state.window.clipboard();
    match remote {
        RemoteClipboard::Text(text) => {
            state.clipboard_setting.set(true);
            clipboard.set_text(&text);
        }
        RemoteClipboard::Files(files) => {
            let count = files.iter().filter(|f| f.relative_path.is_none()).count();
            let weak = Rc::downgrade(state);
            let fetch: remote_files::Fetch = Rc::new(move || {
                let weak = weak.clone();
                Box::pin(async move {
                    let state = weak.upgrade().ok_or("the session window was closed")?;
                    let result = state
                        .session
                        .borrow()
                        .as_ref()
                        .map(|session| session.fetch_clipboard_files(remote_files::paste_dir()))
                        .ok_or("the session has ended")?;
                    let toast = adw::Toast::builder()
                        .title(format!("Copying {count} item(s) from the PC…"))
                        .timeout(0)
                        .build();
                    state.toasts.add_toast(toast.clone());
                    let outcome = result.await.unwrap_or_else(|_| Err("the session has ended".to_owned()));
                    toast.dismiss();
                    if let Err(e) = &outcome {
                        state
                            .toasts
                            .add_toast(adw::Toast::new(&format!("Couldn't copy the files: {e}")));
                    }
                    outcome
                })
            });
            state.clipboard_setting.set(true);
            if let Err(e) = clipboard.set_content(Some(&remote_files::RemoteFiles::new(fetch))) {
                warn!(error = %e, "Couldn't offer the PC's files");
            }
        }
        RemoteClipboard::Image(png) => match gdk::Texture::from_bytes(&glib::Bytes::from_owned(png)) {
            Ok(texture) => {
                state.clipboard_setting.set(true);
                clipboard.set_texture(&texture);
            }
            Err(e) => warn!(error = %e, "Couldn't load the remote clipboard image"),
        },
    }
}

/// Reads the local clipboard and offers it to the remote PC.
pub(super) fn share_local(state: &State) {
    if !state.clipboard || !state.connected.get() || state.clipboard_from_remote.get() {
        return;
    }
    let clipboard = state.window.clipboard();
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let content = read(&clipboard).await;
        let Some(state) = weak.upgrade() else { return };
        if let Some(session) = state.session.borrow().as_ref() {
            session.set_clipboard(content);
        }
    });
}

async fn read(clipboard: &gdk::Clipboard) -> ClipboardContent {
    let formats = clipboard.formats();
    let mut content = ClipboardContent::default();
    // On X11 the formats of a clipboard owned before we started aren't known yet; just try.
    let unknown = formats.mime_types().is_empty() && formats.types().is_empty();
    if unknown
        || formats.contain_mime_type("text/plain;charset=utf-8")
        || formats.contain_mime_type("text/plain")
        || formats.contains_type(glib::GString::static_type())
    {
        match clipboard.read_text_future().await {
            Ok(text) => content.text = text.map(String::from),
            Err(e) => debug!(error = %e, "Couldn't read clipboard text"),
        }
    }
    if unknown || formats.contains_type(gdk::FileList::static_type()) || formats.contain_mime_type("text/uri-list") {
        match clipboard
            .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
            .await
        {
            Ok(value) => {
                if let Ok(list) = value.get::<gdk::FileList>() {
                    content.files = list.files().iter().filter_map(|f| f.path()).collect();
                }
            }
            Err(e) => debug!(error = %e, "Couldn't read copied files"),
        }
    }
    if unknown || formats.contain_mime_type("image/png") {
        content.png = read_png(clipboard).await;
    } else if formats.contains_type(gdk::Texture::static_type()) {
        match clipboard.read_texture_future().await {
            Ok(Some(texture)) => content.png = Some(texture.save_to_png_bytes().to_vec()),
            Ok(None) => {}
            Err(e) => debug!(error = %e, "Couldn't read the clipboard image"),
        }
    }
    debug!(
        formats = %formats.to_str(),
        text = content.text.is_some(),
        png_bytes = content.png.as_ref().map(Vec::len),
        files = content.files.len(),
        "Local clipboard"
    );
    content
}

/// The clipboard's PNG as is, without decoding it.
async fn read_png(clipboard: &gdk::Clipboard) -> Option<Vec<u8>> {
    let (stream, _) = match clipboard.read_future(&["image/png"], glib::Priority::DEFAULT).await {
        Ok(read) => read,
        Err(e) => {
            debug!(error = %e, "Couldn't read the clipboard image");
            return None;
        }
    };
    let mut png = Vec::new();
    loop {
        match stream.read_bytes_future(64 * 1024, glib::Priority::DEFAULT).await {
            Ok(chunk) if chunk.is_empty() => return Some(png),
            Ok(chunk) => png.extend_from_slice(&chunk),
            Err(e) => {
                debug!(error = %e, "Couldn't read the clipboard image");
                return None;
            }
        }
    }
}
