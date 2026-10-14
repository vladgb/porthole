//! Keyboard scancode tables, layouts, and mouse/scroll mapping.
//!
//! Keys are sent as scancodes, so the remote side applies its own keyboard layout, exactly like a
//! physical keyboard. On Linux, GDK hardware keycodes are evdev codes + 8 on both X11 and Wayland.

/// A key as RDP sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// PC/AT set 1 scancode; `extended` adds the E0 prefix.
    Scancode { code: u8, extended: bool },
    /// Pause/Break, which is a make-only E1 sequence and needs special handling.
    Pause,
}

/// Maps a GDK/X11 hardware keycode (evdev + 8) to an RDP key.
pub fn from_xkb_keycode(keycode: u32) -> Option<Key> {
    keycode.checked_sub(8).and_then(from_evdev)
}

/// Maps a Linux evdev key code (`KEY_*` in `linux/input-event-codes.h`) to an RDP key.
pub fn from_evdev(code: u32) -> Option<Key> {
    let plain = |code: u8| Some(Key::Scancode { code, extended: false });
    let ext = |code: u8| Some(Key::Scancode { code, extended: true });
    match code {
        // KEY_ESC (1) .. KEY_KPDOT (83) are numerically identical to set 1 scancodes.
        1..=83 => plain(code as u8),
        85 => plain(0x76),  // KEY_ZENKAKUHANKAKU
        86 => plain(0x56),  // KEY_102ND (ISO key between left Shift and Z)
        87 => plain(0x57),  // KEY_F11
        88 => plain(0x58),  // KEY_F12
        89 => plain(0x73),  // KEY_RO
        90 => plain(0x78),  // KEY_KATAKANA
        91 => plain(0x77),  // KEY_HIRAGANA
        92 => plain(0x79),  // KEY_HENKAN
        93 => plain(0x70),  // KEY_KATAKANAHIRAGANA
        94 => plain(0x7B),  // KEY_MUHENKAN
        95 => plain(0x5C),  // KEY_KPJPCOMMA
        96 => ext(0x1C),    // KEY_KPENTER
        97 => ext(0x1D),    // KEY_RIGHTCTRL
        98 => ext(0x35),    // KEY_KPSLASH
        99 => ext(0x37),    // KEY_SYSRQ (Print Screen)
        100 => ext(0x38),   // KEY_RIGHTALT (AltGr)
        102 => ext(0x47),   // KEY_HOME
        103 => ext(0x48),   // KEY_UP
        104 => ext(0x49),   // KEY_PAGEUP
        105 => ext(0x4B),   // KEY_LEFT
        106 => ext(0x4D),   // KEY_RIGHT
        107 => ext(0x4F),   // KEY_END
        108 => ext(0x50),   // KEY_DOWN
        109 => ext(0x51),   // KEY_PAGEDOWN
        110 => ext(0x52),   // KEY_INSERT
        111 => ext(0x53),   // KEY_DELETE
        113 => ext(0x20),   // KEY_MUTE
        114 => ext(0x2E),   // KEY_VOLUMEDOWN
        115 => ext(0x30),   // KEY_VOLUMEUP
        116 => ext(0x5E),   // KEY_POWER
        117 => plain(0x59), // KEY_KPEQUAL
        119 => Some(Key::Pause),
        121 => plain(0x7E),                            // KEY_KPCOMMA
        122 => plain(0x72),                            // KEY_HANGEUL
        123 => plain(0x71),                            // KEY_HANJA
        124 => plain(0x7D),                            // KEY_YEN
        125 => ext(0x5B),                              // KEY_LEFTMETA (Windows)
        126 => ext(0x5C),                              // KEY_RIGHTMETA
        127 => ext(0x5D),                              // KEY_COMPOSE (Menu)
        128 => ext(0x68),                              // KEY_STOP (browser stop)
        140 => ext(0x21),                              // KEY_CALC
        142 => ext(0x5F),                              // KEY_SLEEP
        143 => ext(0x63),                              // KEY_WAKEUP
        155 => ext(0x6C),                              // KEY_MAIL
        156 => ext(0x66),                              // KEY_BOOKMARKS
        157 => ext(0x6B),                              // KEY_COMPUTER
        158 => ext(0x6A),                              // KEY_BACK
        159 => ext(0x69),                              // KEY_FORWARD
        163 => ext(0x19),                              // KEY_NEXTSONG
        164 => ext(0x22),                              // KEY_PLAYPAUSE
        165 => ext(0x10),                              // KEY_PREVIOUSSONG
        166 => ext(0x24),                              // KEY_STOPCD
        172 => ext(0x32),                              // KEY_HOMEPAGE
        173 => ext(0x67),                              // KEY_REFRESH
        183..=193 => plain(0x64 + (code - 183) as u8), // KEY_F13..KEY_F23
        194 => plain(0x76),                            // KEY_F24
        217 => ext(0x65),                              // KEY_SEARCH
        226 => ext(0x6D),                              // KEY_MEDIA
        _ => None,
    }
}

/// Windows "one notch" of wheel rotation (WHEEL_DELTA).
pub const WHEEL_DELTA: i16 = 120;

/// Turns GTK scroll deltas into RDP wheel rotation units.
///
/// Mouse wheels report whole or fractional notches (high-resolution wheels report e.g. 0.25);
/// touchpads report surface pixels. Fractions accumulate until they amount to a whole unit, so
/// slow scrolls aren't lost and fast ones aren't rounded up.
#[derive(Debug, Default, Clone)]
pub struct ScrollAccumulator {
    pending: f64,
}

/// What a GTK scroll delta is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollUnit {
    /// Wheel notches (`gdk::ScrollUnit::Wheel`).
    Notches,
    /// Pixels on the surface (`gdk::ScrollUnit::Surface`, touchpads).
    Pixels,
}

/// Touchpad pixels that make one notch. Roughly matches Windows precision touchpad defaults.
const PIXELS_PER_NOTCH: f64 = 50.0;

impl ScrollAccumulator {
    /// Adds a GTK delta (positive = down/right) and returns rotation units to send, positive =
    /// away from the user / right as RDP expects, each within ±[`WHEEL_DELTA`].
    pub fn add(&mut self, delta: f64, unit: ScrollUnit) -> Vec<i16> {
        let notches = match unit {
            ScrollUnit::Notches => delta,
            ScrollUnit::Pixels => delta / PIXELS_PER_NOTCH,
        };
        if !notches.is_finite() {
            return Vec::new();
        }
        self.pending -= notches * f64::from(WHEEL_DELTA);
        let whole = self.pending.trunc();
        self.pending -= whole;
        let mut remaining = whole as i64;
        let mut out = Vec::new();
        while remaining != 0 {
            let step = remaining.clamp(-i64::from(WHEEL_DELTA), i64::from(WHEEL_DELTA));
            out.push(step as i16);
            remaining -= step;
        }
        out
    }

    /// Drops a partial notch, e.g. when the scroll direction changes or a gesture ends.
    pub fn reset(&mut self) {
        self.pending = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(code: u8, extended: bool) -> Option<Key> {
        Some(Key::Scancode { code, extended })
    }

    #[test]
    fn common_keys() {
        assert_eq!(from_evdev(1), sc(0x01, false)); // Esc
        assert_eq!(from_evdev(30), sc(0x1E, false)); // A
        assert_eq!(from_evdev(28), sc(0x1C, false)); // Enter
        assert_eq!(from_evdev(96), sc(0x1C, true)); // keypad Enter
        assert_eq!(from_evdev(29), sc(0x1D, false)); // left Ctrl
        assert_eq!(from_evdev(97), sc(0x1D, true)); // right Ctrl
        assert_eq!(from_evdev(100), sc(0x38, true)); // AltGr
        assert_eq!(from_evdev(125), sc(0x5B, true)); // left Windows
        assert_eq!(from_evdev(111), sc(0x53, true)); // Delete
        assert_eq!(from_evdev(83), sc(0x53, false)); // keypad .
        assert_eq!(from_evdev(86), sc(0x56, false)); // ISO <>
        assert_eq!(from_evdev(88), sc(0x58, false)); // F12
        assert_eq!(from_evdev(99), sc(0x37, true)); // Print Screen
        assert_eq!(from_evdev(119), Some(Key::Pause));
        assert_eq!(from_evdev(183), sc(0x64, false)); // F13
        assert_eq!(from_evdev(193), sc(0x6E, false)); // F23
        assert_eq!(from_evdev(0), None);
        assert_eq!(from_evdev(84), None);
        assert_eq!(from_evdev(0x2FF), None);
    }

    #[test]
    fn xkb_keycodes_are_offset_by_eight() {
        assert_eq!(from_xkb_keycode(38), from_evdev(30));
        assert_eq!(from_xkb_keycode(3), None);
    }

    #[test]
    fn no_two_keys_share_a_scancode() {
        let mut seen = std::collections::HashMap::new();
        for code in 0..=0x2FF {
            if let Some(key) = from_evdev(code)
                && let Some(previous) = seen.insert(key, code)
            {
                // F24 and Zenkaku/Hankaku share 0x76 on real hardware as well.
                assert_eq!(
                    (previous, code),
                    (85, 194),
                    "evdev {previous} and {code} both map to {key:?}"
                );
            }
        }
    }

    #[test]
    fn wheel_notches_scroll_one_delta_each_in_rdp_direction() {
        let mut acc = ScrollAccumulator::default();
        assert_eq!(acc.add(1.0, ScrollUnit::Notches), vec![-120]); // GTK down → RDP towards user
        assert_eq!(acc.add(-2.0, ScrollUnit::Notches), vec![120, 120]);
    }

    #[test]
    fn fractions_accumulate() {
        let mut acc = ScrollAccumulator::default();
        let total: i32 = (0..8)
            .flat_map(|_| acc.add(-0.125, ScrollUnit::Notches))
            .map(i32::from)
            .sum();
        assert_eq!(total, 120);
        let mut acc = ScrollAccumulator::default();
        assert!(acc.add(0.1 / PIXELS_PER_NOTCH, ScrollUnit::Pixels).is_empty());
        let total: i32 = (0..10)
            .flat_map(|_| acc.add(5.0, ScrollUnit::Pixels))
            .map(i32::from)
            .sum();
        assert_eq!(total, -120);
    }
}
