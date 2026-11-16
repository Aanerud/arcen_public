// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Sign-in: the host's disclaimer and the credentials screen. The shell owns
//! the draft and maps each action.

use super::theme;
use super::widgets::{back_button, labelled_line_edit};
use crate::connection_draft::ConnectionDraft;

/// What the user did on the credentials screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialAction {
    None,
    Back,
    Connect,
}

/// What the user did on the disclaimer screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclaimerAction {
    None,
    Accept,
    Decline,
}

/// What the credentials screen reads and edits.
#[derive(Debug)]
pub struct CredentialsView<'a> {
    pub draft: &'a mut ConnectionDraft,
    /// Why the host asks for credentials again, if it said.
    pub note: Option<&'a str>,
    /// The last connection error.
    pub error: Option<&'a str>,
    pub remember_username: &'a mut bool,
}

/// Draws the sign-in screen: the host on the left, username and password on
/// the right.
pub fn render_credentials(ui: &mut egui::Ui, view: &mut CredentialsView<'_>) -> CredentialAction {
    let draft = &mut *view.draft;
    let mut action = CredentialAction::None;
    ui.add_space(80.0);
    ui.horizontal_top(|ui| {
        ui.add_space(72.0);
        ui.vertical(|ui| {
            ui.set_width(310.0);
            ui.add_space(28.0);
            ui.heading(
                egui::RichText::new("Connect")
                    .color(egui::Color32::BLACK)
                    .size(30.0),
            );
            ui.add_space(44.0);
            ui.horizontal(|ui| {
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(ui.cursor().min, egui::vec2(2.0, 20.0)),
                    0.0,
                    theme::ACCENT,
                );
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(draft.display_name())
                        .color(egui::Color32::from_rgb(0x3D, 0x3D, 0x3D))
                        .size(17.0),
                );
            });
            ui.add_space(220.0);
            ui.separator();
            ui.add_space(12.0);
            if back_button(ui, "Back").clicked() {
                action = CredentialAction::Back;
            }
        });

        ui.add_space(86.0);
        ui.vertical(|ui| {
            ui.set_width(360.0);
            labelled_line_edit(ui, "Username", &mut draft.username, false);
            ui.add_space(22.0);
            labelled_line_edit(ui, "Password", &mut draft.password, true);
            ui.add_space(24.0);
            if ui
                .add_sized(
                    [360.0, 48.0],
                    egui::Button::new(
                        egui::RichText::new("Connect")
                            .color(egui::Color32::WHITE)
                            .size(17.0),
                    )
                    .fill(theme::ACCENT),
                )
                .clicked()
            {
                action = CredentialAction::Connect;
            }
            ui.add_space(16.0);
            if let Some(note) = view.note {
                ui.label(
                    egui::RichText::new(note)
                        .color(egui::Color32::from_rgb(0x55, 0x55, 0x5A))
                        .size(13.0),
                );
                ui.add_space(8.0);
            }
            if let Some(err) = view.error {
                ui.label(
                    egui::RichText::new(err)
                        .color(egui::Color32::from_rgb(0xC0, 0x20, 0x20))
                        .size(13.0),
                );
                ui.add_space(8.0);
            }
            ui.checkbox(view.remember_username, "Remember username");
        });
    });
    action
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
                .color(egui::Color32::BLACK)
                .size(28.0),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!("Host: {}", draft.endpoint()))
                .color(egui::Color32::DARK_GRAY)
                .monospace(),
        );
        ui.add_space(18.0);
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(0xF7, 0xF7, 0xF7))
            .stroke(egui::Stroke::new(
                1.0,
                egui::Color32::from_rgb(0xD8, 0xD8, 0xD8),
            ))
            .inner_margin(egui::Margin::same(18))
            .show(ui, |ui| {
                ui.set_width(720.0);
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .color(egui::Color32::BLACK)
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
                            .color(egui::Color32::WHITE)
                            .size(16.0),
                    )
                    .fill(theme::ACCENT),
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
