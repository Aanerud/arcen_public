// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Trust on first use: the "Verify this host" prompt for an unknown host and
//! the "identity changed" warning for a remembered one. The shell owns the
//! trust store and maps each action.

use super::theme;

/// The presented certificate, as the prompt shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateFacts {
    pub endpoint: String,
    pub server_name: String,
    /// The address this certificate actually came from.
    pub peer_address: Option<String>,
    pub spki_display: String,
    pub certificate_display: String,
    pub not_before_epoch_secs: u64,
    pub not_after_epoch_secs: u64,
}

/// The identity this Deck remembered for the endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedIdentity {
    pub label: Option<String>,
    pub pinned_at: Option<String>,
    pub spki_display: String,
}

/// What the user did on the "Verify this host" prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateAction {
    None,
    Cancel,
    TrustForSession,
    TrustAndRemember,
}

/// What the user chose when a remembered identity turned out not to match.
///
/// There is deliberately no "trust the new one" here. Accepting a replacement
/// in a single click would show the user a fingerprint at the moment they are
/// most motivated to dismiss it. Forgetting instead drops the stale pin and
/// nothing more; the next connection raises the ordinary first-use prompt,
/// where the new fingerprint is displayed for comparison before it is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateChangedAction {
    None,
    Cancel,
    ForgetAndReconnect,
}

/// Certificate warning rendered as a centred modal overlay.
/// The caller is responsible for rendering the background screen first.
pub fn render_certificate_warning(ui: &mut egui::Ui, info: &CertificateFacts) -> CertificateAction {
    let mut action = CertificateAction::None;
    // Dim background
    let screen = ui.clip_rect();
    ui.painter()
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(120));

    egui::Window::new("cert_warning_modal")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .fixed_size([520.0, 0.0])
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::WHITE)
                .stroke(egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgb(0xDD, 0xDD, 0xDD),
                ))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(36, 30)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(448.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("⚠")
                        .color(egui::Color32::from_rgb(0xE7, 0xB8, 0x1C))
                        .size(28.0),
                );
                ui.add_space(10.0);
                ui.heading(
                    egui::RichText::new("Verify this host")
                        .color(egui::Color32::BLACK)
                        .size(22.0),
                );
            });
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(
                    "The server certificate is not issued by a trusted authority. \
                     Compare these values with the host administrator before granting \
                     session-only trust.",
                )
                .color(egui::Color32::from_rgb(0x44, 0x44, 0x44))
                .size(14.0),
            );
            ui.add_space(14.0);
            let mono_color = egui::Color32::from_rgb(0x55, 0x55, 0x55);
            for (label, value) in [
                ("Endpoint", info.endpoint.clone()),
                ("Server name", info.server_name.clone()),
                // Which address actually presented this certificate. A
                // hostname can resolve to several, and all of them are
                // dialled; without this the dialog could show a fingerprint
                // under a name the user trusts while it came from somewhere
                // else on that name.
                (
                    "Peer address",
                    info.peer_address.clone().unwrap_or_else(|| "—".to_string()),
                ),
                ("SPKI SHA-256", info.spki_display.clone()),
                ("Certificate SHA-256", info.certificate_display.clone()),
                (
                    "Valid (Unix UTC)",
                    format!(
                        "{} – {}",
                        info.not_before_epoch_secs, info.not_after_epoch_secs
                    ),
                ),
            ] {
                ui.label(
                    egui::RichText::new(format!("{label}   {value}"))
                        .color(mono_color)
                        .monospace()
                        .size(12.5),
                );
            }
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Compare this fingerprint with the Pier before trusting it. \
                     Remembering it means you will be warned if this host's identity \
                     ever changes.",
                )
                .strong()
                .color(egui::Color32::from_rgb(0x44, 0x44, 0x44))
                .size(13.0),
            );
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Trust and Remember")
                                .color(egui::Color32::WHITE)
                                .size(14.0),
                        )
                        .fill(theme::ACCENT)
                        .min_size(egui::vec2(184.0, 36.0)),
                    )
                    .clicked()
                {
                    action = CertificateAction::TrustAndRemember;
                }
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Trust Once").size(14.0))
                            .min_size(egui::vec2(120.0, 36.0)),
                    )
                    .clicked()
                {
                    action = CertificateAction::TrustForSession;
                }
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Cancel").size(14.0))
                            .min_size(egui::vec2(88.0, 36.0)),
                    )
                    .clicked()
                {
                    action = CertificateAction::Cancel;
                }
            });
        });
    action
}

/// Shown when a *remembered* identity no longer matches what answered.
///
/// Presents the fingerprint this Deck trusted and when it was trusted, and
/// names both explanations without ranking them — the benign one (the host
/// was reinstalled or its certificate was regenerated) and the one the
/// mechanism exists to catch. The action is gated behind an explicit tick
/// so that clearing trust cannot be done by reflex.
pub fn render_certificate_changed(
    ui: &mut egui::Ui,
    endpoint: &str,
    remembered: &RememberedIdentity,
    acknowledged: &mut bool,
) -> CertificateChangedAction {
    let mut action = CertificateChangedAction::None;
    let screen = ui.clip_rect();
    ui.painter()
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(140));

    let danger = egui::Color32::from_rgb(0xC0, 0x39, 0x2B);
    let remembered_display = &remembered.spki_display;

    egui::Window::new("cert_changed_modal")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .fixed_size([560.0, 0.0])
        .frame(
            egui::Frame::new()
                .fill(egui::Color32::WHITE)
                .stroke(egui::Stroke::new(1.5, danger))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(36, 30)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(488.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("⛔").color(danger).size(28.0));
                ui.add_space(10.0);
                ui.heading(
                    egui::RichText::new("This host's identity changed")
                        .color(egui::Color32::BLACK)
                        .size(22.0),
                );
            });
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(
                    "Arcen remembered this host's certificate, and the server that \
                     answered presented a different one. Arcen refused to send your \
                     credentials.",
                )
                .color(egui::Color32::from_rgb(0x44, 0x44, 0x44))
                .size(14.0),
            );
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(
                    "This happens when the Pier is reinstalled or its certificate is \
                     regenerated — and it is also what interception looks like. Confirm \
                     with whoever runs the host before continuing.",
                )
                .color(egui::Color32::from_rgb(0x44, 0x44, 0x44))
                .size(14.0),
            );
            ui.add_space(16.0);

            let mono_color = egui::Color32::from_rgb(0x55, 0x55, 0x55);
            for (label, value) in [
                ("Endpoint", endpoint.to_string()),
                (
                    "Trusted as",
                    remembered.label.clone().unwrap_or_else(|| "—".to_string()),
                ),
                (
                    "Trusted on",
                    remembered
                        .pinned_at
                        .clone()
                        .unwrap_or_else(|| "—".to_string()),
                ),
                ("Remembered SPKI", remembered_display.clone()),
            ] {
                ui.label(
                    egui::RichText::new(format!("{label}   {value}"))
                        .color(mono_color)
                        .monospace()
                        .size(12.5),
                );
            }
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(
                    "Forgetting does not trust the new certificate. Arcen reconnects \
                     straight away and shows it to you for comparison first.",
                )
                .color(egui::Color32::from_rgb(0x44, 0x44, 0x44))
                .size(13.0),
            );
            ui.add_space(14.0);
            ui.checkbox(
                acknowledged,
                egui::RichText::new(
                    "I confirmed with the host administrator that its certificate changed",
                )
                .color(egui::Color32::BLACK)
                .size(13.0),
            );
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                let armed = *acknowledged;
                if ui
                    .add_enabled(
                        armed,
                        egui::Button::new(
                            egui::RichText::new("Forget and Verify")
                                .color(egui::Color32::WHITE)
                                .size(14.0),
                        )
                        .fill(danger)
                        .min_size(egui::vec2(196.0, 36.0)),
                    )
                    .clicked()
                {
                    action = CertificateChangedAction::ForgetAndReconnect;
                }
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(egui::RichText::new("Cancel").size(14.0))
                            .min_size(egui::vec2(96.0, 36.0)),
                    )
                    .clicked()
                {
                    action = CertificateChangedAction::Cancel;
                }
            });
        });
    action
}
