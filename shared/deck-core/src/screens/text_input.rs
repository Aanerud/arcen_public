// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What typing shows over the video: the "Keyboard: Characters" tag while
//! that mode is on, the input method's composition (shown locally until it
//! commits), and the latest typing notice; with the notices' wording.

use arcen_input::text::KeyboardMode;

use arcen_input::keyboard::DeckShortcut;

use super::wording::{DeckPlatform, chord};

/// How long a typing notice stays over the video.
pub const NOTICE_DURATION: std::time::Duration = std::time::Duration::from_secs(5);

/// Draws the typing overlays over `video`. `notice` is the latest notice
/// while it is still to be shown (the shell times it out).
pub fn paint_text_input_overlays(
    ctx: &egui::Context,
    video: egui::Rect,
    characters_mode: bool,
    preedit: Option<&str>,
    notice: Option<&str>,
) {
    if characters_mode {
        // Characters mode changes what games and shortcuts-by-letter
        // see; it stays visible for as long as it is on.
        egui::Area::new(egui::Id::new("arcen-keyboard-mode"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(video.right() - 150.0, video.top() + 8.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(150))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Keyboard: Characters")
                                .size(11.0)
                                .color(egui::Color32::from_gray(230)),
                        );
                    });
            });
    }
    if let Some(preedit) = preedit.filter(|text| !text.is_empty()) {
        egui::Area::new(egui::Id::new("arcen-text-preedit"))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(video.center().x - 120.0, video.bottom() - 72.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(220))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(preedit)
                                .size(18.0)
                                .underline()
                                .color(egui::Color32::WHITE),
                        );
                    });
            });
    }
    if let Some(text) = notice {
        egui::Area::new(egui::Id::new("arcen-text-notice"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 48.0))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(210))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin::symmetric(16, 10))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(text)
                                .size(14.0)
                                .color(egui::Color32::WHITE),
                        );
                    });
            });
    }
}

/// The notice when Keyboard: Characters is refused because the host cannot
/// type text (`connected`), or before a session.
#[must_use]
pub const fn characters_unavailable_notice(connected: bool) -> &'static str {
    if connected {
        "This host cannot type text sent from the Deck, so the keyboard stays on physical keys."
    } else {
        "Keyboard: Characters applies once connected to a host that types text."
    }
}

/// The notice when the session's keyboard mode is chosen.
#[must_use]
pub fn keyboard_mode_notice(platform: DeckPlatform, mode: KeyboardMode) -> String {
    match mode {
        KeyboardMode::Characters => format!(
            "Keyboard: Characters. Typed characters arrive as on {}; shortcuts stay keys. {} for \
             physical keys.",
            platform.this_machine(),
            chord(platform, DeckShortcut::KeyboardPhysical),
        ),
        KeyboardMode::Physical => {
            "Keyboard: Physical keys. The remote keyboard layout decides each character.".to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_mode_notice_names_this_machine_and_its_chord() {
        assert_eq!(
            keyboard_mode_notice(DeckPlatform::Mac, KeyboardMode::Characters),
            "Keyboard: Characters. Typed characters arrive as on this Mac; shortcuts stay keys. \
             Control-Command-K for physical keys."
        );
        assert_eq!(
            keyboard_mode_notice(DeckPlatform::Pc, KeyboardMode::Characters),
            "Keyboard: Characters. Typed characters arrive as on this PC; shortcuts stay keys. \
             Ctrl+Win+K for physical keys."
        );
    }
}
