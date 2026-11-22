// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Connecting, and the two display recoveries a host can ask for before a
//! session starts (a desktop started without the layout, a host that cannot
//! serve every display). The shell owns the connection and maps each action.

use super::widgets::paint_connecting_progress;
use crate::connection_draft::ConnectionDraft;

/// What the user did on the Connecting screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectingAction {
    None,
    Cancel,
}

/// What the user chose when the host's desktop was started without the
/// requested layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyConflictAction {
    None,
    /// Keep the running desktop, on the primary display only.
    Reconnect,
    /// Replace the desktop with one that serves the layout.
    StartFresh,
    Cancel,
}

/// What the user chose when the host cannot serve every display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplaysUnsupportedAction {
    None,
    UsePrimaryOnly,
    Cancel,
}

pub fn render_connecting(
    ui: &mut egui::Ui,
    draft: &ConnectionDraft,
    status: &str,
    hint: Option<&str>,
) -> ConnectingAction {
    let mut action = ConnectingAction::None;
    ui.vertical_centered(|ui| {
        ui.add_space(180.0);
        paint_connecting_progress(ui, 24.0);
        ui.add_space(28.0);
        ui.heading(
            egui::RichText::new("Connecting")
                .color(super::theme::colours(ui).text)
                .size(28.0),
        );
        ui.add_space(18.0);
        ui.label(
            egui::RichText::new(format!(
                "{} as {}",
                draft.display_name(),
                if draft.username.is_empty() {
                    "user"
                } else {
                    draft.username.as_str()
                }
            ))
            .color(super::theme::colours(ui).text2)
            .size(17.0),
        );
        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(status)
                .color(super::theme::colours(ui).text2)
                .size(15.0),
        );
        if let Some(hint) = hint {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(hint)
                    .color(super::theme::colours(ui).warning)
                    .size(13.0),
            );
        }
        ui.add_space(36.0);
        if ui
            .add_sized([112.0, 40.0], egui::Button::new("Cancel"))
            .clicked()
        {
            action = ConnectingAction::Cancel;
        }
    });
    action
}

/// Presents the two real recoveries for a host multi-monitor topology
/// conflict; neither is preselected.
pub fn render_topology_conflict(
    ui: &mut egui::Ui,
    draft: &ConnectionDraft,
) -> TopologyConflictAction {
    let mut action = TopologyConflictAction::None;
    ui.vertical_centered(|ui| {
        ui.add_space(150.0);
        ui.heading(
            egui::RichText::new("This desktop was started without your layout")
                .color(super::theme::colours(ui).text)
                .size(26.0),
        );
        ui.add_space(18.0);
        ui.label(
            egui::RichText::new(format!(
                "{} already has a session running that was not set up for your \
                 current display arrangement.",
                draft.display_name(),
            ))
            .color(super::theme::colours(ui).text2)
            .size(16.0),
        );
        ui.add_space(34.0);

        ui.horizontal(|ui| {
            // Centre the pair inside the available width.
            let pair_width = 250.0 + 12.0 + 250.0;
            let indent = ((ui.available_width() - pair_width) / 2.0).max(0.0);
            ui.add_space(indent);
            if ui
                .add_sized(
                    [250.0, 44.0],
                    egui::Button::new(
                        egui::RichText::new("Reconnect to your session")
                            .size(15.0)
                            .color(super::theme::colours(ui).on_primary),
                    )
                    .fill(super::theme::colours(ui).primary)
                    .corner_radius(8.0),
                )
                .clicked()
            {
                action = TopologyConflictAction::Reconnect;
            }
            ui.add_space(12.0);
            if ui
                .add_sized(
                    [250.0, 44.0],
                    egui::Button::new(
                        egui::RichText::new("Start fresh with my layout")
                            .size(15.0)
                            .color(super::theme::colours(ui).text2),
                    )
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(1.0, super::theme::colours(ui).border))
                    .corner_radius(8.0),
                )
                .clicked()
            {
                action = TopologyConflictAction::StartFresh;
            }
        });

        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(
                "Reconnecting keeps your open applications and uses your main display \
                 only. Starting fresh matches your layout and closes them.",
            )
            .color(super::theme::colours(ui).text2)
            .size(13.0),
        );
        ui.add_space(28.0);
        if ui
            .add_sized(
                [140.0, 36.0],
                egui::Button::new(
                    egui::RichText::new("Cancel")
                        .size(14.0)
                        .color(super::theme::colours(ui).text2),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE),
            )
            .clicked()
        {
            action = TopologyConflictAction::Cancel;
        }
    });
    action
}

/// Offers Primary Display Only when a host cannot serve the complete layout.
pub fn render_displays_unsupported(
    ui: &mut egui::Ui,
    draft: &ConnectionDraft,
    status: &str,
) -> DisplaysUnsupportedAction {
    let mut action = DisplaysUnsupportedAction::None;
    ui.vertical_centered(|ui| {
        ui.add_space(150.0);
        ui.heading(
            egui::RichText::new("This host cannot match all active displays")
                .color(super::theme::colours(ui).text)
                .size(26.0),
        );
        ui.add_space(18.0);
        ui.label(
            egui::RichText::new(format!(
                "{} cannot serve your complete current layout.\n\n{}",
                draft.display_name(),
                status,
            ))
            .color(super::theme::colours(ui).text2)
            .size(16.0),
        );
        ui.add_space(34.0);

        if ui
            .add_sized(
                [280.0, 44.0],
                egui::Button::new(
                    egui::RichText::new("Use Primary Display Only")
                        .size(15.0)
                        .color(super::theme::colours(ui).on_primary),
                )
                .fill(super::theme::colours(ui).primary)
                .corner_radius(8.0),
            )
            .clicked()
        {
            action = DisplaysUnsupportedAction::UsePrimaryOnly;
        }

        ui.add_space(16.0);
        ui.label(
            egui::RichText::new(
                "This is remembered for this connection only. Your other \
                 connections keep matching your layout.",
            )
            .color(super::theme::colours(ui).text2)
            .size(13.0),
        );
        ui.add_space(28.0);
        if ui
            .add_sized(
                [140.0, 36.0],
                egui::Button::new(
                    egui::RichText::new("Cancel")
                        .size(14.0)
                        .color(super::theme::colours(ui).text2),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE),
            )
            .clicked()
        {
            action = DisplaysUnsupportedAction::Cancel;
        }
    });
    action
}
