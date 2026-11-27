// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The manual's scenes, each built from the real shared types and drawn
//! with the real shared screens, as the shells draw them.
//!
//! Placeholders only: the machines are `grade-1.example.internal`,
//! `edit-2.example.internal` and `203.0.113.10`, the user is `alice`, an
//! address a name "resolves" to is from RFC 5737, and every fingerprint is
//! a SHA-256 of a fixed seed.

use std::sync::Arc;

use arcen_protocol::messages::{ClientMonitor, TabletModeMsg};
use sha2::Digest as _;

use super::capture::Name;
use super::services::DocShotServices;
use super::{DocPlatform, DocShotShell};
use crate::config::{
    ConnectionKind, SavedConnectionSettings, SavedConnectionSummary, SavedTransport,
};
use crate::connection_draft::ConnectionDraft;
use crate::overrides::{AppDefaults, ConnectionOverrides, Override};
use crate::screens::certificate::{
    CertificateChangedStep, CertificateFacts, IdentityChange, IdentityChangeOutcome,
    identity_change_outcome, render_certificate_changed, render_certificate_warning,
};
use crate::screens::connection_test::ConnectionTest;
use crate::screens::home::{HomeAction, HomeEffect, HomeNotice, HomeViewState, apply_home_action};
use crate::screens::launcher::render_launcher_top_bar;
use crate::screens::profile::{ProfileContext, ProfileSharing};
use crate::screens::settings::{
    NativeTabletComponent, SettingsAction, SettingsFacts, SettingsMonitor, SettingsTab,
    SettingsValues, render_settings,
};
use crate::screens::support_bundle::{SupportBundleContext, SupportBundlePanel};
use crate::settings::{
    ClientConfigRepository, ClientSettings, ColorFidelity, DisplaysMode, PerformanceMode,
};
use crate::sign_in::{SignInFailure, SignInStage, SignInState};
use crate::trusted_pins::{StoredPin, TrustedPinStore};

/// Every scene of the manual (`deck-shots/manifest.json`, release 0.20.7),
/// in its order, then the scenes added since (not in the manifest yet).
pub const SCENE_IDS: [&str; 22] = [
    "home",
    "home-empty",
    "add-menu",
    "row-menu",
    "connection-new",
    "connection-picker",
    "connection-test",
    "settings-streaming",
    "settings-displays",
    "settings-input",
    "settings-tablet",
    "settings-logs",
    "settings-advanced",
    "sign-in-error",
    "verify-host",
    "identity-changed",
    "import-review",
    "home-own-settings",
    "sign-in-refused",
    "settings-privacy",
    "connecting-one-display",
    "account-switch",
];

/// The first machine: Grade 1.
pub const GRADE_HOST: &str = "grade-1.example.internal";
/// The second: Edit bay.
pub const EDIT_HOST: &str = "edit-2.example.internal";
/// The third, saved by its address: Studio.
pub const STUDIO_HOST: &str = "203.0.113.10";
/// The address Grade 1's name stands for in the connection test and the
/// certificate prompts (RFC 5737; never looked up).
pub const GRADE_ADDRESS: &str = "203.0.113.11";
/// The user.
pub const USER: &str = "alice";
/// What a Pier answers a sign-in with when nobody is signed in at its
/// desk and its login screen is not running (the sign-in-refused scene).
pub const CONSOLE_REFUSAL: arcen_session::console_session::ConsoleRefusal =
    arcen_session::console_session::ConsoleRefusal::NoConsoleSession;
/// What a Linux Pier answers a wrong password with (its own words reach
/// the Deck's "Details").
pub const PIER_REFUSAL: &str = "Authentication failed: wrong user name or password, or this \
                                account may not sign in at this workstation.";

/// "Now", for certificate dates and the connection test: 2026-11-20.
const NOW_EPOCH_SECS: u64 = 1_795_132_800;
/// A Pier certificate's validity: 2026-11-01 to 2027-11-01.
const NOT_BEFORE_EPOCH_SECS: u64 = 1_793_491_200;
const NOT_AFTER_EPOCH_SECS: u64 = 1_825_027_200;

/// A press and release on the widget `name` names, the `nth` from the top
/// left when several have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Click {
    pub name: Name,
    pub nth: usize,
}

const fn click(name: Name) -> Click {
    Click { name, nth: 0 }
}

/// The screen a scene shows.
#[derive(Debug)]
enum Screen {
    Home,
    Settings,
    Certificate(CertificateFacts),
    CertificateChanged(IdentityChange, CertificateChangedStep),
    /// Connecting, with the attempt's status line.
    Connecting(ConnectionDraft, String),
    /// Connecting, with the host's offer to sign another account out over
    /// it.
    AccountSwitch(
        ConnectionDraft,
        String,
        crate::account_switch::AccountSwitchPrompt,
    ),
}

/// Home's state, owned (the shells keep the same fields).
#[derive(Debug)]
struct HomeModel {
    direct: ConnectionDraft,
    saved: Vec<SavedConnectionSummary>,
    search: String,
    show_add_form: bool,
    editing_index: Option<usize>,
    form_name: String,
    form_host: String,
    form_username: String,
    form_overrides: ConnectionOverrides,
    sign_in: SignInState,
    remember_username: bool,
    test: Option<ConnectionTest>,
    /// What Export or Import last did (no manifest scene shows one).
    notice: Option<HomeNotice>,
}

impl HomeModel {
    fn new(saved: Vec<SavedConnectionSummary>) -> Self {
        Self {
            direct: ConnectionDraft::new(ConnectionKind::DirectMachine),
            saved,
            search: String::new(),
            show_add_form: false,
            editing_index: None,
            form_name: String::new(),
            form_host: String::new(),
            form_username: String::new(),
            form_overrides: ConnectionOverrides::default(),
            sign_in: SignInState::default(),
            remember_username: true,
            test: None,
            notice: None,
        }
    }

    fn view(&mut self, app_defaults: AppDefaults) -> HomeViewState<'_> {
        HomeViewState {
            direct: &mut self.direct,
            saved_connections: &self.saved,
            search: &mut self.search,
            operator_notice: None,
            show_add_form: &mut self.show_add_form,
            editing_index: &mut self.editing_index,
            form_name: &mut self.form_name,
            form_host: &mut self.form_host,
            form_username: &mut self.form_username,
            form_overrides: &mut self.form_overrides,
            app_defaults,
            sign_in: &mut self.sign_in,
            overlay_open: false,
            remember_username: &mut self.remember_username,
            connection_test: self.test.as_ref(),
            notice: &mut self.notice,
        }
    }
}

/// One scene, ready to draw: its state and the clicks that bring it about.
pub struct Scene {
    id: &'static str,
    platform: DocPlatform,
    screen: Screen,
    home: HomeModel,
    settings: ClientSettings,
    values: SettingsValues,
    monitors: Vec<SettingsMonitor>,
    native_tablet: NativeTabletComponent,
    support: SupportBundlePanel,
    profile: ProfileSharing,
    pins: TrustedPinStore,
    repository: ClientConfigRepository,
    services: Arc<DocShotServices>,
    script: Vec<Click>,
    /// What the Pier answers Connect with.
    pier_answer: &'static str,
}

impl std::fmt::Debug for Scene {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Scene")
            .field("id", &self.id)
            .field("platform", &self.platform)
            .field("screen", &self.screen)
            .field("script", &self.script)
            .finish_non_exhaustive()
    }
}

impl Scene {
    /// The scene `id` as `platform`'s Deck shows it, or `None` for an id
    /// the manual does not have.
    #[must_use]
    pub fn build(id: &str, platform: DocPlatform) -> Option<Self> {
        let id = SCENE_IDS.into_iter().find(|known| *known == id)?;
        let mut scene = Self::base(id, platform, three_machines());
        match id {
            "home" | "connection-test" | "sign-in-error" | "add-menu" | "row-menu" => {
                scene.select_first();
            }
            "home-own-settings" => {
                // Grade 1 keeps two values of its own: Home's summary line
                // under Connect names them.
                let overrides = &mut scene.home.saved[0].settings.overrides;
                overrides.typing_mode = Override::Own(arcen_input::text::TypingMode::Keys);
                overrides.displays_mode = Override::Own(DisplaysMode::Windowed);
                scene.select_first();
            }
            "sign-in-refused" => {
                scene.select_first();
                scene.pier_answer = CONSOLE_REFUSAL.deck_reason();
            }
            "home-empty" => scene.home = HomeModel::new(Vec::new()),
            "connection-new" => {
                scene.select_first();
                scene.act(HomeAction::BeginAdd);
                GRADE_HOST.clone_into(&mut scene.home.form_host);
            }
            "connection-picker" => {
                scene.select_first();
                scene.act(HomeAction::Edit(0));
            }
            "settings-streaming" => {
                scene.settings_on(SettingsTab::Streaming);
                // Auto: the standard performance and colour presets.
                scene.values.performance = PerformanceMode::Standard;
                scene.values.color_fidelity.preset = ColorFidelity::Standard;
            }
            "settings-displays" => {
                scene.settings_on(SettingsTab::Displays);
                scene.values.displays = DisplaysMode::MatchLayout;
            }
            "settings-input" => scene.settings_on(SettingsTab::Input),
            "settings-tablet" => {
                scene.settings_on(SettingsTab::Tablet);
                scene.values.tablet_mode_requested = TabletModeMsg::LocalTermination;
            }
            "settings-logs" => scene.settings_on(SettingsTab::Logs),
            "settings-advanced" => scene.settings_on(SettingsTab::Advanced),
            "settings-privacy" => scene.settings_on(SettingsTab::Privacy),
            "verify-host" => {
                scene.select_first();
                let presented = certificate(&grade_draft(), "verify-host presented");
                scene.screen = Screen::Certificate(certificate_facts(&presented));
            }
            "identity-changed" => {
                scene.select_first();
                scene.screen = identity_changed(&mut scene.pins);
            }
            "connecting-one-display" => {
                scene.select_first();
                scene.screen = connecting_one_display();
            }
            "account-switch" => {
                scene.select_first();
                scene.screen = account_switch_question();
            }
            "import-review" => {
                scene.home = HomeModel::new(three_machines().into_iter().take(2).collect());
                scene.select_first();
                let profile = studio_profile();
                scene
                    .profile
                    .review(profile, &scene.home.saved, &scene.pins);
            }
            _ => {}
        }
        // The state as the user leaves it.
        if matches!(
            id,
            "home" | "sign-in-error" | "home-own-settings" | "sign-in-refused"
        ) {
            USER.clone_into(&mut scene.home.direct.username);
        }
        if matches!(id, "sign-in-error" | "sign-in-refused") {
            // A password that the Pier refuses, typed; Connect takes it.
            "not-the-password".clone_into(&mut scene.home.direct.password);
        }
        scene.script = match id {
            "add-menu" => vec![click(Name::Control("Add connection"))],
            "row-menu" => vec![click(Name::Control("Connection menu"))],
            "connection-picker" => vec![click(super::hotspots::TYPING_PICKER)],
            "connection-test" => vec![
                click(Name::Control("Connection menu")),
                click(Name::Control("Test connection")),
            ],
            "settings-logs" => vec![click(Name::Control("Create support bundle\u{2026}"))],
            "sign-in-error" | "sign-in-refused" => vec![click(Name::Control("Connect"))],
            _ => Vec::new(),
        };
        Some(scene)
    }

    fn base(id: &'static str, platform: DocPlatform, saved: Vec<SavedConnectionSummary>) -> Self {
        let settings = ClientSettings::default();
        Self {
            id,
            platform,
            screen: Screen::Home,
            home: HomeModel::new(saved),
            values: SettingsValues::from_client_settings(&settings, SettingsTab::default()),
            settings,
            monitors: monitors(platform),
            native_tablet: NativeTabletComponent::default(),
            support: SupportBundlePanel::default(),
            profile: ProfileSharing::default(),
            pins: TrustedPinStore::default(),
            repository: ClientConfigRepository::new(None),
            services: Arc::new(DocShotServices(platform)),
            script: Vec::new(),
            pier_answer: PIER_REFUSAL,
        }
    }

    /// The scene's id.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    /// The clicks that open what the scene shows, in order.
    #[must_use]
    pub fn script(&self) -> &[Click] {
        &self.script
    }

    /// Keeps every saved name from being looked up while the scene is
    /// drawn (Home shows the name alone, as for a name that does not
    /// resolve).
    pub fn keep_names_unresolved(&self) {
        for saved in &self.home.saved {
            crate::screens::home::keep_unresolved(saved.host.trim(), saved.port);
        }
    }

    fn settings_on(&mut self, tab: SettingsTab) {
        self.screen = Screen::Settings;
        self.values.tab = tab;
    }

    fn select_first(&mut self) {
        if !self.home.saved.is_empty() {
            self.act(HomeAction::SelectConnection(0));
        }
    }

    fn app_defaults(&self) -> AppDefaults {
        AppDefaults::from_settings(
            &self.settings,
            self.platform.deck_platform().system_shortcut_route(),
        )
    }

    /// Applies a Home action as both shells do (`apply_home_action`, then
    /// the effect).
    fn act(&mut self, action: HomeAction) {
        let keeps_test = matches!(
            action,
            HomeAction::TestConnection { .. } | HomeAction::ConnectionTest(_)
        );
        let defaults = self.app_defaults();
        let mut state = self.home.view(defaults);
        let outcome = apply_home_action(
            &mut state,
            action,
            &self.repository,
            self.settings.security_mode,
        );
        if !keeps_test {
            self.home.test = None;
        }
        if let Some(store) = outcome.store {
            self.home.saved = store.connections;
        }
        match outcome.effect {
            HomeEffect::Connect(draft) => self.refused(draft),
            HomeEffect::TestConnection { host, port } => {
                let mut test = ConnectionTest::start(
                    &host,
                    &port,
                    format!("Arcen Deck {}", env!("CARGO_PKG_VERSION")),
                    |target, slot| slot.fill(unanswered_report(target)),
                );
                test.poll();
                self.home.test = Some(test);
            }
            HomeEffect::None
            | HomeEffect::ConnectionTest(_)
            | HomeEffect::ExportProfile(_)
            | HomeEffect::ImportProfile => {}
        }
    }

    /// The attempt ends as the engine ends one the Pier refused with
    /// [`Self::pier_answer`]: the engine's session end, the shared sign-in
    /// mapping, Home again.
    fn refused(&mut self, draft: ConnectionDraft) {
        use crate::runtime::session::{ConnectSmokeError, session_end_for};
        let end = session_end_for(&ConnectSmokeError::AuthFailed(self.pier_answer.to_owned()));
        let detail = crate::sign_in::end_detail(&end);
        let failure = SignInFailure::from_end(&end, SignInStage::Attempt)
            .unwrap_or(SignInFailure::SessionEnded);
        self.home.direct = self.home.sign_in.return_home(failure, draft, Some(detail));
    }

    /// Draws one pass of the window as the shell does: its root frame, the
    /// launcher bar on every screen but Settings, then the screen.
    pub fn draw(&mut self, ui: &mut egui::Ui, shell: &dyn DocShotShell) {
        let available = ui.available_size();
        let frame = shell.root_frame(ui);
        let canvas_available = shell.canvas_available();
        // Both shells name the keyboard switch from the saved settings.
        crate::screens::wording::set_keyboard_switch(ui.ctx(), self.settings.keyboard_switch);
        frame.show(ui, |ui| {
            ui.set_min_size(available);
            if !matches!(self.screen, Screen::Settings) {
                let _ = render_launcher_top_bar(ui);
            }
            match &mut self.screen {
                Screen::Home => self.draw_home(ui),
                Screen::Settings => self.draw_settings(ui, canvas_available),
                Screen::Certificate(facts) => {
                    let _ = render_certificate_warning(ui, facts);
                }
                Screen::CertificateChanged(change, step) => {
                    let _ = render_certificate_changed(ui, change, step);
                }
                Screen::Connecting(draft, status) => {
                    let _ = crate::screens::connecting::render_connecting(ui, draft, status, None);
                }
                Screen::AccountSwitch(draft, status, prompt) => {
                    let _ = crate::screens::connecting::render_connecting(ui, draft, status, None);
                    let _ = crate::screens::account_switch::render_account_switch(ui.ctx(), prompt);
                }
            }
        });
    }

    fn draw_home(&mut self, ui: &mut egui::Ui) {
        // The Windows Deck shows the import summary before Home, the Mac
        // after it.
        if self.platform == DocPlatform::Windows {
            self.draw_profile(ui.ctx());
        }
        let defaults = self.app_defaults();
        let mut state = self.home.view(defaults);
        let action =
            crate::screens::home::render_home_with(ui, &mut state, &crate::presence::NoPresence);
        if let Some(action) = action {
            self.act(action);
        }
        if self.platform == DocPlatform::Mac {
            self.draw_profile(ui.ctx());
        }
    }

    fn draw_profile(&mut self, ctx: &egui::Context) {
        if !self.profile.is_open() {
            return;
        }
        let outcome = self.profile.show(
            ctx,
            &mut ProfileContext {
                services: &*self.services,
                repository: &self.repository,
                security_mode: self.settings.security_mode,
                saved: &self.home.saved,
                pins: &mut self.pins,
            },
        );
        // As both shells apply it: its note on Home.
        if let Some(notice) = outcome.home_notice() {
            self.home.notice = Some(notice);
        }
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui, canvas_available: bool) {
        let mac = self.platform == DocPlatform::Mac;
        let facts = SettingsFacts {
            hdr_screen: true,
            notch: mac.then_some(true),
            monitors: &self.monitors,
            host_clipboard_notice: None,
            connections: &self.home.saved,
            native_tablet: self
                .native_tablet
                .control(&*self.services, self.platform.deck_platform()),
            decode: None,
            support_bundle: Some(self.support.view(&*self.services)),
            connection_displays: None,
            canvas_available,
            system_shortcut_permission: mac.then_some(true),
        };
        let actions = render_settings(ui, &mut self.values, &facts);
        for action in actions {
            if let SettingsAction::SupportBundle(action) = action {
                let services: Arc<dyn crate::DeckServices> = self.services.clone();
                self.support.apply(
                    action,
                    &SupportBundleContext {
                        services,
                        component: arcen_telemetry::BundleComponent {
                            name: "arcen-deck".to_owned(),
                            version: env!("CARGO_PKG_VERSION").to_owned(),
                            os: self.platform.token().to_owned(),
                            arch: "arm64".to_owned(),
                        },
                        log_level: self.values.log_level,
                        displays: &Vec::new,
                        wake: Arc::new(|| {}),
                        ctx: ui.ctx(),
                    },
                );
            }
        }
    }
}

fn saved_connection(index: usize, name: &str, host: &str) -> SavedConnectionSummary {
    SavedConnectionSummary {
        id: format!("doc-shots-{index}"),
        kind: ConnectionKind::DirectMachine,
        name: name.to_owned(),
        host: host.to_owned(),
        port: ConnectionKind::DirectMachine.default_port(),
        use_tls: true,
        transport: SavedTransport::Quic,
        identity_security_mode: Some(ClientSettings::default().security_mode.key().to_owned()),
        settings: SavedConnectionSettings {
            remembered_username: Some(USER.to_owned()),
            ..SavedConnectionSettings::default()
        },
        unknown: serde_json::Map::new(),
    }
}

fn three_machines() -> Vec<SavedConnectionSummary> {
    vec![
        saved_connection(0, "Grade 1", GRADE_HOST),
        saved_connection(1, "Edit bay", EDIT_HOST),
        saved_connection(2, "Studio", STUDIO_HOST),
    ]
}

/// Grade 1's draft, as Connect hands it on.
fn grade_draft() -> ConnectionDraft {
    let saved = saved_connection(0, "Grade 1", GRADE_HOST);
    let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine);
    draft.saved_connection_id = Some(saved.id);
    draft.host = saved.host;
    draft.port = saved.port;
    USER.clone_into(&mut draft.username);
    draft
}

/// Connecting to Grade 1 after "Use Primary Display Only": the retry is on
/// one display, for this session only, and the status says so.
fn connecting_one_display() -> Screen {
    use crate::connect_plan::{
        SessionDisplaysOrigin, connecting_status, displays_mode_source, displays_unsupported_retry,
        resolve_attempt_displays_mode,
    };
    let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine)
        .for_saved(&saved_connection(0, "Grade 1", GRADE_HOST));
    USER.clone_into(&mut draft.username);
    let endpoint = draft.endpoint();
    let mut pending = displays_unsupported_retry(&endpoint).displays_override;
    let source = displays_mode_source(pending.as_ref(), &endpoint, None);
    let mode = resolve_attempt_displays_mode(&mut pending, &endpoint, None);
    let origin = SessionDisplaysOrigin::for_attempt(source, mode);
    let status = connecting_status(&draft, origin.connecting_note());
    Screen::Connecting(draft, status)
}

/// Grade 1 signing in as Alice while another user is signed in there, and
/// the Pier's offer to sign them out.
fn account_switch_question() -> Screen {
    let mut draft = ConnectionDraft::new(ConnectionKind::DirectMachine)
        .for_saved(&saved_connection(0, "Grade 1", GRADE_HOST));
    USER.clone_into(&mut draft.username);
    let status = crate::connect_plan::connecting_status(&draft, None);
    let mut prompt = crate::account_switch::AccountSwitchPrompt::default();
    prompt.on_news(
        crate::account_switch::AccountSwitchNews::Offered {
            expires_in: std::time::Duration::from_secs(
                arcen_session::session_handover::consent::DEFAULT_OFFER_LIFETIME_SECS.into(),
            ),
        },
        std::time::Instant::now(),
    );
    Screen::AccountSwitch(draft, status, prompt)
}

/// SHA-256 of a fixed seed: a fingerprint no real host has.
fn seeded_digest(seed: &str) -> [u8; 32] {
    sha2::Sha256::digest(format!("arcen-doc-shots/{seed}").as_bytes()).into()
}

/// The certificate a throwaway Pier presents for `draft`, from `seed`.
fn certificate(draft: &ConnectionDraft, seed: &str) -> crate::runtime::tls::CertInfo {
    use crate::runtime::tls::format_fingerprint;
    use arcen_transport::tls::{PinKind, TlsPin};
    let certificate_digest = seeded_digest(&format!("{seed}/certificate"));
    let spki_digest = seeded_digest(&format!("{seed}/spki"));
    crate::runtime::tls::CertInfo {
        endpoint: draft.endpoint(),
        server_name: draft.host.clone(),
        peer_address: Some(format!("{GRADE_ADDRESS}:{}", draft.port)),
        certificate_sha256: TlsPin::new(PinKind::CertificateSha256, certificate_digest),
        certificate_sha256_display: format_fingerprint(&certificate_digest),
        spki_sha256: TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, spki_digest),
        spki_sha256_display: format_fingerprint(&spki_digest),
        not_before_epoch_secs: NOT_BEFORE_EPOCH_SECS,
        not_after_epoch_secs: NOT_AFTER_EPOCH_SECS,
        cert_der_len: 512,
    }
}

/// What "Verify this host" shows, as both shells fill it.
fn certificate_facts(info: &crate::runtime::tls::CertInfo) -> CertificateFacts {
    CertificateFacts {
        endpoint: info.endpoint.clone(),
        server_name: info.server_name.clone(),
        peer_address: info.peer_address.clone(),
        spki_display: info.spki_sha256_display.clone(),
        certificate_display: info.certificate_sha256_display.clone(),
        not_before_epoch_secs: info.not_before_epoch_secs,
        not_after_epoch_secs: info.not_after_epoch_secs,
    }
}

/// Grade 1, trusted and remembered, now presenting another certificate:
/// the shared decision and comparison.
fn identity_changed(pins: &mut TrustedPinStore) -> Screen {
    let draft = grade_draft();
    let remembered = certificate(&draft, "identity-changed remembered");
    pins.pins.insert(
        draft.endpoint(),
        StoredPin::from_pin(
            &remembered.spki_sha256,
            Some("Grade 1".to_owned()),
            Some("2026-11-02T09:30:00Z".to_owned()),
        ),
    );
    let presented = certificate(&draft, "identity-changed presented");
    match identity_change_outcome(&draft, pins, &presented) {
        IdentityChangeOutcome::Recover(change) => {
            Screen::CertificateChanged(change, CertificateChangedStep::default())
        }
        IdentityChangeOutcome::Refuse { .. } => Screen::Home,
    }
}

/// Studio's `.arcen` file, as another Deck exports it (with its host
/// identity and two settings of its own), read back as a file is read.
fn studio_profile() -> crate::profile::ConnectionProfile {
    use crate::profile::ConnectionProfile;
    let mut studio = saved_connection(2, "Studio", STUDIO_HOST);
    studio.settings.overrides = ConnectionOverrides {
        displays_mode: Override::Own(DisplaysMode::SinglePrimary),
        clipboard_enabled: Override::Own(false),
        ..ConnectionOverrides::default()
    };
    let mut author_pins = TrustedPinStore::default();
    let identity = seeded_digest("import-review studio/spki");
    author_pins.pins.insert(
        format!("{}:{}", studio.host, studio.port),
        StoredPin::from_pin(
            &arcen_transport::tls::TlsPin::new(
                arcen_transport::tls::PinKind::SubjectPublicKeyInfoSha256,
                identity,
            ),
            None,
            None,
        ),
    );
    let file = ConnectionProfile::from_saved(&studio, &author_pins)
        .map(|profile| serde_json::to_vec_pretty(&profile.to_json()))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    ConnectionProfile::parse(&file).unwrap_or_else(|_| {
        ConnectionProfile::from_saved(&studio, &author_pins)
            .unwrap_or_else(|error| unreachable!("a placeholder connection exports: {error}"))
    })
}

/// The connection test's report for a name that resolves and a Pier that
/// does not answer: the shared assessment of what an adapter observed.
fn unanswered_report(
    target: arcen_session::connection_diagnostics::DiagnosticTarget,
) -> arcen_session::connection_diagnostics::DiagnosticReport {
    use arcen_session::connection_diagnostics::{
        AddressAttempt, ExpectedIdentity, ExpectedPin, HandshakeOutcome, Observations,
        PathObservation, PinKind, ResolutionObservation, RoundTripObservation, assess,
    };
    let address: std::net::IpAddr = GRADE_ADDRESS
        .parse()
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let remembered = crate::runtime::tls::format_fingerprint(&seeded_digest("connection-test"));
    let observations = Observations {
        resolution: ResolutionObservation::Resolved {
            addresses: vec![address],
            elapsed_ms: 4,
        },
        attempts: vec![AddressAttempt {
            address: std::net::SocketAddr::new(address, target.port),
            outcome: HandshakeOutcome::TimedOut { waited_ms: 5_000 },
        }],
        expected_identity: ExpectedIdentity::Remembered(ExpectedPin {
            kind: PinKind::Spki,
            digest: remembered,
        }),
        certificate: None,
        greeting: None,
        path: PathObservation {
            interface_kind: Some("Wi-Fi".to_owned()),
            interface_mtu: Some(1500),
            quic_mtu: None,
        },
        round_trip: RoundTripObservation::default(),
        target,
    };
    assess(
        &observations,
        NOW_EPOCH_SECS,
        arcen_telemetry::QosTargets::default(),
    )
}

/// The displays the Displays panel lists: a Mac's built-in panel and one
/// external display, or a PC's two displays.
fn monitors(platform: DocPlatform) -> Vec<SettingsMonitor> {
    let monitor = |id: u32, name: &str, x: i32, size: [u32; 2], percent: u16, refresh_hz: u32| {
        let points = size.map(|pixels| pixels * 100 / u32::from(percent));
        SettingsMonitor {
            monitor: ClientMonitor {
                id,
                x,
                y: 0,
                width_px: size[0],
                height_px: size[1],
                scale: f32::from(percent) / 100.0,
                refresh_hz,
                is_primary: id == 1,
                name: name.to_owned(),
                ..ClientMonitor::default()
            },
            fullscreen_size: [Some(points), Some(size)],
        }
    };
    match platform {
        DocPlatform::Mac => vec![
            monitor(1, "Built-in Retina Display", 0, [3024, 1964], 200, 120),
            monitor(2, "External Display", 1512, [5120, 2880], 200, 60),
        ],
        DocPlatform::Windows => vec![
            monitor(1, "Display 1", 0, [2560, 1440], 150, 60),
            monitor(2, "Display 2", 1707, [1920, 1080], 100, 60),
        ],
    }
}
