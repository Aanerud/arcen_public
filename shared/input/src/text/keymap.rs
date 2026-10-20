//! An index of a host keyboard layout: which keysyms each key types at each
//! level, and which characters a compositor can therefore type by keysym.
//!
//! The rule is the GNOME compositor's (Mutter 40–49, and gnome-remote-desktop
//! for RDP Unicode input): a keysym is looked up **in the active layout group
//! only**, scanning keycodes upwards from the keymap's minimum (excluding its
//! maximum) and each key's levels upwards; the first match wins. It is typed
//! only at level 0, at level 1 when the group has a `Shift_L` key, or at
//! level 2 when the group has an `ISO_Level3_Shift` key; any other match is
//! dropped. A host that predicts with the same rule knows, before it sends a
//! keysym, whether the compositor will type it.
//!
//! Entries come from a platform adapter that read the compositor's keymap
//! (libxkbcommon on Linux); the index itself is plain data.

use std::collections::{BTreeMap, HashMap};

/// `Shift_L`.
pub const KEYSYM_SHIFT_L: u32 = 0xffe1;
/// `ISO_Level3_Shift` (the `AltGr` key).
pub const KEYSYM_ISO_LEVEL3_SHIFT: u32 = 0xfe03;

/// One keysym at one level of one key in one layout group, as the keymap
/// lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeymapEntry {
    /// XKB keycode (evdev code + 8).
    pub keycode: u32,
    /// Layout group, zero-based.
    pub group: u32,
    /// Shift level, zero-based.
    pub level: u32,
    /// The keysym.
    pub keysym: u32,
    /// The character the keysym stands for, if any (`xkb_keysym_to_utf32`).
    pub character: Option<char>,
}

/// Where a keysym is typed: a key and the level it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyLocation {
    /// XKB keycode.
    pub keycode: u32,
    /// Shift level, zero-based.
    pub level: u32,
}

impl KeyLocation {
    /// The Linux evdev code of the key.
    #[must_use]
    pub const fn evdev_code(self) -> u32 {
        self.keycode.saturating_sub(8)
    }
}

/// The characters a key position types unmodified and with Shift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PositionChars {
    /// Level 0.
    pub base: Option<char>,
    /// Level 1.
    pub shift: Option<char>,
}

#[derive(Debug, Clone, Default)]
struct GroupIndex {
    first_location: HashMap<u32, KeyLocation>,
    keysyms_for_char: HashMap<char, Vec<u32>>,
    positions: BTreeMap<u32, PositionChars>,
    has_shift_l: bool,
    has_level3_shift: bool,
}

/// A host keymap, indexed per layout group.
#[derive(Debug, Clone, Default)]
pub struct KeymapIndex {
    groups: Vec<GroupIndex>,
    layouts: Vec<String>,
}

/// Keypad keysyms (`KP_Space` .. `KP_9`): never used to type text, since
/// what they type depends on Num Lock.
const fn is_keypad_keysym(keysym: u32) -> bool {
    matches!(keysym, 0xff80..=0xffbd)
}

impl KeymapIndex {
    /// Builds the index from `entries`, listed per key in level order (and
    /// within a level in the keymap's own symbol order). Keycodes at or
    /// above `max_keycode` are not scanned, as the compositor does not scan
    /// them. `layouts` names the groups, in order, when known.
    #[must_use]
    pub fn new(
        entries: impl IntoIterator<Item = KeymapEntry>,
        max_keycode: u32,
        layouts: Vec<String>,
    ) -> Self {
        let mut entries: Vec<(usize, KeymapEntry)> = entries
            .into_iter()
            .filter(|entry| entry.keycode < max_keycode)
            .enumerate()
            .collect();
        // Scan order: group, keycode up, level up, then listed order.
        entries.sort_by_key(|(order, entry)| (entry.group, entry.keycode, entry.level, *order));
        let group_count = entries
            .iter()
            .map(|(_, entry)| entry.group as usize + 1)
            .max()
            .unwrap_or(0)
            .max(layouts.len());
        let mut groups = vec![GroupIndex::default(); group_count];
        for (_, entry) in entries {
            let group = &mut groups[entry.group as usize];
            group
                .first_location
                .entry(entry.keysym)
                .or_insert(KeyLocation {
                    keycode: entry.keycode,
                    level: entry.level,
                });
            if entry.keysym == KEYSYM_SHIFT_L {
                group.has_shift_l = true;
            }
            if entry.keysym == KEYSYM_ISO_LEVEL3_SHIFT {
                group.has_level3_shift = true;
            }
            if let Some(character) = entry.character {
                if !is_keypad_keysym(entry.keysym) && !character.is_control() {
                    let keysyms = group.keysyms_for_char.entry(character).or_default();
                    if !keysyms.contains(&entry.keysym) {
                        keysyms.push(entry.keysym);
                    }
                }
            }
            if entry.level <= 1 {
                let position = group.positions.entry(entry.keycode).or_default();
                let slot = if entry.level == 0 {
                    &mut position.base
                } else {
                    &mut position.shift
                };
                if slot.is_none() {
                    *slot = entry.character.filter(|character| !character.is_control());
                }
            }
        }
        Self { groups, layouts }
    }

    /// Number of layout groups.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// The groups' layout names, in order, when known.
    #[must_use]
    pub fn layouts(&self) -> &[String] {
        &self.layouts
    }

    /// The first key and level listing `keysym` in `group`, deliverable or
    /// not: what the compositor would pick.
    #[must_use]
    pub fn locate(&self, group: u32, keysym: u32) -> Option<KeyLocation> {
        self.groups
            .get(group as usize)?
            .first_location
            .get(&keysym)
            .copied()
    }

    /// Where the compositor types `keysym` in `group`, if it can.
    #[must_use]
    pub fn deliverable(&self, group: u32, keysym: u32) -> Option<KeyLocation> {
        let index = self.groups.get(group as usize)?;
        let location = index.first_location.get(&keysym).copied()?;
        let typeable = match location.level {
            0 => true,
            1 => index.has_shift_l,
            2 => index.has_level3_shift,
            _ => false,
        };
        typeable.then_some(location)
    }

    /// The keysym that types `character` in `group`, and where, if one does.
    /// Of several keysyms standing for the character, the first deliverable
    /// one in scan order is chosen. Keypad keysyms never are.
    #[must_use]
    pub fn keysym_for_char(&self, group: u32, character: char) -> Option<(u32, KeyLocation)> {
        let index = self.groups.get(group as usize)?;
        index
            .keysyms_for_char
            .get(&character)?
            .iter()
            .find_map(|keysym| {
                self.deliverable(group, *keysym)
                    .map(|location| (*keysym, location))
            })
    }

    /// The characters each key of `group` types at levels 0 and 1, by XKB
    /// keycode.
    #[must_use]
    pub fn positions(&self, group: u32) -> Option<&BTreeMap<u32, PositionChars>> {
        self.groups
            .get(group as usize)
            .map(|index| &index.positions)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn entry(keycode: u32, group: u32, level: u32, keysym: u32) -> KeymapEntry {
        KeymapEntry {
            keycode,
            group,
            level,
            keysym,
            character: keysym_char(keysym),
        }
    }

    /// The few keysyms the tests use.
    fn keysym_char(keysym: u32) -> Option<char> {
        match keysym {
            0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym),
            0x0100_0000..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000),
            0x20ac => Some('€'),
            0xffb1 => Some('1'),
            _ => None,
        }
    }

    /// A Norwegian-shaped group 0 and a US-shaped group 1.
    pub(crate) fn norwegian_and_us() -> KeymapIndex {
        KeymapIndex::new(
            [
                // <LFSH> Shift_L in both groups.
                entry(50, 0, 0, KEYSYM_SHIFT_L),
                entry(50, 1, 0, KEYSYM_SHIFT_L),
                // <RALT> ISO_Level3_Shift in group 0 only.
                entry(108, 0, 0, KEYSYM_ISO_LEVEL3_SHIFT),
                // <AE02>: 2 " @ (no); 2 @ (us).
                entry(11, 0, 0, 0x32),
                entry(11, 0, 1, 0x22),
                entry(11, 0, 2, 0x40),
                entry(11, 1, 0, 0x32),
                entry(11, 1, 1, 0x40),
                // <AC01>: a A.
                entry(38, 0, 0, 0x61),
                entry(38, 0, 1, 0x41),
                entry(38, 1, 0, 0x61),
                entry(38, 1, 1, 0x41),
                // <AC11>: æ Æ (no); ' " (us).
                entry(48, 0, 0, 0xe6),
                entry(48, 0, 1, 0xc6),
                entry(48, 1, 0, 0x27),
                entry(48, 1, 1, 0x22),
                // <AE05> level 3 (Shift+AltGr) only: ‰ U+2030.
                entry(14, 0, 0, 0x35),
                entry(14, 0, 3, 0x0100_2030),
                // <AE04> level 2: €.
                entry(13, 0, 0, 0x34),
                entry(13, 0, 2, 0x20ac),
                // Keypad 1.
                entry(87, 0, 1, 0xffb1),
                // A key at the maximum keycode, never scanned.
                entry(255, 0, 0, 0xf8),
            ],
            255,
            vec!["no".to_owned(), "us".to_owned()],
        )
    }

    #[test]
    fn lookup_follows_the_compositor_rule() {
        let index = norwegian_and_us();
        assert_eq!(index.group_count(), 2);
        assert_eq!(
            index.keysym_for_char(0, 'æ'),
            Some((
                0xe6,
                KeyLocation {
                    keycode: 48,
                    level: 0
                }
            ))
        );
        assert_eq!(
            index.keysym_for_char(0, 'Æ'),
            Some((
                0xc6,
                KeyLocation {
                    keycode: 48,
                    level: 1
                }
            ))
        );
        // The active group only.
        assert_eq!(index.keysym_for_char(1, 'æ'), None);
        // AltGr level, present in group 0.
        assert_eq!(
            index.keysym_for_char(0, '@'),
            Some((
                0x40,
                KeyLocation {
                    keycode: 11,
                    level: 2
                }
            ))
        );
        assert_eq!(
            index.keysym_for_char(0, '€'),
            Some((
                0x20ac,
                KeyLocation {
                    keycode: 13,
                    level: 2
                }
            ))
        );
        // Level 3 is never typed.
        assert_eq!(
            index.locate(0, 0x0100_2030).map(|location| location.level),
            Some(3)
        );
        assert_eq!(index.keysym_for_char(0, '‰'), None);
        // The maximum keycode is not scanned: ø is not on this keymap.
        assert_eq!(index.keysym_for_char(0, 'ø'), None);
        // Keypad keysyms never type text.
        assert_eq!(index.keysym_for_char(0, '1'), None);
    }

    #[test]
    fn levels_need_their_modifier_key_in_the_group() {
        // Group 1 has no ISO_Level3_Shift: a level-2 symbol is not typeable.
        let index = KeymapIndex::new(
            [
                entry(50, 0, 0, KEYSYM_SHIFT_L),
                entry(11, 0, 0, 0x32),
                entry(11, 0, 2, 0x40),
                entry(38, 0, 1, 0x41),
            ],
            255,
            Vec::new(),
        );
        assert_eq!(index.keysym_for_char(0, '@'), None);
        assert_eq!(
            index.keysym_for_char(0, 'A'),
            Some((
                0x41,
                KeyLocation {
                    keycode: 38,
                    level: 1
                }
            ))
        );
        let without_shift = KeymapIndex::new([entry(38, 0, 1, 0x41)], 255, Vec::new());
        assert_eq!(without_shift.keysym_for_char(0, 'A'), None);
    }

    #[test]
    fn the_first_listing_wins_even_when_it_cannot_be_typed() {
        // `@` first appears at level 3 of keycode 11, then at level 0 of
        // keycode 20: the compositor picks keycode 11 and cannot type it.
        let index = KeymapIndex::new(
            [
                entry(11, 0, 3, 0x40),
                entry(20, 0, 0, 0x40),
                entry(50, 0, 0, KEYSYM_SHIFT_L),
            ],
            255,
            Vec::new(),
        );
        assert_eq!(
            index.locate(0, 0x40).map(|location| location.keycode),
            Some(11)
        );
        assert_eq!(index.keysym_for_char(0, '@'), None);
    }

    #[test]
    fn another_keysym_for_the_same_character_is_tried() {
        // `€` as the legacy EuroSign at an untypeable level, and as its
        // Unicode keysym at level 0 of another key.
        let index = KeymapIndex::new(
            [entry(13, 0, 4, 0x20ac), entry(26, 0, 0, 0x0100_20ac)],
            255,
            Vec::new(),
        );
        assert_eq!(
            index.keysym_for_char(0, '€'),
            Some((
                0x0100_20ac,
                KeyLocation {
                    keycode: 26,
                    level: 0
                }
            ))
        );
    }

    #[test]
    fn positions_carry_base_and_shift_characters() {
        let index = norwegian_and_us();
        let positions = index.positions(0).unwrap();
        assert_eq!(
            positions.get(&48),
            Some(&PositionChars {
                base: Some('æ'),
                shift: Some('Æ')
            })
        );
        assert_eq!(
            index.positions(1).unwrap().get(&48),
            Some(&PositionChars {
                base: Some('\''),
                shift: Some('"')
            })
        );
        assert_eq!(
            KeyLocation {
                keycode: 48,
                level: 0
            }
            .evdev_code(),
            40,
            "evdev KEY_APOSTROPHE"
        );
    }
}
