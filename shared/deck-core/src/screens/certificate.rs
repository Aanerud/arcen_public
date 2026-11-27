// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Trust on first use: the "Verify this host" prompt for an unknown host and
//! the "identity changed" warning for a remembered one. The shell owns the
//! trust store and maps each action.

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

/// A remembered host identity that no longer matches: what Home's
/// "This computer's identity has changed" screen shows, the remembered
/// fingerprint next to the one the host presented instead, of the same kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityChange {
    pub endpoint: String,
    /// What the user called the host when they trusted it.
    pub label: Option<String>,
    /// When it was trusted, as a date.
    pub trusted_on: Option<String>,
    /// "public key" or "certificate": what the fingerprints are of.
    pub fingerprint_of: &'static str,
    pub remembered: String,
    pub presented: String,
    /// The identity remembered for the endpoint when the screen was made
    /// (`None` when the stored entry is unreadable). "Trust and connect"
    /// replaces it only while it is still the one on disk.
    pub previous: Option<arcen_transport::tls::TlsPin>,
    /// The identity the screen shows the host presented, of the remembered
    /// one's kind: exactly what "Trust and connect" remembers and the
    /// retry is held to.
    pub replacement: arcen_transport::tls::TlsPin,
}

#[cfg(feature = "runtime")]
impl IdentityChange {
    /// The facts for `endpoint`'s remembered `pin` and the certificate the
    /// host `presented` instead.
    #[must_use]
    pub fn new(
        endpoint: &str,
        pin: &crate::trusted_pins::StoredPin,
        presented: &crate::runtime::tls::CertInfo,
    ) -> Self {
        use crate::runtime::tls::format_fingerprint;
        use arcen_transport::tls::PinKind;
        let previous = pin.to_pin();
        let (fingerprint_of, replacement) = match previous.map(|pin| pin.kind) {
            Some(PinKind::CertificateSha256) => ("certificate", presented.certificate_sha256),
            _ => ("public key", presented.spki_sha256),
        };
        Self {
            endpoint: endpoint.to_owned(),
            label: pin.label.clone(),
            trusted_on: pin
                .pinned_at
                .as_deref()
                .map(|at| at.get(..10).unwrap_or(at).to_owned()),
            fingerprint_of,
            remembered: pin
                .to_pin()
                .map_or_else(|| pin.digest.clone(), |pin| format_fingerprint(&pin.digest)),
            presented: format_fingerprint(&replacement.digest),
            previous,
            replacement,
        }
    }
}

/// What "Trust and connect" on the identity-changed screen did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityReplaced {
    /// The new identity is remembered, in place of the old one, on disk
    /// and for this run: connect again.
    Saved,
    /// Remembered for this run only (the reason is for the status line):
    /// connect again.
    ThisRunOnly(String),
    /// Another Deck window changed this host's remembered identity while
    /// the screen was open: nothing was trusted, and the user must look
    /// again before anything connects.
    ChangedElsewhere,
}

/// "Trust and connect": replaces the identity `change` showed as
/// remembered with the one it showed instead, the same kind (a
/// certificate pin stays a certificate pin), in one locked write of that
/// endpoint alone, and only while the remembered identity on disk is
/// still the one shown. Both Decks call this; `pins` is the Deck's copy.
#[cfg(feature = "runtime")]
pub fn replace_remembered_identity(
    repository: &crate::settings::ClientConfigRepository,
    pins: &mut crate::trusted_pins::TrustedPinStore,
    change: &IdentityChange,
) -> IdentityReplaced {
    use crate::trusted_pins::{PinWriteError, remember_reviewed};
    let path = repository.trusted_pins_file();
    match remember_reviewed(
        path.as_deref(),
        pins,
        &change.endpoint,
        change.previous,
        &change.replacement,
        change.label.clone(),
    ) {
        Ok(()) => IdentityReplaced::Saved,
        Err(PinWriteError::Changed { .. }) => IdentityReplaced::ChangedElsewhere,
        Err(error) => {
            tracing::warn!(
                target: crate::log_target::UI,
                endpoint = %change.endpoint,
                %error,
                "the new host identity could not be saved; trusted for this run only",
            );
            pins.remember(&change.endpoint, &change.replacement, change.label.clone());
            IdentityReplaced::ThisRunOnly(format!(
                "Trusted for this session only: the identity could not be saved ({error})."
            ))
        }
    }
}

/// What a pinned host's other certificate leads to, on every Deck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityChangeOutcome {
    /// The recovery screen, with this comparison.
    Recover(IdentityChange),
    /// Not this Deck's to replace: Home's plain "identity" box, with this
    /// detail.
    Refuse { detail: String },
}

/// Whether a host's changed identity gets the recovery screen, decided once
/// for both Decks. Only an identity this Deck chose to remember: a pin the
/// connection carries itself (configuration, `--pin-sha256`) is someone
/// else's assertion about the host, wins at dial time and is never offered
/// for replacement, or confirming would write the presented key into the
/// remembered store and a later connection without that pin would trust it
/// silently; a session-only pin leaves with the session.
#[cfg(feature = "runtime")]
#[must_use]
pub fn identity_change_outcome(
    draft: &crate::connection_draft::ConnectionDraft,
    remembered: &crate::trusted_pins::TrustedPinStore,
    presented: &crate::runtime::tls::CertInfo,
) -> IdentityChangeOutcome {
    let endpoint = draft.endpoint();
    match remembered
        .pins
        .get(&endpoint)
        .filter(|_| draft.tls_trust.is_none())
    {
        Some(pin) => IdentityChangeOutcome::Recover(IdentityChange::new(&endpoint, pin, presented)),
        None => IdentityChangeOutcome::Refuse {
            detail: format!(
                "{endpoint} presented a certificate with public key {} instead of the one \
                 this Deck trusts for it.",
                presented.spki_sha256_display
            ),
        },
    }
}

/// A date (UTC) for seconds since the Unix epoch, `YYYY-MM-DD`.
#[must_use]
pub fn utc_date(epoch_secs: u64) -> String {
    // Days to a civil date (proleptic Gregorian), after H. Hinnant's
    // `civil_from_days`.
    let days = i64::try_from(epoch_secs / 86_400).unwrap_or(i64::MAX / 2) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// What the user did on the "Verify this host" prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateAction {
    None,
    Cancel,
    TrustForSession,
    TrustAndRemember,
}

/// The sign-in a "Verify this host" prompt holds for the retry it may
/// approve, after the user's `action`: a trusting answer takes it for that
/// retry, Cancel drops it (it wipes itself), and no answer yet keeps it.
/// The prompt owns it alone: a Deck drops it as well whenever the prompt
/// is no longer the screen (Settings, Home, a menu command), as it does a
/// recovery screen's.
pub fn certificate_retry_auth<A>(action: CertificateAction, held: &mut Option<A>) -> Option<A> {
    match action {
        CertificateAction::None => None,
        CertificateAction::Cancel => {
            *held = None;
            None
        }
        CertificateAction::TrustForSession | CertificateAction::TrustAndRemember => held.take(),
    }
}

/// What the user decided when a remembered identity turned out not to
/// match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateChangedAction {
    None,
    /// Leave everything as it is and go back (the default).
    Cancel,
    /// Replace the remembered identity with the presented one and connect.
    TrustNewIdentity,
}

/// What was pressed on the identity-changed screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertificateChangedPress {
    Cancel,
    /// "Trust the new identity…": asks for confirmation first.
    TrustNewIdentity,
    /// "Trust and connect" on the confirmation.
    Confirm,
    /// "Go back" on the confirmation.
    GoBack,
    /// Enter or Escape: the default button of the step shown.
    Default,
}

/// The identity-changed screen's two steps. Trusting a replacement takes a
/// second, deliberate press, never one click or a key: the default is
/// always to leave the remembered identity alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CertificateChangedStep {
    confirming: bool,
}

impl CertificateChangedStep {
    #[must_use]
    pub const fn confirming(self) -> bool {
        self.confirming
    }

    /// The decision a press makes.
    #[must_use]
    pub const fn press(&mut self, press: CertificateChangedPress) -> CertificateChangedAction {
        match press {
            CertificateChangedPress::Cancel => CertificateChangedAction::Cancel,
            CertificateChangedPress::Default if !self.confirming => {
                CertificateChangedAction::Cancel
            }
            CertificateChangedPress::TrustNewIdentity => {
                self.confirming = true;
                CertificateChangedAction::None
            }
            CertificateChangedPress::Confirm if self.confirming => {
                CertificateChangedAction::TrustNewIdentity
            }
            CertificateChangedPress::Confirm => CertificateChangedAction::None,
            // On the confirmation, Enter or Escape goes back; it never trusts.
            CertificateChangedPress::GoBack | CertificateChangedPress::Default => {
                self.confirming = false;
                CertificateChangedAction::None
            }
        }
    }
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
                .fill(super::theme::colours(ui).raised)
                .stroke(egui::Stroke::new(1.0, super::theme::colours(ui).border))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(36, 30)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(448.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("⚠")
                        .color(super::theme::colours(ui).warning)
                        .size(28.0),
                );
                ui.add_space(10.0);
                ui.heading(
                    egui::RichText::new("Verify this host")
                        .color(super::theme::colours(ui).text)
                        .size(22.0),
                );
            });
            ui.add_space(14.0);
            ui.label(
                egui::RichText::new(
                    "This host's certificate is not from an authority this computer \
                     trusts. Compare its fingerprint with the one your administrator \
                     gives you before you trust it.",
                )
                .color(super::theme::colours(ui).text2)
                .size(14.0),
            );
            ui.add_space(14.0);
            let mono_color = super::theme::colours(ui).text2;
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
                    "Valid (UTC)",
                    format!(
                        "{} to {}",
                        utc_date(info.not_before_epoch_secs),
                        utc_date(info.not_after_epoch_secs)
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
                    "Trust and Remember: this Deck remembers this host and warns you if \
                     its identity ever changes. Trust Once: only until you quit.",
                )
                .strong()
                .color(super::theme::colours(ui).text2)
                .size(13.0),
            );
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                use super::widgets::{button_width, primary_button, secondary_button};
                ui.spacing_mut().item_spacing.x = super::theme::BUTTON_GAP;
                let remember = "Trust and Remember";
                if primary_button(ui, remember, button_width(ui, &[remember])).clicked() {
                    action = CertificateAction::TrustAndRemember;
                }
                if secondary_button(ui, "Trust Once", button_width(ui, &["Trust Once"])).clicked() {
                    action = CertificateAction::TrustForSession;
                }
                if secondary_button(ui, "Cancel", button_width(ui, &["Cancel"])).clicked() {
                    action = CertificateAction::Cancel;
                }
            });
        });
    action
}

/// "This computer's identity has changed": the remembered fingerprint next
/// to the one presented instead, both explanations named, Cancel the
/// default and trusting the new identity behind a confirmation
/// ([`CertificateChangedStep`]).
pub fn render_certificate_changed(
    ui: &mut egui::Ui,
    change: &IdentityChange,
    step: &mut CertificateChangedStep,
) -> CertificateChangedAction {
    let mut press = None;
    let screen = ui.clip_rect();
    ui.painter()
        .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(140));

    let danger = super::theme::colours(ui).error;
    let text = super::theme::colours(ui).text2;
    let host = change.label.as_deref().unwrap_or(&change.endpoint);

    egui::Window::new("cert_changed_modal")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .fixed_size([560.0, 0.0])
        .frame(
            egui::Frame::new()
                .fill(super::theme::colours(ui).raised)
                .stroke(egui::Stroke::new(1.5, danger))
                .corner_radius(12.0)
                .inner_margin(egui::Margin::symmetric(36, 30)),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(488.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("⚠").color(danger).size(28.0));
                ui.add_space(10.0);
                ui.heading(
                    egui::RichText::new("This computer\u{2019}s identity has changed")
                        .color(super::theme::colours(ui).text)
                        .size(22.0),
                );
            });
            ui.add_space(14.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{host} no longer has the identity this Deck remembered. This can \
                         happen after a reinstall, or it can mean someone is intercepting \
                         the connection. Ask your administrator if you\u{2019}re not sure. \
                         Arcen did not send your password."
                    ))
                    .color(text)
                    .size(14.0),
                )
                .wrap(),
            );
            ui.add_space(16.0);
            let mono = super::theme::colours(ui).text2;
            let rows = [
                ("Computer", change.endpoint.clone()),
                (
                    "Trusted on",
                    change.trusted_on.clone().unwrap_or_else(|| "—".to_owned()),
                ),
                (
                    "Remembered",
                    format!("{} ({})", change.remembered, change.fingerprint_of),
                ),
                (
                    "Now presented",
                    format!("{} ({})", change.presented, change.fingerprint_of),
                ),
            ];
            for (label, value) in rows {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("{label}   {value}"))
                            .color(mono)
                            .monospace()
                            .size(12.5),
                    )
                    .wrap(),
                );
            }
            ui.add_space(18.0);
            if step.confirming() {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "Trust the new identity of {host}? Do this only if your \
                             administrator confirmed that it changed. This Deck will \
                             remember the new identity instead of the old one."
                        ))
                        .strong()
                        .color(super::theme::colours(ui).text)
                        .size(13.5),
                    )
                    .wrap(),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    use super::widgets::{button_width, secondary_button};
                    ui.spacing_mut().item_spacing.x = super::theme::BUTTON_GAP;
                    let back = secondary_button(ui, "Go back", button_width(ui, &["Go back"]));
                    back.request_focus();
                    if back.clicked() {
                        press = Some(CertificateChangedPress::GoBack);
                    }
                    if danger_button(ui, "Trust and connect", danger).clicked() {
                        press = Some(CertificateChangedPress::Confirm);
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    use super::widgets::{button_width, primary_button, secondary_button};
                    ui.spacing_mut().item_spacing.x = super::theme::BUTTON_GAP;
                    let cancel = primary_button(ui, "Cancel", button_width(ui, &["Cancel"]));
                    cancel.request_focus();
                    if cancel.clicked() {
                        press = Some(CertificateChangedPress::Cancel);
                    }
                    let trust = "Trust the new identity\u{2026}";
                    if secondary_button(ui, trust, button_width(ui, &[trust])).clicked() {
                        press = Some(CertificateChangedPress::TrustNewIdentity);
                    }
                });
            }
        });
    if press.is_none()
        && ui.ctx().input(|input| {
            input.key_pressed(egui::Key::Escape) || input.key_pressed(egui::Key::Enter)
        })
    {
        press = Some(CertificateChangedPress::Default);
    }
    press.map_or(CertificateChangedAction::None, |press| step.press(press))
}

/// The confirmation that replaces a remembered identity: the shared
/// control's size, text and radius, filled with the danger colour.
fn danger_button(ui: &mut egui::Ui, label: &str, fill: egui::Color32) -> egui::Response {
    use super::theme;
    let width = super::widgets::button_width(ui, &[label]);
    let response = ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .color(theme::colours(ui).on_primary)
                .size(theme::BUTTON_TEXT),
        )
        .fill(fill)
        .corner_radius(theme::CONTROL_RADIUS)
        .min_size(egui::vec2(width, theme::CONTROL_HEIGHT)),
    );
    if response.has_focus() {
        super::widgets::focus_ring(ui, response.rect);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_is_the_default_and_trusting_takes_a_confirmation() {
        let mut step = CertificateChangedStep::default();
        // Enter or Escape on the first step cancels.
        assert_eq!(
            step.press(CertificateChangedPress::Default),
            CertificateChangedAction::Cancel
        );
        // A stray confirm without the first press trusts nothing.
        assert_eq!(
            step.press(CertificateChangedPress::Confirm),
            CertificateChangedAction::None
        );
        // "Trust the new identity…" only asks.
        assert_eq!(
            step.press(CertificateChangedPress::TrustNewIdentity),
            CertificateChangedAction::None
        );
        assert!(step.confirming());
        // A key on the confirmation goes back, it never trusts.
        assert_eq!(
            step.press(CertificateChangedPress::Default),
            CertificateChangedAction::None
        );
        assert!(!step.confirming());
        let _ = step.press(CertificateChangedPress::TrustNewIdentity);
        assert_eq!(
            step.press(CertificateChangedPress::Confirm),
            CertificateChangedAction::TrustNewIdentity
        );
        assert_eq!(
            step.press(CertificateChangedPress::Cancel),
            CertificateChangedAction::Cancel
        );
    }

    #[test]
    fn only_a_trusting_answer_keeps_the_prompts_sign_in_for_its_retry() {
        let mut held = Some("typed");
        assert_eq!(
            certificate_retry_auth(CertificateAction::None, &mut held),
            None
        );
        assert_eq!(held, Some("typed"));
        assert_eq!(
            certificate_retry_auth(CertificateAction::Cancel, &mut held),
            None
        );
        assert_eq!(held, None);
        for trust in [
            CertificateAction::TrustForSession,
            CertificateAction::TrustAndRemember,
        ] {
            let mut held = Some("typed");
            assert_eq!(certificate_retry_auth(trust, &mut held), Some("typed"));
            assert_eq!(held, None);
        }
    }

    #[test]
    fn validity_reads_as_dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_792_288_745), "2026-10-18");
        assert_eq!(utc_date(1_863_568_745), "2029-01-20");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
    }

    #[cfg(feature = "runtime")]
    fn presented_fixture() -> crate::runtime::tls::CertInfo {
        use crate::runtime::tls::{CertInfo, format_fingerprint};
        use arcen_transport::tls::{PinKind, TlsPin};
        CertInfo {
            endpoint: "203.0.113.5:18444".to_owned(),
            server_name: "203.0.113.5".to_owned(),
            peer_address: None,
            certificate_sha256: TlsPin::new(PinKind::CertificateSha256, [2; 32]),
            certificate_sha256_display: format_fingerprint(&[2; 32]),
            spki_sha256: TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, [3; 32]),
            spki_sha256_display: format_fingerprint(&[3; 32]),
            not_before_epoch_secs: 0,
            not_after_epoch_secs: 0,
            cert_der_len: 0,
        }
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn trust_and_connect_remembers_the_kind_it_showed_in_one_reviewed_write() {
        use crate::settings::ClientConfigRepository;
        use crate::trusted_pins::{TrustedPinStore, load, save};
        use arcen_transport::tls::{PinKind, TlsPin};
        let root = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("arcen-identity-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repository = ClientConfigRepository::at(root.clone());
        let path = repository.trusted_pins_file().unwrap();
        let endpoint = "203.0.113.5:18444";
        let old = TlsPin::new(PinKind::CertificateSha256, [1; 32]);
        let mut pins = TrustedPinStore::default();
        pins.remember(endpoint, &old, Some("Studio".to_owned()));
        let mut disk = pins.clone();
        // Another window trusted a second host meanwhile.
        let other = TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, [9; 32]);
        disk.remember("198.51.100.7:18444", &other, None);
        save(&path, &disk).unwrap();

        let presented = presented_fixture();
        let change = IdentityChange::new(endpoint, &pins.pins[endpoint], &presented);
        // A certificate pin: the screen shows the certificate's
        // fingerprint, and that is what is remembered.
        assert_eq!(change.replacement, presented.certificate_sha256);
        assert_eq!(change.presented, presented.certificate_sha256_display);
        assert_eq!(
            replace_remembered_identity(&repository, &mut pins, &change),
            IdentityReplaced::Saved
        );
        let reloaded = load(&path).resolved();
        assert_eq!(reloaded[endpoint], presented.certificate_sha256);
        assert_eq!(reloaded["198.51.100.7:18444"], other);
        assert_eq!(pins.resolved(), reloaded);
        assert_eq!(pins.pins[endpoint].label.as_deref(), Some("Studio"));

        // A second, stale screen for the old identity trusts nothing.
        let mut stale = TrustedPinStore::default();
        stale.remember(endpoint, &old, None);
        let change = IdentityChange::new(endpoint, &stale.pins[endpoint], &presented);
        assert_eq!(
            replace_remembered_identity(&repository, &mut stale, &change),
            IdentityReplaced::ChangedElsewhere
        );
        assert_eq!(stale.resolved(), reloaded);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn only_a_remembered_identity_is_offered_for_replacement() {
        use crate::config::ConnectionKind;
        use crate::connection_draft::ConnectionDraft;
        use arcen_transport::tls::{PinKind, TlsPin};
        let presented = presented_fixture();
        let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
        draft.host = "203.0.113.5".to_owned();
        let mut remembered = crate::trusted_pins::TrustedPinStore::default();
        // Nothing remembered: the plain box.
        assert!(matches!(
            identity_change_outcome(&draft, &remembered, &presented),
            IdentityChangeOutcome::Refuse { .. }
        ));
        remembered.remember(
            "203.0.113.5:18444",
            &TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, [1; 32]),
            None,
        );
        assert!(matches!(
            identity_change_outcome(&draft, &remembered, &presented),
            IdentityChangeOutcome::Recover(_)
        ));
        // A pin the connection carries (configuration, `--pin-sha256`) wins
        // at dial time and is never replaced from the screen, even with a
        // remembered identity for the same endpoint.
        draft.tls_trust = Some(crate::runtime::tls::TlsTrustConfig::pinned_spki([9; 32]));
        let IdentityChangeOutcome::Refuse { detail } =
            identity_change_outcome(&draft, &remembered, &presented)
        else {
            panic!("a configured pin is offered for replacement");
        };
        assert!(detail.contains(&presented.spki_sha256_display));
        assert!(!detail.contains("TOFU"));
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn the_change_compares_fingerprints_of_the_same_kind() {
        use crate::runtime::tls::format_fingerprint;
        use arcen_transport::tls::{PinKind, TlsPin};
        let presented = presented_fixture();
        let spki = crate::trusted_pins::StoredPin::from_pin(
            &TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, [1; 32]),
            Some("Studio".to_owned()),
            Some("2026-11-02T10:00:00Z".to_owned()),
        );
        let change = IdentityChange::new("203.0.113.5:18444", &spki, &presented);
        assert_eq!(change.fingerprint_of, "public key");
        assert_eq!(change.remembered, format_fingerprint(&[1; 32]));
        assert_eq!(change.presented, format_fingerprint(&[3; 32]));
        assert_eq!(change.trusted_on.as_deref(), Some("2026-11-02"));
        let certificate = crate::trusted_pins::StoredPin::from_pin(
            &TlsPin::new(PinKind::CertificateSha256, [1; 32]),
            None,
            None,
        );
        let change = IdentityChange::new("203.0.113.5:18444", &certificate, &presented);
        assert_eq!(change.fingerprint_of, "certificate");
        assert_eq!(change.presented, format_fingerprint(&[2; 32]));
    }

    #[test]
    fn the_connecting_identity_help_and_disclaimer_screens_follow_both_palettes() {
        use super::super::theme::{Palette, apply, checks, contrast_ratio};
        use crate::connection_draft::ConnectionDraft;
        fn flatten(shape: egui::Shape, out: &mut Vec<egui::Shape>) {
            match shape {
                egui::Shape::Vec(shapes) => shapes.into_iter().for_each(|s| flatten(s, out)),
                shape => out.push(shape),
            }
        }
        let draft = ConnectionDraft::new(crate::config::ConnectionKind::DirectMachine);
        let facts = CertificateFacts {
            endpoint: "203.0.113.5:18444".to_owned(),
            server_name: "203.0.113.5".to_owned(),
            peer_address: None,
            spki_display: "AB:CD".to_owned(),
            certificate_display: "EF:01".to_owned(),
            not_before_epoch_secs: 0,
            not_after_epoch_secs: 1_792_288_745,
        };
        let change = IdentityChange {
            endpoint: "203.0.113.5:18444".to_owned(),
            label: Some("Studio".to_owned()),
            trusted_on: Some("2026-10-18".to_owned()),
            fingerprint_of: "public key",
            remembered: "AB:CD".to_owned(),
            presented: "EF:01".to_owned(),
            previous: None,
            replacement: arcen_transport::tls::TlsPin::new(
                arcen_transport::tls::PinKind::SubjectPublicKeyInfoSha256,
                [0xEF; 32],
            ),
        };
        type Scene<'a> = Box<dyn Fn(&mut egui::Ui) + 'a>;
        let scenes: Vec<(&str, Scene<'_>)> = vec![
            (
                "connecting",
                Box::new(|ui| {
                    let _ = super::super::connecting::render_connecting(
                        ui,
                        &draft,
                        "Signing in",
                        Some("Check the address."),
                    );
                }),
            ),
            (
                "topology",
                Box::new(|ui| {
                    let _ = super::super::connecting::render_topology_conflict(ui, &draft);
                }),
            ),
            (
                "displays",
                Box::new(|ui| {
                    let _ = super::super::connecting::render_displays_unsupported(
                        ui,
                        &draft,
                        "Two of three",
                    );
                }),
            ),
            (
                "certificate",
                Box::new(|ui| {
                    let _ = render_certificate_warning(ui, &facts);
                }),
            ),
            (
                "identity changed",
                Box::new(|ui| {
                    let mut step = CertificateChangedStep::default();
                    let _ = render_certificate_changed(ui, &change, &mut step);
                }),
            ),
            (
                "about",
                Box::new(|ui| {
                    let _ = super::super::about::render_about(ui.ctx(), "0.20.7", "test");
                }),
            ),
            (
                "keyboard shortcuts",
                Box::new(|ui| {
                    let mut open = true;
                    super::super::keyboard_help::render_keyboard_shortcuts(
                        ui.ctx(),
                        super::super::wording::DeckPlatform::Mac,
                        true,
                        arcen_input::keyboard::KeyboardSwitchChord::default(),
                        &mut open,
                    );
                }),
            ),
            (
                "account switch",
                Box::new(|ui| {
                    let mut prompt = crate::account_switch::AccountSwitchPrompt::default();
                    prompt.on_news(
                        crate::account_switch::AccountSwitchNews::Offered {
                            expires_in: std::time::Duration::from_secs(120),
                        },
                        std::time::Instant::now(),
                    );
                    let _ =
                        super::super::account_switch::render_account_switch(ui.ctx(), &mut prompt);
                }),
            ),
            (
                "disclaimer",
                Box::new(|ui| {
                    let _ = super::super::sign_in::render_disclaimer(
                        ui,
                        &draft,
                        "Authorised use only.",
                    );
                }),
            ),
        ];
        for dark in [false, true] {
            let colours = Palette::of(dark);
            for (scene, draw) in &scenes {
                let ctx = egui::Context::default();
                apply(&ctx);
                ctx.set_theme(if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                });
                let mut shapes = Vec::new();
                // Windows fade in over a few passes.
                for _ in 0..40 {
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(1120.0, 760.0),
                            )),
                            ..egui::RawInput::default()
                        },
                        |ui| draw(ui),
                    );
                    shapes.clear();
                    for clipped in output.shapes {
                        flatten(clipped.shape, &mut shapes);
                    }
                }
                for colour in checks::painted_colours(&shapes, colours.bg) {
                    assert!(
                        checks::quiet_enough(&colours, colour),
                        "{scene}, dark {dark}: {colour:?}"
                    );
                }
                for (text, ink, ground) in checks::texts_on_their_ground(&shapes, colours.bg) {
                    let ratio = contrast_ratio(ink, ground);
                    assert!(
                        ratio >= 4.5,
                        "{scene}, dark {dark}: {text:?} {ratio:.2}:1 ({ink:?} on {ground:?})"
                    );
                }
            }
        }
    }
}
