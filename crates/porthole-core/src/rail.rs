//! RemoteApp (MS-RDPERP): one program's windows instead of the whole desktop.
//!
//! The `rail` static channel carries the conversation (handshake, starting the program, focus,
//! window commands); the windows themselves are described by windowing orders in fast-path
//! Orders updates, which IronRDP skips, so `rail_orders` reads them off the wire.

use ironrdp::pdu::PduResult;
use ironrdp::pdu::gcc::ChannelName;
use ironrdp::svc::{ChannelFlags, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, info, warn};

use crate::event::SessionEvent;
use crate::rail_orders::Reader;

/// The program to start instead of the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteApp {
    /// A path, a name on the PC's `PATH` (`notepad.exe`), or `||alias` for a published program.
    pub program: String,
    pub arguments: String,
    /// Empty: the program's default.
    pub working_dir: String,
}

impl RemoteApp {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            arguments: String::new(),
            working_dir: String::new(),
        }
    }
}

/// What happened to the program and its windows.
#[derive(Debug, Clone)]
pub enum RemoteAppEvent {
    /// The PC started the program. With `window_images` (enhanced RemoteApp), each window's
    /// pixels come separately ([`crate::SessionHandle::window_image`]) instead of on the desktop.
    Started {
        window_images: bool,
    },
    /// The PC couldn't start the program; worded for people.
    StartFailed(String),
    /// A window appeared or changed: its whole current state.
    Window(RemoteWindow),
    WindowRemoved(u32),
    /// The window with the keyboard focus on the PC (0: none of the program's).
    ActiveWindow(u32),
    /// The windows from top to bottom, as stacked on the PC.
    ZOrder(Vec<u32>),
    /// The user started or finished dragging a window's title bar or edge (or chose Move or
    /// Size from its menu). The client moves or resizes the local window, then calls
    /// [`crate::SessionHandle::end_move_size`].
    MoveSize {
        window: u32,
        start: bool,
        kind: MoveSizeKind,
    },
}

/// A window of the remote program, in remote desktop pixels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteWindow {
    pub id: u32,
    /// The window that owns this one (a dialog's main window), 0 if none.
    pub owner: u32,
    pub style: u32,
    pub ex_style: u32,
    /// Win32 `SW_*` show state.
    pub show: u8,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// The invisible resize borders around the window (left, top, right, bottom). The rectangle
    /// above excludes them; [`crate::SessionHandle::move_window`] wants them included.
    pub resize_margins: (u32, u32, u32, u32),
}

const WS_CHILD: u32 = 0x4000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_EX_APPWINDOW: u32 = 0x0004_0000;

const SW_HIDE: u8 = 0;
const SW_SHOWMINIMIZED: u8 = 2;
const SW_SHOWMAXIMIZED: u8 = 3;
const SW_MINIMIZE: u8 = 6;
const SW_SHOWMINNOACTIVE: u8 = 7;

impl RemoteWindow {
    pub fn is_shown(&self) -> bool {
        self.show != SW_HIDE && self.width > 0 && self.height > 0
    }

    pub fn is_minimized(&self) -> bool {
        matches!(self.show, SW_SHOWMINIMIZED | SW_MINIMIZE | SW_SHOWMINNOACTIVE)
    }

    pub fn is_maximized(&self) -> bool {
        self.show == SW_SHOWMAXIMIZED
    }

    /// A menu, dropdown, tooltip or other shown window that isn't an app window.
    /// (Windows reports some of these, like a combo box's list, with `WS_CHILD`: they're
    /// children of the desktop.)
    pub fn is_popup(&self) -> bool {
        self.is_shown() && !self.is_app_window()
    }

    /// A main window or a dialog, as opposed to menus, tooltips and the shell's hidden helpers.
    /// Roughly the windows Windows gives a taskbar button, plus dialogs (which have a caption).
    pub fn is_app_window(&self) -> bool {
        if !self.is_shown() || self.style & WS_CHILD != 0 {
            return false;
        }
        self.style & WS_CAPTION == WS_CAPTION
            || self.ex_style & WS_EX_APPWINDOW != 0
            || (self.owner == 0 && self.ex_style & WS_EX_TOOLWINDOW == 0)
    }
}

/// Which edge is being dragged, or a move (`RAIL_WMSZ_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveSizeKind {
    Left,
    Right,
    Top,
    TopLeft,
    TopRight,
    Bottom,
    BottomLeft,
    BottomRight,
    Move,
    /// Move or size with the arrow keys (from the window menu).
    KeyMove,
    KeySize,
}

impl MoveSizeKind {
    fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            1 => Self::Left,
            2 => Self::Right,
            3 => Self::Top,
            4 => Self::TopLeft,
            5 => Self::TopRight,
            6 => Self::Bottom,
            7 => Self::BottomLeft,
            8 => Self::BottomRight,
            9 => Self::Move,
            10 => Self::KeyMove,
            11 => Self::KeySize,
            _ => return None,
        })
    }
}

/// Window menu commands (`SC_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCommand {
    Minimize,
    Maximize,
    Restore,
    Close,
}

impl WindowCommand {
    fn code(self) -> u16 {
        match self {
            Self::Minimize => 0xF020,
            Self::Maximize => 0xF030,
            Self::Close => 0xF060,
            Self::Restore => 0xF120,
        }
    }
}

/// Requests from the UI, sent on the `rail` channel.
#[derive(Debug, Clone)]
pub(crate) enum Request {
    Activate {
        window: u32,
        active: bool,
    },
    Command {
        window: u32,
        command: WindowCommand,
    },
    /// Client Window Move: the window's new rectangle, which also ends a local move or resize.
    Move {
        window: u32,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    },
}

const EXEC: u16 = 0x0001;
const ACTIVATE: u16 = 0x0002;
const SYSPARAM: u16 = 0x0003;
const SYSCOMMAND: u16 = 0x0004;
const HANDSHAKE: u16 = 0x0005;
const WINDOWMOVE: u16 = 0x0008;
const LOCALMOVESIZE: u16 = 0x0009;
const MINMAXINFO: u16 = 0x000A;
const CLIENTSTATUS: u16 = 0x000B;
const HANDSHAKE_EX: u16 = 0x0013;
const EXEC_RESULT: u16 = 0x0080;

/// TS_RAIL_ORDER_HANDSHAKEEX_FLAGS_HIDEF: enhanced RemoteApp.
const HANDSHAKEEX_FLAGS_HIDEF: u32 = 0x0001;

/// Build number in our Handshake PDU (the one FreeRDP sends).
const CLIENT_BUILD: u32 = 0x1DB0;
const CLIENTSTATUS_ALLOWLOCALMOVESIZE: u32 = 0x0001;

const SPI_SETMOUSEBUTTONSWAP: u32 = 0x0021;
const SPI_SETDRAGFULLWINDOWS: u32 = 0x0025;
const SPI_SETWORKAREA: u32 = 0x002F;
const SPI_SETHIGHCONTRAST: u32 = 0x0043;
const SPI_SETKEYBOARDPREF: u32 = 0x0045;
const SPI_SETKEYBOARDCUES: u32 = 0x100B;
const RAIL_SPI_TASKBARPOS: u32 = 0xF000;
const RAIL_SPI_DISPLAYCHANGE: u32 = 0xF001;

/// The `rail` static channel. Windows only uses it when it's announced with the options mstsc
/// uses, which the vendored connector adds when `enable_rail` is set.
#[derive(Debug)]
pub(crate) struct RailChannel {
    app: RemoteApp,
    width: u16,
    height: u16,
    events: async_channel::Sender<SessionEvent>,
    started: bool,
    /// The server agreed to enhanced RemoteApp (HandshakeEx with the HIDEF flag).
    window_images: bool,
}

impl RailChannel {
    pub(crate) fn new(app: RemoteApp, width: u16, height: u16, events: async_channel::Sender<SessionEvent>) -> Self {
        Self {
            app,
            width,
            height,
            events,
            started: false,
            window_images: false,
        }
    }

    pub(crate) fn request(&self, request: Request) -> Vec<SvcMessage> {
        if !self.started {
            debug!(?request, "RemoteApp request before the handshake; dropped");
            return Vec::new();
        }
        let msg = match request {
            Request::Activate { window, active } => {
                let mut body = window.to_le_bytes().to_vec();
                body.push(u8::from(active));
                pdu(ACTIVATE, &body)
            }
            Request::Command { window, command } => {
                let mut body = window.to_le_bytes().to_vec();
                body.extend_from_slice(&command.code().to_le_bytes());
                pdu(SYSCOMMAND, &body)
            }
            Request::Move {
                window,
                x,
                y,
                width,
                height,
            } => {
                let clamp = |v: i64| i16::try_from(v.clamp(i64::from(i16::MIN), i64::from(i16::MAX))).unwrap_or(0);
                let (x, y) = (i64::from(x), i64::from(y));
                let mut body = window.to_le_bytes().to_vec();
                for v in [x, y, x + i64::from(width), y + i64::from(height)] {
                    body.extend_from_slice(&clamp(v).to_le_bytes());
                }
                pdu(WINDOWMOVE, &body)
            }
        };
        vec![msg]
    }

    /// Our answer to the server's handshake: handshake, client status, system parameters, then
    /// the request to start the program (MS-RDPERP 1.3.2.1).
    fn start(&mut self) -> Vec<SvcMessage> {
        if self.started {
            return Vec::new();
        }
        self.started = true;

        let screen = rect16(0, 0, self.width, self.height);
        let mut high_contrast = 0x7Eu32.to_le_bytes().to_vec(); // as FreeRDP; empty scheme name
        high_contrast.extend_from_slice(&2u32.to_le_bytes());
        high_contrast.extend_from_slice(&[0, 0]);

        let exe = utf16(&self.app.program);
        let dir = utf16(&self.app.working_dir);
        let args = utf16(&self.app.arguments);
        // TS_RAIL_EXEC_FLAG_EXPAND_WORKINGDIRECTORY | TS_RAIL_EXEC_FLAG_EXPAND_ARGUMENTS
        let mut exec = 0x0009u16.to_le_bytes().to_vec();
        for s in [&exe, &dir, &args] {
            exec.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_le_bytes());
        }
        exec.extend_from_slice(&exe);
        exec.extend_from_slice(&dir);
        exec.extend_from_slice(&args);

        info!(program = %self.app.program, "Starting the program");
        vec![
            pdu(HANDSHAKE, &CLIENT_BUILD.to_le_bytes()),
            pdu(CLIENTSTATUS, &CLIENTSTATUS_ALLOWLOCALMOVESIZE.to_le_bytes()),
            sysparam(SPI_SETHIGHCONTRAST, &high_contrast),
            sysparam(SPI_SETMOUSEBUTTONSWAP, &[0]),
            sysparam(SPI_SETKEYBOARDPREF, &[0]),
            sysparam(SPI_SETDRAGFULLWINDOWS, &[1]),
            sysparam(SPI_SETKEYBOARDCUES, &[0]),
            sysparam(SPI_SETWORKAREA, &screen),
            sysparam(RAIL_SPI_DISPLAYCHANGE, &screen),
            // No taskbar of ours to describe: an empty strip along the bottom.
            sysparam(RAIL_SPI_TASKBARPOS, &rect16(0, self.height, self.width, self.height)),
            pdu(EXEC, &exec),
        ]
    }

    fn emit(&self, event: RemoteAppEvent) {
        let _ = self.events.try_send(SessionEvent::RemoteApp(event));
    }
}

impl SvcProcessor for RailChannel {
    fn channel_name(&self) -> ChannelName {
        ChannelName::from_static(b"rail\0\0\0\0")
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let mut r = Reader::new(payload);
        let (Some(order_type), Some(order_length)) = (r.u16(), r.u16()) else {
            warn!(len = payload.len(), "RemoteApp PDU too short");
            return Ok(Vec::new());
        };
        let body_len = usize::from(order_length).saturating_sub(4).min(r.rest().len());
        let mut b = Reader::new(&r.rest()[..body_len]);

        match order_type {
            HANDSHAKE | HANDSHAKE_EX => {
                let build = b.u32();
                let flags = if order_type == HANDSHAKE_EX {
                    b.u32().unwrap_or(0)
                } else {
                    0
                };
                self.window_images = flags & HANDSHAKEEX_FLAGS_HIDEF != 0;
                debug!(
                    order_type,
                    ?build,
                    flags = format_args!("{flags:#x}"),
                    "RemoteApp handshake"
                );
                return Ok(self.start());
            }
            EXEC_RESULT => {
                b.u16(); // flags
                let result = b.u16().unwrap_or(u16::MAX);
                let raw = b.u32().unwrap_or(0);
                if result == 0 {
                    info!(window_images = self.window_images, "The program started");
                    self.emit(RemoteAppEvent::Started {
                        window_images: self.window_images,
                    });
                } else {
                    warn!(result, raw = format_args!("{raw:#x}"), "The program didn't start");
                    self.emit(RemoteAppEvent::StartFailed(exec_failure(result, &self.app.program)));
                }
            }
            LOCALMOVESIZE => {
                let window = b.u32().unwrap_or(0);
                let start = b.u16().unwrap_or(0) != 0;
                let kind = b.u16().and_then(MoveSizeKind::from_u16);
                debug!(window = format_args!("{window:#x}"), start, ?kind, "Local move/size");
                if let Some(kind) = kind {
                    self.emit(RemoteAppEvent::MoveSize { window, start, kind });
                }
            }
            MINMAXINFO | SYSPARAM => {}
            other => debug!(
                order_type = format_args!("{other:#06x}"),
                body_len, "Unhandled RemoteApp PDU"
            ),
        }
        Ok(Vec::new())
    }
}

impl SvcClientProcessor for RailChannel {}

ironrdp::core::impl_as_any!(RailChannel);

fn exec_failure(result: u16, program: &str) -> String {
    match result {
        3 => format!(
            "The PC only allows programs that were published for Remote Desktop, and “{program}” isn't one of them."
        ),
        5 => format!("The PC couldn't find “{program}”."),
        7 => "The PC is locked.".to_owned(),
        _ => format!("The PC couldn't start “{program}” (error {result})."),
    }
}

/// TS_RAIL_PDU_HEADER + body. `Vec<u8>` is an `SvcEncode`, so it goes out as is.
fn pdu(order_type: u16, body: &[u8]) -> SvcMessage {
    let mut v = Vec::with_capacity(4 + body.len());
    v.extend_from_slice(&order_type.to_le_bytes());
    v.extend_from_slice(&u16::try_from(4 + body.len()).unwrap_or(u16::MAX).to_le_bytes());
    v.extend_from_slice(body);
    // The channel is announced with CHANNEL_OPTION_SHOW_PROTOCOL.
    SvcMessage::from(v).with_flags(ChannelFlags::SHOW_PROTOCOL)
}

fn sysparam(param: u32, value: &[u8]) -> SvcMessage {
    let mut body = param.to_le_bytes().to_vec();
    body.extend_from_slice(value);
    pdu(SYSPARAM, &body)
}

/// TS_RECTANGLE_16 (right and bottom exclusive).
fn rect16(left: u16, top: u16, right: u16, bottom: u16) -> Vec<u8> {
    [left, top, right, bottom]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

/// UTF-16LE without a terminator.
fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(style: u32, ex_style: u32, owner: u32) -> RemoteWindow {
        RemoteWindow {
            id: 1,
            owner,
            style,
            ex_style,
            show: 5,
            width: 100,
            height: 100,
            ..Default::default()
        }
    }

    #[test]
    fn tells_app_windows_from_menus_and_helpers() {
        // Notepad's main window, Character Map (a captionless dialog app), a Save As dialog, a
        // context menu, a tooltip, an owned popup, the hidden shell.
        assert!(window(0x14CF_0000, 0, 0).is_app_window());
        assert!(window(0x800A_0000, 0, 0).is_app_window());
        assert!(window(0x96C8_0000, 0x0001_0101, 7).is_app_window());
        assert!(!window(0x8400_0000, 0x0000_0181, 0).is_app_window());
        assert!(!window(0x8400_0000, 0x0000_0088, 7).is_app_window());
        assert!(!window(0x8400_0000, 0, 7).is_app_window());
        let hidden = RemoteWindow {
            show: 0,
            ..window(0x14CF_0000, 0, 0)
        };
        assert!(!hidden.is_app_window());
    }

    #[test]
    fn encodes_requests_after_the_handshake() {
        let (tx, _rx) = async_channel::unbounded();
        let mut channel = RailChannel::new(RemoteApp::new("notepad.exe"), 1920, 1080, tx);
        let close = Request::Command {
            window: 0x30042,
            command: WindowCommand::Close,
        };
        assert!(channel.request(close.clone()).is_empty());

        let handshake = [0x05, 0x00, 0x08, 0x00, 0xB0, 0x1D, 0x00, 0x00];
        let reply = channel.process(&handshake).unwrap();
        assert_eq!(reply.len(), 11);

        let bytes = channel.request(close)[0].encode_unframed_pdu().unwrap();
        assert_eq!(bytes, [0x04, 0x00, 0x0A, 0x00, 0x42, 0x00, 0x03, 0x00, 0x60, 0xF0]);

        let moved = channel.request(Request::Move {
            window: 1,
            x: -8,
            y: 10,
            width: 100,
            height: 50,
        });
        let bytes = moved[0].encode_unframed_pdu().unwrap();
        assert_eq!(&bytes[8..], [0xF8, 0xFF, 0x0A, 0x00, 0x5C, 0x00, 0x3C, 0x00]);
    }
}
