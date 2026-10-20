//! The compositor's keymap, compiled with the system's libxkbcommon and
//! indexed for committed text.
//!
//! libxkbcommon is loaded at run time (`xkbcommon-dl`): a host without it
//! types committed text unverified, as before it had a keymap. The keymap is
//! read exactly as the compositor wrote it, so the index lists the same
//! keysyms at the same levels the compositor's own lookup scans.

use std::ffi::CString;

use arcen_input::text::{
    layouts_from_symbols_section, symbols_section_name, KeymapEntry, KeymapIndex,
};
use xkbcommon_dl::{
    xkb_context_flags, xkb_keymap_compile_flags, xkb_keymap_format, xkbcommon_option,
};

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
    let max_keycode = unsafe {
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
        (xkb.xkb_keymap_unref)(keymap);
        max
    };
    let layouts = symbols_section_name(text)
        .map(layouts_from_symbols_section)
        .unwrap_or_default();
    Ok(KeymapIndex::new(entries, max_keycode, layouts))
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
