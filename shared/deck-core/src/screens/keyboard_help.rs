// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Who has the keyboard, in words, and Help ▸ Keyboard Shortcuts.
//!
//! Both Decks show the same wording, generated from the shared tables
//! (`arcen_input::keyboard::{DeckShortcut, SendKey, KeyboardCapturePolicy}`),
//! so the help can never drift from what the keyboard does.

use arcen_input::keyboard::{
    DeckShortcut, KeyboardCapturePolicy, KeyboardOwner, KeyboardStatus, KeyboardSwitchChord,
    KeyboardSwitchPreset, SendKey, SwitchChordCheck,
};

use super::wording::{DeckPlatform, chord};

/// The keyboard switch's chord as this Deck spells it: "Ctrl+Alt+Home" on a
/// PC, "Control-Option-Home" on a Mac.
#[must_use]
pub fn switch_chord_text(platform: DeckPlatform, switch: KeyboardSwitchChord) -> String {
    match platform {
        DeckPlatform::Mac => switch.mac_text(),
        DeckPlatform::Pc => switch.pc_text(),
    }
}

/// A Deck shortcut's chord, with the keyboard switch's chosen one.
#[must_use]
pub fn shortcut_chord_text(
    platform: DeckPlatform,
    shortcut: DeckShortcut,
    switch: KeyboardSwitchChord,
) -> String {
    if shortcut == DeckShortcut::ReleaseKeyboard {
        switch_chord_text(platform, switch)
    } else {
        chord(platform, shortcut)
    }
}

/// The indicator: "Keyboard: Z8 (Ctrl+Alt+Home to release)" while the host
/// has it, "Keyboard: this PC (click to return)" while this computer does;
/// empty outside a session.
#[must_use]
pub fn keyboard_status_line(
    status: KeyboardStatus,
    host: &str,
    platform: DeckPlatform,
    switch: KeyboardSwitchChord,
) -> String {
    let escape = switch_chord_text(platform, switch);
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

/// The session's status line, as a Deck with one status line shows it
/// (the Windows Deck's menu bar and connection bar): who has the keyboard,
/// then any clipboard notice as a sentence of its own
/// (`crate::clipboard::HostClipboard::session_notice`). Empty outside a
/// session.
#[must_use]
pub fn session_status_line(keyboard: String, clipboard: Option<&str>) -> String {
    match clipboard {
        Some(notice) if !keyboard.is_empty() => format!("{keyboard}. {notice}"),
        _ => keyboard,
    }
}

/// The one-time question when ⌘Tab is pressed in a session without the
/// permission.
pub const MAC_SHORTCUT_OFFER_TITLE: &str = "Send ⌘Tab to the remote?";

/// Its explanation.
pub const MAC_SHORTCUT_OFFER_TEXT: &str = "macOS needs the Accessibility permission. You can \
     change this later in Settings ▸ Input.";

/// Settings ▸ Input ▸ Keyboard switch: what the switch is.
#[must_use]
pub fn keyboard_switch_help(platform: DeckPlatform, switch: KeyboardSwitchChord) -> String {
    let keys = switch_chord_text(platform, switch);
    let machine = platform.this_machine();
    let shortcuts = match platform {
        DeckPlatform::Pc => "Alt+Tab, the Windows key and Print Screen",
        DeckPlatform::Mac => "⌘Tab, ⌘Space, Mission Control and Control-←/→",
    };
    format!(
        "{keys} flips the keyboard between the remote computer (Remote) and {machine} (Local). \
         In Remote, every key and {shortcuts} go to the remote computer; in Local, they all \
         stay on {machine}. Clicking the picture also returns to Remote."
    )
}

/// The Settings label of a switch preset.
#[must_use]
pub fn keyboard_switch_preset_label(
    platform: DeckPlatform,
    preset: KeyboardSwitchPreset,
) -> String {
    preset.chord().map_or_else(
        || "Custom chord".to_owned(),
        |chord| switch_chord_text(platform, chord),
    )
}

/// What Settings says of a typed chord: `Ok(None)` fine, `Ok(Some(..))` a
/// warning (it works), `Err(..)` why it cannot be used.
///
/// # Errors
///
/// The chord cannot be read, or cannot be the keyboard switch.
pub fn keyboard_switch_verdict(
    text: &str,
) -> Result<(KeyboardSwitchChord, Option<String>), String> {
    let Some(chord) = KeyboardSwitchChord::parse(text) else {
        return Err(arcen_input::keyboard::SwitchChordRejection::Unreadable
            .message()
            .to_owned());
    };
    match chord.check() {
        SwitchChordCheck::Ok => Ok((chord, None)),
        SwitchChordCheck::ProAppCollision(description) => Ok((
            chord,
            Some(format!(
                "Works, but the remote computer loses this chord ({}).",
                description.replace(" -- ", ": ")
            )),
        )),
        SwitchChordCheck::Rejected(rejection) => Err(rejection.message().to_owned()),
    }
}

/// The short notice every flip of the switch shows: "Keyboard: Remote —
/// Alt+Tab goes to Z8", or "Keyboard: This computer". `system_shortcuts`:
/// the system shortcuts go to the remote computer now (on a Mac, only with
/// the ⌘Tab switch on and allowed).
#[must_use]
pub fn keyboard_switch_toast(
    owner: KeyboardOwner,
    host: &str,
    platform: DeckPlatform,
    system_shortcuts: bool,
) -> String {
    match owner {
        KeyboardOwner::ThisComputer => "Keyboard: This computer".to_owned(),
        KeyboardOwner::Host if system_shortcuts => {
            let switcher = match platform {
                DeckPlatform::Pc => "Alt+Tab",
                DeckPlatform::Mac => "⌘Tab",
            };
            format!("Keyboard: Remote — {switcher} goes to {host}")
        }
        KeyboardOwner::Host => format!("Keyboard: Remote — keys go to {host}"),
    }
}

/// How long the notice stays, in seconds.
pub const KEYBOARD_TOAST_SECONDS: f64 = 2.5;

/// The notice on screen, from when it was shown (egui time).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyboardToast {
    /// What it says.
    pub text: String,
    /// `egui::InputState::time` when it was shown.
    pub shown_at: f64,
}

impl KeyboardToast {
    /// A notice shown now.
    #[must_use]
    pub fn new(ctx: &egui::Context, text: String) -> Self {
        Self {
            text,
            shown_at: ctx.input(|input| input.time),
        }
    }
}

/// Paints the notice at the top of the window until it is due to go, then
/// clears it. Drawn over the picture for its few seconds only.
pub fn render_keyboard_toast(ctx: &egui::Context, toast: &mut Option<KeyboardToast>) {
    let Some(shown) = toast.as_ref() else {
        return;
    };
    let left = KEYBOARD_TOAST_SECONDS - (ctx.input(|input| input.time) - shown.shown_at);
    if left <= 0.0 {
        *toast = None;
        return;
    }
    egui::Area::new(egui::Id::new("arcen-keyboard-toast"))
        .order(egui::Order::Tooltip)
        .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 24.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(210))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(&shown.text)
                            .color(egui::Color32::WHITE)
                            .size(15.0),
                    );
                });
        });
    ctx.request_repaint_after(std::time::Duration::from_secs_f64(left.max(0.05)));
}

/// The one-time ⌘Tab question (macOS Deck): `Some(true)` Allow,
/// `Some(false)` Not now, `None` not answered yet.
#[must_use]
pub fn render_mac_shortcut_offer(ctx: &egui::Context) -> Option<bool> {
    let mut answer = None;
    egui::Window::new(MAC_SHORTCUT_OFFER_TITLE)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label(MAC_SHORTCUT_OFFER_TEXT);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Allow").clicked() {
                    answer = Some(true);
                }
                if ui.button("Not now").clicked() {
                    answer = Some(false);
                }
            });
        });
    answer
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
pub fn capture_policy_help(
    policy: KeyboardCapturePolicy,
    platform: DeckPlatform,
    switch: KeyboardSwitchChord,
) -> String {
    let shortcuts = match platform {
        DeckPlatform::Pc => "Alt+Tab, Alt+Shift+Tab, the Windows key and Print Screen",
        DeckPlatform::Mac => "⌘Tab, ⌘Space and Mission Control (with the switch below on)",
    };
    let escape = switch_chord_text(platform, switch);
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
    switch: KeyboardSwitchChord,
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
        let keys = shortcut_chord_text(platform, shortcut, switch);
        if keys.is_empty() {
            continue;
        }
        let action = match shortcut {
            DeckShortcut::Quit => match platform {
                DeckPlatform::Mac => "Hold to quit Arcen Deck (a tap goes to the remote computer)",
                DeckPlatform::Pc => "Hold to exit Arcen Deck (a tap goes to the remote computer)",
            },
            DeckShortcut::ReleaseKeyboard => {
                "The keyboard switch: Remote (every key and the system shortcuts go to the \
                 remote computer) or Local (they all stay on this computer); keys held on the \
                 remote computer are released. Change it in Settings ▸ Input"
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
                row(
                    "⌘Tab, ⌘Space, Control-↑, Control-←, Control-→".to_owned(),
                    "To the remote computer in Remote, with Settings ▸ Input ▸ System \
                     shortcuts on (needs Accessibility); on this Mac in Local",
                ),
                row(
                    "Option-⌘-Esc, Control-⌘-Q".to_owned(),
                    "Force Quit and Lock Screen, always on this Mac",
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
    switch: KeyboardSwitchChord,
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
            for (title, rows) in keyboard_shortcut_sheet(platform, capture, switch) {
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
                DeckPlatform::Pc,
                KeyboardSwitchChord::DEFAULT
            ),
            "Keyboard: Z8 (Ctrl+Alt+Home to release)"
        );
        assert_eq!(
            keyboard_status_line(
                KeyboardStatus::ThisComputer,
                "Z8",
                DeckPlatform::Pc,
                KeyboardSwitchChord::DEFAULT
            ),
            "Keyboard: this PC (click to return)"
        );
        assert_eq!(
            keyboard_status_line(
                KeyboardStatus::Host {
                    system_shortcuts: true
                },
                "Z8",
                DeckPlatform::Mac,
                KeyboardSwitchChord::DEFAULT
            ),
            "Keyboard: Z8 (Control-Option-Home to release)"
        );
        assert!(
            keyboard_status_line(
                KeyboardStatus::NoSession,
                "Z8",
                DeckPlatform::Pc,
                KeyboardSwitchChord::DEFAULT
            )
            .is_empty()
        );
    }

    #[test]
    fn the_cheat_sheet_comes_from_the_shared_tables() {
        let sheet = keyboard_shortcut_sheet(DeckPlatform::Pc, true, KeyboardSwitchChord::DEFAULT);
        let all: Vec<_> = sheet.iter().flat_map(|(_, rows)| rows.iter()).collect();
        assert!(all.iter().any(|row| row.keys == "Ctrl+Alt+Home"));
        assert!(all.iter().any(|row| row.keys == "Ctrl+Alt+End"));
        // F9 and the keyboard-mode chords are the host's now.
        assert!(!all.iter().any(|row| row.keys.contains("F9")));
        assert!(!all.iter().any(|row| row.keys.contains("Win+K")));
        assert!(all.iter().any(|row| row.keys.ends_with("Print Screen")));
        // A Deck that cannot release its keyboard does not offer it.
        let mac = keyboard_shortcut_sheet(DeckPlatform::Mac, false, KeyboardSwitchChord::DEFAULT);
        let mac: Vec<_> = mac.iter().flat_map(|(_, rows)| rows.iter()).collect();
        assert!(!mac.iter().any(|row| row.keys.contains("Home")));
        assert!(!mac.iter().any(|row| row.keys == "Click the picture"));
        assert!(mac.iter().any(|row| row.keys == "F13, F14, F15"));
        // The Mac Deck does: its chord, the click, and where its system
        // chords go.
        let mac = keyboard_shortcut_sheet(DeckPlatform::Mac, true, KeyboardSwitchChord::DEFAULT);
        let mac: Vec<_> = mac.iter().flat_map(|(_, rows)| rows.iter()).collect();
        assert!(mac.iter().any(|row| row.keys == "Control-Option-Home"));
        assert!(mac.iter().any(|row| row.keys == "Click the picture"));
        assert!(mac.iter().any(|row| row.keys.starts_with("⌘Tab")));
        assert!(mac.iter().any(|row| row.keys.contains("Control-←")));
    }

    #[test]
    fn every_policy_has_a_label_and_names_the_way_out() {
        for policy in KeyboardCapturePolicy::ALL {
            assert!(!capture_policy_label(policy).is_empty());
            assert!(
                capture_policy_help(policy, DeckPlatform::Pc, KeyboardSwitchChord::DEFAULT)
                    .contains("Ctrl+Alt+Home")
            );
        }
    }

    #[test]
    fn a_chosen_chord_shows_everywhere_the_switch_is_named() {
        let switch = KeyboardSwitchChord::ALT_F1;
        let sheet = keyboard_shortcut_sheet(DeckPlatform::Pc, true, switch);
        let all: Vec<_> = sheet.iter().flat_map(|(_, rows)| rows.iter()).collect();
        assert!(all.iter().any(|row| row.keys == "Alt+F1"));
        assert!(!all.iter().any(|row| row.keys == "Ctrl+Alt+Home"));
        assert_eq!(
            keyboard_status_line(
                KeyboardStatus::Host {
                    system_shortcuts: true
                },
                "Z8",
                DeckPlatform::Mac,
                switch
            ),
            "Keyboard: Z8 (Option-F1 to release)"
        );
        assert!(keyboard_switch_help(DeckPlatform::Pc, switch).starts_with("Alt+F1 flips"));
        assert!(
            capture_policy_help(KeyboardCapturePolicy::Host, DeckPlatform::Pc, switch)
                .contains("Alt+F1")
        );
    }

    #[test]
    fn every_flip_says_where_the_keyboard_went() {
        assert_eq!(
            keyboard_switch_toast(KeyboardOwner::Host, "Z8", DeckPlatform::Pc, true),
            "Keyboard: Remote — Alt+Tab goes to Z8"
        );
        assert_eq!(
            keyboard_switch_toast(KeyboardOwner::Host, "Z8", DeckPlatform::Mac, true),
            "Keyboard: Remote — ⌘Tab goes to Z8"
        );
        assert_eq!(
            keyboard_switch_toast(KeyboardOwner::Host, "Z8", DeckPlatform::Mac, false),
            "Keyboard: Remote — keys go to Z8"
        );
        assert_eq!(
            keyboard_switch_toast(KeyboardOwner::ThisComputer, "Z8", DeckPlatform::Pc, true),
            "Keyboard: This computer"
        );
    }

    #[test]
    fn a_typed_chord_is_checked_in_shared_code() {
        assert_eq!(
            keyboard_switch_verdict("ctrl+alt+shift+z")
                .map(|(chord, warning)| (chord, warning.is_none())),
            Ok((KeyboardSwitchChord::CONTROL_ALT_SHIFT_Z, true))
        );
        assert!(keyboard_switch_verdict("Win+L").is_err());
        assert!(keyboard_switch_verdict("Ctrl+Alt+Del").is_err());
        assert!(keyboard_switch_verdict("q").is_err());
        assert!(keyboard_switch_verdict("nonsense").is_err());
        let (_, warning) = keyboard_switch_verdict("Ctrl+Alt+Shift+P").unwrap();
        assert!(warning.unwrap().contains("Flame"));
        assert_eq!(
            keyboard_switch_preset_label(DeckPlatform::Mac, KeyboardSwitchPreset::ControlAltHome),
            "Control-Option-Home"
        );
    }

    #[test]
    fn the_toast_goes_away_by_itself() {
        let ctx = egui::Context::default();
        let mut toast = Some(KeyboardToast {
            text: "Keyboard: This computer".to_owned(),
            shown_at: -10.0,
        });
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            render_keyboard_toast(ui.ctx(), &mut toast);
        });
        assert!(toast.is_none());
    }

    #[test]
    fn the_status_line_adds_the_hosts_clipboard_notice_in_a_session_only() {
        let notice = Some(crate::clipboard::HOST_TURNED_CLIPBOARD_OFF);
        assert_eq!(
            session_status_line("Keyboard: Z8".to_owned(), notice),
            "Keyboard: Z8. Clipboard is turned off by this host."
        );
        assert_eq!(session_status_line(String::new(), notice), "");
        assert_eq!(
            session_status_line("Keyboard: Z8".to_owned(), None),
            "Keyboard: Z8"
        );
    }
}
