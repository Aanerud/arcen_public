// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The compositor's keymap, compiled with the system's libxkbcommon and
//! indexed for committed text.
//!
//! libxkbcommon is loaded at run time (`xkbcommon-dl`): a host without it
//! types committed text unverified, as before it had a keymap. The keymap is
//! read exactly as the compositor wrote it, so the index lists the same
//! keysyms at the same levels the compositor's own lookup scans.
//!
//! It also proves **strokes**: for every group, each key is replayed on a
//! libxkbcommon state with that group locked and Shift, `AltGr`, both or
//! neither held down (by their own keys, as the session will press them),
//! and the character that comes out is recorded. Typing those strokes by
//! keycode types exactly that character; nothing else is typed by key.

use std::ffi::CString;

use arcen_input::text::keymap::{KEYSYM_ISO_LEVEL3_SHIFT, KEYSYM_SHIFT_L};
use arcen_input::text::{
    layouts_from_symbols_section, symbols_section_name, KeymapEntry, KeymapIndex, StrokeEntry,
    TextStroke,
};
use xkbcommon_dl::{
    xkb_context_flags, xkb_key_direction, xkb_keymap, xkb_keymap_compile_flags, xkb_keymap_format,
    xkb_state, xkb_state_component, xkbcommon_option, XkbCommon,
};

/// `<LFSH>` and `<RALT>`, preferred as the Shift and level-three keys when
/// they hold `Shift_L` and `ISO_Level3_Shift`.
const PREFERRED_SHIFT_KEY: u32 = 50;
const PREFERRED_LEVEL3_KEY: u32 = 108;

/// The key typing `keysym` unmodified in the state's group, preferring
/// `preferred`. Read from the state, not the keymap's per-group listing:
/// a key with fewer groups (`<LFSH>` lists one) serves every group.
///
/// # Safety
///
/// `state` must be a live state from `xkb`.
unsafe fn modifier_key(
    xkb: &XkbCommon,
    state: *mut xkb_state,
    (min, max): (u32, u32),
    keysym: u32,
    preferred: u32,
) -> Option<u32> {
    let mut keys =
        (min..max).filter(|&keycode| (xkb.xkb_state_key_get_one_sym)(state, keycode) == keysym);
    let first = keys.next()?;
    Some(
        if first == preferred || keys.any(|keycode| keycode == preferred) {
            preferred
        } else {
            first
        },
    )
}

/// Replays every key of every group with each modifier combination and
/// records what it types.
///
/// # Safety
///
/// `keymap` must be a live keymap from `xkb`.
unsafe fn prove_strokes(
    xkb: &XkbCommon,
    keymap: *mut xkb_keymap,
    (min, max): (u32, u32),
) -> Vec<StrokeEntry> {
    let mut strokes = Vec::new();
    let groups = (xkb.xkb_keymap_num_layouts)(keymap);
    for group in 0..groups {
        let base = (xkb.xkb_state_new)(keymap);
        if base.is_null() {
            continue;
        }
        (xkb.xkb_state_update_mask)(base, 0, 0, 0, 0, 0, group);
        let shift = modifier_key(xkb, base, (min, max), KEYSYM_SHIFT_L, PREFERRED_SHIFT_KEY);
        let level3 = modifier_key(
            xkb,
            base,
            (min, max),
            KEYSYM_ISO_LEVEL3_SHIFT,
            PREFERRED_LEVEL3_KEY,
        );
        (xkb.xkb_state_unref)(base);
        let mut combos = vec![(None, None)];
        combos.extend(shift.map(|key| (Some(key), None)));
        combos.extend(level3.map(|key| (None, Some(key))));
        combos.extend(
            shift
                .zip(level3)
                .map(|(shift, level3)| (Some(shift), Some(level3))),
        );
        for (with_shift, with_level3) in combos {
            let state = (xkb.xkb_state_new)(keymap);
            if state.is_null() {
                continue;
            }
            (xkb.xkb_state_update_mask)(state, 0, 0, 0, 0, 0, group);
            for key in with_shift.into_iter().chain(with_level3) {
                (xkb.xkb_state_update_key)(state, key, xkb_key_direction::XKB_KEY_DOWN);
            }
            // A modifier key that switched the group would make the replay
            // describe another group.
            let effective = (xkb.xkb_state_serialize_layout)(
                state,
                xkb_state_component::XKB_STATE_LAYOUT_EFFECTIVE,
            );
            if effective == group {
                let stroke_modifiers = [with_shift, with_level3];
                for keycode in min..max {
                    if stroke_modifiers.contains(&Some(keycode)) {
                        continue;
                    }
                    let code = (xkb.xkb_state_key_get_utf32)(state, keycode);
                    let Some(character) = char::from_u32(code).filter(|_| code != 0) else {
                        continue;
                    };
                    strokes.push(StrokeEntry {
                        group,
                        stroke: TextStroke {
                            keycode,
                            shift: with_shift,
                            level3: with_level3,
                        },
                        keysym: (xkb.xkb_state_key_get_one_sym)(state, keycode),
                        character,
                    });
                }
            }
            (xkb.xkb_state_unref)(state);
        }
    }
    strokes
}

/// Why a keymap could not be indexed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum XkbIndexError {
    /// `libxkbcommon.so` could not be loaded.
    #[error("libxkbcommon is not available on this host")]
    LibraryMissing,
    /// The keymap text holds a NUL byte.
    #[error("the keymap text holds a NUL byte")]
    InvalidText,
    /// libxkbcommon could not create a context.
    #[error("libxkbcommon could not create a context")]
    Context,
    /// libxkbcommon could not compile the keymap.
    #[error("libxkbcommon could not compile the keymap")]
    Compile,
}

/// Whether libxkbcommon can be loaded.
#[must_use]
pub fn available() -> bool {
    xkbcommon_option().is_some()
}

/// Compiles `text` (an XKB keymap in the text format, as a compositor sends
/// it) and indexes every key's keysyms per group and level.
///
/// # Errors
///
/// [`XkbIndexError`] when libxkbcommon is missing or refuses the keymap.
pub fn index_keymap(text: &str) -> Result<KeymapIndex, XkbIndexError> {
    let xkb = xkbcommon_option().ok_or(XkbIndexError::LibraryMissing)?;
    let source =
        CString::new(text.trim_end_matches('\0')).map_err(|_| XkbIndexError::InvalidText)?;
    // SAFETY: `xkb_context_new` has no preconditions; a null result is
    // checked. The context needs no include path: a compiled keymap in the
    // text format is self-contained.
    let context =
        unsafe { (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_DEFAULT_INCLUDES) };
    if context.is_null() {
        return Err(XkbIndexError::Context);
    }
    // SAFETY: `context` is a live context and `source` a NUL-terminated
    // string that outlives the call; a null result is checked.
    let keymap = unsafe {
        (xkb.xkb_keymap_new_from_string)(
            context,
            source.as_ptr(),
            xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
            xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
        )
    };
    // SAFETY: the keymap holds its own reference to the context; this one
    // is no longer used.
    unsafe { (xkb.xkb_context_unref)(context) };
    if keymap.is_null() {
        return Err(XkbIndexError::Compile);
    }
    let mut entries = Vec::new();
    // SAFETY: `keymap` is a live keymap for the rest of this block. Every
    // keysym slice comes from `xkb_keymap_key_get_syms_by_level`, which
    // returns its length and a pointer into the keymap valid while the
    // keymap lives; it is read before the keymap is released.
    let (max_keycode, strokes) = unsafe {
        let min = (xkb.xkb_keymap_min_keycode)(keymap);
        let max = (xkb.xkb_keymap_max_keycode)(keymap);
        for keycode in min..=max {
            let layouts = (xkb.xkb_keymap_num_layouts_for_key)(keymap, keycode);
            for group in 0..layouts {
                let levels = (xkb.xkb_keymap_num_levels_for_key)(keymap, keycode, group);
                for level in 0..levels {
                    let mut syms: *const u32 = std::ptr::null();
                    let count = (xkb.xkb_keymap_key_get_syms_by_level)(
                        keymap,
                        keycode,
                        group,
                        level,
                        &raw mut syms,
                    );
                    let Ok(count) = usize::try_from(count) else {
                        continue;
                    };
                    if count == 0 || syms.is_null() {
                        continue;
                    }
                    for &keysym in std::slice::from_raw_parts(syms, count) {
                        let code = (xkb.xkb_keysym_to_utf32)(keysym);
                        entries.push(KeymapEntry {
                            keycode,
                            group,
                            level,
                            keysym,
                            character: (code != 0).then(|| char::from_u32(code)).flatten(),
                        });
                    }
                }
            }
        }
        // The compositor scans keys below the maximum; so does the proof.
        let strokes = prove_strokes(xkb, keymap, (min, max));
        (xkb.xkb_keymap_unref)(keymap);
        (max, strokes)
    };
    let layouts = symbols_section_name(text)
        .map(layouts_from_symbols_section)
        .unwrap_or_default();
    Ok(KeymapIndex::new(entries, max_keycode, layouts).with_strokes(strokes))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal two-group keymap in the text format.
    pub(crate) const KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "test" {
    minimum = 8;
    maximum = 255;
    <AC01> = 38;
    <AC11> = 48;
    <LFSH> = 50;
    <RALT> = 108;
};
xkb_types "test" {
    virtual_modifiers LevelThree;
    type "ONE_LEVEL" { modifiers = none; level_name[Level1] = "Any"; };
    type "TWO_LEVEL" { modifiers = Shift; map[Shift] = Level2; level_name[Level1] = "Base"; level_name[Level2] = "Shift"; };
    type "ALPHABETIC" { modifiers = Shift+Lock; map[Shift] = Level2; map[Lock] = Level2; level_name[Level1] = "Base"; level_name[Level2] = "Caps"; };
    type "FOUR_LEVEL" { modifiers = Shift+LevelThree; map[Shift] = Level2; map[LevelThree] = Level3; map[Shift+LevelThree] = Level4; level_name[Level1] = "Base"; level_name[Level2] = "Shift"; level_name[Level3] = "Alt Base"; level_name[Level4] = "Shift Alt"; };
};
xkb_compatibility "test" {
    virtual_modifiers LevelThree;
    interpret ISO_Level3_Shift { action = SetMods(modifiers=LevelThree); };
    interpret Shift_L { action = SetMods(modifiers=Shift); };
};
xkb_symbols "pc+no+us:2+inet(evdev)" {
    name[Group1] = "Norwegian";
    name[Group2] = "English (US)";
    key <AC01> { type = "ALPHABETIC", symbols[Group1] = [ a, A ], symbols[Group2] = [ a, A ] };
    key <AC11> { type[Group1] = "FOUR_LEVEL", type[Group2] = "TWO_LEVEL", symbols[Group1] = [ ae, AE, dead_acute, dead_doubleacute ], symbols[Group2] = [ apostrophe, quotedbl ] };
    key <LFSH> { symbols[Group1] = [ Shift_L ], symbols[Group2] = [ Shift_L ] };
    key <RALT> { symbols[Group1] = [ ISO_Level3_Shift ], symbols[Group2] = [ ISO_Level3_Shift ] };
    modifier_map Shift { <LFSH> };
    modifier_map Mod5 { <RALT> };
};
};
"#;

    #[test]
    fn a_compositor_keymap_is_indexed_per_group() {
        if !available() {
            eprintln!("libxkbcommon is not installed here; skipping");
            return;
        }
        let index = index_keymap(KEYMAP).unwrap();
        assert_eq!(index.group_count(), 2);
        assert_eq!(index.layouts(), ["no", "us"]);
        let (keysym, location) = index.keysym_for_char(0, 'æ').unwrap();
        assert_eq!(keysym, 0xe6);
        assert_eq!((location.keycode, location.level), (48, 0));
        assert_eq!(
            index.keysym_for_char(0, 'Æ').map(|(keysym, _)| keysym),
            Some(0xc6)
        );
        assert_eq!(index.keysym_for_char(1, 'æ'), None);
        assert_eq!(
            index.keysym_for_char(1, '"').map(|(_, at)| at.level),
            Some(1)
        );
        // Dead keys type no character.
        let positions = index.positions(0).unwrap();
        assert_eq!(positions.get(&48).unwrap().base, Some('æ'));
        // Strokes are proven by replaying the keys: Shift by <LFSH>.
        let stroke = |keycode, shift: bool, level3: bool| TextStroke {
            keycode,
            shift: shift.then_some(50),
            level3: level3.then_some(108),
        };
        assert_eq!(
            index.stroke_for_char(Some(0), 'Æ'),
            Some(stroke(48, true, false))
        );
        assert_eq!(
            index.stroke_for_char(Some(1), '"'),
            Some(stroke(48, true, false))
        );
        assert_eq!(index.stroke_for_char(Some(1), 'æ'), None);
        // `a` is the same key in both groups; `æ` is not.
        assert_eq!(
            index.stroke_for_char(None, 'a'),
            Some(stroke(38, false, false))
        );
        assert_eq!(index.stroke_for_char(None, 'æ'), None);
        assert!(!index.groups_alike());
    }

    /// The keymap libxkbcommon compiles from the system's XKB data for
    /// `layout` (as Mutter does), in the text format; `None` without the data.
    fn system_keymap(layout: &str) -> Option<String> {
        let xkb = xkbcommon_option()?;
        if !std::path::Path::new("/usr/share/X11/xkb/rules/evdev").exists() {
            return None;
        }
        let rules = CString::new("evdev").ok()?;
        let model = CString::new("pc105").ok()?;
        let layout = CString::new(layout).ok()?;
        let names = xkbcommon_dl::xkb_rule_names {
            rules: rules.as_ptr(),
            model: model.as_ptr(),
            layout: layout.as_ptr(),
            variant: std::ptr::null(),
            options: std::ptr::null(),
        };
        // SAFETY: test-only; every pointer is checked and the strings
        // outlive the calls. The keymap string is leaked (a test).
        unsafe {
            let context = (xkb.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            if context.is_null() {
                return None;
            }
            let keymap = (xkb.xkb_keymap_new_from_names)(
                context,
                &raw const names,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            (xkb.xkb_context_unref)(context);
            if keymap.is_null() {
                return None;
            }
            let text = (xkb.xkb_keymap_get_as_string)(
                keymap,
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
            );
            (xkb.xkb_keymap_unref)(keymap);
            (!text.is_null()).then(|| {
                std::ffi::CStr::from_ptr(text)
                    .to_string_lossy()
                    .into_owned()
            })
        }
    }

    #[test]
    fn a_uk_keymap_types_less_and_greater_with_shift_not_z_and_x() {
        let Some(keymap) = system_keymap("gb,gb") else {
            eprintln!("libxkbcommon or XKB data missing here; skipping");
            return;
        };
        let index = index_keymap(&keymap).unwrap();
        // `latin` lists `less` first on <AB01> level 3: Mutter's keysym
        // lookup pressed <AB01> bare and typed `z`.
        assert_eq!(
            index.locate(0, 0x3c).map(|at| (at.keycode, at.level)),
            Some((52, 3))
        );
        assert!(index.groups_alike());
        let shifted = |keycode| TextStroke {
            keycode,
            shift: Some(50),
            level3: None,
        };
        assert_eq!(index.stroke_for_char(None, '<'), Some(shifted(59)));
        assert_eq!(index.stroke_for_char(None, '>'), Some(shifted(60)));
        assert_eq!(index.stroke_for_char(None, '£'), Some(shifted(12)));
        for missing in ['å', 'é', 'ü', '😀', 'Ж'] {
            assert_eq!(index.stroke_for_char(Some(0), missing), None, "{missing}");
        }
    }

    #[test]
    fn a_norwegian_keymap_types_altgr_characters_with_altgr() {
        let Some(keymap) = system_keymap("no,gb") else {
            return;
        };
        let index = index_keymap(&keymap).unwrap();
        assert!(!index.groups_alike());
        let stroke = |keycode, shift: bool, level3: bool| TextStroke {
            keycode,
            shift: shift.then_some(50),
            level3: level3.then_some(108),
        };
        assert_eq!(
            index.stroke_for_char(Some(0), '@'),
            Some(stroke(11, false, true))
        );
        assert_eq!(
            index.stroke_for_char(Some(0), 'æ'),
            Some(stroke(48, false, false))
        );
        assert_eq!(
            index.stroke_for_char(Some(0), 'Å'),
            Some(stroke(34, true, false))
        );
        assert_eq!(
            index.stroke_for_char(Some(0), '<'),
            Some(stroke(94, false, false))
        );
        // `gb` has `æ` too, on AltGr+A.
        assert_eq!(
            index.stroke_for_char(Some(1), 'æ'),
            Some(stroke(38, false, true))
        );
        // `æ` is only in the Norwegian group: unknown group, no stroke.
        assert_eq!(index.stroke_for_char(None, 'æ'), None);
        assert_eq!(
            index.stroke_for_char(None, 'a'),
            Some(stroke(38, false, false))
        );
    }

    #[test]
    fn a_broken_keymap_is_refused() {
        if !available() {
            return;
        }
        assert_eq!(
            index_keymap("xkb_keymap { nonsense").err(),
            Some(XkbIndexError::Compile)
        );
        assert_eq!(index_keymap("a\0b").err(), Some(XkbIndexError::InvalidText));
    }
}
