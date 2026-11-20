// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The one place shared screens name the machine the Deck runs on.
//!
//! Every other string is the same on both Decks. A shell says once which
//! Deck it is ([`set_deck_platform`]) and the screens ask
//! ([`deck_platform`]) when they name "this Mac" or "this PC".

/// Which Deck draws the screens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeckPlatform {
    /// The macOS Deck (the default, so a Mac that never sets it is unchanged).
    #[default]
    Mac,
    /// The Windows Deck.
    Pc,
}

impl DeckPlatform {
    /// "this Mac" / "this PC".
    #[must_use]
    pub const fn this_machine(self) -> &'static str {
        match self {
            Self::Mac => "this Mac",
            Self::Pc => "this PC",
        }
    }

    /// "This Mac" / "This PC", at the start of a sentence.
    #[must_use]
    pub const fn this_machine_capitalised(self) -> &'static str {
        match self {
            Self::Mac => "This Mac",
            Self::Pc => "This PC",
        }
    }

    /// "this Mac's" / "this PC's".
    #[must_use]
    pub const fn this_machines(self) -> &'static str {
        match self {
            Self::Mac => "this Mac's",
            Self::Pc => "this PC's",
        }
    }

    /// "Mac" / "PC", as in "Mac applications".
    #[must_use]
    pub const fn machine(self) -> &'static str {
        match self {
            Self::Mac => "Mac",
            Self::Pc => "PC",
        }
    }
}

use arcen_input::keyboard::DeckShortcut;

fn platform_id() -> egui::Id {
    egui::Id::new("arcen-deck-platform")
}

/// Records which Deck this is; call once when the egui context is created.
pub fn set_deck_platform(ctx: &egui::Context, platform: DeckPlatform) {
    ctx.data_mut(|data| data.insert_temp(platform_id(), platform));
}

/// Which Deck this is ([`DeckPlatform::Mac`] until a shell says otherwise).
#[must_use]
pub fn deck_platform(ctx: &egui::Context) -> DeckPlatform {
    ctx.data(|data| data.get_temp::<DeckPlatform>(platform_id()))
        .unwrap_or_default()
}

fn keyboard_switch_id() -> egui::Id {
    egui::Id::new("arcen-deck-keyboard-switch")
}

/// Records the keyboard switch's chord (Settings ▸ Input ▸ Keyboard switch)
/// for the menus and help that name it; call when settings load or change.
pub fn set_keyboard_switch(
    ctx: &egui::Context,
    switch: arcen_input::keyboard::KeyboardSwitchChord,
) {
    ctx.data_mut(|data| data.insert_temp(keyboard_switch_id(), switch));
}

/// The keyboard switch's chord (the default until a shell says otherwise).
#[must_use]
pub fn keyboard_switch(ctx: &egui::Context) -> arcen_input::keyboard::KeyboardSwitchChord {
    ctx.data(|data| data.get_temp(keyboard_switch_id()))
        .unwrap_or_default()
}

/// A Deck chord as the menus spell it: "Control-Option-Home" on a Mac,
/// "Ctrl+Alt+Home" on a PC (PARITY §4.1); empty for an action without one.
pub fn chord(platform: DeckPlatform, shortcut: DeckShortcut) -> String {
    match platform {
        DeckPlatform::Mac => shortcut.mac_chord().map_or_else(String::new, |chord| {
            let mut parts = Vec::new();
            if chord.control {
                parts.push("Control".to_owned());
            }
            if chord.option {
                parts.push("Option".to_owned());
            }
            if chord.shift {
                parts.push("Shift".to_owned());
            }
            if chord.command {
                parts.push("Command".to_owned());
            }
            parts.push(key_name(chord.key));
            parts.join("-")
        }),
        DeckPlatform::Pc => shortcut
            .pc_chords()
            .first()
            .map_or_else(String::new, |chord| {
                let mut parts = Vec::new();
                if chord.control {
                    parts.push("Ctrl".to_owned());
                }
                if chord.alt {
                    parts.push("Alt".to_owned());
                }
                if chord.shift {
                    parts.push("Shift".to_owned());
                }
                if chord.windows {
                    parts.push("Win".to_owned());
                }
                parts.push(key_name(chord.key));
                parts.join("+")
            }),
    }
}

/// The printed name of a Qt key code in a chord.
fn key_name(key: u32) -> String {
    use arcen_input::keyboard::pc::{KEY_END, KEY_HOME, KEY_PAUSE};
    const KEY_F1: u32 = 0x0100_0030;
    const KEY_F35: u32 = 0x0100_0052;
    match key {
        0x20..=0x7E => char::from_u32(key).map_or_else(String::new, |c| c.to_string()),
        KEY_F1..=KEY_F35 => format!("F{}", key - KEY_F1 + 1),
        KEY_PAUSE => "Break".to_owned(),
        KEY_END => "End".to_owned(),
        KEY_HOME => "Home".to_owned(),
        _ => format!("{key:#x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_names_its_machine_once_and_every_screen_sees_it() {
        let ctx = egui::Context::default();
        assert_eq!(deck_platform(&ctx).this_machine(), "this Mac");
        set_deck_platform(&ctx, DeckPlatform::Pc);
        assert_eq!(deck_platform(&ctx).this_machine(), "this PC");
        assert_eq!(deck_platform(&ctx).this_machines(), "this PC's");
    }
}
