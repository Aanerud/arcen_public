// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Home's Export… and Import… ([`crate::profile`]): the platform's file
//! dialogs through [`DeckServices`], and the import summary, which saves
//! nothing until the user confirms and trusts a host identity only when
//! asked to. Both Decks own one [`ProfileSharing`] and call it from Home.

use std::path::Path;

use crate::DeckServices;
use crate::config::SavedConnectionSummary;
use crate::profile::{
    ConnectionProfile, HostFingerprint, ImportChoice, ImportDecision, ImportReview, PinReview,
    ProfileError, commit_import, export_connection, review_import, suggested_file_name,
};
use crate::settings::{ClientConfigRepository, SavedConnectionsStore, SecurityMode};
use crate::trusted_pins::TrustedPinStore;

/// What Export and Import need from the shell.
pub struct ProfileContext<'a> {
    /// The file dialogs.
    pub services: &'a dyn DeckServices,
    /// Where connections are saved.
    pub repository: &'a ClientConfigRepository,
    /// The security mode a new connection records.
    pub security_mode: SecurityMode,
    /// The saved connections as Home shows them.
    pub saved: &'a [SavedConnectionSummary],
    /// The host identities this Deck remembers (the shell's copy, which a
    /// confirmed import updates and saves).
    pub pins: &'a mut TrustedPinStore,
}

impl std::fmt::Debug for ProfileContext<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProfileContext")
            .field("saved", &self.saved.len())
            .finish_non_exhaustive()
    }
}

/// What the shell applies afterwards, as with a Home outcome.
#[derive(Debug, Default)]
pub struct ProfileOutcome {
    /// The saved connections, when they changed.
    pub store: Option<SavedConnectionsStore>,
    /// The status line: what Home shows as its notice
    /// ([`Self::home_notice`]).
    pub status: Option<String>,
    /// Whether the status says something did not happen or was only
    /// partly done.
    pub problem: bool,
    /// The connection to select (an import).
    pub select: Option<String>,
}

impl ProfileOutcome {
    fn problem(status: String) -> Self {
        Self {
            status: Some(status),
            problem: true,
            ..Self::default()
        }
    }

    /// Home's notice for this outcome, if it has one
    /// ([`super::home::HomeViewState::notice`]).
    #[must_use]
    pub fn home_notice(&self) -> Option<super::home::HomeNotice> {
        self.status.as_ref().map(|text| super::home::HomeNotice {
            text: text.clone(),
            problem: self.problem,
        })
    }
}

#[derive(Debug)]
struct Pending {
    review: ImportReview,
    choice: Option<ImportChoice>,
    trust: bool,
    error: Option<String>,
}

/// Export and import of connection profiles.
#[derive(Debug, Default)]
pub struct ProfileSharing {
    pending: Option<Pending>,
}

enum ReviewAction {
    Import,
    Cancel,
}

impl ProfileSharing {
    /// Whether the import summary is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.pending.is_some()
    }

    /// **UI thread.** Export… on a saved connection: asks where, then
    /// writes the file.
    pub fn export(&mut self, index: usize, context: &ProfileContext<'_>) -> ProfileOutcome {
        let Some(connection) = context.saved.get(index) else {
            return ProfileOutcome::default();
        };
        let Some(path) = context
            .services
            .choose_profile_to_save(&suggested_file_name(&connection.name))
        else {
            return ProfileOutcome::default();
        };
        match export_connection(connection, context.pins, &path) {
            Ok(status) => ProfileOutcome {
                status: Some(status),
                ..ProfileOutcome::default()
            },
            Err(error) => ProfileOutcome::problem(format!("Not exported: {error}")),
        }
    }

    /// **UI thread.** Import…: asks for a file, then opens its summary.
    pub fn import(&mut self, context: &ProfileContext<'_>) -> ProfileOutcome {
        match context.services.choose_profile_to_open() {
            Some(path) => self.open_file(&path, context),
            None => ProfileOutcome::default(),
        }
    }

    /// Reads `path` and opens its summary, or says why it cannot.
    pub fn open_file(&mut self, path: &Path, context: &ProfileContext<'_>) -> ProfileOutcome {
        match ConnectionProfile::read(path) {
            Ok(profile) => {
                self.review(profile, context.saved, context.pins);
                ProfileOutcome::default()
            }
            Err(error) => ProfileOutcome::problem(format!("Not imported: {error}")),
        }
    }

    /// Opens the summary of a profile already read.
    pub fn review(
        &mut self,
        profile: ConnectionProfile,
        saved: &[SavedConnectionSummary],
        pins: &TrustedPinStore,
    ) {
        let review = review_import(profile, saved, pins);
        self.pending = Some(Pending {
            choice: review.default_choice(),
            review,
            trust: false,
            error: None,
        });
    }

    /// Shows the import summary while one is open and applies the user's
    /// answer.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        context: &mut ProfileContext<'_>,
    ) -> ProfileOutcome {
        let Some(pending) = &mut self.pending else {
            return ProfileOutcome::default();
        };
        match render_review(ctx, pending) {
            None => ProfileOutcome::default(),
            Some(ReviewAction::Cancel) => {
                self.pending = None;
                ProfileOutcome {
                    status: Some("Import cancelled; nothing was saved.".to_owned()),
                    ..ProfileOutcome::default()
                }
            }
            Some(ReviewAction::Import) => {
                let Some(choice) = pending.choice else {
                    pending.error = Some(ProfileError::ChoiceRequired.to_string());
                    return ProfileOutcome::default();
                };
                let decision = ImportDecision {
                    choice,
                    trust_host_identity: pending.trust,
                };
                match commit_import(
                    &pending.review,
                    decision,
                    context.repository,
                    context.security_mode,
                    context.saved,
                    context.pins,
                ) {
                    Ok(committed) => {
                        self.pending = None;
                        ProfileOutcome {
                            store: Some(committed.store),
                            status: Some(committed.status),
                            problem: committed.pin_unsaved,
                            select: Some(committed.connection_id),
                        }
                    }
                    Err(error @ ProfileError::Changed) => {
                        self.pending = None;
                        ProfileOutcome::problem(format!("Not imported: {error}"))
                    }
                    // The summary is made again with the identity trusted
                    // now, and asks again.
                    Err(error @ ProfileError::IdentityChanged) => {
                        let review = review_import(
                            pending.review.profile.clone(),
                            context.saved,
                            context.pins,
                        );
                        *pending = Pending {
                            choice: pending.choice.or_else(|| review.default_choice()),
                            review,
                            trust: false,
                            error: Some(error.to_string()),
                        };
                        ProfileOutcome::default()
                    }
                    Err(error) => {
                        pending.error = Some(error.to_string());
                        ProfileOutcome::default()
                    }
                }
            }
        }
    }
}

/// Selects the saved connection `id` on Home, as clicking it would: the
/// draft takes its address and username, and no password; another
/// connection's sign-in outcome and certificate trust do not follow it.
pub fn select(
    draft: &mut crate::connection_draft::ConnectionDraft,
    sign_in: &mut crate::sign_in::SignInState,
    saved: &[SavedConnectionSummary],
    id: &str,
) {
    let Some(connection) = saved.iter().find(|connection| connection.id == id) else {
        return;
    };
    if draft.saved_connection_id.as_deref() != Some(id) {
        sign_in.clear();
    }
    // As on Home: another endpoint's certificate trust does not follow.
    draft.select_saved(connection);
}

fn render_review(ctx: &egui::Context, pending: &mut Pending) -> Option<ReviewAction> {
    let mut action = None;
    let response = egui::Modal::new(egui::Id::new("arcen-profile-import")).show(ctx, |ui| {
        ui.set_max_width(480.0);
        ui.heading("Import connection");
        ui.add_space(8.0);
        egui::Grid::new("arcen-profile-import-facts")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                for line in pending.review.connection_lines() {
                    ui.label(
                        egui::RichText::new(line.label).color(super::theme::colours(ui).text2),
                    );
                    ui.label(line.value);
                    ui.end_row();
                }
            });
        ui.add_space(8.0);
        ui.label(egui::RichText::new("On this machine").strong());
        let overrides = pending.review.override_lines();
        if overrides.is_empty() {
            ui.label("Every setting follows Settings.");
        } else {
            egui::Grid::new("arcen-profile-import-overrides")
                .num_columns(2)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for line in overrides {
                        ui.label(
                            egui::RichText::new(line.label).color(super::theme::colours(ui).text2),
                        );
                        ui.label(line.value);
                        ui.end_row();
                    }
                });
            ui.label(
                egui::RichText::new("Everything else follows Settings.")
                    .color(super::theme::colours(ui).text2),
            );
        }
        ui.label(
            egui::RichText::new("The host's own policy still decides what a session may do.")
                .color(super::theme::colours(ui).text2),
        );
        ui.add_space(8.0);
        identity(ui, pending);
        for note in pending.review.field_notes() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(note)
                    .color(super::theme::colours(ui).text2)
                    .size(11.5),
            );
        }
        if let Some(existing) = &pending.review.existing {
            ui.add_space(8.0);
            ui.label(format!(
                "You already have \u{201c}{}\u{201d} for this machine.",
                existing.connection.name
            ));
            ui.radio_value(
                &mut pending.choice,
                Some(ImportChoice::Replace),
                "Replace it with this file's connection",
            );
            ui.radio_value(
                &mut pending.choice,
                Some(ImportChoice::KeepBoth),
                "Keep both",
            );
        }
        if let Some(error) = &pending.error {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(error).color(super::theme::colours(ui).error));
        }
        ui.add_space(12.0);
        let platform = super::wording::deck_platform(ui.ctx());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = super::theme::BUTTON_GAP;
            let width = super::widgets::button_width(ui, &["Cancel", "Import"]);
            // The connection form's order on each platform: Import is its
            // Save.
            for button in super::home::form_button_order(platform) {
                match button {
                    super::home::FormButton::Cancel => {
                        if super::widgets::secondary_button(ui, "Cancel", width).clicked() {
                            action = Some(ReviewAction::Cancel);
                        }
                    }
                    super::home::FormButton::Save => {
                        let import = ui
                            .add_enabled_ui(pending.choice.is_some(), |ui| {
                                super::widgets::primary_button(ui, "Import", width)
                            })
                            .inner;
                        if import.clicked() {
                            action = Some(ReviewAction::Import);
                        }
                    }
                }
            }
        });
    });
    if action.is_none() && response.should_close() {
        action = Some(ReviewAction::Cancel);
    }
    action
}

fn identity(ui: &mut egui::Ui, pending: &mut Pending) {
    ui.label(egui::RichText::new("Host identity").strong());
    let differs = matches!(pending.review.pin, PinReview::Differs { .. });
    ui.label(
        egui::RichText::new(pending.review.identity_text()).color(if differs {
            super::theme::colours(ui).error
        } else {
            super::theme::colours(ui).text2
        }),
    );
    match pending.review.pin {
        PinReview::NotIncluded => {}
        PinReview::AlreadyTrusted(file) => fingerprint(ui, "Fingerprint", file),
        PinReview::New(file) => {
            fingerprint(ui, "In this file", file);
            ui.checkbox(&mut pending.trust, "Trust this host identity");
        }
        PinReview::Differs { saved, file } => {
            fingerprint(ui, "You trust now", saved);
            fingerprint(ui, "In this file", file);
            ui.checkbox(
                &mut pending.trust,
                "Replace the identity I trust with the one in this file",
            );
        }
    }
}

fn fingerprint(ui: &mut egui::Ui, label: &str, fingerprint: HostFingerprint) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(format!("{label} ({})", fingerprint.kind_label()))
            .color(super::theme::colours(ui).text2)
            .size(11.5),
    );
    ui.add(
        egui::Label::new(
            egui::RichText::new(fingerprint.display())
                .monospace()
                .size(11.0),
        )
        .wrap(),
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::screens::wording::{DeckPlatform, set_deck_platform};

    /// Where the summary draws `Cancel` and `Import` (x of each label).
    fn buttons_shown(platform: DeckPlatform) -> (f32, f32) {
        fn texts(shape: &egui::Shape, out: &mut Vec<(String, f32)>) {
            match shape {
                egui::Shape::Text(text) => out.push((
                    text.galley.job.text.clone(),
                    text.galley.rect.translate(text.pos.to_vec2()).center().x,
                )),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| texts(shape, out)),
                _ => {}
            }
        }
        let bytes = br#"{"format":"arcen-connection","version":1,"name":"Studio","address":"pier.example.internal","port":18444}"#;
        let review = review_import(
            ConnectionProfile::parse(bytes).unwrap(),
            &[],
            &TrustedPinStore::default(),
        );
        let mut pending = Pending {
            choice: review.default_choice(),
            review,
            trust: false,
            error: None,
        };
        let ctx = egui::Context::default();
        set_deck_platform(&ctx, platform);
        let mut shown = Vec::new();
        // The modal's first pass only measures it.
        for _ in 0..2 {
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1120.0, 760.0),
                    )),
                    ..egui::RawInput::default()
                },
                |ui| {
                    let _ = render_review(ui.ctx(), &mut pending);
                },
            );
            shown.clear();
            for clipped in &output.shapes {
                texts(&clipped.shape, &mut shown);
            }
        }
        let x = |label: &str| {
            shown
                .iter()
                .find(|(text, _)| text == label)
                .map_or_else(|| panic!("no {label}: {shown:?}"), |(_, x)| *x)
        };
        (x("Cancel"), x("Import"))
    }

    #[test]
    fn the_import_summary_orders_its_buttons_as_the_connection_form() {
        for platform in [DeckPlatform::Mac, DeckPlatform::Pc] {
            let (cancel, import) = buttons_shown(platform);
            let order = if cancel < import {
                [
                    super::super::home::FormButton::Cancel,
                    super::super::home::FormButton::Save,
                ]
            } else {
                [
                    super::super::home::FormButton::Save,
                    super::super::home::FormButton::Cancel,
                ]
            };
            assert_eq!(
                order,
                super::super::home::form_button_order(platform),
                "{platform:?}"
            );
        }
    }
}
