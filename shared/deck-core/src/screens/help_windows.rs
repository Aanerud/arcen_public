// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Help ▸ About and Help ▸ Keyboard Shortcuts for a Deck that draws them
//! itself (the Windows Deck; the macOS Deck uses the system About panel).
//!
//! Over a session they are modal, like the system panel they stand in for:
//! while one is open [`HelpWindows::modal`] is published as
//! `input::overlay_pointer::OverlayPointerRegions::modal`, so the window's
//! input gives the Deck's UI the pointer and the keyboard and the host gets
//! neither. Its OK button and close button work, and Esc or Enter closes the
//! one in front while the Deck's window has the keyboard focus and no text
//! field takes the keys ([`HelpWindows::take_dismiss_key`]). In a session
//! the window's input takes Esc and Enter before the UI sees them, in
//! whichever session window has the keyboard, and keeps the key's repeats
//! and release from the host (`arcen_input::deck_events::DeckInputEvent::
//! DismissModal`, then [`HelpWindows::close_front`]); on a PC a
//! quick Alt+F4 closes it too ([`HelpWindows::close_front`]), while a held
//! one still quits. Otherwise Esc is the host's, as before.

use arcen_input::keyboard::KeyboardSwitchChord;

use super::about::{AboutAction, render_about};
use super::keyboard_help::render_keyboard_shortcuts;
use super::wording::DeckPlatform;

/// One of the Help windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpWindow {
    About,
    KeyboardShortcuts,
}

/// Which Help windows are open, the one in front last.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HelpWindows {
    open: Vec<HelpWindow>,
}

impl HelpWindows {
    /// Opens `window` (or brings it in front).
    pub fn open(&mut self, window: HelpWindow) {
        self.open.retain(|open| *open != window);
        self.open.push(window);
    }

    /// Whether `window` is open.
    #[must_use]
    pub fn is_open(&self, window: HelpWindow) -> bool {
        self.open.contains(&window)
    }

    /// Whether a Help window is open: it takes the session window's pointer
    /// and keyboard until it closes.
    #[must_use]
    pub fn modal(&self) -> bool {
        !self.open.is_empty()
    }

    /// Closes the window in front (Esc, Enter, a quick Alt+F4 on a PC);
    /// returns it.
    pub fn close_front(&mut self) -> Option<HelpWindow> {
        self.open.pop()
    }

    /// Esc or Enter this pass closes the window in front, and is consumed:
    /// only while one is open, the Deck's window has the keyboard focus and
    /// no text field has it. Call before anything else reads Esc this pass
    /// (the connection bar unpins on it). Returns the window closed.
    pub fn take_dismiss_key(&mut self, ctx: &egui::Context) -> Option<HelpWindow> {
        if !self.open.is_empty() && take_dismiss_key(ctx) {
            self.close_front()
        } else {
            None
        }
    }

    /// Draws the open windows; their OK and close buttons close them.
    /// `version` and `build` are the Deck's own, for About.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        version: &str,
        build: &str,
        platform: DeckPlatform,
        switch: KeyboardSwitchChord,
    ) {
        if self.is_open(HelpWindow::About)
            && render_about(ctx, version, build) == AboutAction::Close
        {
            self.open.retain(|open| *open != HelpWindow::About);
        }
        let mut shortcuts = self.is_open(HelpWindow::KeyboardShortcuts);
        render_keyboard_shortcuts(ctx, platform, true, switch, &mut shortcuts);
        if !shortcuts {
            self.open
                .retain(|open| *open != HelpWindow::KeyboardShortcuts);
        }
    }
}

/// Takes this pass's Esc or Enter for a Deck window over the screen (a
/// Help window, the macOS Deck's Keyboard Shortcuts), so nothing under it
/// acts on the key: only while the Deck's window has the keyboard focus
/// and no text field has it. Call only while such a window is open, before
/// anything else reads the key this pass.
#[must_use]
pub fn take_dismiss_key(ctx: &egui::Context) -> bool {
    !ctx.egui_wants_keyboard_input()
        && ctx.input_mut(|input| {
            input.focused
                && (input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
                    || input.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: egui::Rect = egui::Rect {
        min: egui::Pos2::ZERO,
        max: egui::Pos2::new(1280.0, 800.0),
    };

    /// One pass; returns where the text "OK" is drawn, if it is.
    fn pass(
        ctx: &egui::Context,
        help: &mut HelpWindows,
        events: Vec<egui::Event>,
        focused: bool,
    ) -> Option<egui::Pos2> {
        let input = egui::RawInput {
            screen_rect: Some(SCREEN),
            focused,
            events,
            ..egui::RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| {
            let _ = help.take_dismiss_key(ui.ctx());
            help.show(
                ui.ctx(),
                "0.20.6",
                "0.20.6",
                DeckPlatform::Pc,
                KeyboardSwitchChord::DEFAULT,
            );
        });
        output
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) if text.galley.text() == "OK" => {
                    Some(text.galley.rect.translate(text.pos.to_vec2()).center())
                }
                _ => None,
            })
    }

    fn key(key: egui::Key) -> Vec<egui::Event> {
        vec![
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ]
    }

    #[test]
    fn ok_closes_about_and_it_is_modal_until_then() {
        let ctx = egui::Context::default();
        let mut help = HelpWindows::default();
        assert!(!help.modal());
        help.open(HelpWindow::About);
        assert!(help.modal());
        // A window lays itself out on its first pass and shows on the next.
        let _ = pass(&ctx, &mut help, Vec::new(), true);
        let at = pass(&ctx, &mut help, Vec::new(), true).expect("About shows OK");
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let _ = pass(&ctx, &mut help, vec![egui::Event::PointerMoved(at)], true);
        let _ = pass(&ctx, &mut help, vec![click(true)], true);
        assert!(help.modal());
        let _ = pass(&ctx, &mut help, vec![click(false)], true);
        assert!(!help.is_open(HelpWindow::About));
        assert!(!help.modal());
    }

    #[test]
    fn esc_or_enter_closes_the_window_in_front_only_with_the_keyboard_focus() {
        let ctx = egui::Context::default();
        let mut help = HelpWindows::default();
        help.open(HelpWindow::KeyboardShortcuts);
        help.open(HelpWindow::About);
        // The Deck's window does not have the keyboard: nothing closes.
        let _ = pass(&ctx, &mut help, key(egui::Key::Escape), false);
        assert!(help.is_open(HelpWindow::About));
        // Esc closes the one in front, Enter the next.
        let _ = pass(&ctx, &mut help, key(egui::Key::Escape), true);
        assert!(!help.is_open(HelpWindow::About));
        assert!(help.is_open(HelpWindow::KeyboardShortcuts));
        let _ = pass(&ctx, &mut help, key(egui::Key::Enter), true);
        assert!(!help.modal());
        // With none open, Esc is left for whoever reads it.
        let input = egui::RawInput {
            screen_rect: Some(SCREEN),
            focused: true,
            events: key(egui::Key::Escape),
            ..egui::RawInput::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            assert_eq!(help.take_dismiss_key(ui.ctx()), None);
            assert!(ui.input(|input| input.key_pressed(egui::Key::Escape)));
        });
    }
}
