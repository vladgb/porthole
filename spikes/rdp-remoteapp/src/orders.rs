//! Windowing Alternate Secondary Drawing Orders (MS-RDPERP 2.2.1.3), which IronRDP's session
//! skips. They arrive in fast-path Orders updates and describe each remote window: title,
//! position, size and the region of it that's visible.

use std::collections::BTreeMap;

use tracing::{debug, info, warn};

use crate::wire::Reader;

const TS_SECONDARY: u8 = 0x02;
const TS_ALTSEC_WINDOW: u8 = 0x0B;

const TYPE_WINDOW: u32 = 0x0100_0000;
const TYPE_NOTIFY: u32 = 0x0200_0000;
const TYPE_DESKTOP: u32 = 0x0400_0000;
const STATE_NEW: u32 = 0x1000_0000;
const STATE_DELETED: u32 = 0x2000_0000;
const ICON: u32 = 0x4000_0000;
const CACHED_ICON: u32 = 0x8000_0000;

const FIELD_OWNER: u32 = 0x0000_0002;
const FIELD_TITLE: u32 = 0x0000_0004;
const FIELD_STYLE: u32 = 0x0000_0008;
const FIELD_SHOW: u32 = 0x0000_0010;
const FIELD_RESIZEMARGIN_X: u32 = 0x0000_0080;
const FIELD_WNDRECTS: u32 = 0x0000_0100;
const FIELD_VISIBILITY: u32 = 0x0000_0200;
const FIELD_WNDSIZE: u32 = 0x0000_0400;
const FIELD_WNDOFFSET: u32 = 0x0000_0800;
const FIELD_VISOFFSET: u32 = 0x0000_1000;
const FIELD_CLIENTAREAOFFSET: u32 = 0x0000_4000;
const FIELD_WNDCLIENTDELTA: u32 = 0x0000_8000;
const FIELD_CLIENTAREASIZE: u32 = 0x0001_0000;
const FIELD_RPCONTENT: u32 = 0x0002_0000;
const FIELD_ROOTPARENT: u32 = 0x0004_0000;
const FIELD_RESIZEMARGIN_Y: u32 = 0x0800_0000;

const DESKTOP_ZORDER: u32 = 0x0000_0010;
const DESKTOP_ACTIVEWND: u32 = 0x0000_0020;

const WS_EX_TOOLWINDOW: u32 = 0x0000_0080;
const WS_CHILD: u32 = 0x4000_0000;
const WS_POPUP: u32 = 0x8000_0000;

/// TS_RECTANGLE_16: right and bottom exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) left: u16,
    pub(crate) top: u16,
    pub(crate) right: u16,
    pub(crate) bottom: u16,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Window {
    pub(crate) owner: u32,
    pub(crate) style: u32,
    pub(crate) ex_style: u32,
    pub(crate) show: u8,
    pub(crate) title: String,
    pub(crate) offset: (i32, i32),
    pub(crate) size: (u32, u32),
    pub(crate) client_offset: (i32, i32),
    pub(crate) client_size: (u32, u32),
    pub(crate) root_parent: u32,
    pub(crate) window_rects: Vec<Rect>,
    pub(crate) visible_offset: (i32, i32),
    pub(crate) visible_rects: Vec<Rect>,
}

impl Window {
    /// What a client would turn this into locally.
    pub(crate) fn kind(&self) -> &'static str {
        if self.style & WS_CHILD != 0 {
            "child"
        } else if self.owner != 0 || self.style & WS_POPUP != 0 || self.ex_style & WS_EX_TOOLWINDOW != 0 {
            "popup/owned"
        } else {
            "toplevel"
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Windows {
    pub(crate) windows: BTreeMap<u32, Window>,
    pub(crate) active: Option<u32>,
    pub(crate) z_order: Vec<u32>,
    pub(crate) orders: usize,
    /// Reassembly buffer for fragmented fast-path Orders updates.
    fragment: Option<Vec<u8>>,
}

impl Windows {
    /// Feeds one fast-path Orders update (`fragmentation` per TS_FP_UPDATE).
    pub(crate) fn fast_path_orders(&mut self, data: &[u8], fragmentation: ironrdp::pdu::fast_path::Fragmentation) {
        use ironrdp::pdu::fast_path::Fragmentation;
        match fragmentation {
            Fragmentation::Single => self.parse_update(data),
            Fragmentation::First => self.fragment = Some(data.to_vec()),
            Fragmentation::Next => {
                if let Some(f) = self.fragment.as_mut() {
                    f.extend_from_slice(data);
                }
            }
            Fragmentation::Last => {
                if let Some(mut f) = self.fragment.take() {
                    f.extend_from_slice(data);
                    self.parse_update(&f);
                }
            }
        }
    }

    /// TS_FP_UPDATE_ORDERS: numberOrders, then the orders back to back.
    pub(crate) fn parse_update(&mut self, data: &[u8]) {
        let mut r = Reader::new(data);
        let Some(count) = r.u16() else { return };
        for i in 0..count {
            let order = r.rest();
            let Some(control) = r.u8() else { return };
            if control & 0x03 != TS_SECONDARY || control >> 2 != TS_ALTSEC_WINDOW {
                // Drawing orders have no common length field, so the rest can't be skipped.
                warn!(
                    control = format!("{control:#04x}"),
                    index = i,
                    count,
                    "Not a windowing order; skipping the rest of this update"
                );
                return;
            }
            let Some(size) = r.u16().map(usize::from) else { return };
            if size < 7 || size > order.len() {
                warn!(size, available = order.len(), "Windowing order with a bad size");
                return;
            }
            self.parse_window_order(&order[3..size]);
            self.orders += 1;
            r = Reader::new(&order[size..]);
        }
    }

    /// One order after its header byte and OrderSize: FieldsPresentFlags and the fields.
    fn parse_window_order(&mut self, body: &[u8]) {
        let mut r = Reader::new(body);
        let Some(flags) = r.u32() else { return };

        if flags & TYPE_DESKTOP != 0 {
            if flags & DESKTOP_ACTIVEWND != 0 {
                self.active = r.u32();
            }
            if flags & DESKTOP_ZORDER != 0 {
                // NumWindowIds is a single byte.
                let n = r.u8().unwrap_or(0);
                self.z_order = (0..n).map_while(|_| r.u32()).collect();
            }
            info!(flags = format!("{flags:#010x}"), active = ?self.active.map(hex), z_order = ?self.z_order.iter().copied().map(hex).collect::<Vec<_>>(), "Desktop");
            return;
        }

        let Some(id) = r.u32() else { return };
        if flags & TYPE_NOTIFY != 0 {
            debug!(window = hex(id), notify_icon = ?r.u32(), "Notification area icon (ignored)");
            return;
        }
        if flags & TYPE_WINDOW == 0 {
            warn!(flags = format!("{flags:#010x}"), "Windowing order of unknown type");
            return;
        }
        if flags & STATE_DELETED != 0 {
            let title = self.windows.remove(&id).map(|w| w.title).unwrap_or_default();
            info!(window = hex(id), %title, "Window deleted");
            return;
        }
        if flags & (ICON | CACHED_ICON) != 0 {
            debug!(
                window = hex(id),
                cached = flags & CACHED_ICON != 0,
                "Window icon (ignored)"
            );
            return;
        }

        let new = flags & STATE_NEW != 0;
        let w = self.windows.entry(id).or_default();
        if read_fields(&mut r, flags, w).is_none() {
            warn!(
                window = hex(id),
                flags = format!("{flags:#010x}"),
                "Window order ended early"
            );
        }
        info!(
            window = hex(id),
            event = if new { "new" } else { "update" },
            kind = w.kind(),
            title = %w.title,
            show = w.show,
            x = w.offset.0,
            y = w.offset.1,
            width = w.size.0,
            height = w.size.1,
            visible_rects = w.visible_rects.len(),
            flags = format!("{flags:#010x}"),
            "Window"
        );
    }
}

/// Fields of a New or Existing Window order (MS-RDPERP 2.2.1.3.1.2.1), in wire order.
fn read_fields(r: &mut Reader<'_>, flags: u32, w: &mut Window) -> Option<()> {
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
        w.client_offset = (r.i32()?, r.i32()?);
    }
    if has(FIELD_CLIENTAREASIZE) {
        w.client_size = (r.u32()?, r.u32()?);
    }
    if has(FIELD_RESIZEMARGIN_X) {
        r.take(8)?;
    }
    if has(FIELD_RESIZEMARGIN_Y) {
        r.take(8)?;
    }
    if has(FIELD_RPCONTENT) {
        r.u8()?;
    }
    if has(FIELD_ROOTPARENT) {
        w.root_parent = r.u32()?;
    }
    if has(FIELD_WNDOFFSET) {
        w.offset = (r.i32()?, r.i32()?);
    }
    if has(FIELD_WNDCLIENTDELTA) {
        r.take(8)?;
    }
    if has(FIELD_WNDSIZE) {
        w.size = (r.u32()?, r.u32()?);
    }
    if has(FIELD_WNDRECTS) {
        w.window_rects = rects(r)?;
    }
    if has(FIELD_VISOFFSET) {
        w.visible_offset = (r.i32()?, r.i32()?);
    }
    if has(FIELD_VISIBILITY) {
        w.visible_rects = rects(r)?;
    }
    // Overlay description, taskbar button, z-order enforcement and app bar fields follow;
    // the spike doesn't need them and OrderSize lets us skip them.
    Some(())
}

fn rects(r: &mut Reader<'_>) -> Option<Vec<Rect>> {
    let n = r.u16()?;
    (0..n)
        .map(|_| {
            Some(Rect {
                left: r.u16()?,
                top: r.u16()?,
                right: r.u16()?,
                bottom: r.u16()?,
            })
        })
        .collect()
}

fn hex(id: u32) -> String {
    format!("{id:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(flags: u32, fields: &[u8]) -> Vec<u8> {
        let size = u16::try_from(1 + 2 + 4 + fields.len()).unwrap();
        let mut v = vec![(TS_ALTSEC_WINDOW << 2) | TS_SECONDARY];
        v.extend_from_slice(&size.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(fields);
        v
    }

    fn update(orders: &[Vec<u8>]) -> Vec<u8> {
        let mut v = u16::try_from(orders.len()).unwrap().to_le_bytes().to_vec();
        for o in orders {
            v.extend_from_slice(o);
        }
        v
    }

    #[test]
    fn new_window_then_move_then_delete() {
        let mut f = 0x0003_0042u32.to_le_bytes().to_vec(); // window id
        f.extend_from_slice(&0x14CF_0000u32.to_le_bytes()); // style
        f.extend_from_slice(&0u32.to_le_bytes()); // ex style
        f.push(5); // SW_SHOW
        let title = crate::wire::utf16("Untitled - Notepad");
        f.extend_from_slice(&u16::try_from(title.len()).unwrap().to_le_bytes());
        f.extend_from_slice(&title);
        f.extend_from_slice(&100i32.to_le_bytes());
        f.extend_from_slice(&50i32.to_le_bytes());
        f.extend_from_slice(&800u32.to_le_bytes());
        f.extend_from_slice(&600u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        for v in [0u16, 0, 800, 600] {
            f.extend_from_slice(&v.to_le_bytes());
        }
        let new = order(
            TYPE_WINDOW
                | STATE_NEW
                | FIELD_STYLE
                | FIELD_SHOW
                | FIELD_TITLE
                | FIELD_WNDOFFSET
                | FIELD_WNDSIZE
                | FIELD_VISIBILITY,
            &f,
        );

        let mut m = 0x0003_0042u32.to_le_bytes().to_vec();
        m.extend_from_slice(&(-8i32).to_le_bytes());
        m.extend_from_slice(&20i32.to_le_bytes());
        let moved = order(TYPE_WINDOW | FIELD_WNDOFFSET, &m);

        let mut ws = Windows::default();
        ws.parse_update(&update(&[new, moved]));
        assert_eq!(ws.orders, 2);
        let w = &ws.windows[&0x0003_0042];
        assert_eq!(w.title, "Untitled - Notepad");
        assert_eq!(w.offset, (-8, 20));
        assert_eq!(w.size, (800, 600));
        assert_eq!(
            w.visible_rects,
            [Rect {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600
            }]
        );
        assert_eq!(w.kind(), "toplevel");

        let del = order(TYPE_WINDOW | STATE_DELETED, &0x0003_0042u32.to_le_bytes());
        ws.parse_update(&update(&[del]));
        assert!(ws.windows.is_empty());
    }

    #[test]
    fn desktop_order_and_garbage() {
        let mut f = 7u32.to_le_bytes().to_vec();
        f.push(2);
        f.extend_from_slice(&7u32.to_le_bytes());
        f.extend_from_slice(&9u32.to_le_bytes());
        let mut ws = Windows::default();
        ws.parse_update(&update(&[order(TYPE_DESKTOP | DESKTOP_ACTIVEWND | DESKTOP_ZORDER, &f)]));
        assert_eq!(ws.active, Some(7));
        assert_eq!(ws.z_order, [7, 9]);

        // A primary drawing order, then truncated input: no panic, nothing parsed.
        ws.parse_update(&[1, 0, 0x01, 0xFF]);
        ws.parse_update(&[2, 0, 0x2E, 0xFF, 0xFF]);
        assert_eq!(ws.orders, 1);
    }
}
