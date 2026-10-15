//! Local keyboard layout → Windows keyboard layout id (KLID).
//!
//! Keys are sent as scancodes, so the remote PC must use the same layout as the local keyboard for
//! the right characters to appear. The layout is announced once, when connecting.

/// US English, used when the local layout is unknown.
pub const DEFAULT_LAYOUT: u32 = 0x0000_0409;

/// XKB layout descriptions (as GDK reports them, e.g. "German (no dead keys)") and their KLIDs.
const LAYOUTS: &[(&str, u32)] = &[
    ("English", 0x0000_0409),
    ("English (US)", 0x0000_0409),
    ("English (US, intl., with dead keys)", 0x0002_0409), // United States-International
    ("English (Dvorak)", 0x0001_0409),
    ("English (UK)", 0x0000_0809),
    ("Irish", 0x0000_1809),
    ("German", 0x0000_0407),
    ("German (Austria)", 0x0000_0407),
    ("German (Switzerland)", 0x0000_0807),
    ("French", 0x0000_040C),
    ("French (Switzerland)", 0x0000_100C),
    ("French (Canada)", 0x0000_1009),
    ("Belgian", 0x0000_080C),
    ("Spanish", 0x0000_040A),
    ("Spanish (Latin American)", 0x0000_080A),
    ("Italian", 0x0000_0410),
    ("Portuguese", 0x0000_0816),
    ("Portuguese (Brazil)", 0x0000_0416),
    ("Dutch", 0x0000_0413),
    ("Danish", 0x0000_0406),
    ("Norwegian", 0x0000_0414),
    ("Swedish", 0x0000_041D),
    ("Finnish", 0x0000_040B),
    ("Icelandic", 0x0000_040F),
    ("Estonian", 0x0000_0425),
    ("Polish", 0x0000_0415), // Polish (Programmers), which XKB's default matches
    ("Czech", 0x0000_0405),
    ("Slovak", 0x0000_041B),
    ("Hungarian", 0x0000_040E),
    ("Romanian", 0x0001_0418), // Romanian (Standard)
    ("Croatian", 0x0000_041A),
    ("Slovenian", 0x0000_0424),
    ("Serbian", 0x0000_0C1A),
    ("Serbian (Latin)", 0x0000_081A),
    ("Russian", 0x0000_0419),
    ("Ukrainian", 0x0000_0422),
    ("Greek", 0x0000_0408),
    ("Turkish", 0x0000_041F),
    ("Hebrew", 0x0000_040D),
    ("Arabic", 0x0000_0401),
    ("Japanese", 0x0000_0411),
    ("Korean", 0x0000_0412),
    ("Chinese", 0x0000_0804),
];

/// The KLID for an XKB layout description, or `None` if it isn't known.
///
/// Variants without their own Windows layout fall back to their base layout:
/// "English (UK, extended, Windows)" → "English (UK)", "German (no dead keys)" → "German".
pub fn klid_from_layout_name(name: &str) -> Option<u32> {
    let lookup = |name: &str| LAYOUTS.iter().find(|(n, _)| *n == name).map(|&(_, klid)| klid);
    let name = name.trim();
    lookup(name)
        .or_else(|| {
            let (head, _) = name.split_once(',')?;
            lookup(&format!("{head})"))
        })
        .or_else(|| lookup(name.split_once(" (")?.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_layouts_and_variants() {
        assert_eq!(klid_from_layout_name("English (US)"), Some(0x0409));
        assert_eq!(klid_from_layout_name("English (UK)"), Some(0x0809));
        assert_eq!(klid_from_layout_name("English (UK, extended, Windows)"), Some(0x0809));
        assert_eq!(klid_from_layout_name("English (Dvorak)"), Some(0x1_0409));
        assert_eq!(klid_from_layout_name("German (no dead keys)"), Some(0x0407));
        assert_eq!(klid_from_layout_name("German (Switzerland, legacy)"), Some(0x0807));
        assert_eq!(klid_from_layout_name("Klingon"), None);
    }

    #[test]
    fn layout_names_are_unique() {
        let mut names: Vec<_> = LAYOUTS.iter().map(|(n, _)| n).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), LAYOUTS.len());
    }
}
