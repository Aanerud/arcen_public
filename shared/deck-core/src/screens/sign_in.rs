// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Sign-in: the host's disclaimer, a real step before signing in. There is
//! no separate credentials page: Home asks for the password under its field
//! (`crate::sign_in`). The shell owns the draft and maps each action.

use crate::connection_draft::ConnectionDraft;

/// What the user did on the disclaimer screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclaimerAction {
    None,
    Accept,
    Decline,
}

/// Draws the host's disclaimer, shown before sign-in when the host has one.
pub fn render_disclaimer(
    ui: &mut egui::Ui,
    draft: &ConnectionDraft,
    text: &str,
) -> DisclaimerAction {
    let mut action = DisclaimerAction::None;
    ui.add_space(52.0);
    ui.vertical_centered(|ui| {
        ui.set_max_width(760.0);
        ui.heading(
            egui::RichText::new("Review before connect")
                .color(super::theme::colours(ui).text)
                .size(28.0),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!("Host: {}", draft.endpoint()))
                .color(super::theme::colours(ui).text2)
                .monospace(),
        );
        ui.add_space(18.0);
        egui::Frame::new()
            .fill(super::theme::colours(ui).panel)
            .stroke(egui::Stroke::new(1.0, super::theme::colours(ui).border))
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_width(720.0);
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .color(super::theme::colours(ui).text)
                                    .size(16.0),
                            )
                            .selectable(true)
                            .wrap(),
                        );
                    });
            });
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if ui
                .add_sized(
                    [140.0, 44.0],
                    egui::Button::new(
                        egui::RichText::new("Accept")
                            .color(super::theme::colours(ui).on_primary)
                            .size(16.0),
                    )
                    .fill(super::theme::colours(ui).primary),
                )
                .clicked()
            {
                action = DisclaimerAction::Accept;
            }
            if ui
                .add_sized([140.0, 44.0], egui::Button::new("Decline"))
                .clicked()
            {
                action = DisclaimerAction::Decline;
            }
        });
    });
    action
}
