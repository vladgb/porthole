//! Session engine: connect, run loop, and events. UI-agnostic.
//!
//! ```no_run
//! # async fn demo(runtime: tokio::runtime::Handle) {
//! use rdesk_core::{Credentials, SessionConfig, SessionEvent};
//!
//! let config = SessionConfig::new("pc.example", Credentials::parse(r"CONTOSO\alice", "secret"));
//! let (session, events) = rdesk_core::connect(config, &runtime);
//! while let Ok(event) = events.recv().await {
//!     match event {
//!         SessionEvent::CertificatePrompt(prompt) => prompt.reject(),
//!         SessionEvent::FrameReady => session.framebuffer().take_damage(|_damage| { /* upload */ }),
//!         SessionEvent::Disconnected(reason) => println!("{reason}"),
//!         _ => {}
//!     }
//! }
//! # }
//! ```

mod audin;
mod camera;
pub(crate) mod clipboard;
mod clipboard_files;
mod config;
mod drive;
mod event;
mod framebuffer;
mod gfx;
mod h264;
mod kerberos;
mod session;
mod tls;

pub use audin::{CaptureFormat, Microphone};
pub use camera::{CameraBackend, CameraDevice, Cameras, FrameSink, VideoFormat, VideoMode};
pub use clipboard::{ClipboardContent, RemoteClipboard};
pub use clipboard_files::{FetchResult, RemoteFile};
pub use config::{AddressError, AudioOutput, Credentials, MonitorRect, Security, SessionConfig, parse_address};
pub use drive::SharedFolder;
pub use event::{
    AuthFailure, CertificateInfo, CertificateProblem, CertificatePrompt, DecodedPointer, DisconnectReason,
    PointerEvent, SessionEvent,
};
pub use framebuffer::{Damage, Framebuffer, Rect};
pub use session::{SessionHandle, connect};
pub use tls::fingerprint;

/// Input types accepted by [`SessionHandle::send_input`].
pub mod input {
    pub use ironrdp::input::{MouseButton, MousePosition, Operation, Scancode, WheelRotations};
}
