// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The launcher's frame: the dark bar above every screen that is not a
//! session or Settings, with the Settings button.

/// What the user did in the launcher bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherAction {
    None,
    OpenSettings,
}

/// Draws the launcher bar across the full width.
pub fn render_launcher_top_bar(ui: &mut egui::Ui) -> LauncherAction {
    let mut action = LauncherAction::None;
    let width = ui.available_width();
    egui::Frame::new()
        .fill(egui::Color32::from_rgb(0x20, 0x23, 0x21))
        .inner_margin(egui::Margin::symmetric(20, 8))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.set_min_height(34.0);
            ui.horizontal(|ui| {
                if toolbar_icon(ui, "⚙").clicked() {
                    action = LauncherAction::OpenSettings;
                }
            });
        });
    action
}

fn toolbar_icon(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .color(egui::Color32::WHITE)
                .size(18.0),
        )
        .fill(egui::Color32::from_white_alpha(16))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(22)))
        .corner_radius(8.0)
        .min_size(egui::vec2(34.0, 30.0)),
    )
}
