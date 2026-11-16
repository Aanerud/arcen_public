// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The host side of the `host_keymap` notice: what a Pier's session layout
//! types at each printable key position, told to a Deck that types committed
//! text, so its Automatic typing sends as text exactly the keystrokes the
//! host would type differently.
//!
//! Every Pier builds the notice here. A platform adapter only reads its
//! operating system — XKB/compositor keymaps on Linux, the foreground
//! thread's keyboard layout (`ToUnicodeEx`) on Windows, the current input
//! source (`UCKeyTranslate`) on macOS — and answers, per Qt key position,
//! the characters typed unmodified and with Shift:
//!
//! - [`PRINTABLE_QT_KEYS`]: the positions a notice describes.
//! - [`host_keymap_msg`]: the notice, from the adapter's answers; every
//!   answer passes [`key_text`], so a dead key, a control character or an
//!   implausibly long result is "no character", never sent.
//! - [`HostKeymapNotices`]: what to send when — the first notice, then one
//!   per change, each with a new generation.
//! - [`windows_hkl_layout`], [`windows_layout_name`], [`macos_layout_name`]:
//!   a Windows keyboard layout handle and identifier (KLID) or a macOS input
//!   source identifier as the XKB-style layout name the notice carries
//!   (`no`, `de(mac)`).

use arcen_protocol::messages::{HostKeyCharsMsg, HostKeymapMsg, MAX_HOST_KEYMAP_KEYS};

/// `HostKeymapMsg::source` of a Windows Pier: the foreground window's
/// keyboard layout.
pub const WINDOWS_KEYBOARD_LAYOUT_SOURCE: &str = "windows_keyboard_layout";
/// `HostKeymapMsg::source` of a macOS Pier: the current keyboard input
/// source.
pub const MACOS_INPUT_SOURCE: &str = "macos_input_source";

/// The Qt key positions a Deck sends for printable keys, the positions a
/// [`HostKeymapMsg`] describes: space, the US punctuation positions, digits
/// and letters.
pub const PRINTABLE_QT_KEYS: &[u32] = &[
    0x20, 0x27, 0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3B, 0x3D, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E,
    0x4F, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x5B, 0x5C, 0x5D, 0x60,
];

/// Most characters one key level may type (a layout's ligature key types a
/// few; anything longer is not a key's character).
pub const MAX_KEY_TEXT_CHARS: usize = 4;

/// The characters one key level types, if they are characters a Deck can
/// compare: non-empty, at most [`MAX_KEY_TEXT_CHARS`], and no control
/// character (C0, DEL, C1). Space is a character.
#[must_use]
pub fn key_text(text: &str) -> Option<String> {
    let count = text.chars().count();
    if count == 0 || count > MAX_KEY_TEXT_CHARS || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.to_owned())
}

/// [`key_text`] of UTF-16 units as an operating system returned them
/// (`ToUnicodeEx`, `UCKeyTranslate`); unpaired surrogates are no character.
#[must_use]
pub fn key_text_utf16(units: &[u16]) -> Option<String> {
    key_text(&String::from_utf16(units).ok()?)
}

/// What a host adapter read for one key position: the characters typed
/// unmodified and with Shift (`None`: a dead key or no character).
pub type PositionText = (Option<String>, Option<String>);

/// The notice for the active layout. `chars_at` answers per Qt key position
/// of [`PRINTABLE_QT_KEYS`] (`None`: the layout does not describe that
/// position). Generation 0: [`HostKeymapNotices::offer`] numbers notices.
#[must_use]
pub fn host_keymap_msg(
    source: &str,
    layouts: Vec<String>,
    active_layout: Option<String>,
    mut chars_at: impl FnMut(u32) -> Option<PositionText>,
) -> HostKeymapMsg {
    let keys = PRINTABLE_QT_KEYS
        .iter()
        .filter_map(|&qt_key| {
            let (base, shift) = chars_at(qt_key)?;
            Some(HostKeyCharsMsg {
                qt_key,
                base: base.as_deref().and_then(key_text),
                shift: shift.as_deref().and_then(key_text),
            })
        })
        .take(MAX_HOST_KEYMAP_KEYS)
        .collect();
    HostKeymapMsg {
        source: source.to_owned(),
        layouts,
        active_layout,
        keys,
        ..HostKeymapMsg::default()
    }
}

/// Which `host_keymap` notices a session sends: the first one, then one
/// whenever what the layout types (or its names) changed, each with the
/// next generation. A host reads its layout as often as it likes and offers
/// every reading here.
#[derive(Debug, Clone, Default)]
pub struct HostKeymapNotices {
    last: Option<HostKeymapMsg>,
    generation: u64,
}

impl HostKeymapNotices {
    /// The notice to send for this reading (its generation set), or `None`
    /// when it says what the last one said.
    pub fn offer(&mut self, mut notice: HostKeymapMsg) -> Option<HostKeymapMsg> {
        notice.generation = 0;
        if self.last.as_ref() == Some(&notice) {
            return None;
        }
        self.last = Some(notice.clone());
        self.generation += 1;
        notice.generation = self.generation;
        Some(notice)
    }

    /// The generation of the last notice offered (0: none yet).
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// The longest platform identifier a fallback layout name carries.
const MAX_PLATFORM_LAYOUT_ID: usize = 96;

/// A Windows keyboard layout identifier (KLID, `GetKeyboardLayoutNameW`:
/// layout variant in the high word, language in the low word) as an XKB
/// layout name; an unknown one is `windows:XXXXXXXX`.
#[must_use]
pub fn windows_layout_name(klid: u32) -> String {
    let name = match klid {
        0x0000_0401 => "ara",
        0x0000_0402 => "bg",
        0x0000_0404 => "tw",
        0x0000_0405 => "cz",
        0x0000_0406 => "dk",
        0x0000_0407 => "de",
        0x0000_0408 => "gr",
        0x0000_0409 => "us",
        0x0000_040A => "es",
        0x0000_040B => "fi",
        0x0000_040C => "fr",
        0x0000_040D => "il",
        0x0000_040E => "hu",
        0x0000_040F => "is",
        0x0000_0410 => "it",
        0x0000_0411 => "jp",
        0x0000_0412 => "kr",
        0x0000_0413 => "nl",
        0x0000_0414 => "no",
        0x0000_0415 => "pl",
        0x0000_0416 => "br",
        0x0000_0418 => "ro",
        0x0000_0419 => "ru",
        0x0000_041A => "hr",
        0x0000_041B => "sk",
        0x0000_041D => "se",
        0x0000_041F => "tr",
        0x0000_0422 => "ua",
        0x0000_0424 => "si",
        0x0000_0425 => "ee",
        0x0000_0426 => "lv",
        0x0000_0427 => "lt",
        0x0000_042F => "mk",
        0x0000_0452 => "gb(extd)",
        0x0000_0804 => "cn",
        0x0000_0807 => "ch",
        0x0000_0809 => "gb",
        0x0000_080A => "latam",
        0x0000_080C | 0x0000_0813 => "be",
        0x0000_0816 => "pt",
        0x0000_1009 => "ca",
        0x0000_100C => "ch(fr)",
        0x0000_1809 => "ie",
        0x0001_0409 => "us(dvorak)",
        0x0001_0415 => "pl(qwertz)",
        0x0002_0409 => "us(intl)",
        _ => return format!("windows:{klid:08X}"),
    };
    name.to_owned()
}

/// The keyboard layout a Windows layout handle (`HKL`) names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsHklLayout {
    /// The layout's identifier (KLID): a language's default layout (the
    /// handle's high word), or an input method's handle itself.
    Klid(u32),
    /// A layout variant (high word `0xFnnn`): the KLID whose registry value
    /// `Layout Id` is this number (`Keyboard Layouts\<KLID>`).
    Variant(u16),
}

/// What a keyboard layout handle (`HKL`, only its low 32 bits are
/// significant) names.
#[must_use]
pub const fn windows_hkl_layout(hkl: u64) -> WindowsHklLayout {
    let device = ((hkl >> 16) & 0xFFFF) as u16;
    match device & 0xF000 {
        0xF000 => WindowsHklLayout::Variant(device & 0x0FFF),
        0xE000 => WindowsHklLayout::Klid((hkl & 0xFFFF_FFFF) as u32),
        _ => WindowsHklLayout::Klid(device as u32),
    }
}

/// A registry `Layout Id` value (four hex digits, `0002`).
#[must_use]
pub fn parse_layout_id(text: &str) -> Option<u16> {
    let text = text.trim_end_matches('\0');
    if text.is_empty() || text.len() > 4 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(text, 16).ok()
}

/// A KLID as `GetKeyboardLayoutNameW` writes it (eight hex digits).
#[must_use]
pub fn parse_klid(text: &str) -> Option<u32> {
    let text = text.trim_end_matches('\0');
    if text.len() != 8 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(text, 16).ok()
}

/// A macOS input source identifier (`kTISPropertyInputSourceID`, such as
/// `com.apple.keylayout.Norwegian`) as an XKB layout name; an unknown one
/// is `macos:<identifier>` (printable ASCII only, bounded).
#[must_use]
pub fn macos_layout_name(input_source_id: &str) -> String {
    let name = input_source_id
        .strip_prefix("com.apple.keylayout.")
        .and_then(|layout| {
            Some(match layout {
                "US" | "ABC" => "us(mac)",
                "USInternational-PC" => "us(intl)",
                "Dvorak" => "us(dvorak-mac)",
                "British" => "gb(mac)",
                "British-PC" => "gb",
                "Irish" => "ie",
                "Canadian" => "ca",
                "Norwegian" | "NorwegianExtended" => "no(mac)",
                "Swedish" | "Swedish-Pro" => "se(mac)",
                "Danish" => "dk(mac)",
                "Finnish" | "FinnishExtended" => "fi(mac)",
                "Icelandic" => "is(mac)",
                "German" => "de(mac)",
                "Austrian" => "at(mac)",
                "SwissGerman" => "ch(de_mac)",
                "SwissFrench" => "ch(fr_mac)",
                "French" => "fr(mac)",
                "French-PC" => "fr",
                "Belgian" => "be",
                "Dutch" => "nl(mac)",
                "Spanish" | "Spanish-ISO" => "es(mac)",
                "Portuguese" => "pt(mac)",
                "Brazilian" | "Brazilian-ABNT2" => "br",
                "Italian" | "Italian-Pro" => "it(mac)",
                "Polish" | "PolishPro" => "pl",
                "Czech" | "Czech-QWERTY" => "cz",
                "Hungarian" => "hu",
                "Russian" | "Russian-Phonetic" => "ru(mac)",
                "Ukrainian" | "Ukrainian-PC" => "ua",
                "Turkish" | "Turkish-QWERTY-PC" => "tr",
                "Greek" => "gr",
                "Estonian" => "ee",
                "Latvian" => "lv",
                "Lithuanian" => "lt",
                _ => return None,
            })
        });
    name.map_or_else(
        || {
            let identifier: String = input_source_id
                .chars()
                .filter(char::is_ascii_graphic)
                .take(MAX_PLATFORM_LAYOUT_ID)
                .collect();
            format!("macos:{identifier}")
        },
        str::to_owned,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::HostKeyPositions;

    const PINNED_US_NOTICE: &str = concat!(
        r##"{"type":"host_keymap","generation":1,"source":"x11_core_keymap","layouts":["us","##,
        r##""no"],"active_layout":"us","keys":[{"qt_key":32,"base":" ","shift":" "},{"qt_key"##,
        r##"":39,"base":"'","shift":"\""},{"qt_key":44,"base":",","shift":"<"},{"qt_key":45,"##,
        r##""base":"-","shift":"_"},{"qt_key":46,"base":".","shift":">"},{"qt_key":47,"base""##,
        r##":"/","shift":"?"},{"qt_key":48,"base":"0","shift":")"},{"qt_key":49,"base":"1",""##,
        r##"shift":"!"},{"qt_key":50,"base":"2","shift":"@"},{"qt_key":51,"base":"3","shift""##,
        r##":"#"},{"qt_key":52,"base":"4","shift":"$"},{"qt_key":53,"base":"5","shift":"%"},"##,
        r##"{"qt_key":54,"base":"6","shift":"^"},{"qt_key":55,"base":"7","shift":"&"},{"qt_k"##,
        r##"ey":56,"base":"8","shift":"*"},{"qt_key":57,"base":"9","shift":"("},{"qt_key":59"##,
        r##","base":";","shift":":"},{"qt_key":61,"base":"=","shift":"+"},{"qt_key":65,"base"##,
        r##"":"a","shift":"A"},{"qt_key":66,"base":"b","shift":"B"},{"qt_key":67,"base":"c","##,
        r##""shift":"C"},{"qt_key":68,"base":"d","shift":"D"},{"qt_key":69,"base":"e","shift"##,
        r##"":"E"},{"qt_key":70,"base":"f","shift":"F"},{"qt_key":71,"base":"g","shift":"G"}"##,
        r##",{"qt_key":72,"base":"h","shift":"H"},{"qt_key":73,"base":"i","shift":"I"},{"qt_"##,
        r##"key":74,"base":"j","shift":"J"},{"qt_key":75,"base":"k","shift":"K"},{"qt_key":7"##,
        r##"6,"base":"l","shift":"L"},{"qt_key":77,"base":"m","shift":"M"},{"qt_key":78,"bas"##,
        r##"e":"n","shift":"N"},{"qt_key":79,"base":"o","shift":"O"},{"qt_key":80,"base":"p""##,
        r##","shift":"P"},{"qt_key":81,"base":"q","shift":"Q"},{"qt_key":82,"base":"r","shif"##,
        r##"t":"R"},{"qt_key":83,"base":"s","shift":"S"},{"qt_key":84,"base":"t","shift":"T""##,
        r##"},{"qt_key":85,"base":"u","shift":"U"},{"qt_key":86,"base":"v","shift":"V"},{"qt"##,
        r##"_key":87,"base":"w","shift":"W"},{"qt_key":88,"base":"x","shift":"X"},{"qt_key":"##,
        r##"89,"base":"y","shift":"Y"},{"qt_key":90,"base":"z","shift":"Z"},{"qt_key":91,"ba"##,
        r##"se":"[","shift":"{"},{"qt_key":92,"base":"\\","shift":"|"},{"qt_key":93,"base":""##,
        r##"]","shift":"}"},{"qt_key":96,"base":"`","shift":"~"}]}"##,
    );

    fn norwegian(qt_key: u32) -> Option<PositionText> {
        let text = |base: &str, shift: &str| Some((Some(base.into()), Some(shift.into())));
        match qt_key {
            0x27 => text("æ", "Æ"),
            0x3B => text("ø", "Ø"),
            0x5B => text("å", "Å"),
            // `¨` is a dead key: no character at either level.
            0x5D => Some((None, None)),
            0x41 => text("a", "A"),
            0x20 => text(" ", " "),
            _ => None,
        }
    }

    #[test]
    fn a_notice_lets_a_deck_send_the_characters_the_host_types_elsewhere() {
        let notice = host_keymap_msg(
            WINDOWS_KEYBOARD_LAYOUT_SOURCE,
            vec!["us".into(), "no".into()],
            Some("no".into()),
            norwegian,
        );
        assert_eq!(notice.generation, 0);
        assert_eq!(notice.source, WINDOWS_KEYBOARD_LAYOUT_SOURCE);
        assert_eq!(notice.keys.len(), 6);
        let positions = HostKeyPositions::from_message(&notice);
        // A US Deck's `'` key types `æ` on this host: the Deck sends `'` as text.
        assert_eq!(positions.types_same(0x27, false, "'"), Some(false));
        // A Norwegian Deck's `æ` key types `æ` here too: sent as a key.
        assert_eq!(positions.types_same(0x27, false, "æ"), Some(true));
        assert_eq!(positions.types_same(0x27, true, "Æ"), Some(true));
        assert_eq!(positions.types_same(0x41, false, "a"), Some(true));
        assert_eq!(positions.types_same(0x20, false, " "), Some(true));
        // Undescribed position: the Deck assumes the layouts match.
        assert_eq!(positions.types_same(0x31, false, "1"), None);
        // A dead key types nothing a Deck could compare.
        assert_eq!(positions.types_same(0x5D, false, "¨"), Some(false));
    }

    #[test]
    fn only_printable_positions_are_asked_and_the_notice_is_bounded() {
        let mut asked = Vec::new();
        let notice = host_keymap_msg(MACOS_INPUT_SOURCE, Vec::new(), None, |qt_key| {
            asked.push(qt_key);
            Some((Some("x".into()), None))
        });
        assert_eq!(asked, PRINTABLE_QT_KEYS);
        assert_eq!(notice.keys.len(), PRINTABLE_QT_KEYS.len());
        assert!(notice.keys.len() <= MAX_HOST_KEYMAP_KEYS);
        assert!(notice.keys.iter().all(|key| key.shift.is_none()));
    }

    #[test]
    fn key_text_refuses_what_no_key_types() {
        assert_eq!(key_text("a"), Some("a".into()));
        assert_eq!(key_text(" "), Some(" ".into()));
        assert_eq!(key_text("æ"), Some("æ".into()));
        // A ligature key (Windows layouts may type two or three characters).
        assert_eq!(key_text("لا"), Some("لا".into()));
        assert_eq!(key_text(""), None);
        assert_eq!(key_text("\r"), None);
        assert_eq!(key_text("\u{1b}"), None);
        assert_eq!(key_text("\u{7f}"), None);
        assert_eq!(key_text("\u{85}"), None);
        assert_eq!(key_text("abcde"), None);
        assert_eq!(key_text_utf16(&[0x00E6]), Some("æ".into()));
        assert_eq!(key_text_utf16(&[0xD83D, 0xDE00]), Some("😀".into()));
        assert_eq!(key_text_utf16(&[0xD83D]), None);
        assert_eq!(key_text_utf16(&[]), None);
        let notice = host_keymap_msg(WINDOWS_KEYBOARD_LAYOUT_SOURCE, Vec::new(), None, |_| {
            Some((Some("\u{8}".into()), Some("toolong".into())))
        });
        assert!(
            notice
                .keys
                .iter()
                .all(|key| key.base.is_none() && key.shift.is_none())
        );
    }

    #[test]
    fn notices_are_sent_once_per_change_with_increasing_generations() {
        let reading = |layout: &str| {
            host_keymap_msg(
                MACOS_INPUT_SOURCE,
                vec![layout.into()],
                Some(layout.into()),
                norwegian,
            )
        };
        let mut notices = HostKeymapNotices::default();
        assert_eq!(notices.generation(), 0);
        let first = notices
            .offer(reading("no(mac)"))
            .expect("the first is sent");
        assert_eq!(first.generation, 1);
        assert!(notices.offer(reading("no(mac)")).is_none(), "unchanged");
        let mut stamped = reading("no(mac)");
        stamped.generation = 9;
        assert!(
            notices.offer(stamped).is_none(),
            "generation is not content"
        );
        let second = notices.offer(reading("us(mac)")).expect("changed");
        assert_eq!(second.generation, 2);
        let back = notices.offer(reading("no(mac)")).expect("changed back");
        assert_eq!(back.generation, 3);
        assert_eq!(notices.generation(), 3);
    }

    /// A US layout, position by position.
    fn us(qt_key: u32) -> Option<PositionText> {
        const SHIFTED: &[(u32, &str, &str)] = &[
            (0x20, " ", " "),
            (0x27, "'", "\""),
            (0x2C, ",", "<"),
            (0x2D, "-", "_"),
            (0x2E, ".", ">"),
            (0x2F, "/", "?"),
            (0x30, "0", ")"),
            (0x31, "1", "!"),
            (0x32, "2", "@"),
            (0x33, "3", "#"),
            (0x34, "4", "$"),
            (0x35, "5", "%"),
            (0x36, "6", "^"),
            (0x37, "7", "&"),
            (0x38, "8", "*"),
            (0x39, "9", "("),
            (0x3B, ";", ":"),
            (0x3D, "=", "+"),
            (0x5B, "[", "{"),
            (0x5C, "\\", "|"),
            (0x5D, "]", "}"),
            (0x60, "`", "~"),
        ];
        if let Some((_, base, shift)) = SHIFTED.iter().find(|(key, ..)| *key == qt_key) {
            return Some((Some((*base).into()), Some((*shift).into())));
        }
        let letter = char::from_u32(qt_key).filter(char::is_ascii_uppercase)?;
        Some((
            Some(letter.to_ascii_lowercase().to_string()),
            Some(letter.to_string()),
        ))
    }

    /// The exact bytes a Deck receives for a US session, pinned: the notice
    /// every Pier sends is the one the Linux Pier sent before the builder
    /// moved here (positions in this order, Shift spelled out, empty fields
    /// omitted).
    #[test]
    fn the_notice_a_deck_receives_is_pinned() {
        let mut notices = HostKeymapNotices::default();
        let notice = notices
            .offer(host_keymap_msg(
                "x11_core_keymap",
                vec!["us".into(), "no".into()],
                Some("us".into()),
                us,
            ))
            .expect("first notice");
        let json = serde_json::to_string(&notice).unwrap();
        assert_eq!(json, PINNED_US_NOTICE);
    }

    #[test]
    fn windows_layout_identifiers_map_to_layout_names() {
        assert_eq!(windows_layout_name(0x0000_0414), "no");
        assert_eq!(windows_layout_name(0x0000_0409), "us");
        assert_eq!(windows_layout_name(0x0002_0409), "us(intl)");
        assert_eq!(windows_layout_name(0x0000_0809), "gb");
        assert_eq!(windows_layout_name(0x0000_0407), "de");
        assert_eq!(windows_layout_name(0x0000_041D), "se");
        assert_eq!(windows_layout_name(0x0004_0409), "windows:00040409");
        assert_eq!(parse_klid("00000414"), Some(0x0000_0414));
        assert_eq!(parse_klid("00020409\0"), Some(0x0002_0409));
        assert_eq!(parse_klid("0000041"), None);
        assert_eq!(parse_klid("0000041g"), None);
        assert_eq!(parse_klid("+0000414"), None);
        // HKL 0x04140414: Norwegian language, Norwegian layout.
        assert_eq!(
            windows_hkl_layout(0x0414_0414),
            WindowsHklLayout::Klid(0x0000_0414)
        );
        // Norwegian language with the US layout.
        assert_eq!(
            windows_hkl_layout(0x0409_0414),
            WindowsHklLayout::Klid(0x0000_0409)
        );
        // US-International: a variant, resolved through `Layout Id`.
        assert_eq!(
            windows_hkl_layout(0xFFFF_FFFF_F001_0409),
            WindowsHklLayout::Variant(1)
        );
        // A legacy input method's handle is its KLID.
        assert_eq!(
            windows_hkl_layout(0xE001_0411),
            WindowsHklLayout::Klid(0xE001_0411)
        );
        assert_eq!(parse_layout_id("0002"), Some(2));
        assert_eq!(parse_layout_id("00c0\0"), Some(0xC0));
        assert_eq!(parse_layout_id(""), None);
        assert_eq!(parse_layout_id("00002"), None);
        assert_eq!(parse_layout_id("-001"), None);
    }

    #[test]
    fn macos_input_sources_map_to_layout_names() {
        assert_eq!(
            macos_layout_name("com.apple.keylayout.Norwegian"),
            "no(mac)"
        );
        assert_eq!(macos_layout_name("com.apple.keylayout.ABC"), "us(mac)");
        assert_eq!(macos_layout_name("com.apple.keylayout.British-PC"), "gb");
        assert_eq!(
            macos_layout_name("com.apple.keylayout.Colemak"),
            "macos:com.apple.keylayout.Colemak"
        );
        assert_eq!(
            macos_layout_name("org.example.layout.\u{0}weird\nid æ"),
            "macos:org.example.layout.weirdid"
        );
        let long = format!("x.{}", "y".repeat(500));
        assert_eq!(
            macos_layout_name(&long).len(),
            "macos:".len() + MAX_PLATFORM_LAYOUT_ID
        );
    }
}
