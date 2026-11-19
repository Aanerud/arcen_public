// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The PC keys outside the main block and F1–F12, and how each Pier presses
//! them: Insert, Print Screen, Scroll Lock, Pause, Menu and F13–F24.
//!
//! These are the keys a Deck's Send Key menu offers and a PC keyboard (or a
//! Mac's F13–F19) sends that a host injects by a code of its own rather than
//! by a main-block position. One table holds every Pier's code for them, so
//! a Pier's injection, its hello ([`advertised_pc_keys`]) and a Deck's
//! reading of that hello ([`host_accepts`]) cannot disagree.
//!
//! A key a platform has no code for is `None` in its column and never
//! substituted: macOS has no Insert (the key in its place is Help), Print
//! Screen, Scroll Lock or Pause, and no virtual key code beyond `kVK_F20`.

use arcen_protocol::messages::{HostPlatform, InputCapabilityAvailability, PcKeysMsg};

use super::pc::{KEY_F1, KEY_MENU, KEY_PAUSE, KEY_PRINT, KEY_SCROLL_LOCK};
use super::scancode::Set1Scancode;

/// `Qt::Key_Insert`.
pub const KEY_INSERT: u32 = 0x0100_0006;
/// `Qt::Key_F13`; F14–F24 follow it.
pub const KEY_F13: u32 = KEY_F1 + 12;
/// `Qt::Key_F24`.
pub const KEY_F24: u32 = KEY_F1 + 23;

/// One key and its code on every Pier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcHostKey {
    /// The key identifier a Deck sends.
    pub qt_key: u32,
    /// The key's name in a menu or a log.
    pub name: &'static str,
    /// The PC keyboard's set 1 make code (Pause's is its Break code, `E0 46`).
    pub set1: Set1Scancode,
    /// The Linux evdev code a real keyboard reports. Print Screen's is
    /// `KEY_SYSRQ`, which a kernel virtual keyboard never sends: the Linux
    /// Pier presses `KEY_PRINT` instead (`kernel_safe_key`).
    pub evdev: u16,
    /// The Windows Virtual-Key.
    pub windows_vk: u16,
    /// Whether `SendInput` marks the Virtual-Key extended.
    pub windows_extended: bool,
    /// The macOS virtual key code (`kVK_*`), where macOS has the key.
    pub macos_key: Option<u16>,
}

const fn plain(code: u8) -> Set1Scancode {
    Set1Scancode {
        code,
        extended: false,
    }
}

const fn extended(code: u8) -> Set1Scancode {
    Set1Scancode {
        code,
        extended: true,
    }
}

const fn function_key(
    number: u8,
    set1: u8,
    windows_vk: u16,
    macos_key: Option<u16>,
    name: &'static str,
) -> PcHostKey {
    PcHostKey {
        qt_key: KEY_F1 + number as u32 - 1,
        name,
        set1: plain(set1),
        // `KEY_F13` (183) to `KEY_F24` (194).
        evdev: 183 + number as u16 - 13,
        windows_vk,
        windows_extended: false,
        macos_key,
    }
}

/// Every key, in Send Key menu order.
pub const PC_HOST_KEYS: [PcHostKey; 17] = [
    PcHostKey {
        qt_key: KEY_INSERT,
        name: "Insert",
        set1: extended(0x52),
        evdev: 110,
        windows_vk: 0x2D,
        windows_extended: true,
        macos_key: None,
    },
    PcHostKey {
        qt_key: KEY_PRINT,
        name: "Print Screen",
        set1: extended(0x37),
        evdev: 99,
        windows_vk: 0x2C,
        windows_extended: false,
        macos_key: None,
    },
    PcHostKey {
        qt_key: KEY_SCROLL_LOCK,
        name: "Scroll Lock",
        set1: plain(0x46),
        evdev: 70,
        windows_vk: 0x91,
        windows_extended: false,
        macos_key: None,
    },
    PcHostKey {
        qt_key: KEY_PAUSE,
        name: "Pause",
        set1: extended(0x46),
        evdev: 119,
        windows_vk: 0x13,
        windows_extended: false,
        macos_key: None,
    },
    // The PC's Menu key is `KEY_COMPOSE` to Linux (keysym `Menu` in every
    // stock keymap; `KEY_MENU` would be `XF86MenuKB`), `VK_APPS` to Windows,
    // and `kVK_ContextualMenu`, the code macOS gives a PC keyboard's Menu
    // key, to a Mac.
    PcHostKey {
        qt_key: KEY_MENU,
        name: "Menu",
        set1: extended(0x5D),
        evdev: 127,
        windows_vk: 0x5D,
        windows_extended: true,
        macos_key: Some(0x6E),
    },
    function_key(13, 0x64, 0x7C, Some(0x69), "F13"),
    function_key(14, 0x65, 0x7D, Some(0x6B), "F14"),
    function_key(15, 0x66, 0x7E, Some(0x71), "F15"),
    function_key(16, 0x67, 0x7F, Some(0x6A), "F16"),
    function_key(17, 0x68, 0x80, Some(0x40), "F17"),
    function_key(18, 0x69, 0x81, Some(0x4F), "F18"),
    function_key(19, 0x6A, 0x82, Some(0x50), "F19"),
    function_key(20, 0x6B, 0x83, Some(0x5A), "F20"),
    function_key(21, 0x6C, 0x84, None, "F21"),
    function_key(22, 0x6D, 0x85, None, "F22"),
    function_key(23, 0x6E, 0x86, None, "F23"),
    function_key(24, 0x76, 0x87, None, "F24"),
];

/// The table's entry for a key identifier, whatever the modifiers (a
/// keypad's Insert, with Num Lock off, is Insert too); `None` for any other
/// key.
#[must_use]
pub fn pc_host_key(qt_key: u32) -> Option<&'static PcHostKey> {
    PC_HOST_KEYS.iter().find(|key| key.qt_key == qt_key)
}

/// Whether a Pier on `platform` injects `key`.
fn platform_has(platform: HostPlatform, key: &PcHostKey) -> Option<bool> {
    match platform {
        HostPlatform::Linux | HostPlatform::Windows => Some(true),
        HostPlatform::MacOs => Some(key.macos_key.is_some()),
        HostPlatform::Unknown => None,
    }
}

const fn availability(has: bool) -> InputCapabilityAvailability {
    if has {
        InputCapabilityAvailability::Available
    } else {
        InputCapabilityAvailability::Unavailable
    }
}

/// What a Pier on `platform` says in its hello: the keys its column of
/// [`PC_HOST_KEYS`] has. Unknown for [`HostPlatform::Unknown`].
#[must_use]
pub fn advertised_pc_keys(platform: HostPlatform) -> PcKeysMsg {
    let has = |qt_key: u32| {
        pc_host_key(qt_key)
            .and_then(|key| platform_has(platform, key))
            .map_or(InputCapabilityAvailability::Unknown, availability)
    };
    if platform == HostPlatform::Unknown {
        return PcKeysMsg::default();
    }
    let last_function_key = (13..=24)
        .take_while(|number| has(KEY_F1 + number - 1) == InputCapabilityAvailability::Available)
        .last()
        .map_or(12, |number| u8::try_from(number).unwrap_or(12));
    PcKeysMsg {
        insert: has(KEY_INSERT),
        print_screen: has(KEY_PRINT),
        scroll_lock: has(KEY_SCROLL_LOCK),
        pause: has(KEY_PAUSE),
        menu: has(KEY_MENU),
        last_function_key,
    }
}

/// What a Pier of `platform` injected before its hello said: Linux and
/// Windows Piers took Insert, Print Screen, Scroll Lock and Pause, no Pier
/// took Menu or F13–F24, and the macOS Pier none of them.
fn legacy_accepts(platform: HostPlatform, qt_key: u32) -> Option<bool> {
    match platform {
        HostPlatform::Linux | HostPlatform::Windows => Some(matches!(
            qt_key,
            KEY_INSERT | KEY_PRINT | KEY_SCROLL_LOCK | KEY_PAUSE
        )),
        HostPlatform::MacOs => Some(false),
        HostPlatform::Unknown => None,
    }
}

/// Whether a host injects `qt_key`, from its hello's `pc_keys` and
/// platform: what a Deck greys out in its Send Key menu. `None` for a key
/// outside [`PC_HOST_KEYS`], and when neither the hello nor the platform
/// says.
#[must_use]
pub fn host_accepts(keys: &PcKeysMsg, platform: HostPlatform, qt_key: u32) -> Option<bool> {
    pc_host_key(qt_key)?;
    let stated = match qt_key {
        KEY_INSERT => keys.insert,
        KEY_PRINT => keys.print_screen,
        KEY_SCROLL_LOCK => keys.scroll_lock,
        KEY_PAUSE => keys.pause,
        KEY_MENU => keys.menu,
        _ if keys.last_function_key == 0 => InputCapabilityAvailability::Unknown,
        _ => availability(qt_key - KEY_F1 < u32::from(keys.last_function_key)),
    };
    match stated {
        InputCapabilityAvailability::Available => Some(true),
        InputCapabilityAvailability::Unavailable => Some(false),
        InputCapabilityAvailability::Unknown => legacy_accepts(platform, qt_key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::pc::{LockKey, PcKey, pc_key};

    const PLATFORMS: [HostPlatform; 4] = [
        HostPlatform::Linux,
        HostPlatform::Windows,
        HostPlatform::MacOs,
        HostPlatform::Unknown,
    ];

    #[test]
    fn a_pc_keyboard_sends_each_key_as_its_table_identifier() {
        for key in PC_HOST_KEYS {
            let expected = if key.qt_key == KEY_SCROLL_LOCK {
                PcKey::Lock(LockKey::Scroll)
            } else {
                PcKey::Position {
                    qt_key: key.qt_key,
                    keypad: false,
                }
            };
            assert_eq!(pc_key(key.set1), expected, "{}", key.name);
        }
    }

    #[test]
    fn function_keys_have_their_own_codes_on_every_pier() {
        for (index, number) in (13..=24_u32).enumerate() {
            let key = pc_host_key(KEY_F1 + number - 1).expect("F13-F24");
            assert_eq!(key.name, format!("F{number}"));
            assert_eq!(u32::from(key.evdev), 183 + u32::try_from(index).unwrap());
            assert_eq!(
                u32::from(key.windows_vk),
                0x7C + u32::try_from(index).unwrap()
            );
        }
        // kVK_F13 ... kVK_F20 (HIToolbox `Events.h`); nothing beyond.
        let mac: Vec<Option<u16>> = (13..=24)
            .map(|number| pc_host_key(KEY_F1 + number - 1).unwrap().macos_key)
            .collect();
        assert_eq!(
            mac,
            [
                Some(0x69),
                Some(0x6B),
                Some(0x71),
                Some(0x6A),
                Some(0x40),
                Some(0x4F),
                Some(0x50),
                Some(0x5A),
                None,
                None,
                None,
                None
            ]
        );
    }

    #[test]
    fn no_code_is_a_kernel_or_power_key() {
        // Power, sleep, wake, suspend, power2; SysRq only as Print Screen's
        // real-keyboard code, which the Linux Pier never sends.
        for key in PC_HOST_KEYS {
            assert!(
                ![116, 142, 143, 205, 356].contains(&key.evdev),
                "{}",
                key.name
            );
            assert!(key.evdev != 99 || key.qt_key == KEY_PRINT, "{}", key.name);
            // VK_SLEEP and the browser/media/launch range.
            assert!(
                key.windows_vk != 0x5F && !(0xA6..=0xB7).contains(&key.windows_vk),
                "{}",
                key.name
            );
        }
    }

    #[test]
    fn codes_are_unique_per_pier() {
        let mut evdev: Vec<u16> = PC_HOST_KEYS.iter().map(|key| key.evdev).collect();
        let mut vk: Vec<u16> = PC_HOST_KEYS.iter().map(|key| key.windows_vk).collect();
        let mut mac: Vec<u16> = PC_HOST_KEYS
            .iter()
            .filter_map(|key| key.macos_key)
            .collect();
        for codes in [&mut evdev, &mut vk, &mut mac] {
            let count = codes.len();
            codes.sort_unstable();
            codes.dedup();
            assert_eq!(codes.len(), count);
        }
    }

    #[test]
    fn each_pier_advertises_its_column() {
        let all = PcKeysMsg {
            insert: InputCapabilityAvailability::Available,
            print_screen: InputCapabilityAvailability::Available,
            scroll_lock: InputCapabilityAvailability::Available,
            pause: InputCapabilityAvailability::Available,
            menu: InputCapabilityAvailability::Available,
            last_function_key: 24,
        };
        assert_eq!(advertised_pc_keys(HostPlatform::Linux), all);
        assert_eq!(advertised_pc_keys(HostPlatform::Windows), all);
        let unavailable = InputCapabilityAvailability::Unavailable;
        assert_eq!(
            advertised_pc_keys(HostPlatform::MacOs),
            PcKeysMsg {
                insert: unavailable,
                print_screen: unavailable,
                scroll_lock: unavailable,
                pause: unavailable,
                menu: InputCapabilityAvailability::Available,
                last_function_key: 20,
            }
        );
        assert!(advertised_pc_keys(HostPlatform::Unknown).is_unknown());
    }

    #[test]
    fn a_deck_reads_a_hello_as_the_pier_meant_it() {
        for platform in PLATFORMS {
            let hello = advertised_pc_keys(platform);
            for key in PC_HOST_KEYS {
                assert_eq!(
                    host_accepts(&hello, platform, key.qt_key),
                    platform_has(platform, &key),
                    "{platform:?} {}",
                    key.name
                );
            }
        }
        assert_eq!(
            host_accepts(&PcKeysMsg::default(), HostPlatform::Linux, KEY_F1),
            None
        );
    }

    #[test]
    fn a_host_that_predates_the_hello_gets_what_it_always_took() {
        let legacy = PcKeysMsg::default();
        for platform in [HostPlatform::Linux, HostPlatform::Windows] {
            for qt_key in [KEY_INSERT, KEY_PRINT, KEY_SCROLL_LOCK, KEY_PAUSE] {
                assert_eq!(host_accepts(&legacy, platform, qt_key), Some(true));
            }
            for qt_key in [KEY_MENU, KEY_F13, KEY_F24] {
                assert_eq!(host_accepts(&legacy, platform, qt_key), Some(false));
            }
        }
        for key in PC_HOST_KEYS {
            assert_eq!(
                host_accepts(&legacy, HostPlatform::MacOs, key.qt_key),
                Some(false)
            );
            assert_eq!(
                host_accepts(&legacy, HostPlatform::Unknown, key.qt_key),
                None
            );
        }
    }

    #[test]
    fn only_the_tables_keys_are_in_it() {
        assert_eq!(pc_host_key(0x41), None);
        assert_eq!(pc_host_key(KEY_F1 + 11), None);
        assert_eq!(pc_host_key(KEY_F24 + 1), None);
        assert_eq!(pc_host_key(KEY_F24).map(|key| key.set1), Some(plain(0x76)));
    }
}
