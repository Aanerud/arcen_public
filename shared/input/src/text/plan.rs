//! One commit, planned: which characters go by keysym through the session's
//! keyboard layout, which go as input-method commits, and which cannot be
//! typed, with the report the client is told.

use arcen_protocol::messages::{MAX_REFUSED_TEXT_CHARS, TextRefusalReason};

use super::keymap::KeymapIndex;

/// Largest `ei_text.utf8` string, in bytes: the libei protocol caps it at
/// 255 bytes including the terminating NUL.
pub const MAX_UTF8_CHUNK_BYTES: usize = 254;

/// Most characters a host types in one step. Typing a character takes the
/// host a few compositor round trips, so a whole commit (up to 1024 bytes)
/// is typed in parts that each finish well inside one command's budget.
pub const MAX_TEXT_PART_CHARS: usize = 64;

/// Splits `text` into parts of at most `max_chars` characters, on character
/// boundaries; typed one after the other they type `text`.
#[must_use]
pub fn text_parts(text: &str, max_chars: usize) -> Vec<&str> {
    let max_chars = max_chars.max(1);
    let mut parts = Vec::new();
    let mut start = 0;
    for (count, (index, _)) in text.char_indices().enumerate() {
        if count > 0 && count % max_chars == 0 {
            parts.push(&text[start..index]);
            start = index;
        }
    }
    if start < text.len() {
        parts.push(&text[start..]);
    }
    parts
}

/// What the session proves it can type with.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextRoutes<'a> {
    /// The session's keymap and its active group, when both are known.
    pub keymap: Option<(&'a KeymapIndex, u32)>,
    /// Without a known keymap: send keysyms anyway, unproven (the
    /// compositor drops what its layout lacks, silently).
    pub unverified_keysyms: bool,
    /// The compositor takes input-method commits (`ei_text.utf8`).
    pub utf8: bool,
}

/// A run of consecutive characters typed the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextRun {
    /// Keysyms, each pressed and released in order.
    Keysyms(Vec<u32>),
    /// Input-method commits, each at most [`MAX_UTF8_CHUNK_BYTES`].
    Utf8(Vec<String>),
}

/// What became of a commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextDeliveryReport {
    /// Characters typed by keysym, proven on the session's layout.
    pub keymap_chars: u32,
    /// Characters typed by keysym without proof.
    pub unverified_chars: u32,
    /// Characters committed through the input method.
    pub utf8_chars: u32,
    /// Characters not typed.
    pub refused_chars: u32,
    /// The characters not typed, in order, at most
    /// [`MAX_REFUSED_TEXT_CHARS`].
    pub refused_text: String,
    /// Why the first of them was not typed.
    pub reason: Option<TextRefusalReason>,
}

impl TextDeliveryReport {
    /// A report refusing all of `text` for `reason`.
    #[must_use]
    pub fn refused(text: &str, reason: TextRefusalReason) -> Self {
        let mut report = Self::default();
        for character in text.chars() {
            report.refuse(character, reason);
        }
        report
    }

    /// Characters typed, proven or not.
    #[must_use]
    pub const fn delivered_chars(&self) -> u32 {
        self.keymap_chars
            .saturating_add(self.unverified_chars)
            .saturating_add(self.utf8_chars)
    }

    /// Whether the client must be told: something was refused or typed
    /// without proof.
    #[must_use]
    pub const fn needs_notice(&self) -> bool {
        self.refused_chars > 0 || self.unverified_chars > 0
    }

    fn refuse(&mut self, character: char, reason: TextRefusalReason) {
        self.refused_chars = self.refused_chars.saturating_add(1);
        if self.refused_text.chars().count() < MAX_REFUSED_TEXT_CHARS {
            self.refused_text.push(character);
        }
        self.reason.get_or_insert(reason);
    }
}

/// A commit's runs and report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextPlan {
    /// The runs, in text order.
    pub runs: Vec<TextRun>,
    /// The report.
    pub report: TextDeliveryReport,
    /// Some keysym run types a cased letter: Caps Lock must be off while it
    /// is typed, or the compositor types the other case.
    pub caps_sensitive: bool,
}

impl TextPlan {
    /// Whether any run types keysyms: held modifiers must be released while
    /// they are typed, or Shift, Control or Alt would change them into
    /// other characters or shortcuts.
    #[must_use]
    pub fn types_keysyms(&self) -> bool {
        self.runs
            .iter()
            .any(|run| matches!(run, TextRun::Keysyms(_)))
    }
}

/// Splits `text` into strings of at most `max_bytes` UTF-8 bytes, on
/// character boundaries. Splitting inside a grapheme cluster is harmless:
/// consecutive commits are inserted one after the other.
#[must_use]
pub fn utf8_chunks(text: &str, max_bytes: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if !current.is_empty() && current.len() + character.len_utf8() > max_bytes {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

enum Route {
    Keysym(u32, bool),
    Utf8,
    Refused(TextRefusalReason),
}

/// Plans `text` (already validated as a commit) over `routes`.
/// `fallback_keysym` names a character's keysym when the keymap is unknown
/// (`xkb_utf32_to_keysym`).
#[must_use]
pub fn plan_text(
    text: &str,
    routes: &TextRoutes<'_>,
    fallback_keysym: impl Fn(char) -> Option<u32>,
) -> TextPlan {
    let mut plan = TextPlan::default();
    let mut pending_utf8 = String::new();
    let flush_utf8 = |plan: &mut TextPlan, pending: &mut String| {
        if !pending.is_empty() {
            let chunks = utf8_chunks(pending, MAX_UTF8_CHUNK_BYTES);
            pending.clear();
            match plan.runs.last_mut() {
                Some(TextRun::Utf8(previous)) => previous.extend(chunks),
                _ => plan.runs.push(TextRun::Utf8(chunks)),
            }
        }
    };
    for character in text.chars() {
        let route = match routes.keymap {
            Some((index, group)) => match index.keysym_for_char(group, character) {
                Some((keysym, _)) => Route::Keysym(keysym, true),
                None if routes.utf8 => Route::Utf8,
                None => Route::Refused(TextRefusalReason::CompositorLacksTextInput),
            },
            // Without a keymap, a proven input-method route beats an
            // unproven keysym.
            None if routes.utf8 => Route::Utf8,
            None if routes.unverified_keysyms => match fallback_keysym(character) {
                Some(keysym) => Route::Keysym(keysym, false),
                None => Route::Refused(TextRefusalReason::NotInSessionLayout),
            },
            None => Route::Refused(TextRefusalReason::KeymapUnknown),
        };
        match route {
            Route::Keysym(keysym, proven) => {
                flush_utf8(&mut plan, &mut pending_utf8);
                if proven {
                    plan.report.keymap_chars = plan.report.keymap_chars.saturating_add(1);
                } else {
                    plan.report.unverified_chars = plan.report.unverified_chars.saturating_add(1);
                }
                if character.is_uppercase() || character.is_lowercase() {
                    plan.caps_sensitive = true;
                }
                match plan.runs.last_mut() {
                    Some(TextRun::Keysyms(keysyms)) => keysyms.push(keysym),
                    _ => plan.runs.push(TextRun::Keysyms(vec![keysym])),
                }
            }
            Route::Utf8 => {
                plan.report.utf8_chars = plan.report.utf8_chars.saturating_add(1);
                pending_utf8.push(character);
            }
            Route::Refused(reason) => {
                plan.report.refuse(character, reason);
            }
        }
    }
    flush_utf8(&mut plan, &mut pending_utf8);
    plan
}

#[cfg(test)]
mod tests {
    use super::super::keymap::tests::norwegian_and_us;
    use super::*;

    #[test]
    fn a_long_commit_is_typed_in_parts_on_character_boundaries() {
        let text = "æ".repeat(130);
        let parts = text_parts(&text, MAX_TEXT_PART_CHARS);
        assert_eq!(
            parts
                .iter()
                .map(|part| part.chars().count())
                .collect::<Vec<_>>(),
            [64, 64, 2]
        );
        assert_eq!(parts.concat(), text);
        assert_eq!(text_parts("abc", MAX_TEXT_PART_CHARS), ["abc"]);
        assert!(text_parts("", MAX_TEXT_PART_CHARS).is_empty());
        assert_eq!(text_parts("😀日本", 1), ["😀", "日", "本"]);
    }

    #[allow(clippy::unnecessary_wraps)]
    fn unicode_keysym(character: char) -> Option<u32> {
        Some(0x0100_0000 | u32::from(character))
    }

    #[test]
    fn layout_characters_go_by_keysym_in_order() {
        let index = norwegian_and_us();
        let routes = TextRoutes {
            keymap: Some((&index, 0)),
            ..TextRoutes::default()
        };
        let plan = plan_text("aæÆ@", &routes, unicode_keysym);
        assert_eq!(plan.runs, [TextRun::Keysyms(vec![0x61, 0xe6, 0xc6, 0x40])]);
        assert_eq!(plan.report.keymap_chars, 4);
        assert!(!plan.report.needs_notice());
        assert!(plan.caps_sensitive);
        assert!(plan.types_keysyms());
    }

    #[test]
    fn other_characters_are_refused_without_text_input_and_named() {
        let index = norwegian_and_us();
        let routes = TextRoutes {
            keymap: Some((&index, 1)),
            ..TextRoutes::default()
        };
        let plan = plan_text("aæ😀", &routes, unicode_keysym);
        assert_eq!(plan.runs, [TextRun::Keysyms(vec![0x61])]);
        assert_eq!(plan.report.refused_chars, 2);
        assert_eq!(plan.report.refused_text, "æ😀");
        assert_eq!(
            plan.report.reason,
            Some(TextRefusalReason::CompositorLacksTextInput)
        );
        assert_eq!(plan.report.delivered_chars(), 1);
        assert!(plan.report.needs_notice());
    }

    #[test]
    fn other_characters_become_input_method_commits_when_offered() {
        let index = norwegian_and_us();
        let routes = TextRoutes {
            keymap: Some((&index, 0)),
            utf8: true,
            ..TextRoutes::default()
        };
        let plan = plan_text("æ日本語a😀", &routes, unicode_keysym);
        assert_eq!(
            plan.runs,
            [
                TextRun::Keysyms(vec![0xe6]),
                TextRun::Utf8(vec!["日本語".to_owned()]),
                TextRun::Keysyms(vec![0x61]),
                TextRun::Utf8(vec!["😀".to_owned()]),
            ]
        );
        assert_eq!(plan.report.utf8_chars, 4);
        assert_eq!(plan.report.keymap_chars, 2);
        assert!(!plan.report.needs_notice());
        let only_utf8 = plan_text("日本", &routes, unicode_keysym);
        assert!(!only_utf8.types_keysyms());
        assert!(!only_utf8.caps_sensitive);
    }

    #[test]
    fn an_unknown_keymap_is_typed_unverified_or_refused() {
        let unverified = TextRoutes {
            unverified_keysyms: true,
            ..TextRoutes::default()
        };
        let plan = plan_text("æ😀", &unverified, unicode_keysym);
        assert_eq!(
            plan.runs,
            [TextRun::Keysyms(vec![0x0100_00e6, 0x0101_f600])]
        );
        assert_eq!(plan.report.unverified_chars, 2);
        assert!(plan.report.needs_notice());
        let with_input_method = TextRoutes {
            utf8: true,
            ..unverified
        };
        let proven = plan_text("æ😀", &with_input_method, unicode_keysym);
        assert_eq!(proven.runs, [TextRun::Utf8(vec!["æ😀".to_owned()])]);
        assert!(!proven.report.needs_notice());
        let nothing = plan_text("æ", &TextRoutes::default(), unicode_keysym);
        assert!(nothing.runs.is_empty());
        assert_eq!(
            nothing.report.reason,
            Some(TextRefusalReason::KeymapUnknown)
        );
    }

    #[test]
    fn chunks_respect_the_protocol_limit_on_character_boundaries() {
        let text = format!("{}{}", "a".repeat(253), "😀😀");
        let chunks = utf8_chunks(&text, MAX_UTF8_CHUNK_BYTES);
        assert_eq!(chunks.len(), 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_UTF8_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), text);
        let long = "日".repeat(200);
        let chunks = utf8_chunks(&long, MAX_UTF8_CHUNK_BYTES);
        assert_eq!(chunks.len(), 3);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_UTF8_CHUNK_BYTES)
        );
        assert!(utf8_chunks("", MAX_UTF8_CHUNK_BYTES).is_empty());
    }

    #[test]
    fn refused_text_is_capped() {
        let report = TextDeliveryReport::refused(
            &"😀".repeat(MAX_REFUSED_TEXT_CHARS + 5),
            TextRefusalReason::HostUnsupported,
        );
        assert_eq!(report.refused_chars as usize, MAX_REFUSED_TEXT_CHARS + 5);
        assert_eq!(report.refused_text.chars().count(), MAX_REFUSED_TEXT_CHARS);
    }
}
