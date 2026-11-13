// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One commit, planned: which characters go by key (a proven stroke, or a
//! keysym) through the session's keyboard layout, which go as input-method
//! commits, and which cannot be typed, with the report the client is told.
//! Nothing is typed that the layout does not prove, unless the caller asks
//! for unverified keysyms on a host whose keymap is unknown.

use arcen_protocol::messages::{MAX_REFUSED_TEXT_CHARS, TextRefusalReason};

use super::keymap::{KeymapIndex, TextStroke};

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
    /// The session's keymap and its active group, when both are known and
    /// the group is live ([`super::GroupResolution::is_live`]): a group
    /// merely inferred belongs in `unresolved_keymap`.
    pub keymap: Option<(&'a KeymapIndex, u32)>,
    /// The session's keymap when its active group is not known: only
    /// strokes that type the same character in every group are used.
    pub unresolved_keymap: Option<&'a KeymapIndex>,
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
    /// Proven strokes, typed by keycode: modifiers down, key down and up,
    /// modifiers up.
    Strokes(Vec<TextStroke>),
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
    /// Whether any run types keys (keysyms or strokes): held modifiers must
    /// be released while they are typed, or Shift, Control or Alt would
    /// change them into other characters or shortcuts.
    #[must_use]
    pub fn types_keysyms(&self) -> bool {
        self.runs
            .iter()
            .any(|run| matches!(run, TextRun::Keysyms(_) | TextRun::Strokes(_)))
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

/// Most UTF-16 code units one synthetic key event carries: macOS documents
/// that applications read at most 20 from `CGEventKeyboardSetUnicodeString`.
pub const MAX_UTF16_CHUNK_UNITS: usize = 20;

/// Whether `character` continues the character before it rather than
/// starting a new one: a combining mark, variation selector, emoji modifier,
/// tag or zero-width joiner. A small, dependency-free stand-in for grapheme
/// segmentation, enough to keep accented letters and emoji sequences whole.
const fn extends_previous(character: char) -> bool {
    matches!(
        character as u32,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200D
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0x1F3FB..=0x1F3FF
            | 0xE0020..=0xE007F
            | 0xE0100..=0xE01EF
    )
}

const fn is_regional_indicator(character: char) -> bool {
    matches!(character as u32, 0x1F1E6..=0x1F1FF)
}

/// Splits `text` into strings of at most `max_units` UTF-16 code units
/// (at least 2), for hosts that type text as UTF-16 strings in synthetic key
/// events.
///
/// Never splits a character, so a surrogate pair stays in one string, and
/// splits a cluster (a letter and its combining marks, an emoji with its
/// modifiers or zero-width joiners, a flag's two regional indicators) only
/// when it alone is longer than `max_units`. Typed one after the other the
/// strings type `text`.
#[must_use]
pub fn utf16_chunks(text: &str, max_units: usize) -> Vec<&str> {
    let max_units = max_units.max(2);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut units = 0;
    // The last place after `start` where a new cluster begins.
    let mut boundary = None;
    let mut after_joiner = false;
    let mut regional_run = 0_usize;
    for (index, character) in text.char_indices() {
        let pairs_flag = is_regional_indicator(character) && regional_run % 2 == 1;
        if index > start && !extends_previous(character) && !after_joiner && !pairs_flag {
            boundary = Some(index);
        }
        after_joiner = character == '\u{200D}';
        regional_run = if is_regional_indicator(character) {
            regional_run + 1
        } else {
            0
        };
        let width = character.len_utf16();
        if units + width > max_units {
            if let Some(split) = boundary.filter(|&split| split > start) {
                chunks.push(&text[start..split]);
                units = text[split..index].encode_utf16().count();
                start = split;
            }
            if units + width > max_units {
                chunks.push(&text[start..index]);
                units = 0;
                start = index;
            }
            boundary = None;
        }
        units += width;
    }
    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

enum Route {
    Keysym(u32, bool),
    Stroke(TextStroke),
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
        let route = match (routes.keymap, routes.unresolved_keymap) {
            (Some((index, group)), _) if index.has_strokes() => {
                // A stroke every group agrees on stays right even if the
                // group changed since it was read.
                match index
                    .stroke_for_char(None, character)
                    .or_else(|| index.stroke_for_char(Some(group), character))
                {
                    Some(stroke) => Route::Stroke(stroke),
                    None if routes.utf8 => Route::Utf8,
                    None => Route::Refused(TextRefusalReason::CompositorLacksTextInput),
                }
            }
            (Some((index, group)), _) => match index.keysym_for_char(group, character) {
                Some((keysym, _)) => Route::Keysym(keysym, true),
                None if routes.utf8 => Route::Utf8,
                None => Route::Refused(TextRefusalReason::CompositorLacksTextInput),
            },
            (None, Some(index)) => {
                match index.stroke_for_char(None, character) {
                    Some(stroke) => Route::Stroke(stroke),
                    None if routes.utf8 => Route::Utf8,
                    // Some group types it, but not every one: which one
                    // is active decides, and that is not known.
                    None if !index.has_strokes()
                        || (0..index.group_count()).any(|group| {
                            index
                                .stroke_for_char(u32::try_from(group).ok(), character)
                                .is_some()
                        }) =>
                    {
                        Route::Refused(TextRefusalReason::KeymapUnknown)
                    }
                    None => Route::Refused(TextRefusalReason::CompositorLacksTextInput),
                }
            }
            // Without a keymap, a proven input-method route beats an
            // unproven keysym.
            (None, None) if routes.utf8 => Route::Utf8,
            (None, None) if routes.unverified_keysyms => match fallback_keysym(character) {
                Some(keysym) => Route::Keysym(keysym, false),
                None => Route::Refused(TextRefusalReason::NotInSessionLayout),
            },
            (None, None) => Route::Refused(TextRefusalReason::KeymapUnknown),
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
            Route::Stroke(stroke) => {
                flush_utf8(&mut plan, &mut pending_utf8);
                plan.report.keymap_chars = plan.report.keymap_chars.saturating_add(1);
                if character.is_uppercase() || character.is_lowercase() {
                    plan.caps_sensitive = true;
                }
                match plan.runs.last_mut() {
                    Some(TextRun::Strokes(strokes)) => strokes.push(stroke),
                    _ => plan.runs.push(TextRun::Strokes(vec![stroke])),
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
    use super::super::keymap::tests::{gb_strokes, norwegian_and_us, stroke};
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
    fn proven_strokes_beat_keysyms_and_nothing_unproven_is_typed() {
        let index = gb_strokes(1, '<');
        let routes = TextRoutes {
            keymap: Some((&index, 0)),
            unverified_keysyms: true,
            ..TextRoutes::default()
        };
        let plan = plan_text("z<é", &routes, unicode_keysym);
        assert_eq!(
            plan.runs,
            [TextRun::Strokes(vec![
                stroke(52, false, false),
                stroke(59, true, false)
            ])]
        );
        assert!(plan.types_keysyms());
        assert!(plan.caps_sensitive);
        assert_eq!(plan.report.keymap_chars, 2);
        assert_eq!(plan.report.unverified_chars, 0);
        assert_eq!(plan.report.refused_text, "é");
        assert_eq!(
            plan.report.reason,
            Some(TextRefusalReason::CompositorLacksTextInput)
        );
    }

    #[test]
    fn a_live_group_still_prefers_a_stroke_every_group_agrees_on() {
        let index = gb_strokes(2, ';');
        let routes = TextRoutes {
            keymap: Some((&index, 0)),
            ..TextRoutes::default()
        };
        // Group 0 alone would take Shift+<AB08>; Shift+AltGr+<AB01> types
        // `<` in both groups, so it is chosen.
        let plan = plan_text("<", &routes, unicode_keysym);
        assert_eq!(plan.runs, [TextRun::Strokes(vec![stroke(52, true, true)])]);
        // `;` exists only in group 1, and group 1 is not the live one.
        let plan = plan_text(";", &routes, unicode_keysym);
        assert_eq!(plan.report.refused_text, ";");
    }

    #[test]
    fn an_unresolved_group_types_only_what_every_group_agrees_on() {
        let index = gb_strokes(2, ';');
        let routes = TextRoutes {
            unresolved_keymap: Some(&index),
            unverified_keysyms: true,
            ..TextRoutes::default()
        };
        let plan = plan_text(",;é", &routes, unicode_keysym);
        assert_eq!(
            plan.runs,
            [TextRun::Strokes(vec![stroke(59, false, false)])]
        );
        assert_eq!(plan.report.unverified_chars, 0);
        assert_eq!(plan.report.refused_text, ";é");
        assert_eq!(plan.report.reason, Some(TextRefusalReason::KeymapUnknown));
        let with_input_method = TextRoutes {
            utf8: true,
            ..routes
        };
        let plan = plan_text(",é", &with_input_method, unicode_keysym);
        assert_eq!(
            plan.runs,
            [
                TextRun::Strokes(vec![stroke(59, false, false)]),
                TextRun::Utf8(vec!["é".to_owned()])
            ]
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

    fn utf16_len(text: &str) -> usize {
        text.encode_utf16().count()
    }

    #[test]
    fn utf16_chunks_keep_surrogate_pairs_and_clusters_whole() {
        // 19 units then an emoji (a surrogate pair): the pair moves whole.
        let text = format!("{}😀b", "a".repeat(19));
        let chunks = utf16_chunks(&text, MAX_UTF16_CHUNK_UNITS);
        assert_eq!(chunks, vec!["a".repeat(19).as_str(), "😀b"]);

        // A family emoji (ZWJ sequence, 8 units) and a skin-toned thumb
        // (4 units) straddling the limit stay in one chunk each.
        let family = "👨\u{200D}👩\u{200D}👧";
        let thumb = "👍\u{1F3FD}";
        let text = format!("{}{family}{}{thumb}", "x".repeat(15), "y".repeat(7));
        let chunks = utf16_chunks(&text, MAX_UTF16_CHUNK_UNITS);
        assert_eq!(chunks.concat(), text);
        assert!(
            chunks
                .iter()
                .all(|chunk| utf16_len(chunk) <= MAX_UTF16_CHUNK_UNITS)
        );
        assert!(chunks.iter().any(|chunk| chunk.starts_with(family)));
        assert!(chunks.iter().any(|chunk| chunk.ends_with(thumb)));

        // A flag's two regional indicators, a letter and its combining mark.
        let text = format!("{}🇳🇴{}e\u{301}", "a".repeat(18), "b".repeat(15));
        let chunks = utf16_chunks(&text, MAX_UTF16_CHUNK_UNITS);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().any(|chunk| chunk.starts_with("🇳🇴")));
        assert!(chunks.iter().any(|chunk| chunk.ends_with("e\u{301}")));
        assert!(
            chunks
                .iter()
                .all(|chunk| utf16_len(chunk) <= MAX_UTF16_CHUNK_UNITS)
        );
    }

    #[test]
    fn utf16_chunks_split_long_text_and_oversized_clusters_within_the_limit() {
        let long = "æøå😀".repeat(100);
        let chunks = utf16_chunks(&long, MAX_UTF16_CHUNK_UNITS);
        assert_eq!(chunks.concat(), long);
        assert!(
            chunks
                .iter()
                .all(|chunk| utf16_len(chunk) <= MAX_UTF16_CHUNK_UNITS)
        );
        assert_eq!(chunks.len(), 500_usize.div_ceil(20));
        // One letter with 30 combining marks cannot fit: split, never lost.
        let heavy = format!("a{}", "\u{301}".repeat(30));
        let chunks = utf16_chunks(&heavy, MAX_UTF16_CHUNK_UNITS);
        assert_eq!(chunks.concat(), heavy);
        assert!(
            chunks
                .iter()
                .all(|chunk| utf16_len(chunk) <= MAX_UTF16_CHUNK_UNITS)
        );
        assert!(utf16_chunks("", MAX_UTF16_CHUNK_UNITS).is_empty());
        // A zero limit still makes progress (two units: one surrogate pair).
        assert_eq!(utf16_chunks("😀😀", 0), vec!["😀", "😀"]);
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
