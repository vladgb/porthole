//! The `rail` static virtual channel (MS-RDPERP 2.2.2): handshake, client status, system
//! parameters and the Execute request. Everything the server sends is logged; the spike only
//! answers what's needed to get the program started.

use ironrdp::pdu::PduResult;
use ironrdp::pdu::gcc::ChannelName;
use ironrdp::svc::{ChannelFlags, SvcClientProcessor, SvcMessage, SvcProcessor};
use tracing::{debug, info, warn};

use crate::wire::{Reader, utf16};

const EXEC: u16 = 0x0001;
const SYSPARAM: u16 = 0x0003;
const HANDSHAKE: u16 = 0x0005;
const LOCALMOVESIZE: u16 = 0x0009;
const MINMAXINFO: u16 = 0x000A;
const CLIENTSTATUS: u16 = 0x000B;
const LANGBARINFO: u16 = 0x000D;
const GET_APPID_RESP: u16 = 0x000F;
const TASKBARINFO: u16 = 0x0010;
const HANDSHAKE_EX: u16 = 0x0013;
const ZORDER_SYNC: u16 = 0x0014;
const CLOAK: u16 = 0x0015;
const POWER_DISPLAY_REQUEST: u16 = 0x0016;
const EXEC_RESULT: u16 = 0x0080;

/// Build number we report in our Handshake PDU (the one FreeRDP sends).
const CLIENT_BUILD: u32 = 0x1DB0;

const TS_RAIL_CLIENTSTATUS_ALLOWLOCALMOVESIZE: u32 = 0x0001;

const SPI_SETMOUSEBUTTONSWAP: u32 = 0x0021;
const SPI_SETDRAGFULLWINDOWS: u32 = 0x0025;
const SPI_SETWORKAREA: u32 = 0x002F;
const SPI_SETHIGHCONTRAST: u32 = 0x0043;
const SPI_SETKEYBOARDPREF: u32 = 0x0045;
const SPI_SETKEYBOARDCUES: u32 = 0x100B;
const RAIL_SPI_TASKBARPOS: u32 = 0xF000;
const RAIL_SPI_DISPLAYCHANGE: u32 = 0xF001;

#[derive(Debug)]
pub(crate) struct RailChannel {
    program: String,
    arguments: String,
    working_dir: String,
    width: u16,
    height: u16,
    started: bool,
}

impl RailChannel {
    pub(crate) fn new(program: String, arguments: String, working_dir: String, width: u16, height: u16) -> Self {
        Self {
            program,
            arguments,
            working_dir,
            width,
            height,
            started: false,
        }
    }

    /// Whether the server has opened the conversation (it only does once sign-in is complete).
    pub(crate) fn started(&self) -> bool {
        self.started
    }

    /// Our answer to the server's handshake: handshake, client status, system parameters, then
    /// the Execute request (MS-RDPERP 1.3.2.1).
    fn start_program(&mut self) -> Vec<SvcMessage> {
        if self.started {
            return Vec::new();
        }
        self.started = true;

        let screen = rect16(0, 0, self.width, self.height);
        let mut out = vec![
            pdu(HANDSHAKE, &CLIENT_BUILD.to_le_bytes()),
            pdu(CLIENTSTATUS, &TS_RAIL_CLIENTSTATUS_ALLOWLOCALMOVESIZE.to_le_bytes()),
        ];
        // TS_HIGHCONTRAST: HCF_AVAILABLE | HCF_HOTKEYAVAILABLE | ... as FreeRDP, empty scheme name.
        let mut high_contrast = 0x7Eu32.to_le_bytes().to_vec();
        high_contrast.extend_from_slice(&2u32.to_le_bytes());
        high_contrast.extend_from_slice(&[0, 0]);
        out.push(sysparam(SPI_SETHIGHCONTRAST, &high_contrast));
        out.push(sysparam(SPI_SETMOUSEBUTTONSWAP, &[0]));
        out.push(sysparam(SPI_SETKEYBOARDPREF, &[0]));
        out.push(sysparam(SPI_SETDRAGFULLWINDOWS, &[1]));
        out.push(sysparam(SPI_SETKEYBOARDCUES, &[0]));
        out.push(sysparam(SPI_SETWORKAREA, &screen));
        out.push(sysparam(RAIL_SPI_DISPLAYCHANGE, &screen));
        // No local taskbar to describe; an empty strip along the bottom edge.
        out.push(sysparam(
            RAIL_SPI_TASKBARPOS,
            &rect16(0, self.height, self.width, self.height),
        ));

        if self.program.is_empty() {
            // Only reattach: the server re-sends every window the session already has.
            info!("RAIL: sent handshake, client status and system parameters, no Execute");
            return out;
        }
        let exe = utf16(&self.program);
        let dir = utf16(&self.working_dir);
        let args = utf16(&self.arguments);
        let mut exec = Vec::new();
        // TS_RAIL_EXEC_FLAG_EXPAND_WORKINGDIRECTORY | TS_RAIL_EXEC_FLAG_EXPAND_ARGUMENTS
        exec.extend_from_slice(&0x0009u16.to_le_bytes());
        for s in [&exe, &dir, &args] {
            exec.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_le_bytes());
        }
        exec.extend_from_slice(&exe);
        exec.extend_from_slice(&dir);
        exec.extend_from_slice(&args);
        out.push(pdu(EXEC, &exec));

        info!(program = %self.program, arguments = %self.arguments, "RAIL: sent handshake, client status, system parameters and Execute");
        out
    }
}

impl SvcProcessor for RailChannel {
    fn channel_name(&self) -> ChannelName {
        ChannelName::from_utf8("rail").expect("valid channel name")
    }

    fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
        let mut r = Reader::new(payload);
        let (Some(order_type), Some(order_length)) = (r.u16(), r.u16()) else {
            warn!(len = payload.len(), "RAIL: PDU too short for a header");
            return Ok(Vec::new());
        };
        let body_len = usize::from(order_length).saturating_sub(4).min(r.rest().len());
        let mut b = Reader::new(&r.rest()[..body_len]);

        match order_type {
            HANDSHAKE => {
                info!(server_build = ?b.u32(), "RAIL: server Handshake");
                return Ok(self.start_program());
            }
            HANDSHAKE_EX => {
                info!(server_build = ?b.u32(), flags = ?b.u32().map(|f| format!("{f:#x}")), "RAIL: server HandshakeEx");
                return Ok(self.start_program());
            }
            EXEC_RESULT => {
                let flags = b.u16();
                let result = b.u16();
                let raw = b.u32();
                b.u16(); // padding
                let exe = b.u16().and_then(|n| b.utf16(usize::from(n)));
                let meaning = match result {
                    Some(0) => "OK",
                    Some(1) => "HOOK_NOT_LOADED",
                    Some(2) => "DECODE_FAILED",
                    Some(3) => "NOT_IN_ALLOWLIST (set fAllowUnlistedRemotePrograms=1 on the PC)",
                    Some(5) => "FILE_NOT_FOUND",
                    Some(6) => "FAIL",
                    Some(7) => "SESSION_LOCKED",
                    _ => "unknown",
                };
                if result == Some(0) {
                    info!(?exe, ?flags, "RAIL: Execute result {meaning}");
                } else {
                    warn!(?exe, ?flags, ?result, raw = ?raw.map(|r| format!("{r:#x}")), "RAIL: Execute result {meaning}");
                }
            }
            SYSPARAM => {
                info!(param = ?b.u32().map(|p| format!("{p:#x}")), value = ?b.u8(), "RAIL: server SysParam");
            }
            MINMAXINFO => {
                let window = b.u32();
                let v: Vec<_> = (0..8).map_while(|_| b.i16()).collect();
                info!(window = ?window.map(|w| format!("{w:#x}")), max_size = ?v.get(0..2), max_pos = ?v.get(2..4), min_track = ?v.get(4..6), max_track = ?v.get(6..8), "RAIL: MinMaxInfo");
            }
            LOCALMOVESIZE => {
                info!(window = ?b.u32().map(|w| format!("{w:#x}")), start = ?b.u16(), kind = ?b.u16(), x = ?b.i16(), y = ?b.i16(), "RAIL: LocalMoveSize");
            }
            ZORDER_SYNC => info!(marker = ?b.u32().map(|w| format!("{w:#x}")), "RAIL: Z-order sync"),
            CLOAK => info!(window = ?b.u32().map(|w| format!("{w:#x}")), cloaked = ?b.u8(), "RAIL: Cloak"),
            LANGBARINFO => info!(status = ?b.u32().map(|s| format!("{s:#x}")), "RAIL: Language bar info"),
            TASKBARINFO => info!("RAIL: Taskbar info"),
            GET_APPID_RESP => info!(window = ?b.u32().map(|w| format!("{w:#x}")), "RAIL: App ID response"),
            POWER_DISPLAY_REQUEST => info!(active = ?b.u32(), "RAIL: Power display request"),
            other => debug!(
                order_type = format!("{other:#06x}"),
                body_len, "RAIL: unhandled server PDU"
            ),
        }
        Ok(Vec::new())
    }
}

ironrdp::core::impl_as_any!(RailChannel);

impl SvcClientProcessor for RailChannel {}

/// TS_RAIL_PDU_HEADER + body. `Vec<u8>` is an `SvcEncode`, so it goes out as is.
fn pdu(order_type: u16, body: &[u8]) -> SvcMessage {
    let mut v = Vec::with_capacity(4 + body.len());
    v.extend_from_slice(&order_type.to_le_bytes());
    v.extend_from_slice(&u16::try_from(4 + body.len()).unwrap_or(u16::MAX).to_le_bytes());
    v.extend_from_slice(body);
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
