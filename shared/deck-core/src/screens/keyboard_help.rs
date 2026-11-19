// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Who has the keyboard, in words, and Help ▸ Keyboard Shortcuts.
//!
//! Both Decks show the same wording, generated from the shared tables
//! (`arcen_input::keyboard::{DeckShortcut, SendKey, KeyboardCapturePolicy}`),
//! so the help can never drift from what the keyboard does.

use arcen_input::keyboard::{DeckShortcut, KeyboardCapturePolicy, KeyboardStatus, SendKey};

use super::wording::{DeckPlatform, chord};

/// The indicator: "Keyboard: Z8 (Ctrl+Alt+Home to release)" while the host
/// has it, "Keyboard: this PC (click to return)" while this computer does;
/// empty outside a session.
#[must_use]
pub fn keyboard_status_line(status: KeyboardStatus, host: &str, platform: DeckPlatform) -> String {
    let escape = chord(platform, DeckShortcut::ReleaseKeyboard);
    match status {
        KeyboardStatus::NoSession => String::new(),
        KeyboardStatus::Host {
            system_shortcuts: true,
        } => format!("Keyboard: {host} ({escape} to release)"),
        KeyboardStatus::Host {
            system_shortcuts: false,
        } => format!(
            "Keyboard: {host}, system shortcuts on {} ({escape} to release)",
            platform.this_machine()
        ),
        KeyboardStatus::ThisComputer => {
            format!("Keyboard: {} (click to return)", platform.this_machine())
        }
    }
}

/// A Settings choice's label.
#[must_use]
pub const fn capture_policy_label(policy: KeyboardCapturePolicy) -> &'static str {
    match policy {
        KeyboardCapturePolicy::Host => "The remote computer",
        KeyboardCapturePolicy::Fullscreen => "The remote computer when fullscreen",
        KeyboardCapturePolicy::ThisComputer => "This computer",
    }
}

/// The line under the Settings choice: what goes where, and the way out.
#[must_use]
pub fn capture_policy_help(policy: KeyboardCapturePolicy, platform: DeckPlatform) -> String {
    let shortcuts = match platform {
        DeckPlatform::Pc => "Alt+Tab, Alt+Shift+Tab, the Windows key and Print Screen",
        DeckPlatform::Mac => "⌘Tab, ⌘Space and Mission Control",
    };
    let escape = chord(platform, DeckShortcut::ReleaseKeyboard);
    let machine = platform.this_machine();
    match policy {
        KeyboardCapturePolicy::Host => format!(
            "{shortcuts} go to the remote computer while a session window has focus, windowed \
             or fullscreen. {escape} gives the keyboard back to {machine}; click the picture \
             or press {escape} again to return."
        ),
        KeyboardCapturePolicy::Fullscreen => format!(
            "{shortcuts} go to the remote computer while the session is fullscreen, and stay \
             on {machine} while it is windowed. {escape} gives the keyboard back to {machine}."
        ),
        KeyboardCapturePolicy::ThisComputer => format!(
            "{shortcuts} stay on {machine}; every other key goes to the remote computer. \
             {escape} still gives the whole keyboard back to {machine}."
        ),
    }
}

/// One line of the cheat sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutRow {
    /// The keys, or where in the menus.
    pub keys: String,
    /// What it does.
    pub action: String,
}

/// Help ▸ Keyboard Shortcuts, in sections. `capture`: this Deck gives the
/// keyboard to this computer and back (the escape chord and the click).
#[must_use]
pub fn keyboard_shortcut_sheet(
    platform: DeckPlatform,
    capture: bool,
) -> Vec<(&'static str, Vec<ShortcutRow>)> {
    let row = |keys: String, action: &str| ShortcutRow {
        keys,
        action: action.to_owned(),
    };
    let mut deck = Vec::new();
    for shortcut in DeckShortcut::ALL {
        if !capture && shortcut == DeckShortcut::ReleaseKeyboard {
            continue;
        }
        let keys = chord(platform, shortcut);
        if keys.is_empty() {
            continue;
        }
        let action = match shortcut {
            DeckShortcut::Quit => match platform {
                DeckPlatform::Mac => "Hold to quit Arcen Deck (a tap goes to the remote computer)",
                DeckPlatform::Pc => "Hold to exit Arcen Deck (a tap goes to the remote computer)",
            },
            DeckShortcut::ReleaseKeyboard => {
                "Keyboard to this computer and back (keys held on the remote computer are \
                 released)"
            }
            other => other.title(),
        };
        deck.push(row(keys, action));
    }
    if capture {
        deck.push(row(
            "Click the picture".to_owned(),
            "Keyboard back to the remote computer (the click is not sent)",
        ));
    }
    let menu = vec![
        row(
            "Connection ▸ Release Stuck Keys".to_owned(),
            "Let go of every key and button held on the remote computer",
        ),
        row(
            "Connection ▸ Keyboard: Physical Keys / Characters".to_owned(),
            "How typed keys reach the remote computer",
        ),
        row(
            "Connection ▸ Paste as Keystrokes".to_owned(),
            "Type the clipboard's text",
        ),
    ];
    let send_keys = SendKey::ALL
        .iter()
        .filter(|key| !matches!(key, SendKey::Function(14..=24)))
        .map(|key| match key {
            SendKey::Function(_) => row(
                "Connection ▸ Send Key ▸ F13 … F24".to_owned(),
                "Function keys most keyboards lack",
            ),
            other => row(
                format!("Connection ▸ Send Key ▸ {}", other.title()),
                "Send it to the remote computer",
            ),
        })
        .collect();
    let mut sections = vec![
        ("Kept by Arcen Deck", deck),
        ("In the menus", menu),
        ("Keys your keyboard may lack", send_keys),
    ];
    if platform == DeckPlatform::Mac {
        sections.push((
            "On a Mac keyboard",
            vec![
                row(
                    "F13, F14, F15".to_owned(),
                    "Print Screen, Scroll Lock and Pause on a Windows or Linux host",
                ),
                row(
                    "Fn+F1 … F12".to_owned(),
                    "F1–F12 to the remote computer (or use F1, F2, etc. as standard function keys)",
                ),
            ],
        ));
    }
    sections
}

/// Paints Help ▸ Keyboard Shortcuts while `open`; the window's close button
/// clears it.
pub fn render_keyboard_shortcuts(
    ctx: &egui::Context,
    platform: DeckPlatform,
    capture: bool,
    open: &mut bool,
) {
    if !*open {
        return;
    }
    egui::Window::new("Keyboard Shortcuts")
        .open(open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            for (title, rows) in keyboard_shortcut_sheet(platform, capture) {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(title).strong());
                egui::Grid::new(title)
                    .num_columns(2)
                    .spacing([18.0, 4.0])
                    .show(ui, |ui| {
                        for row in rows {
                            ui.label(egui::RichText::new(row.keys).monospace());
                            ui.label(row.action);
                            ui.end_row();
                        }
                    });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_indicator_says_who_has_the_keyboard() {
        assert_eq!(
            keyboard_status_line(
                KeyboardStatus::Host {
                    system_shortcuts: true
                },
                "Z8",
                DeckPlatform::Pc
            ),
            "Keyboard: Z8 (Ctrl+Alt+Home to release)"
        );
        assert_eq!(
            keyboard_status_line(KeyboardStatus::ThisComputer, "Z8", DeckPlatform::Pc),
            "Keyboard: this PC (click to return)"
        );
        assert_eq!(
            keyboard_status_line(
                KeyboardStatus::Host {
                    system_shortcuts: true
                },
                "Z8",
                DeckPlatform::Mac
            ),
            "Keyboard: Z8 (Control-Option-Home to release)"
        );
        assert!(keyboard_status_line(KeyboardStatus::NoSession, "Z8", DeckPlatform::Pc).is_empty());
    }

    #[test]
    fn the_cheat_sheet_comes_from_the_shared_tables() {
        let sheet = keyboard_shortcut_sheet(DeckPlatform::Pc, true);
        let all: Vec<_> = sheet.iter().flat_map(|(_, rows)| rows.iter()).collect();
        assert!(all.iter().any(|row| row.keys == "Ctrl+Alt+Home"));
        assert!(all.iter().any(|row| row.keys == "Ctrl+Alt+End"));
        // F9 and the keyboard-mode chords are the host's now.
        assert!(!all.iter().any(|row| row.keys.contains("F9")));
        assert!(!all.iter().any(|row| row.keys.contains("Win+K")));
        assert!(all.iter().any(|row| row.keys.ends_with("Print Screen")));
        // A Deck that cannot release its keyboard yet does not offer it.
        let mac = keyboard_shortcut_sheet(DeckPlatform::Mac, false);
        let mac: Vec<_> = mac.iter().flat_map(|(_, rows)| rows.iter()).collect();
        assert!(!mac.iter().any(|row| row.keys.contains("Home")));
        assert!(!mac.iter().any(|row| row.keys == "Click the picture"));
        assert!(mac.iter().any(|row| row.keys == "F13, F14, F15"));
    }

    #[test]
    fn every_policy_has_a_label_and_names_the_way_out() {
        for policy in KeyboardCapturePolicy::ALL {
            assert!(!capture_policy_label(policy).is_empty());
            assert!(capture_policy_help(policy, DeckPlatform::Pc).contains("Ctrl+Alt+Home"));
        }
    }
}
