//! Files the PC copied, on the local clipboard: offered as file URIs, downloaded only when an app
//! actually pastes them.

use std::cell::RefCell;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use gtk4::prelude::*;
use gtk4::subclass::prelude::*;
use gtk4::{gdk, gio, glib};
use tracing::{debug, warn};

/// Downloads the files and resolves to the local top-level items.
pub(super) type Fetch = Rc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<Vec<PathBuf>, String>>>>>;

const URI_LIST: &str = "text/uri-list";
const GNOME_COPIED_FILES: &str = "x-special/gnome-copied-files";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RemoteFiles {
        pub(super) fetch: RefCell<Option<Fetch>>,
        /// The download, shared by every format request (started once).
        pub(super) result: RefCell<Option<futures_shared::Shared>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RemoteFiles {
        const NAME: &'static str = "RdeskRemoteFiles";
        type Type = super::RemoteFiles;
        type ParentType = gdk::ContentProvider;
    }

    impl ObjectImpl for RemoteFiles {}

    impl ContentProviderImpl for RemoteFiles {
        fn formats(&self) -> gdk::ContentFormats {
            // Other apps get URIs; GTK turns them into a GdkFileList for apps that ask for one.
            gdk::ContentFormatsBuilder::new()
                .add_mime_type(URI_LIST)
                .add_mime_type(GNOME_COPIED_FILES)
                .build()
        }

        fn write_mime_type_future(
            &self,
            mime_type: &str,
            stream: &gio::OutputStream,
            io_priority: glib::Priority,
        ) -> Pin<Box<dyn Future<Output = Result<(), glib::Error>> + 'static>> {
            let mime_type = mime_type.to_owned();
            let stream = stream.clone();
            let files = self.download();
            debug!(%mime_type, "Pasting the PC's files");
            Box::pin(async move {
                let paths = files
                    .await
                    .map_err(|e| glib::Error::new(gio::IOErrorEnum::Failed, &e))?;
                debug!(count = paths.len(), "Handing over the PC's files");
                let uris: Vec<String> = paths.iter().map(|p| gio::File::for_path(p).uri().to_string()).collect();
                let text = match mime_type.as_str() {
                    URI_LIST => uris.iter().map(|u| format!("{u}\r\n")).collect::<String>(),
                    GNOME_COPIED_FILES => std::iter::once("copy".to_owned())
                        .chain(uris)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    other => {
                        return Err(glib::Error::new(
                            gio::IOErrorEnum::NotSupported,
                            &format!("{other} isn't offered"),
                        ));
                    }
                };
                let written = stream.write_all_future(text.into_bytes(), io_priority).await;
                debug!(ok = written.is_ok(), "Wrote the PC's files");
                written.map(|_| ()).map_err(|(_, e)| e)
            })
        }
    }

    impl RemoteFiles {
        fn download(&self) -> futures_shared::Shared {
            let mut result = self.result.borrow_mut();
            if let Some(shared) = &*result {
                return shared.clone();
            }
            let fetch = self.fetch.borrow().clone();
            let shared = futures_shared::Shared::new(async move {
                match fetch {
                    Some(fetch) => fetch().await,
                    None => Err("nothing to paste".to_owned()),
                }
            });
            *result = Some(shared.clone());
            shared
        }
    }
}

glib::wrapper! {
    pub struct RemoteFiles(ObjectSubclass<imp::RemoteFiles>) @extends gdk::ContentProvider;
}

impl RemoteFiles {
    pub(super) fn new(fetch: Fetch) -> Self {
        let provider: Self = glib::Object::new();
        provider.imp().fetch.replace(Some(fetch));
        provider
    }
}

/// A fresh folder for one paste, under `$XDG_CACHE_HOME/rdesk/clipboard`. Pastes older than a
/// day are removed (the pasting app has copied them by then).
pub(super) fn paste_dir() -> PathBuf {
    let base = glib::user_cache_dir().join("rdesk").join("clipboard");
    if let Ok(entries) = std::fs::read_dir(&base) {
        let day_ago = SystemTime::now() - Duration::from_secs(24 * 60 * 60);
        for entry in entries.flatten() {
            let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < day_ago);
            if old && let Err(e) = std::fs::remove_dir_all(entry.path()) {
                debug!(path = %entry.path().display(), error = %e, "Couldn't remove an old paste");
            }
        }
    }
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = base.join(format!("{stamp}"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(dir = %dir.display(), error = %e, "Couldn't create the paste folder");
    }
    dir
}

/// A future that several awaiters can share, resolving once (single-threaded).
mod futures_shared {
    use std::cell::RefCell;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::task::{Context, Poll, Waker};

    type Output = Result<Vec<PathBuf>, String>;

    struct Inner {
        future: Option<Pin<Box<dyn Future<Output = Output>>>>,
        output: Option<Output>,
        wakers: Vec<Waker>,
    }

    #[derive(Clone)]
    pub struct Shared(Rc<RefCell<Inner>>);

    impl Shared {
        pub fn new(future: impl Future<Output = Output> + 'static) -> Self {
            Self(Rc::new(RefCell::new(Inner {
                future: Some(Box::pin(future)),
                output: None,
                wakers: Vec::new(),
            })))
        }
    }

    impl Future for Shared {
        type Output = Output;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Output> {
            let mut inner = self.0.borrow_mut();
            if let Some(output) = &inner.output {
                return Poll::Ready(output.clone());
            }
            let polled = inner.future.as_mut().map(|future| future.as_mut().poll(cx));
            match polled {
                Some(Poll::Ready(output)) => {
                    inner.future = None;
                    inner.output = Some(output.clone());
                    for waker in inner.wakers.drain(..) {
                        waker.wake();
                    }
                    Poll::Ready(output)
                }
                Some(Poll::Pending) | None => {
                    inner.wakers.push(cx.waker().clone());
                    Poll::Pending
                }
            }
        }
    }
}
