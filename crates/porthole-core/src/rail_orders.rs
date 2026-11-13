//! Windowing Alternate Secondary Drawing Orders (MS-RDPERP 2.2.1.3). IronRDP's session skips
//! fast-path Orders updates, so the session loop hands each fast-path frame here first.

use std::collections::HashMap;

use ironrdp::core::{ReadCursor, decode_cursor};
use ironrdp::pdu::fast_path::{FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode};
use tracing::{debug, trace, warn};

use crate::rail::{RemoteAppEvent, RemoteWindow};

const TS_SECONDARY: u8 = 0x02;
const TS_ALTSEC_WINDOW: u8 = 0x0B;

const TYPE_WINDOW: u32 = 0x0100_0000;
const TYPE_NOTIFY: u32 = 0x0200_0000;
const TYPE_DESKTOP: u32 = 0x0400_0000;
const STATE_DELETED: u32 = 0x2000_0000;
const ICON: u32 = 0x4000_0000;
const CACHED_ICON: u32 = 0x8000_0000;

const DESKTOP_ZORDER: u32 = 0x0000_0010;
const DESKTOP_ACTIVEWND: u32 = 0x0000_0020;

const FIELD_OWNER: u32 = 0x0000_0002;
const FIELD_TITLE: u32 = 0x0000_0004;
const FIELD_STYLE: u32 = 0x0000_0008;
const FIELD_SHOW: u32 = 0x0000_0010;
const FIELD_RESIZEMARGIN_X: u32 = 0x0000_0080;
const FIELD_WNDSIZE: u32 = 0x0000_0400;
const FIELD_WNDOFFSET: u32 = 0x0000_0800;
const FIELD_CLIENTAREAOFFSET: u32 = 0x0000_4000;
const FIELD_WNDCLIENTDELTA: u32 = 0x0000_8000;
const FIELD_CLIENTAREASIZE: u32 = 0x0001_0000;
const FIELD_RPCONTENT: u32 = 0x0002_0000;
const FIELD_ROOTPARENT: u32 = 0x0004_0000;
const FIELD_RESIZEMARGIN_Y: u32 = 0x0800_0000;

/// The windows the server has described, kept up to date from its orders.
#[derive(Debug, Default)]
pub(crate) struct WindowOrders {
    windows: HashMap<u32, RemoteWindow>,
    /// Reassembly buffer for fragmented Orders updates.
    fragment: Option<Vec<u8>>,
}

impl WindowOrders {
    /// Reads the windowing orders in a fast-path frame (the whole frame, header included).
    pub(crate) fn fast_path_frame(&mut self, frame: &[u8]) -> Vec<RemoteAppEvent> {
        let mut events = Vec::new();
        let mut cursor = ReadCursor::new(frame);
        if decode_cursor::<FastPathHeader>(&mut cursor).is_err() {
            return events;
        }
        while !cursor.remaining().is_empty() {
            let Ok(update) = decode_cursor::<FastPathUpdatePdu<'_>>(&mut cursor) else {
                break;
            };
            if update.update_code != UpdateCode::Orders {
                continue;
            }
            // Bulk compression is never negotiated (`compression_type: None`).
            if update.compression_flags.is_some_and(|f| !f.is_empty()) {
                warn!("Compressed Orders update ignored");
                continue;
            }
            match update.fragmentation {
                Fragmentation::Single => self.update(update.data, &mut events),
                Fragmentation::First => self.fragment = Some(update.data.to_vec()),
                Fragmentation::Next => {
                    if let Some(f) = self.fragment.as_mut() {
                        f.extend_from_slice(update.data);
                    }
                }
                Fragmentation::Last => {
                    if let Some(mut f) = self.fragment.take() {
                        f.extend_from_slice(update.data);
                        self.update(&f, &mut events);
                    }
                }
            }
        }
        events
    }

    /// TS_FP_UPDATE_ORDERS: numberOrders, then the orders back to back.
    fn update(&mut self, data: &[u8], events: &mut Vec<RemoteAppEvent>) {
        let mut r = Reader::new(data);
        let Some(count) = r.u16() else { return };
        for _ in 0..count {
            let order = r.rest();
            let Some(control) = r.u8() else { return };
            if control & 0x03 != TS_SECONDARY || control >> 2 != TS_ALTSEC_WINDOW {
                // Drawing orders have no common length field, so the rest can't be skipped.
                warn!(
                    control = format_args!("{control:#04x}"),
                    "Unexpected order; skipping the update"
                );
                return;
            }
            let Some(size) = r.u16().map(usize::from) else { return };
            if size < 7 || size > order.len() {
                warn!(size, available = order.len(), "Windowing order with a bad size");
                return;
            }
            self.order(&order[3..size], events);
            r = Reader::new(&order[size..]);
        }
    }

    /// One order after its header byte and OrderSize: FieldsPresentFlags, then the fields.
    fn order(&mut self, body: &[u8], events: &mut Vec<RemoteAppEvent>) {
        if let Some(event) = self.parse_order(body, events) {
            events.push(event);
        }
    }

    fn parse_order(&mut self, body: &[u8], events: &mut Vec<RemoteAppEvent>) -> Option<RemoteAppEvent> {
        let mut r = Reader::new(body);
        let flags = r.u32()?;
        if flags & TYPE_DESKTOP != 0 {
            if flags & DESKTOP_ACTIVEWND != 0 {
                // 0xFFFFFFFF: no window has the focus.
                let active = r.u32()?;
                let active = if active == u32::MAX { 0 } else { active };
                debug!(active = format_args!("{active:#x}"), "Active window on the PC");
                events.push(RemoteAppEvent::ActiveWindow(active));
            }
            if flags & DESKTOP_ZORDER != 0 {
                let count = r.u8()?;
                let order: Option<Vec<u32>> = (0..count).map(|_| r.u32()).collect();
                return Some(RemoteAppEvent::ZOrder(order?));
            }
            return None;
        }
        let id = r.u32()?;
        if flags & TYPE_NOTIFY != 0 || flags & TYPE_WINDOW == 0 {
            trace!(flags = format_args!("{flags:#010x}"), "Notification area order ignored");
            return None;
        }
        if flags & STATE_DELETED != 0 {
            return self.windows.remove(&id).map(|_| RemoteAppEvent::WindowRemoved(id));
        }
        if flags & (ICON | CACHED_ICON) != 0 {
            return None;
        }
        let window = self.windows.entry(id).or_insert_with(|| RemoteWindow {
            id,
            ..RemoteWindow::default()
        });
        if read_fields(&mut r, flags, window).is_none() {
            warn!(
                window = format_args!("{id:#x}"),
                flags = format_args!("{flags:#010x}"),
                "Window order ended early"
            );
        }
        debug!(
            window = format_args!("{id:#x}"),
            title = %window.title,
            show = window.show,
            rect = format_args!("{}x{}+{}+{}", window.width, window.height, window.x, window.y),
            style = format_args!("{:#x}/{:#x}", window.style, window.ex_style),
            margins = ?window.resize_margins,
            owner = format_args!("{:#x}", window.owner),
            app = window.is_app_window(),
            "Window"
        );
        Some(RemoteAppEvent::Window(window.clone()))
    }
}

/// Fields of a New or Existing Window order (MS-RDPERP 2.2.1.3.1.2.1), in wire order. Visible
/// regions and later fields aren't needed; OrderSize skips them.
fn read_fields(r: &mut Reader<'_>, flags: u32, w: &mut RemoteWindow) -> Option<()> {
    let has = |f: u32| flags & f != 0;
    if has(FIELD_OWNER) {
        w.owner = r.u32()?;
    }
    if has(FIELD_STYLE) {
        w.style = r.u32()?;
        w.ex_style = r.u32()?;
    }
    if has(FIELD_SHOW) {
        w.show = r.u8()?;
    }
    if has(FIELD_TITLE) {
        let n = usize::from(r.u16()?);
        w.title = r.utf16(n)?;
    }
    if has(FIELD_CLIENTAREAOFFSET) {
        r.take(8)?;
    }
    if has(FIELD_CLIENTAREASIZE) {
        r.take(8)?;
    }
    if has(FIELD_RESIZEMARGIN_X) {
        w.resize_margins.0 = r.u32()?;
        w.resize_margins.2 = r.u32()?;
    }
    if has(FIELD_RESIZEMARGIN_Y) {
        w.resize_margins.1 = r.u32()?;
        w.resize_margins.3 = r.u32()?;
    }
    if has(FIELD_RPCONTENT) {
        r.u8()?;
    }
    if has(FIELD_ROOTPARENT) {
        r.u32()?;
    }
    if has(FIELD_WNDOFFSET) {
        w.x = r.i32()?;
        w.y = r.i32()?;
    }
    if has(FIELD_WNDCLIENTDELTA) {
        r.take(8)?;
    }
    if has(FIELD_WNDSIZE) {
        w.width = r.u32()?;
        w.height = r.u32()?;
    }
    Some(())
}

/// Little-endian reader that returns `None` instead of panicking on short input.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.buf
    }

    pub(crate) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.buf.len() < n {
            return None;
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    pub(crate) fn u8(&mut self) -> Option<u8> {
        self.array::<1>().map(|b| b[0])
    }

    pub(crate) fn u16(&mut self) -> Option<u16> {
        self.array().map(u16::from_le_bytes)
    }

    pub(crate) fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    pub(crate) fn i32(&mut self) -> Option<i32> {
        self.array().map(i32::from_le_bytes)
    }

    /// `n` bytes of UTF-16LE.
    pub(crate) fn utf16(&mut self, n: usize) -> Option<String> {
        let units: Vec<u16> = self
            .take(n)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Some(String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE_NEW: u32 = 0x1000_0000;

    fn order(flags: u32, fields: &[u8]) -> Vec<u8> {
        let size = u16::try_from(1 + 2 + 4 + fields.len()).unwrap();
        let mut v = vec![(TS_ALTSEC_WINDOW << 2) | TS_SECONDARY];
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(fields);
        v
    }

    /// A single-fragment fast-path frame carrying one Orders update with these orders.
    fn frame(orders: &[Vec<u8>]) -> Vec<u8> {
        let mut data = u16::try_from(orders.len()).unwrap().to_le_bytes().to_vec();
        for o in orders {
            data.extend_from_slice(o);
        }
        let mut update = vec![0x00]; // Orders, single fragment, uncompressed
        update.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
        update.extend_from_slice(&data);
        let len = update.len() + 3;
        let mut frame = vec![
            0x00,
            0x80 | u8::try_from(len >> 8).unwrap(),
            u8::try_from(len & 0xFF).unwrap(),
        ];
        frame.extend_from_slice(&update);
        frame
    }

    fn new_window(id: u32, title: &str) -> Vec<u8> {
        let mut f = id.to_le_bytes().to_vec();
        f.extend_from_slice(&0x14CF_0000u32.to_le_bytes()); // style
        f.extend_from_slice(&0u32.to_le_bytes()); // ex style
        f.push(5); // SW_SHOW
        let title: Vec<u8> = title.encode_utf16().flat_map(u16::to_le_bytes).collect();
        f.extend_from_slice(&u16::try_from(title.len()).unwrap().to_le_bytes());
        f.extend_from_slice(&title);
        f.extend_from_slice(&100i32.to_le_bytes());
        f.extend_from_slice(&50i32.to_le_bytes());
        f.extend_from_slice(&800u32.to_le_bytes());
        f.extend_from_slice(&600u32.to_le_bytes());
        order(
            TYPE_WINDOW | STATE_NEW | FIELD_STYLE | FIELD_SHOW | FIELD_TITLE | FIELD_WNDOFFSET | FIELD_WNDSIZE,
            &f,
        )
    }

    #[test]
    fn tracks_windows_through_their_orders() {
        let mut orders = WindowOrders::default();
        let mut moved = 0x42u32.to_le_bytes().to_vec();
        moved.extend_from_slice(&(-8i32).to_le_bytes());
        moved.extend_from_slice(&20i32.to_le_bytes());
        let events = orders.fast_path_frame(&frame(&[
            new_window(0x42, "Untitled - Notepad"),
            order(TYPE_WINDOW | FIELD_WNDOFFSET, &moved),
            order(TYPE_WINDOW | ICON, &0x42u32.to_le_bytes()),
        ]));
        assert_eq!(events.len(), 2);
        let RemoteAppEvent::Window(w) = &events[1] else {
            panic!("{events:?}")
        };
        assert_eq!(w.title, "Untitled - Notepad");
        assert_eq!((w.x, w.y, w.width, w.height), (-8, 20, 800, 600));
        assert!(w.is_app_window());

        let events = orders.fast_path_frame(&frame(&[order(TYPE_WINDOW | STATE_DELETED, &0x42u32.to_le_bytes())]));
        assert!(matches!(events[..], [RemoteAppEvent::WindowRemoved(0x42)]));
    }

    #[test]
    fn survives_unexpected_input() {
        let mut orders = WindowOrders::default();
        // A desktop order, then a primary drawing order that stops the update.
        let mut fields = 7u32.to_le_bytes().to_vec();
        fields.push(2);
        fields.extend_from_slice(&9u32.to_le_bytes());
        fields.extend_from_slice(&7u32.to_le_bytes());
        let desktop = order(TYPE_DESKTOP | DESKTOP_ACTIVEWND | DESKTOP_ZORDER, &fields);
        let mut drawing = frame(&[desktop, vec![0x01, 0xFF]]);
        let events = orders.fast_path_frame(&drawing);
        assert!(matches!(events[0], RemoteAppEvent::ActiveWindow(7)));
        assert!(matches!(&events[1], RemoteAppEvent::ZOrder(z) if z == &[9, 7]));
        drawing.truncate(12);
        assert!(orders.fast_path_frame(&drawing).is_empty());
        assert!(orders.fast_path_frame(&[]).is_empty());
    }
}
