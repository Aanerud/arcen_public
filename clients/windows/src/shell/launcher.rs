// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The launcher: the same screens as the macOS Deck, drawn by the shared
//! `arcen_deck_core::screens` code, with this shell's state and the shared
//! action handling. Connect builds the session from the shared connection
//! plan and shows the desktop through core's media composition; the
//! certificate, disclaimer and credential prompts are the shared screens.
//! A lost connection is resumed with the shared reconnect controller and
//! flow (`arcen_deck_core::reconnect_flow`), and a host's sign-in screen is
//! followed into its desktop (`arcen_session::login_window_handover`), as on
//! the Mac. The displays follow the Mac's rules too (`super::displays`):
//! the per-connection Displays mode, Match My Layout's layout and its two
//! recoveries, the pinned fullscreen and the fit-to-window updates.

use std::sync::Arc;

use arcen_deck_core::DeckServices;
use arcen_deck_core::client_telemetry::ClientTelemetry;
use arcen_deck_core::config::{
    ConnectionKind, SavedConnectionSummary, load_client_settings, load_saved_connections,
    save_client_settings,
};
use arcen_deck_core::connect_plan::tls_trust_for;
use arcen_deck_core::connect_plan::{
    DisplaysRecovery, DisplaysRecoveryRetry, SessionDisplaysOrigin, display_fit_gate,
    displays_recovery_for, displays_unsupported_retry, resolve_attempt_displays_mode,
    topology_conflict_retry,
};
use arcen_deck_core::connection_draft::ConnectionDraft;
use arcen_deck_core::display::{SessionFullscreen, SessionFullscreenStep};
use arcen_deck_core::display_fit::DisplayFitTracker;
use arcen_deck_core::reconnect::{
    ConnectionIdentity, MonotonicClock, ReconnectController, SystemClock, SystemJitter,
    fresh_holder_nonce,
};
use arcen_deck_core::reconnect_flow::{
    ReconnectOverlay, ResumeLaunch, accept_authentication, connection_identity_with_live_monitors,
    is_reconnecting, next_resume_launch, schedule_after_end,
};
use arcen_deck_core::runtime::clipboard_controller::{
    ClipboardController, POLL_INTERVAL as CLIPBOARD_POLL,
};
use arcen_deck_core::runtime::session::{
    AuthSubmission, AuthenticationKind, ConnectOptions, DisconnectReason, SessionEnd,
    TerminalDisconnect,
};
use arcen_deck_core::runtime::tls::CertInfo;
use arcen_deck_core::screens::certificate::{
    CertificateAction, CertificateChangedAction, CertificateChangedStep, CertificateFacts,
    IdentityChange, IdentityChangeOutcome, IdentityReplaced, certificate_retry_auth,
    identity_change_outcome, render_certificate_changed, render_certificate_warning,
    replace_remembered_identity,
};
use arcen_deck_core::screens::connecting::{
    ConnectingAction, DisplaysUnsupportedAction, TopologyConflictAction, render_connecting,
    render_displays_unsupported, render_topology_conflict,
};
use arcen_deck_core::screens::help_windows::{HelpWindow, HelpWindows};
use arcen_deck_core::screens::home::{HomeEffect, HomeViewState, apply_home_action, render_home};
use arcen_deck_core::screens::keyboard_help::{
    KeyboardToast, keyboard_status_line, keyboard_switch_toast, render_keyboard_toast,
};
use arcen_deck_core::screens::launcher::{TopBarAction, open_user_guide, render_launcher_top_bar};
use arcen_deck_core::screens::menu::{
    MenuCommand, QuitHoldStep, connection_bar_reveal_zone, paint_quit_hold_notice, quit_hold_step,
    render_connection_bar,
};
use arcen_deck_core::screens::reconnect::{
    HeldFrame, HeldFrameAction, render_reconnecting, render_signing_in,
};
use arcen_deck_core::screens::settings::{
    NativeTabletComponent, SettingsAction, SettingsFacts, SettingsMonitor, SettingsTab,
    SettingsValues, render_settings,
};
use arcen_deck_core::screens::sign_in::{DisclaimerAction, render_disclaimer};
use arcen_deck_core::screens::statistics::{
    DecodeTruth, TabletMonitorFacts, paint_negotiated_truth_panel, paint_stream_stats,
    paint_tablet_monitor_panel,
};
use arcen_deck_core::screens::wording::DeckPlatform;
use arcen_deck_core::settings::{
    ClientConfigRepository, ClientSettings, DisplaysMode, SecurityMode, SystemShortcutCapture,
    SystemShortcutRoute,
};
use arcen_deck_core::sign_in::{SignInFailure, SignInStage, SignInState};
use arcen_deck_core::trusted_pins::TrustedPinStore;
use arcen_deck_core::ui_pacing::{PacedValue, STATS_PANEL_REFRESH};
use arcen_input::keyboard::pc::KEY_F4;

use super::displays::DisplayPlan;
use super::session::{
    Attach, Connection, LocalInput, MultiWindowVerdict, SessionNews, WindowObservation,
    connect_options,
};
use crate::input::WindowInput;
use crate::media::VideoSurface;
use arcen_deck_core::input::overlay_pointer::OverlayPointerRegions;
use arcen_deck_core::multi_window::runtime::ActiveDisplayInfo;
use arcen_deck_core::overrides::ConnectionOverrides;
use arcen_deck_core::screens::host_pointer::paint_host_pointer;
use arcen_deck_core::screens::text_input::{
    NOTICE_DURATION, characters_unavailable_notice, keyboard_mode_notice, paint_text_input_overlays,
};
use arcen_input::deck_pen::ImageRect;
use arcen_input::keyboard::DeckShortcut;
use arcen_input::text::{KeyboardMode, TypingMode};
use arcen_media::SessionMonitorId;
use arcen_protocol::messages::TabletModeMsg;
use windows::Win32::Foundation::HWND;

/// What the window must do after a menu command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuOutcome {
    Handled,
    Exit,
    /// The window toggles fullscreen (the shell owns the window).
    ToggleFullscreen,
}

#[derive(Debug)]
enum Screen {
    Home,
    Settings,
    Connecting(ConnectionDraft),
    Certificate(ConnectionDraft, Box<CertInfo>),
    Disclaimer(ConnectionDraft, String),
    /// "This computer's identity has changed" (the Mac's screen, shared).
    CertificateChanged(Box<ChangedIdentity>),
    InSession,
    /// "Lost connection, attempting to reconnect" over the last picture;
    /// `true` once an attempt is resuming.
    Reconnecting(ReconnectOverlay, bool),
    /// "Signing in…": following the host's sign-in screen into its desktop.
    SigningIn,
    /// "This desktop was started without your layout" (CN-08).
    TopologyConflict(ConnectionDraft),
    /// "This host cannot match all active displays" (CN-09).
    DisplaysUnsupported(ConnectionDraft),
}

/// The identity-changed screen's facts: the connection and the comparison
/// shown, which carries the presented identity "Trust and connect" trusts
/// (`IdentityChange::replacement`).
#[derive(Debug, Clone)]
struct ChangedIdentity {
    draft: ConnectionDraft,
    change: IdentityChange,
}

/// A sign-in screen the Deck follows into the desktop that replaces it (the
/// Mac's `LoginWindowHandover`): the shared schedule and the credentials
/// that sign in again (`AuthSubmission` wipes itself when dropped).
struct LoginWindowHandover {
    schedule: arcen_deck_core::reconnect_flow::HandoverSchedule,
    auth: AuthSubmission,
}

/// The launcher's state; one per window.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per panel or overlay the user toggles"
)]
pub(super) struct Launcher {
    services: Arc<dyn DeckServices>,
    repository: ClientConfigRepository,
    settings: ClientSettings,
    settings_tab: SettingsTab,
    saved: Vec<SavedConnectionSummary>,
    /// The connection the tablet mode is remembered for (the Mac's rule:
    /// the last one connected to).
    last_selected: Option<String>,
    monitors: Vec<SettingsMonitor>,
    screen: Screen,
    status: String,
    direct: ConnectionDraft,
    search: String,
    show_add_form: bool,
    editing_index: Option<usize>,
    form_name: String,
    form_host: String,
    form_username: String,
    form_overrides: ConnectionOverrides,
    remember_username: bool,
    /// The last sign-in outcome Home shows under the selected connection's
    /// field, or in an acknowledgement box (`arcen_deck_core::sign_in`).
    sign_in: SignInState,
    /// Help ▸ About and Help ▸ Keyboard Shortcuts: modal over a session.
    help: HelpWindows,
    native_tablet: NativeTabletComponent,
    /// Settings › Logs › "Create support bundle…" (shared panel).
    support_bundle: arcen_deck_core::screens::support_bundle::SupportBundlePanel,
    /// Home's Export… and Import… of `.arcen` connection files, and the
    /// import summary (shared).
    profile_sharing: arcen_deck_core::screens::profile::ProfileSharing,
    /// What the decoder proved, read once off the UI thread (a D3D11
    /// probe), for Settings' Grading/HDR reasons.
    decode_report: Arc<std::sync::OnceLock<arcen_deck_core::decode_report::DecodeReport>>,
    surface: VideoSurface,
    connection: Option<Connection>,
    generation: u64,
    /// Credentials typed for this attempt, kept for the host's auth
    /// request. Cleared whenever the attempt ends, so they never reach
    /// another host.
    deferred_auth: Option<AuthSubmission>,
    /// Pins the user trusted for this run of the Deck (never saved).
    accepted_pins: TrustedPinStore,
    remembered_pins: TrustedPinStore,
    /// What Export or Import last did, under Home's list until dismissed.
    home_notice: Option<arcen_deck_core::screens::home::HomeNotice>,
    show_stream_stats: bool,
    /// Stream Statistics' text, rebuilt at the Mac's rate: text that
    /// changed every pass presented the overlay with every video frame.
    stream_stats_text: PacedValue<Vec<String>>,
    show_negotiated_truth: bool,
    show_tablet_monitor: bool,
    /// `--quit-after-secs`: an unattended run ends this long after the
    /// desktop appears.
    quit_after: Option<std::time::Duration>,
    streaming_since: Option<std::time::Instant>,
    unattended_started: Option<std::time::Instant>,
    stats_reported_at: Option<std::time::Instant>,
    /// The local clipboard ↔ the session's (the Mac's controller, shared).
    clipboard: ClipboardController,
    /// The window is fullscreen: the session shows the connection bar on
    /// demand, and nothing else over the video.
    fullscreen: bool,
    /// Ctrl+Alt+Home opened the connection bar and gave it the keyboard.
    bar_pinned: bool,
    /// Where the fullscreen connection bar took the pointer this pass.
    bar_regions: OverlayPointerRegions,
    /// The connection's name, for the connection bar.
    session_title: String,
    /// Commands chosen on the connection bar or by a Deck chord, for the
    /// shell to apply.
    bar_commands: Vec<MenuCommand>,
    /// The window whose keyboard, mouse and pen a session takes.
    hwnd: HWND,
    /// The own values of the connection being made (Command, Typing,
    /// Option keys); everything else follows Settings.
    session_overrides: ConnectionOverrides,
    /// Keyboard: Physical Keys / Characters, chosen from the menu for this
    /// session.
    session_keyboard_mode: Option<KeyboardMode>,
    /// The input method's composition, shown until it commits.
    preedit: Option<String>,
    /// The latest typing notice and when it was given.
    notice: Option<(std::time::Instant, String)>,
    /// The keyboard switch's notice after a flip, while it shows.
    keyboard_toast: Option<KeyboardToast>,
    /// Who had the keyboard at the last pass (`None` outside a session), so
    /// every flip (chord, menu, click into the picture) gets its notice.
    shown_keyboard_owner: Option<arcen_input::keyboard::KeyboardStatus>,
    /// Since when Alt+F4 has been held in a session.
    quit_hold: Option<std::time::Instant>,
    /// A held Alt+F4 asked the Deck to exit.
    exit_requested: bool,
    /// Resumes this connection after a lost transport (the Mac's).
    reconnect: Option<ReconnectController>,
    /// The connection's credential-free options, for resume attempts.
    reconnect_options: Option<ConnectOptions>,
    /// The connection being made or resumed (no password).
    active_connection: Option<ConnectionDraft>,
    /// The clipboard policy the session's host advertised in its hello.
    host_clipboard: Option<arcen_protocol::messages::ClipboardPolicyMsg>,
    reconnect_clock: SystemClock,
    reconnect_jitter: SystemJitter,
    /// The lost connection, kept only so its last picture stays under the
    /// reconnect or signing-in overlay; its input is released.
    held: Option<Connection>,
    /// The credentials the host last accepted, kept until its hello says
    /// whether the desktop is a sign-in screen to follow.
    submitted_auth: Option<AuthSubmission>,
    login_window_handover: Option<LoginWindowHandover>,
    /// A recovery's one-shot Displays mode for the next attempt to that
    /// endpoint (the Mac's `pending_displays_override`).
    pending_displays_override: Option<(String, DisplaysMode)>,
    /// Why this attempt, and the session it starts, shows the displays it
    /// does (shared `SessionDisplaysOrigin`, the Mac's `displays_origin`):
    /// one display for this session only after a display recovery. Set
    /// with each attempt's Displays mode, so the next ordinary connection
    /// resets it; never saved.
    displays_origin: SessionDisplaysOrigin,
    /// The Windows Ink hint was shown this run
    /// (`arcen_input::deck_pen::TABLET_AS_MOUSE_HINT`).
    tablet_hint_shown: bool,
    /// The topology conflict's "Start fresh": the next attempt asks the
    /// host to replace its desktop.
    pending_desktop_replacement: bool,
    /// The credentials of an attempt a display recovery, a changed identity
    /// or the certificate warning stopped, carried only into that screen's
    /// own retry (the Mac's rule); any other way out drops them
    /// (`leave_recovery`).
    recovery_auth: Option<AuthSubmission>,
    /// The identity-changed screen's step (asking, or confirming the trust).
    certificate_change_step: CertificateChangedStep,
    /// The connection's display plan (pinned or following the window).
    display_plan: Option<DisplayPlan>,
    /// The Match My Layout layout this connection asked for.
    requested_topology: Option<arcen_media::RequestedMonitorTopology>,
    /// Fit-to-window updates of a Windowed session.
    display_fit: DisplayFitTracker,
    /// The display the window is on (`\\.\DISPLAYn`'s `n`), as the shell
    /// last noted it.
    window_display: Option<u32>,
    /// The window moved to another display since the follower last looked
    /// (`drive_display_follow`).
    window_display_moved: bool,
    /// When the follower last looked at the displays.
    display_follow_polled: Option<std::time::Instant>,
    /// The fullscreen a pinned session holds.
    session_fullscreen: SessionFullscreen,
}

/// How long an unattended run waits for the desktop before it gives up.
const UNATTENDED_DESKTOP_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// How an unattended (`--quit-after-secs`) run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnattendedEnd {
    /// The desktop streamed for the whole time.
    Streamed,
    /// The session ended or never showed the desktop.
    Failed,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launcher")
            .field("screen", &self.screen)
            .field("saved", &self.saved.len())
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl Launcher {
    /// Loads the saved settings and connections from the Deck's folders.
    pub(super) fn new(services: Arc<dyn DeckServices>, surface: VideoSurface, hwnd: HWND) -> Self {
        let decode_report = probe_decode_report(&services);
        let repository = ClientConfigRepository::new(services.directories().config);
        let mut status = "Ready".to_owned();
        let settings = load_client_settings(&repository).unwrap_or_else(|error| {
            status = format!("Settings were not loaded: {error}");
            ClientSettings::default()
        });
        // As on the Mac: the saved "Log level" applies to the logging the
        // process started with (ARCEN_LOG_LEVEL or the default).
        arcen_deck_core::logging::set_level(settings.log_level.profile());
        let (saved, last_selected) = match load_saved_connections(&repository) {
            Ok(Some(store)) => (store.connections, store.last_selected_connection_id),
            Ok(None) => (Vec::new(), None),
            Err(error) => {
                status = format!("Saved connections were not loaded: {error}");
                (Vec::new(), None)
            }
        };
        // As on the Mac: an older copy that may open instead of this one is
        // named at start, after the errors above.
        if status == "Ready"
            && let Some(notice) = crate::platform::installed_copies::startup_notice()
        {
            status = notice;
        }
        let monitors = super::displays::settings_monitors();
        let remembered_pins = repository
            .trusted_pins_file()
            .map(|path| arcen_deck_core::trusted_pins::load(&path))
            .unwrap_or_default();
        Self {
            services,
            repository,
            settings,
            settings_tab: SettingsTab::Streaming,
            saved,
            last_selected,
            monitors,
            screen: Screen::Home,
            status,
            direct: ConnectionDraft::new(ConnectionKind::DirectMachine),
            search: String::new(),
            show_add_form: false,
            editing_index: None,
            form_name: String::new(),
            form_host: String::new(),
            form_username: String::new(),
            form_overrides: ConnectionOverrides::default(),
            remember_username: true,
            sign_in: SignInState::default(),
            help: HelpWindows::default(),
            native_tablet: NativeTabletComponent::default(),
            support_bundle: arcen_deck_core::screens::support_bundle::SupportBundlePanel::default(),
            profile_sharing: arcen_deck_core::screens::profile::ProfileSharing::default(),
            decode_report,
            surface,
            connection: None,
            generation: 0,
            deferred_auth: None,
            accepted_pins: TrustedPinStore::default(),
            remembered_pins,
            home_notice: None,
            show_stream_stats: false,
            stream_stats_text: PacedValue::new(STATS_PANEL_REFRESH),
            show_negotiated_truth: false,
            show_tablet_monitor: false,
            quit_after: None,
            streaming_since: None,
            unattended_started: None,
            stats_reported_at: None,
            clipboard: ClipboardController::new(),
            fullscreen: false,
            bar_pinned: false,
            bar_regions: OverlayPointerRegions::default(),
            session_title: String::new(),
            bar_commands: Vec::new(),
            hwnd,
            session_overrides: ConnectionOverrides::default(),
            session_keyboard_mode: None,
            preedit: None,
            notice: None,
            keyboard_toast: None,
            shown_keyboard_owner: None,
            quit_hold: None,
            exit_requested: false,
            reconnect: None,
            reconnect_options: None,
            active_connection: None,
            host_clipboard: None,
            reconnect_clock: SystemClock::default(),
            reconnect_jitter: SystemJitter,
            held: None,
            submitted_auth: None,
            login_window_handover: None,
            pending_displays_override: None,
            displays_origin: SessionDisplaysOrigin::Chosen,
            tablet_hint_shown: false,
            pending_desktop_replacement: false,
            recovery_auth: None,
            certificate_change_step: CertificateChangedStep::default(),
            display_plan: None,
            requested_topology: None,
            display_fit: DisplayFitTracker::new(),
            window_display: None,
            window_display_moved: false,
            display_follow_polled: None,
            session_fullscreen: SessionFullscreen::default(),
        }
    }

    /// **UI thread.** The window is now on display `display` (the shell
    /// notes every change).
    pub(super) fn note_window_display(&mut self, display: Option<u32>) {
        self.window_moved_to(display);
    }

    fn window_moved_to(&mut self, display: Option<u32>) {
        if display != self.window_display {
            self.window_display = display;
            self.window_display_moved = true;
        }
    }

    /// Connects straight away, as `--connect` asks.
    pub(super) fn quick_connect(
        &mut self,
        draft: ConnectionDraft,
        stream_stats: bool,
        quit_after: Option<std::time::Duration>,
        ctx: &egui::Context,
    ) {
        self.show_stream_stats = stream_stats;
        self.quit_after = quit_after;
        self.unattended_started = quit_after.map(|_| std::time::Instant::now());
        let mut draft = draft;
        // The saved connection the command line names unambiguously, as on
        // the Mac: its own values (a withheld clipboard among them) apply,
        // and a `--pin-sha256` stays.
        if let Some(index) = draft.match_saved(&self.saved, self.settings.security_mode) {
            let id = self.saved[index].id.clone();
            if draft.username.is_empty() {
                draft.username = self.saved[index]
                    .settings
                    .remembered_username
                    .clone()
                    .unwrap_or_default();
            }
            match self.repository.set_last_selected(&id) {
                Ok(store) => self.reconcile(store),
                Err(error) => tracing::warn!(%error, "quick-connect selection was not persisted"),
            }
        }
        self.start_connection(draft, None, ctx);
    }

    /// The menu's keyboard mode for this session. Characters needs a host
    /// that types text; with one that does not, the keys stay physical (as
    /// on the Mac).
    fn choose_keyboard_mode(&mut self, mode: KeyboardMode) {
        if mode == KeyboardMode::Characters
            && self
                .connection
                .as_ref()
                .and_then(Connection::host_types_text)
                == Some(false)
        {
            self.notify(characters_unavailable_notice(true).to_owned());
            return;
        }
        self.session_keyboard_mode = Some(mode);
        self.apply_typing_mode();
        self.notify(keyboard_mode_notice(DeckPlatform::Pc, mode));
    }

    fn notify(&mut self, text: String) {
        self.notice = Some((std::time::Instant::now(), text));
    }

    /// When the system shortcuts go to the host: the session's connection's
    /// own switch over Settings'.
    fn keyboard_capture(&self) -> arcen_input::keyboard::KeyboardCapturePolicy {
        session_keyboard_capture(&self.session_overrides, &self.settings)
    }

    /// The typing mode in force: the session's keyboard mode over the
    /// connection's Typing, else Settings'.
    fn typing_mode(&self) -> TypingMode {
        let setting = self
            .session_overrides
            .effective_typing_mode(self.settings.typing_mode)
            .value;
        self.session_keyboard_mode
            .map_or(setting, |mode| mode.typing_mode(setting))
    }

    fn apply_typing_mode(&self) {
        let typing = self.typing_mode();
        if let Some(connection) = &self.connection {
            connection.set_typing(typing);
        }
    }

    /// Whether a held Alt+F4 asked the Deck to exit.
    pub(super) const fn exit_requested(&self) -> bool {
        self.exit_requested
    }

    /// Follows a held Alt+F4: exits after the hold, or sends the tap to the
    /// host when it is let go sooner (closes the Help window in front when
    /// one is open).
    fn drive_quit_hold(&mut self, ctx: &egui::Context) {
        let Some(started) = self.quit_hold else {
            return;
        };
        let held = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.holds_deck_key(KEY_F4));
        match quit_hold_step(held, started.elapsed()) {
            QuitHoldStep::Holding => {
                paint_quit_hold_notice(ctx, DeckPlatform::Pc, started.elapsed());
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            QuitHoldStep::Quit => {
                tracing::info!("Alt+F4 held; exiting the Deck");
                self.quit_hold = None;
                self.exit_requested = true;
            }
            // Under a Help window the keys are the Deck's: a quick Alt+F4
            // closes it, as on any Windows dialog.
            QuitHoldStep::SendToHost if self.help.modal() => {
                self.quit_hold = None;
                let _ = self.help.close_front();
            }
            QuitHoldStep::SendToHost => {
                self.quit_hold = None;
                self.window_input(|input| input.tap_on_host(KEY_F4));
            }
        }
    }

    /// Applies `apply` to the session window's input, if a session has it.
    fn window_input(&self, apply: impl FnOnce(&mut WindowInput)) {
        if let Some(connection) = &self.connection {
            let _ = connection.with_window_input(apply);
        }
    }

    fn window_input_value<T>(&self, read: impl FnOnce(&WindowInput) -> T) -> Option<T> {
        self.connection
            .as_ref()?
            .with_window_input(|input| read(input))
    }

    /// Whether this is an unattended (`--quit-after-secs`) run.
    pub(super) const fn unattended(&self) -> bool {
        self.quit_after.is_some()
    }

    /// For an unattended run: whether it is over. While it streams, Stream
    /// Statistics' lines go to standard error every ten seconds, and once
    /// more before the session ends.
    pub(super) fn unattended_end(&mut self) -> Option<UnattendedEnd> {
        let quit_after = self.quit_after?;
        let now = std::time::Instant::now();
        let Some(since) = self.streaming_since else {
            // Nobody is there to answer a prompt, and the engine waits for
            // an answer without a deadline.
            let prompt = matches!(
                self.screen,
                Screen::Certificate(..) | Screen::Disclaimer(..) | Screen::CertificateChanged(_)
            );
            let late = self
                .unattended_started
                .is_some_and(|at| now.duration_since(at) >= UNATTENDED_DESKTOP_DEADLINE);
            if !(prompt || late || self.connection.is_none()) {
                return None;
            }
            if late && !prompt && self.connection.is_some() {
                "the desktop did not appear in time".clone_into(&mut self.status);
            }
            eprintln!("arcen-deck: {}", self.status);
            self.disconnect_quietly();
            return Some(UnattendedEnd::Failed);
        };
        let due = now.duration_since(since) >= quit_after;
        let Some(connection) = &self.connection else {
            // A lost connection being resumed is still this run's session.
            if self.recovering() && !due {
                return None;
            }
            eprintln!("arcen-deck: {}", self.status);
            self.disconnect_quietly();
            return Some(UnattendedEnd::Failed);
        };
        if due
            || self
                .stats_reported_at
                .is_none_or(|at| now.duration_since(at) >= std::time::Duration::from_secs(10))
        {
            self.stats_reported_at = Some(now);
            let elapsed = now.duration_since(since).as_secs();
            for line in connection.stream_stats_lines(self.displays_origin.session_note()) {
                eprintln!("[stats +{elapsed}s] {line}");
            }
        }
        if !due {
            return None;
        }
        self.disconnect("Disconnected");
        Some(UnattendedEnd::Streamed)
    }

    /// Applies a menu command (the window menu bar).
    pub(super) fn menu_command(&mut self, command: MenuCommand) -> MenuOutcome {
        match command {
            // In a session a tapped Alt+F4 belongs to the host; only a hold
            // exits (the Mac's ⌘Q).
            MenuCommand::Quit {
                from_keyboard: true,
            } if self.connection.is_some() => {
                self.quit_hold.get_or_insert_with(std::time::Instant::now);
            }
            MenuCommand::Quit { .. } => return MenuOutcome::Exit,
            MenuCommand::ToggleFullscreen if self.connection.is_some() => {
                return MenuOutcome::ToggleFullscreen;
            }
            MenuCommand::About => self.help.open(HelpWindow::About),
            MenuCommand::Disconnect => self.disconnect("Disconnected"),
            MenuCommand::ToggleStreamStats => self.show_stream_stats = !self.show_stream_stats,
            MenuCommand::ReleaseModifiers => self.window_input(WindowInput::release_modifiers),
            MenuCommand::KeyboardPhysical => self.choose_keyboard_mode(KeyboardMode::Physical),
            MenuCommand::KeyboardCharacters => self.choose_keyboard_mode(KeyboardMode::Characters),
            MenuCommand::SendSecureAttention => {
                self.window_input(WindowInput::send_secure_attention);
            }
            MenuCommand::ToggleKeyboardCapture => {
                self.window_input(WindowInput::toggle_keyboard);
                self.keyboard_owner_changed();
            }
            MenuCommand::SendKey(key) => self.window_input(|input| input.send_key(key)),
            MenuCommand::KeyboardShortcuts => self.help.open(HelpWindow::KeyboardShortcuts),
            MenuCommand::TogglePointerLock => self.window_input(|input| {
                let locked = input.pointer_locked();
                input.set_pointer_lock(!locked);
            }),
            MenuCommand::ToggleTabletMonitor => {
                self.show_tablet_monitor = !self.show_tablet_monitor;
            }
            MenuCommand::ToggleNegotiatedTruth => {
                self.show_negotiated_truth = !self.show_negotiated_truth;
            }
            // The rest follow with input and the shared controller.
            _ => {}
        }
        MenuOutcome::Handled
    }

    /// Draws one pass, exactly as the macOS Deck frames its launcher: white
    /// behind every launcher screen, the launcher bar above all but Settings.
    pub(super) fn ui(&mut self, ui: &mut egui::Ui) {
        // Before the connection bar reads Esc: it closes a Help window first.
        let _ = self.help.take_dismiss_key(ui.ctx());
        self.bar_regions = OverlayPointerRegions::default();
        self.drive_connection(ui.ctx());
        let mut show_connection_bar = false;
        if let Some(connection) = self.connection.as_mut() {
            let bridged_pen = self.settings.tablet_mode_requested == TabletModeMsg::UsbTabletBridge;
            connection.sync_window_input(&self.surface.tree, bridged_pen);
            for news in connection.take_local_input() {
                match news {
                    LocalInput::Shortcut(DeckShortcut::ReleaseKeyboard) => {
                        // The input already gave the keyboard away (or back).
                        show_connection_bar = true;
                    }
                    LocalInput::Shortcut(shortcut) => {
                        if let Some(command) = MenuCommand::for_shortcut(DeckPlatform::Pc, shortcut)
                        {
                            self.bar_commands.push(command);
                        }
                    }
                    // The menu shows no tick yet; the input already let go.
                    LocalInput::PointerLockReleased => {}
                    LocalInput::Preedit(text) => self.preedit = text,
                    // Esc or Enter in any session window, which the input
                    // keeps from the host until it is let go.
                    LocalInput::DismissModal => {
                        let _ = self.help.close_front();
                    }
                }
            }
            self.clipboard
                .sync(std::time::Instant::now(), &*self.services);
            ui.ctx().request_repaint_after(CLIPBOARD_POLL);
        }
        if show_connection_bar {
            self.keyboard_owner_changed();
        }
        self.watch_keyboard_switch(&ui.ctx().clone());
        self.drive_quit_hold(&ui.ctx().clone());
        let available = ui.available_size();
        let in_session = matches!(self.screen, Screen::InSession) || self.recovering();
        egui::Frame::new()
            // The video visual is under this overlay: a session leaves it
            // clear.
            .fill(if in_session {
                egui::Color32::TRANSPARENT
            } else {
                launcher_root_frame(ui).fill
            })
            .inner_margin(0.0)
            .show(ui, |ui| {
                ui.set_min_size(available);
                let launcher_screen = !(in_session || matches!(self.screen, Screen::Settings));
                match launcher_screen
                    .then(|| render_launcher_top_bar(ui))
                    .flatten()
                {
                    Some(TopBarAction::Settings) => self.screen = Screen::Settings,
                    Some(TopBarAction::Help) => open_user_guide(ui.ctx()),
                    None => {}
                }
                match &self.screen {
                    Screen::Home => self.home(ui),
                    Screen::Settings => self.settings_screen(ui),
                    Screen::Connecting(draft) => {
                        let draft = draft.clone();
                        if render_connecting(ui, &draft, &self.status, None)
                            == ConnectingAction::Cancel
                        {
                            self.disconnect("Ready");
                        }
                    }
                    Screen::Certificate(..) => self.certificate_screen(ui),
                    Screen::CertificateChanged(_) => self.certificate_changed_screen(ui),
                    Screen::Disclaimer(draft, text) => {
                        let (draft, text) = (draft.clone(), text.clone());
                        match render_disclaimer(ui, &draft, &text) {
                            DisclaimerAction::None => {}
                            DisclaimerAction::Accept => self.answer_auth(draft),
                            DisclaimerAction::Decline => {
                                if let Some(connection) = &self.connection {
                                    connection.decline_disclaimer();
                                }
                                self.disconnect("Disclaimer declined");
                            }
                        }
                    }
                    Screen::InSession => {
                        self.drive_display_fit(ui);
                        self.drive_display_follow(ui);
                        self.session_overlays(ui);
                    }
                    Screen::Reconnecting(..) | Screen::SigningIn => self.recovery_overlay(ui),
                    Screen::TopologyConflict(_) | Screen::DisplaysUnsupported(_) => {
                        self.display_recovery_screen(ui);
                    }
                }
            });
        self.help.show(
            ui.ctx(),
            crate::VERSION,
            crate::VERSION,
            DeckPlatform::Pc,
            self.settings.keyboard_switch,
        );
        self.leave_recovery();
        self.publish_local_ui();
    }

    /// The Mac's `leave_recovery`: the password kept for a recovery choice
    /// (or the certificate warning's retry) lives only while that screen
    /// shows; Settings, Home or anything
    /// else drops it (`AuthSubmission` wipes itself on drop). A choice takes
    /// it itself.
    fn leave_recovery(&mut self) {
        if self.recovery_auth.is_some()
            && !matches!(
                self.screen,
                Screen::TopologyConflict(_)
                    | Screen::DisplaysUnsupported(_)
                    | Screen::CertificateChanged(_)
                    | Screen::Certificate(..)
            )
        {
            self.recovery_auth = None;
        }
    }

    /// Where the Deck's own UI takes the session windows' input, after this
    /// pass drew it: the fullscreen connection bar (the root window only),
    /// and an open Help window (modal, windowed or fullscreen, in every
    /// session window, Match My Layout's secondary ones too: its OK button
    /// and Esc are the Deck's, never the host's).
    fn publish_local_ui(&mut self) {
        if self.connection.is_none() {
            return;
        }
        let regions = OverlayPointerRegions {
            modal: self.help.modal(),
            ..self.bar_regions
        };
        if let Some(connection) = self.connection.as_mut() {
            connection.set_local_ui(regions);
        }
    }

    /// Settings changed: what takes effect at once does, and they are saved.
    fn settings_changed(&mut self) {
        // Logs › Log level takes effect at once, as on the Mac.
        arcen_deck_core::logging::set_level(self.settings.log_level.profile());
        let policy = self.keyboard_capture();
        self.window_input(|input| input.set_capture_policy(policy));
        if let Err(error) = save_client_settings(&self.repository, &self.settings) {
            self.status = format!("Settings were not saved: {error}");
        }
    }

    /// The status line the menu bar and the connection bar show: who has
    /// the keyboard, and whether the host turned the clipboard off.
    pub(super) fn session_status_line(&self) -> String {
        arcen_deck_core::screens::keyboard_help::session_status_line(
            self.keyboard_status_line(),
            self.host_clipboard_notice(),
        )
    }

    /// The host's clipboard notice for the clipboard value this session
    /// connected with (the connection's own over Settings); `None` outside
    /// a session.
    fn host_clipboard_notice(&self) -> Option<&'static str> {
        arcen_deck_core::clipboard::HostClipboard::from_hello(self.host_clipboard)
            .connection_notice(&self.session_overrides, self.settings.clipboard_enabled)
    }

    /// Who has the keyboard, as the indicator says it ("Keyboard: Z8
    /// (Ctrl+Alt+Home to release)"); empty outside a session.
    pub(super) fn keyboard_status_line(&self) -> String {
        self.window_input_value(WindowInput::keyboard_status)
            .map(|status| {
                keyboard_status_line(
                    status,
                    &self.session_title,
                    DeckPlatform::Pc,
                    self.settings.keyboard_switch,
                )
            })
            .unwrap_or_default()
    }

    /// The keyboard switch's chord (Settings ▸ Input ▸ Keyboard switch).
    pub(super) const fn keyboard_switch(&self) -> arcen_input::keyboard::KeyboardSwitchChord {
        self.settings.keyboard_switch
    }

    /// The keyboard switch's chord to every session window and the menus,
    /// and a notice whenever the keyboard changed hands since the last pass.
    fn watch_keyboard_switch(&mut self, ctx: &egui::Context) {
        use arcen_input::keyboard::{KeyboardOwner, KeyboardStatus};
        let switch = self.settings.keyboard_switch;
        arcen_deck_core::screens::wording::set_keyboard_switch(ctx, switch);
        if let Some(connection) = &self.connection {
            connection.set_switch_chord(switch);
        }
        let status = self
            .window_input_value(WindowInput::keyboard_status)
            .filter(|status| *status != KeyboardStatus::NoSession);
        let owner = |status: KeyboardStatus| match status {
            KeyboardStatus::ThisComputer => KeyboardOwner::ThisComputer,
            _ => KeyboardOwner::Host,
        };
        if let (Some(before), Some(now)) = (self.shown_keyboard_owner, status)
            && owner(before) != owner(now)
        {
            let system_shortcuts = matches!(
                now,
                KeyboardStatus::Host {
                    system_shortcuts: true
                }
            );
            self.keyboard_toast = Some(KeyboardToast::new(
                ctx,
                keyboard_switch_toast(
                    owner(now),
                    &self.session_title,
                    DeckPlatform::Pc,
                    system_shortcuts,
                ),
            ));
        }
        self.shown_keyboard_owner = status;
        render_keyboard_toast(ctx, &mut self.keyboard_toast);
    }

    /// The keyboard went to this PC or back: in fullscreen pin
    /// the connection bar while this PC has it (the bar is then the
    /// indicator; fullscreen draws nothing over the video otherwise).
    fn keyboard_owner_changed(&mut self) {
        let released = self.window_input_value(WindowInput::keyboard_released) == Some(true);
        if released {
            self.pin_bar();
        } else if self.bar_pinned {
            self.bar_pinned = false;
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        if self.profile_sharing.is_open() {
            let outcome = self.profile_sharing.show(
                ui.ctx(),
                &mut arcen_deck_core::screens::profile::ProfileContext {
                    services: &*self.services,
                    repository: &self.repository,
                    security_mode: self.settings.security_mode,
                    saved: &self.saved,
                    pins: &mut self.remembered_pins,
                },
            );
            self.apply_profile_outcome(outcome);
        }
        let operator_notice = (self.settings.security_mode.in_force() == SecurityMode::Low)
            .then_some(
                "Insecure mode: this Deck is not verifying Pier certificates. \
             Anyone able to intercept the connection can read and control \
             this session.",
            );
        let mut state = HomeViewState {
            direct: &mut self.direct,
            saved_connections: &self.saved,
            search: &mut self.search,
            operator_notice,
            show_add_form: &mut self.show_add_form,
            editing_index: &mut self.editing_index,
            form_name: &mut self.form_name,
            form_host: &mut self.form_host,
            form_username: &mut self.form_username,
            form_overrides: &mut self.form_overrides,
            app_defaults: arcen_deck_core::overrides::AppDefaults::from_settings(
                &self.settings,
                SystemShortcutRoute::PcKeyboardHook,
            ),
            sign_in: &mut self.sign_in,
            overlay_open: self.help.modal(),
            remember_username: &mut self.remember_username,
            connection_test: None,
            notice: &mut self.home_notice,
        };
        let Some(action) = render_home(ui, &mut state) else {
            return;
        };
        let outcome = apply_home_action(
            &mut state,
            action,
            &self.repository,
            self.settings.security_mode,
        );
        if let Some(store) = outcome.store {
            self.reconcile(store);
        }
        if let Some(status) = outcome.status {
            self.status = status;
        }
        match outcome.effect {
            HomeEffect::None | HomeEffect::ConnectionTest(_) => {}
            HomeEffect::ExportProfile(index) => {
                let outcome = self.profile_sharing.export(
                    index,
                    &arcen_deck_core::screens::profile::ProfileContext {
                        services: &*self.services,
                        repository: &self.repository,
                        security_mode: self.settings.security_mode,
                        saved: &self.saved,
                        pins: &mut self.remembered_pins,
                    },
                );
                self.apply_profile_outcome(outcome);
            }
            HomeEffect::ImportProfile => {
                let outcome = self.profile_sharing.import(
                    &arcen_deck_core::screens::profile::ProfileContext {
                        services: &*self.services,
                        repository: &self.repository,
                        security_mode: self.settings.security_mode,
                        saved: &self.saved,
                        pins: &mut self.remembered_pins,
                    },
                );
                self.apply_profile_outcome(outcome);
            }
            HomeEffect::TestConnection { .. } => {
                "Connection test is not in this build of the Windows Deck yet."
                    .clone_into(&mut self.status);
            }
            HomeEffect::Connect(draft) => {
                if let Some(id) = &draft.saved_connection_id {
                    match self.repository.set_last_selected(id) {
                        Ok(store) => self.reconcile(store),
                        Err(error) => {
                            self.status = format!("Connection could not be selected: {error}");
                            return;
                        }
                    }
                }
                self.start_connection(draft, None, ui.ctx());
            }
        }
    }

    /// Starts a connection attempt for `draft`. A password typed on Home is
    /// kept for the host's auth request.
    fn start_connection(
        &mut self,
        mut draft: ConnectionDraft,
        preloaded_auth: Option<AuthSubmission>,
        ctx: &egui::Context,
    ) {
        self.disconnect_quietly();
        self.login_window_handover = None;
        self.deferred_auth = if draft.password.is_empty() {
            preloaded_auth
        } else {
            Some(AuthSubmission {
                username: draft.username.clone(),
                password: std::mem::take(&mut draft.password),
            })
        };
        let endpoint = draft.endpoint();
        let pin = self
            .remembered_pins
            .resolved()
            .get(&endpoint)
            .copied()
            .or_else(|| self.accepted_pins.resolved().get(&endpoint).copied());
        // This connection's own Displays choice, or a recovery's one-shot
        // for this endpoint, re-read per attempt (the Mac's rule).
        let saved_mode = draft
            .saved_connection_id
            .as_ref()
            .and_then(|id| self.saved.iter().find(|saved| &saved.id == id))
            .and_then(|saved| saved.settings.overrides.displays_mode.own());
        let source = arcen_deck_core::connect_plan::displays_mode_source(
            self.pending_displays_override.as_ref(),
            &endpoint,
            saved_mode,
        );
        let resolved = resolve_attempt_displays_mode(
            &mut self.pending_displays_override,
            &endpoint,
            saved_mode,
        );
        self.displays_origin = SessionDisplaysOrigin::for_attempt(source, resolved);
        let mode = resolved.unwrap_or(self.settings.displays_mode);
        tracing::info!(
            target: "arcen::display",
            mode = mode.as_wire(),
            source,
            settings = self.settings.displays_mode.as_wire(),
            "Displays mode for this connection attempt"
        );
        // A simulated layout (developer builds) is always Match My Layout.
        #[cfg(feature = "dev-tools")]
        let mode = if super::windowed_test::layout().is_some() {
            DisplaysMode::MatchLayout
        } else {
            mode
        };
        let approved = match super::displays::approve_match_layout(mode, &self.settings) {
            Ok(approved) => approved,
            Err(message) => {
                self.deferred_auth = None;
                self.status = message;
                self.screen = Screen::Home;
                return;
            }
        };
        let tls = tls_trust_for(self.settings.security_mode, endpoint, pin);
        let telemetry = Arc::new(ClientTelemetry::default());
        self.session_overrides = draft
            .saved_connection_id
            .as_ref()
            .and_then(|id| self.saved.iter().find(|saved| &saved.id == id))
            .map(|saved| saved.settings.overrides)
            .unwrap_or_default();
        self.session_keyboard_mode = None;
        let (options, plan) = connect_options(
            &draft,
            &self.session_overrides.apply_to(&self.settings),
            mode,
            approved,
            std::mem::take(&mut self.pending_desktop_replacement),
            &self.services,
            tls,
            &telemetry,
        );
        self.display_plan = Some(plan);
        self.requested_topology = options
            .multi_monitor_topology
            .as_ref()
            .map(|selection| selection.topology.clone());
        self.display_fit.reset();
        self.generation = self.generation.wrapping_add(1);
        self.status = arcen_deck_core::connect_plan::connecting_status(
            &draft,
            self.displays_origin.connecting_note(),
        );
        draft.display_name().clone_into(&mut self.session_title);
        self.configure_reconnect(&draft, &options);
        let holder_nonce = self
            .reconnect
            .as_ref()
            .map(|controller| controller.holder_nonce().to_owned())
            .filter(|nonce| !nonce.is_empty());
        match Connection::start(
            options,
            &self.surface,
            ctx,
            self.generation,
            Attach::Fresh { holder_nonce },
        ) {
            Ok(connection) => {
                // As on the Mac: bound for the connection's life; the
                // session lets items through only once negotiated.
                self.clipboard
                    .attach(self.generation, connection.media().clipboard().clone());
                self.connection = Some(connection);
                self.stream_stats_text.clear();
                self.screen = Screen::Connecting(draft);
            }
            Err(error) => {
                // The attempt never started: its credentials go now.
                self.deferred_auth = None;
                self.submitted_auth = None;
                self.status = format!("Connection error: {error}");
                self.end_reconnect();
                self.active_connection = None;
                self.direct = self.sign_in.return_home(
                    SignInFailure::ConnectionFailed,
                    draft,
                    Some(error.to_string()),
                );
                self.screen = Screen::Home;
            }
        }
    }

    /// One pass of the connection: what the session asked for, the
    /// resume of a lost one, the hand-over from a sign-in screen.
    fn drive_connection(&mut self, ctx: &egui::Context) {
        self.poll_session(ctx);
        self.drive_reconnect(ctx);
        self.drive_login_window_handover(ctx);
    }

    /// What the session asked for since the last pass.
    fn poll_session(&mut self, ctx: &egui::Context) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        let draft = match &self.screen {
            Screen::Connecting(draft) | Screen::Disclaimer(draft, _) => Some(draft.clone()),
            _ => self.active_connection.clone(),
        };
        match connection.poll() {
            SessionNews::Quiet => {}
            SessionNews::DeskPrivacy { note, warning } => {
                if warning {
                    tracing::warn!(target: "arcen::session", note, "host desk privacy changed");
                } else {
                    tracing::info!(target: "arcen::session", note, "host desk privacy changed");
                }
                self.notify(note.to_owned());
            }
            SessionNews::Authenticated(authentication) => {
                self.handle_authentication(authentication);
            }
            SessionNews::Streaming { login_window } => {
                self.deferred_auth = None;
                // The lost picture goes. Presenters share one owner per
                // pipeline, so its teardown may hide a frame the resumed
                // session already showed: that frame is shown again.
                if self.held.take().is_some()
                    && let Some(connection) = &self.connection
                {
                    connection.media().represent_last();
                }
                self.streaming_since
                    .get_or_insert_with(std::time::Instant::now);
                self.note_hello_desktop(login_window);
                "Receiving video stream".clone_into(&mut self.status);
                let Some(connection) = self.connection.as_mut() else {
                    return;
                };
                self.host_clipboard = connection
                    .media()
                    .shared()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .server_hello
                    .as_ref()
                    .and_then(|hello| hello.clipboard);
                // A host that applied Match My Layout gets one window per
                // monitor, or the session ends (the Mac's rule).
                if let Err(reason) = connection.begin_multi_window(self.requested_topology.as_ref())
                {
                    tracing::error!(target: "arcen::display", %reason, "multi-window entry refused");
                    self.multi_window_failure(&reason, ctx);
                    return;
                }
                let layout_note = connection.layout_note().map(str::to_owned);
                let display_note = connection.display_note();
                let desk_note = connection.desk_note().map(str::to_owned);
                let displays_note = self.displays_origin.session_note();
                if let Some(note) = displays_note {
                    tracing::info!(
                        target: "arcen::display",
                        note,
                        "display recovery holds one display"
                    );
                }
                if let Some(note) = &display_note {
                    tracing::warn!(
                        target: "arcen::display",
                        note = %note,
                        "host did not honour the requested display size"
                    );
                }
                let attached = connection.attach_input(
                    self.hwnd,
                    self.session_overrides.keyboard_settings(),
                    self.session_overrides
                        .effective_option_keys(self.settings.option_key_mode)
                        .value,
                    (
                        self.settings.tablet_mode_requested,
                        self.settings.pen_buttons,
                    ),
                    session_keyboard_capture(&self.session_overrides, &self.settings),
                );
                if let Err(error) = attached {
                    tracing::warn!(%error, "the session window's input was not attached");
                    self.status = format!("Receiving video stream (no input: {error})");
                }
                // A window already fullscreen (a resumed session keeps it)
                // gets no `set_fullscreen` call; the new input learns it here.
                let fullscreen = self.fullscreen;
                self.window_input(|input| input.set_fullscreen(fullscreen));
                self.apply_typing_mode();
                let tablet_hint = (!self.tablet_hint_shown
                    && crate::platform::pen::tablet_sends_mouse(
                        self.settings.tablet_mode_requested == TabletModeMsg::LocalTermination,
                    ))
                .then(|| {
                    self.tablet_hint_shown = true;
                    tracing::warn!(
                        target: "arcen::input",
                        "a pen tablet is attached but Windows does not route it as a pen \
                         (Windows Ink off): Tablet support receives mouse input only"
                    );
                    arcen_input::deck_pen::TABLET_AS_MOUSE_HINT.to_owned()
                });
                let notes: Vec<String> = desk_note
                    .into_iter()
                    .chain(displays_note.map(str::to_owned))
                    .chain(layout_note)
                    .chain(display_note)
                    .chain(tablet_hint)
                    .collect();
                if !notes.is_empty() {
                    self.notify(notes.join(" "));
                }
                self.screen = Screen::InSession;
            }
            SessionNews::CertificateUntrusted(info) => {
                let typed = self.deferred_auth.take();
                self.disconnect_quietly();
                // The warning owns it: only its approved retry takes it.
                self.recovery_auth = typed;
                self.status = format!("Verify server certificate for {}", info.endpoint);
                if let Some(draft) = draft {
                    self.screen = Screen::Certificate(draft, Box::new(info));
                }
            }
            SessionNews::CertificateChanged(presented) => {
                self.handle_certificate_changed(draft, presented);
            }
            SessionNews::AuthRequired(request) => {
                let Some(draft) = draft else { return };
                if let Some(text) = request.disclaimer {
                    self.screen = Screen::Disclaimer(draft, text);
                } else {
                    self.answer_auth(draft);
                }
            }
            SessionNews::Ended(end) => self.handle_connection_closed(&end, ctx),
        }
    }

    /// The Mac's `configure_reconnect`: a connection that may auto-reconnect
    /// gets a controller and a holder nonce, so the host issues it a resume
    /// grant.
    fn configure_reconnect(&mut self, draft: &ConnectionDraft, options: &ConnectOptions) {
        if let Some(controller) = &mut self.reconnect {
            controller.terminal();
        }
        self.reconnect = None;
        self.reconnect_options = None;
        let mut active = draft.clone();
        zeroize::Zeroize::zeroize(&mut active.password);
        self.active_connection = Some(active);
        if !draft.auto_reconnect {
            return;
        }
        match fresh_holder_nonce() {
            Ok(holder_nonce) => {
                let identity = self.reconnect_identity(draft, options, true);
                self.reconnect = Some(ReconnectController::new(
                    self.generation,
                    identity,
                    holder_nonce,
                ));
                self.reconnect_options = Some(options.credential_free_clone());
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "auto reconnect disabled because system randomness is unavailable",
                );
            }
        }
    }

    /// The identity a resume is bound to, with this PC's displays now; the
    /// `baseline` of a multi-monitor session is its negotiated layout (the
    /// Mac's `connection_identity_baseline`).
    fn reconnect_identity(
        &self,
        draft: &ConnectionDraft,
        options: &ConnectOptions,
        baseline: bool,
    ) -> ConnectionIdentity {
        let (monitors, topology, safe_area) =
            super::displays::identity_display_state(options, &self.settings, baseline);
        connection_identity_with_live_monitors(
            draft,
            options,
            self.settings.security_mode,
            &monitors,
            topology.as_ref(),
            safe_area,
        )
    }

    /// The Mac's `handle_authentication`: records the grant, then lets the
    /// session continue.
    fn handle_authentication(
        &mut self,
        mut authentication: arcen_deck_core::runtime::session::SessionAuthentication,
    ) {
        let kind = authentication.kind;
        let Some(controller) = self
            .reconnect
            .as_mut()
            .filter(|controller| controller.generation() == self.generation)
        else {
            // No controller (auto-reconnect off): accepted without a grant.
            if kind != AuthenticationKind::Refresh
                && let Some(connection) = &self.connection
            {
                connection.accept_authentication();
            }
            return;
        };
        if accept_authentication(controller, &mut authentication, self.reconnect_clock.now())
            .is_err()
        {
            controller.terminal();
            "Host returned an invalid session-resume response".clone_into(&mut self.status);
            self.return_home(SignInFailure::SessionEnded, Some(self.status.clone()));
            return;
        }
        if kind != AuthenticationKind::Refresh
            && !self
                .connection
                .as_ref()
                .is_some_and(Connection::accept_authentication)
        {
            "Connection closed before resume was accepted".clone_into(&mut self.status);
            self.return_home(SignInFailure::SessionEnded, Some(self.status.clone()));
            return;
        }
        if kind == AuthenticationKind::Resume {
            tracing::info!("session resumed; waiting for a fresh keyframe");
            "Session resumed; waiting for a fresh keyframe".clone_into(&mut self.status);
        }
    }

    /// The Mac's `handle_connection_closed`, for what this Deck serves: a
    /// sign-in screen is followed, a lost transport is resumed, anything
    /// else ends the connection with its reason.
    fn handle_connection_closed(&mut self, end: &SessionEnd, ctx: &egui::Context) {
        self.clipboard.detach();
        self.deferred_auth = None;
        self.preedit = None;
        self.quit_hold = None;
        self.status = format!("Connection error: {}", end.message);
        // Whether a desktop was up decides the words (shared
        // `SignInFailure::from_end`).
        let stage = if matches!(
            self.screen,
            Screen::InSession | Screen::Reconnecting(..) | Screen::SigningIn
        ) {
            SignInStage::Session
        } else {
            SignInStage::Attempt
        };
        let lost = self.connection.take();
        let resuming = matches!(self.screen, Screen::Reconnecting(_, true));
        if self.follow_login_window(end) {
            self.hold(lost);
            ctx.request_repaint();
            return;
        }
        // The host could not serve this attempt's layout: only the user
        // can choose the recovery (the Mac's screens and rules).
        if let Some(recovery) = displays_recovery_for(&end.message)
            && let Some(draft) = self.active_connection.clone()
        {
            drop(lost);
            self.held = None;
            self.recovery_auth = self.submitted_auth.take();
            tracing::info!(?recovery, "the host offered a display recovery");
            self.screen = match recovery {
                DisplaysRecovery::TopologyConflict => {
                    "This desktop was started without your layout.".clone_into(&mut self.status);
                    Screen::TopologyConflict(draft)
                }
                DisplaysRecovery::Unsupported => Screen::DisplaysUnsupported(draft),
            };
            ctx.request_repaint();
            return;
        }
        if schedule_after_end(
            self.reconnect.as_mut(),
            self.generation,
            end,
            &self.reconnect_clock,
            &mut self.reconnect_jitter,
        ) {
            tracing::info!(reason = %end.message, "connection lost; reconnecting");
            self.hold(lost);
            self.refresh_reconnect_screen();
            ctx.request_repaint();
            return;
        }
        drop(lost);
        self.held = None;
        if matches!(
            end.reason,
            DisconnectReason::Terminal(TerminalDisconnect::Manual)
        ) {
            self.end_reconnect();
            "Disconnected".clone_into(&mut self.status);
            self.screen = Screen::Home;
            return;
        }
        // Back on Home with the connection selected and the outcome said in
        // plain words: a refused resume asks to sign in again, as does
        // anything else that ends the attempt or session.
        let failure = if resuming {
            SignInFailure::from_end(end, SignInStage::Session)
        } else {
            SignInFailure::from_end(end, stage)
        };
        self.return_home(
            failure.unwrap_or(SignInFailure::SessionEnded),
            Some(arcen_deck_core::sign_in::end_detail(end)),
        );
    }

    /// The Mac's `handle_certificate_changed`: a host this Deck pinned
    /// presented another certificate. A remembered identity gets the shared
    /// recovery screen; anything else goes back to Home with the plain box.
    fn handle_certificate_changed(&mut self, draft: Option<ConnectionDraft>, presented: CertInfo) {
        let recovery_auth = self
            .deferred_auth
            .take()
            .or_else(|| self.submitted_auth.take());
        self.disconnect("This computer's identity has changed.");
        let Some(draft) = draft else {
            return;
        };
        // Shared with the Mac: a pin the connection carries (`--pin-sha256`)
        // is never offered for replacement.
        match identity_change_outcome(&draft, &self.remembered_pins, &presented) {
            IdentityChangeOutcome::Recover(change) => {
                tracing::warn!(
                    endpoint = %change.endpoint,
                    remembered_fingerprint = %change.remembered,
                    presented_fingerprint = %change.presented,
                    "a remembered host identity no longer matches; offering the recovery",
                );
                self.certificate_change_step = CertificateChangedStep::default();
                self.recovery_auth = recovery_auth;
                self.screen =
                    Screen::CertificateChanged(Box::new(ChangedIdentity { draft, change }));
            }
            IdentityChangeOutcome::Refuse { detail } => {
                self.direct = self.sign_in.return_home(
                    SignInFailure::HostIdentityChanged,
                    draft,
                    Some(detail),
                );
            }
        }
    }

    /// The shared identity-changed screen: Cancel leaves the remembered
    /// identity; "Trust and connect" replaces it and connects.
    fn certificate_changed_screen(&mut self, ui: &mut egui::Ui) {
        let Screen::CertificateChanged(changed) = &self.screen else {
            return;
        };
        let changed = changed.clone();
        match render_certificate_changed(ui, &changed.change, &mut self.certificate_change_step) {
            CertificateChangedAction::None => {}
            CertificateChangedAction::Cancel => {
                self.certificate_change_step = CertificateChangedStep::default();
                self.recovery_auth = None;
                "Connection cancelled; this host is still trusted under its previous identity."
                    .clone_into(&mut self.status);
                self.screen = Screen::Home;
            }
            CertificateChangedAction::TrustNewIdentity => {
                self.certificate_change_step = CertificateChangedStep::default();
                let ChangedIdentity { draft, change, .. } = *changed;
                let preloaded = self.recovery_auth.take();
                // The identity shown, of the remembered one's kind, in one
                // locked write of this endpoint (shared with the Mac).
                self.accepted_pins.forget(&change.endpoint);
                match replace_remembered_identity(
                    &self.repository,
                    &mut self.remembered_pins,
                    &change,
                ) {
                    IdentityReplaced::Saved => {
                        self.start_connection(draft, preloaded, ui.ctx());
                    }
                    IdentityReplaced::ThisRunOnly(status) => {
                        self.status = status;
                        self.start_connection(draft, preloaded, ui.ctx());
                    }
                    IdentityReplaced::ChangedElsewhere => {
                        "This host's trusted identity changed in another Arcen Deck window. \
                         Nothing was trusted: connect again to check it."
                            .clone_into(&mut self.status);
                        self.screen = Screen::Home;
                    }
                }
            }
        }
    }

    /// The two display recoveries a host can ask for (CN-08, CN-09), with
    /// the shared screens and choices.
    fn display_recovery_screen(&mut self, ui: &mut egui::Ui) {
        match &self.screen {
            Screen::TopologyConflict(draft) => {
                let draft = draft.clone();
                match render_topology_conflict(ui, &draft) {
                    TopologyConflictAction::None => {}
                    TopologyConflictAction::Cancel => self.cancel_display_recovery(),
                    TopologyConflictAction::Reconnect => {
                        let retry = topology_conflict_retry(&draft.endpoint(), false);
                        self.retry_display_recovery(draft, &retry, ui.ctx());
                    }
                    TopologyConflictAction::StartFresh => {
                        let retry = topology_conflict_retry(&draft.endpoint(), true);
                        self.retry_display_recovery(draft, &retry, ui.ctx());
                    }
                }
            }
            Screen::DisplaysUnsupported(draft) => {
                let draft = draft.clone();
                match render_displays_unsupported(ui, &draft, &self.status) {
                    DisplaysUnsupportedAction::None => {}
                    DisplaysUnsupportedAction::Cancel => self.cancel_display_recovery(),
                    DisplaysUnsupportedAction::UsePrimaryOnly => {
                        let retry = displays_unsupported_retry(&draft.endpoint());
                        self.retry_display_recovery(draft, &retry, ui.ctx());
                    }
                }
            }
            _ => {}
        }
    }

    /// A display recovery cancelled: Home, and its credentials are gone.
    fn cancel_display_recovery(&mut self) {
        self.status.clear();
        self.recovery_auth = None;
        self.end_reconnect();
        self.active_connection = None;
        self.screen = Screen::Home;
    }

    /// Retries `draft` with a display recovery's choice (the shared
    /// `DisplaysRecoveryRetry`): the one-shot Displays mode and the desktop
    /// replacement. Nothing is saved on the connection.
    fn retry_display_recovery(
        &mut self,
        draft: ConnectionDraft,
        retry: &DisplaysRecoveryRetry,
        ctx: &egui::Context,
    ) {
        self.pending_desktop_replacement = retry.replace_desktop;
        if let Some(pending) = retry.displays_override.clone() {
            self.pending_displays_override = Some(pending);
        }
        retry.status.clone_into(&mut self.status);
        let auth = self.recovery_auth.take();
        self.start_connection(draft, auth, ctx);
    }

    /// Keeps a lost connection's last picture under the overlay. A failed
    /// resume attempt (no picture) never replaces the picture already held.
    fn hold(&mut self, lost: Option<Connection>) {
        if let Some(mut lost) = lost {
            lost.release_input();
            if !self
                .held
                .as_ref()
                .is_some_and(|held| held.media().has_presented_picture())
            {
                self.held = Some(lost);
            }
        }
    }

    /// What stays under the reconnect and signing-in overlays.
    fn held_frame(&self) -> HeldFrame<fn(&egui::Painter, egui::Rect)> {
        if self
            .held
            .as_ref()
            .is_some_and(|held| held.media().has_presented_picture())
        {
            HeldFrame::UnderOverlay
        } else {
            HeldFrame::Nothing
        }
    }

    /// The reconnect or signing-in overlay over the held picture; its
    /// Disconnect ends the session for good.
    fn recovery_overlay(&mut self, ui: &mut egui::Ui) {
        let action = match &self.screen {
            Screen::Reconnecting(overlay, resuming) => {
                let retry_left = overlay.retry_left_secs(self.reconnect_clock.now());
                render_reconnecting(ui, self.held_frame(), *resuming, retry_left)
            }
            Screen::SigningIn => render_signing_in(ui, self.held_frame()),
            _ => HeldFrameAction::None,
        };
        if action == HeldFrameAction::Disconnect {
            self.disconnect("Disconnected");
        }
    }

    /// Whether a lost connection is being resumed or a sign-in followed.
    fn recovering(&self) -> bool {
        matches!(self.screen, Screen::Reconnecting(..) | Screen::SigningIn)
    }

    /// The Mac's `drive_reconnect`: expires the restore window, and starts
    /// the resume attempt once it is due.
    fn drive_reconnect(&mut self, ctx: &egui::Context) {
        let now = self.reconnect_clock.now();
        let Some(controller) = self.reconnect.as_mut() else {
            return;
        };
        if controller.expire(now) {
            "Session resume window expired".clone_into(&mut self.status);
            self.return_home(SignInFailure::SessionEnded, Some(self.status.clone()));
            return;
        }
        if !is_reconnecting(self.reconnect.as_ref()) {
            return;
        }
        // The overlay counts down and the attempt starts on time.
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        let Some(identity) = self
            .active_connection
            .as_ref()
            .zip(self.reconnect_options.as_ref())
            .map(|(draft, options)| self.reconnect_identity(draft, options, false))
        else {
            self.return_home(SignInFailure::SessionEnded, None);
            return;
        };
        let generation = self.generation;
        let Some(controller) = self.reconnect.as_mut() else {
            return;
        };
        match next_resume_launch(
            controller,
            &self.reconnect_clock,
            generation,
            &identity,
            self.reconnect_options.as_ref(),
        ) {
            ResumeLaunch::Idle => {}
            ResumeLaunch::Abandon => {
                self.return_home(SignInFailure::SessionEnded, None);
                return;
            }
            ResumeLaunch::Launch {
                options,
                attempt,
                deadline,
            } => {
                tracing::info!(attempt = attempt.attempt, "resuming the lost session");
                // The lost picture stays under "Authenticating…" until the
                // resumed session's hello; its fresh decoder then waits for a
                // fresh keyframe.
                match Connection::start(
                    *options,
                    &self.surface,
                    ctx,
                    self.generation,
                    Attach::Resume { attempt, deadline },
                ) {
                    Ok(connection) => {
                        self.clipboard
                            .attach(self.generation, connection.media().clipboard().clone());
                        self.connection = Some(connection);
                        self.stream_stats_text.clear();
                    }
                    Err(error) => {
                        self.status = format!("Connection error: {error}");
                        self.return_home(SignInFailure::SessionEnded, Some(error.to_string()));
                        return;
                    }
                }
            }
        }
        self.refresh_reconnect_screen();
    }

    fn refresh_reconnect_screen(&mut self) {
        if let Some((overlay, resuming)) = self
            .reconnect
            .as_ref()
            .and_then(|controller| ReconnectOverlay::from_phase(controller.phase()))
        {
            self.screen = Screen::Reconnecting(overlay, resuming);
        }
    }

    /// The Mac's `return_home`: the attempt or session is over without a
    /// desktop. Home, the same connection selected, its password cleared
    /// and focused, and `failure` in plain words under it (or in a box), by
    /// the shared `arcen_deck_core::sign_in`; `detail` goes to the log and
    /// "Details".
    fn return_home(&mut self, failure: SignInFailure, detail: Option<String>) {
        self.disconnect_quietly();
        self.end_reconnect();
        if let Some(draft) = self.active_connection.take() {
            self.direct = self.sign_in.return_home(failure, draft, detail);
        }
        self.screen = Screen::Home;
    }

    /// Drops the reconnect controller as terminal (never resumed again).
    fn end_reconnect(&mut self) {
        if let Some(controller) = &mut self.reconnect {
            controller.terminal();
        }
        self.reconnect = None;
        self.reconnect_options = None;
        self.held = None;
    }

    /// The Mac's `note_hello_desktop`: a sign-in screen keeps this
    /// attempt's credentials to follow it into the desktop; a signed-in
    /// desktop ends any hand-over.
    fn note_hello_desktop(&mut self, login_window: bool) {
        let auth = self.submitted_auth.take();
        if login_window {
            if let Some(auth) = auth {
                tracing::info!(
                    "host is showing its sign-in screen; will follow it into the desktop"
                );
                let schedule = self
                    .login_window_handover
                    .take()
                    .map(|handover| handover.schedule)
                    .unwrap_or_default();
                self.login_window_handover = Some(LoginWindowHandover { schedule, auth });
            }
        } else if self.login_window_handover.take().is_some() {
            tracing::info!("followed the sign-in screen into the signed-in desktop");
        }
    }

    /// The Mac's `follow_login_window`: whether a closed session was a
    /// sign-in screen giving way to a desktop (then the reconnect is
    /// scheduled). What is followed is shared
    /// (`reconnect_flow::follows_login_window`): never a definite refusal
    /// such as `console_other_user`.
    fn follow_login_window(&mut self, end: &SessionEnd) -> bool {
        let followable = arcen_deck_core::reconnect_flow::follows_login_window(end)
            && self.active_connection.is_some();
        let now = self.reconnect_clock.observed_at(end.observed_at);
        let Some(handover) = self.login_window_handover.as_mut() else {
            return false;
        };
        let due = if followable {
            arcen_deck_core::reconnect_flow::login_window_reconnect_due(
                &mut handover.schedule,
                end,
                now,
            )
        } else {
            None
        };
        if due.is_none() {
            self.login_window_handover = None;
            return false;
        }
        tracing::info!(
            attempt = handover.schedule.attempts_made() + 1,
            reason = %end.message,
            "sign-in screen closed; reconnecting to the desktop that replaces it",
        );
        "Signing in…".clone_into(&mut self.status);
        self.screen = Screen::SigningIn;
        true
    }

    /// The Mac's `drive_login_window_handover`: starts the hand-over's
    /// connection once it is due.
    fn drive_login_window_handover(&mut self, ctx: &egui::Context) {
        let now = self.reconnect_clock.now();
        let Some(handover) = self.login_window_handover.as_mut() else {
            return;
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        if !handover.schedule.take_due(now) {
            return;
        }
        let auth = handover.auth.clone();
        let Some(draft) = self.active_connection.clone() else {
            self.login_window_handover = None;
            "Sign-in hand-over could not continue: connection destination was lost."
                .clone_into(&mut self.status);
            self.return_home(SignInFailure::SessionEnded, Some(self.status.clone()));
            return;
        };
        // Starting a connection drops the hand-over; it is carried across
        // so a desktop that is not ready yet is tried again.
        let handover = self.login_window_handover.take();
        let held = self.held.take();
        self.start_connection(draft, Some(auth), ctx);
        if matches!(self.screen, Screen::Connecting(_)) {
            self.screen = Screen::SigningIn;
            self.login_window_handover = handover;
            self.held = held;
        }
    }

    /// Answers the host's auth request with the credentials already typed.
    /// Without them the waiting attempt is closed and Home asks for the
    /// password under the connection's field; Connect there starts afresh.
    fn answer_auth(&mut self, draft: ConnectionDraft) {
        let Some(submission) = self.deferred_auth.take() else {
            let status = format!("Enter password for {}", draft.display_name());
            self.disconnect(&status);
            self.direct = self
                .sign_in
                .return_home(SignInFailure::PasswordRequired, draft, None);
            return;
        };
        self.submitted_auth = Some(submission.clone());
        let sent = self
            .connection
            .as_ref()
            .is_some_and(|connection| connection.submit(submission));
        if sent {
            self.status = format!("Authenticating and launching {}...", draft.display_name());
            if !matches!(self.screen, Screen::SigningIn) {
                self.screen = Screen::Connecting(draft);
            }
        } else {
            self.disconnect("Connection closed before credentials were submitted");
        }
    }

    fn certificate_screen(&mut self, ui: &mut egui::Ui) {
        let Screen::Certificate(draft, info) = &self.screen else {
            return;
        };
        let facts = CertificateFacts {
            endpoint: info.endpoint.clone(),
            server_name: info.server_name.clone(),
            peer_address: info.peer_address.clone(),
            spki_display: info.spki_sha256_display.clone(),
            certificate_display: info.certificate_sha256_display.clone(),
            not_before_epoch_secs: info.not_before_epoch_secs,
            not_after_epoch_secs: info.not_after_epoch_secs,
        };
        let action = render_certificate_warning(ui, &facts);
        let (draft, info) = (draft.clone(), info.clone());
        match action {
            CertificateAction::None => {}
            CertificateAction::Cancel => {
                let _ = certificate_retry_auth(action, &mut self.recovery_auth);
                "Certificate trust cancelled".clone_into(&mut self.status);
                self.screen = Screen::Home;
            }
            CertificateAction::TrustForSession | CertificateAction::TrustAndRemember => {
                let endpoint = draft.endpoint();
                self.accepted_pins
                    .remember(&endpoint, &info.spki_sha256, None);
                if action == CertificateAction::TrustAndRemember {
                    self.remember_pin(&draft, &endpoint, &info);
                }
                let preloaded = certificate_retry_auth(action, &mut self.recovery_auth);
                self.start_connection(draft, preloaded, ui.ctx());
            }
        }
    }

    /// "Trust and remember" on the certificate warning: this endpoint only,
    /// under the identities' lock, and only while none is remembered for
    /// it on disk either (the warning showed it as unknown).
    fn remember_pin(&mut self, draft: &ConnectionDraft, endpoint: &str, info: &CertInfo) {
        let label = draft
            .saved_connection_id
            .as_ref()
            .and_then(|id| self.saved.iter().find(|saved| &saved.id == id))
            .map(|saved| saved.name.clone())
            .filter(|name| !name.trim().is_empty());
        let expected = self.remembered_pins.resolved().get(endpoint).copied();
        let path = self.repository.trusted_pins_file();
        if let Err(error) = arcen_deck_core::trusted_pins::remember_reviewed(
            path.as_deref(),
            &mut self.remembered_pins,
            endpoint,
            expected,
            &info.spki_sha256,
            label,
        ) {
            tracing::warn!(%endpoint, %error, "could not record the remembered host identity");
            self.status = format!("Trusted for this session only: {error}.");
        }
    }

    /// The fullscreen a pinned session holds, this pass (the shared
    /// `SessionFullscreen` rule): the change to make and the display the
    /// session's stream was sized for. A pinned session keeps the window
    /// fullscreen while it reconnects or follows a sign-in screen.
    pub(super) fn session_fullscreen_step(
        &mut self,
        fullscreen: bool,
    ) -> Option<(SessionFullscreenStep, Option<u32>)> {
        // A simulated layout's windows stay windows (developer builds).
        #[cfg(feature = "dev-tools")]
        if super::windowed_test::layout().is_some() {
            return None;
        }
        let on_screen = matches!(
            self.screen,
            Screen::InSession | Screen::Reconnecting(..) | Screen::SigningIn
        );
        let plan = self.display_plan;
        let pinned = on_screen && plan.is_some_and(|plan| plan.policy.is_pinned());
        // In a multi-window session the root window shows its own monitor.
        let display = self
            .connection
            .as_ref()
            .and_then(Connection::multi_window_plan)
            .map(|(root, _)| root)
            .or_else(|| plan.and_then(|plan| plan.pinned_display));
        self.session_fullscreen
            .step(pinned, fullscreen)
            .map(|step| (step, display))
    }

    /// Tells a host that follows it (`display_follow`) when the session
    /// window has settled on another display (the Mac's
    /// `follow_window_display`). The displays are read only after the shell
    /// noted a move and while the window settles, at most four times a
    /// second: reading them costs DXGI and display-config queries.
    fn drive_display_follow(&mut self, ui: &egui::Ui) {
        const POLL: std::time::Duration = std::time::Duration::from_millis(250);
        let window_display = self.window_display;
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        if !connection.host_follows_display_moves() {
            return;
        }
        let follower = connection.display_follow();
        if follower.has_observed() && !self.window_display_moved && !follower.is_settling() {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .display_follow_polled
            .is_some_and(|polled| now.saturating_duration_since(polled) < POLL)
        {
            ui.ctx().request_repaint_after(POLL);
            return;
        }
        self.display_follow_polled = Some(now);
        self.window_display_moved = false;
        let follower = connection.display_follow();
        let update = follower.observe(&crate::display::enumerate(), window_display, now);
        if follower.is_settling() {
            ui.ctx().request_repaint_after(POLL);
        }
        let Some(update) = update else {
            return;
        };
        if let Ok(value) = serde_json::to_value(&update)
            && connection.send_json(value)
        {
            tracing::info!(
                target: "arcen::display",
                display = ?update.refresh.stream_display_id,
                refresh_hz = ?update.refresh.stream_refresh_hz(),
                sequence = update.sequence,
                "display_refresh_update sent: the session window moved to another display",
            );
        }
    }

    /// A Windowed session asks the host for the window's size once it
    /// settles (the Mac's display-fit tracker): resize the window, or put it
    /// fullscreen on any monitor, and the host's desktop follows, 1:1.
    fn drive_display_fit(&mut self, ui: &egui::Ui) {
        let Some(plan) = self.display_plan else {
            return;
        };
        let Some(connection) = self.connection.as_ref() else {
            return;
        };
        if let Some(result) = connection.take_display_update_result() {
            tracing::info!(
                target: "arcen::display",
                accepted = result.accepted,
                width = result.width,
                height = result.height,
                message = result.message.as_str(),
                "display_update result",
            );
            self.display_fit.handle_result(&result);
        }
        // The scale for HiDPI streaming is the monitor's own (not a zoom).
        let fit_scale = if self.settings.hidpi_streaming {
            ui.ctx()
                .input(|input| input.viewport().native_pixels_per_point)
                .unwrap_or(1.0)
        } else {
            1.0
        };
        let rect = ui.ctx().content_rect();
        let observed = DisplayFitTracker::prepare(rect.width(), rect.height(), fit_scale);
        let actual = self
            .surface
            .tree
            .lock()
            .ok()
            .and_then(|tree| tree.shown_fit())
            .map(|(_, source)| [source.0, source.1]);
        let gate = display_fit_gate(
            plan.policy,
            connection.host_supports_display_update(),
            matches!(self.screen, Screen::InSession),
            connection.frame_fresh(),
        );
        let reason = if fit_scale > 1.0 {
            "retina"
        } else {
            "connect_fit"
        };
        if gate && actual != Some(observed) {
            // The tracker waits for the size to settle: look again.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        if let Some(message) = self
            .display_fit
            .poll(observed, actual, gate, fit_scale, reason)
            && let Ok(value) = serde_json::to_value(&message)
            && connection.send_json(value)
        {
            tracing::info!(
                target: "arcen::display",
                width = message.width,
                height = message.height,
                scale = message.scale,
                reason = message.reason.as_str(),
                "display_update sent",
            );
        }
    }

    /// A multi-window session that cannot go on (the Mac's
    /// `multi_window_failure`): a reconnect when this connection may resume,
    /// otherwise Home, asking to sign in again.
    fn multi_window_failure(&mut self, reason: &str, ctx: &egui::Context) {
        self.status = format!("Multi-monitor session ended: {reason}");
        self.clipboard.detach();
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
        let generation = self.generation;
        if let Some(controller) = self
            .reconnect
            .as_mut()
            .filter(|controller| controller.generation() == generation)
            && controller
                .schedule(
                    reason.to_owned(),
                    self.reconnect_clock.now(),
                    &mut self.reconnect_jitter,
                )
                .is_ok()
        {
            self.refresh_reconnect_screen();
            ctx.request_repaint();
            return;
        }
        self.return_home(SignInFailure::SessionEnded, Some(reason.to_owned()));
        ctx.request_repaint();
    }

    /// The windows a multi-window session needs (see
    /// `Connection::multi_window_plan`).
    pub(super) fn multi_window_plan(&self) -> Option<(u32, Vec<(SessionMonitorId, u32)>)> {
        self.connection
            .as_ref()
            .filter(|_| matches!(self.screen, Screen::InSession))?
            .multi_window_plan()
    }

    /// One pass of the multi-window entry from the shell's observations;
    /// returns whether the layout committed this pass (the shell then gives
    /// the secondary windows their input).
    pub(super) fn observe_multi_window(
        &mut self,
        observations: &[WindowObservation],
        live: &[ActiveDisplayInfo],
        any_close_requested: bool,
        ctx: &egui::Context,
    ) -> bool {
        let Some(connection) = self.connection.as_mut() else {
            return false;
        };
        match connection.observe_multi_window(observations, live, any_close_requested) {
            MultiWindowVerdict::Steady => false,
            MultiWindowVerdict::Committed => true,
            MultiWindowVerdict::Failed(reason) => {
                tracing::error!(target: "arcen::display", %reason, "multi-window session ended");
                self.multi_window_failure(&reason, ctx);
                false
            }
        }
    }

    /// Gives a secondary window's input to the session.
    pub(super) fn attach_secondary_input(&mut self, hwnd: HWND, monitor: SessionMonitorId) {
        let capture = self.keyboard_capture();
        let (keyboard, option_keys) = (
            self.session_overrides.keyboard_settings(),
            self.session_overrides
                .effective_option_keys(self.settings.option_key_mode)
                .value,
        );
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        if let Err(error) =
            connection.attach_secondary_input(hwnd, monitor, keyboard, option_keys, capture)
        {
            tracing::warn!(%error, monitor = monitor.get(), "a secondary window's input was not attached");
        }
    }

    /// The live connection, for the shell's secondary windows.
    pub(super) const fn connection(&self) -> Option<&Connection> {
        self.connection.as_ref()
    }

    /// The window went fullscreen or back.
    pub(super) fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
        self.window_input(|input| input.set_fullscreen(fullscreen));
        if !fullscreen {
            self.unpin_bar();
            self.bar_regions = OverlayPointerRegions::default();
            self.publish_local_ui();
        }
    }

    /// Ctrl+Alt+Home gave the keyboard to this PC: in fullscreen the
    /// connection bar opens and stays open (Remote Desktop's chord) until
    /// the keyboard goes back: the chord again, a click into the picture, a
    /// command chosen, or Esc.
    fn pin_bar(&mut self) {
        if self.fullscreen && self.connection.is_some() {
            self.bar_pinned = true;
        }
    }

    /// The pinned bar closes (Esc, a command): the keyboard goes back to the
    /// host.
    fn unpin_bar(&mut self) {
        if std::mem::take(&mut self.bar_pinned) {
            self.window_input(|input| input.set_keyboard_released(false));
        }
    }

    /// The commands chosen on the connection bar since the last call.
    pub(super) fn take_bar_commands(&mut self) -> Vec<MenuCommand> {
        std::mem::take(&mut self.bar_commands)
    }

    /// The fullscreen connection bar: nothing is drawn over the video until
    /// the pointer reaches the top edge above it or Ctrl+Alt+Home pins it.
    /// The window's input learns where the bar takes the pointer at the end
    /// of the pass (`publish_local_ui`).
    fn connection_bar(&mut self, ctx: &egui::Context) {
        if self.bar_pinned
            && (self.connection.is_none()
                || self.window_input_value(WindowInput::keyboard_released) == Some(false)
                || ctx.input(|input| input.key_pressed(egui::Key::Escape)))
        {
            // A click into the picture took the keyboard back, or Esc.
            self.unpin_bar();
        }
        let status = self.session_status_line();
        let title = if status.is_empty() {
            self.session_title.clone()
        } else {
            format!("{} \u{2014} {status}", self.session_title)
        };
        let pass = render_connection_bar(
            ctx,
            DeckPlatform::Pc,
            &title,
            self.bar_pinned,
            // As the menu bar: the Windows Deck has no Colour check yet.
            |command| command != MenuCommand::ToggleColourCheck,
        );
        if let Some(command) = pass.command {
            self.bar_commands.push(command);
            // The keyboard item gives the keyboard back itself (and closes
            // the bar); every other command closes it here.
            if command != MenuCommand::ToggleKeyboardCapture {
                self.unpin_bar();
            }
        }
        let ppp = f64::from(ctx.pixels_per_point());
        let pixels = |rect: egui::Rect| ImageRect {
            left: f64::from(rect.left()) * ppp,
            top: f64::from(rect.top()) * ppp,
            width: f64::from(rect.width()) * ppp,
            height: f64::from(rect.height()) * ppp,
        };
        self.bar_regions = OverlayPointerRegions {
            reveal: Some(pixels(connection_bar_reveal_zone(ctx.content_rect()))),
            owned: pass.area.map(pixels),
            whole_window: pass.menu_open,
            modal: false,
        };
    }

    /// The session's overlays over the video (menu toggles).
    fn session_overlays(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        if self
            .notice
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() > NOTICE_DURATION)
        {
            self.notice = None;
        }
        if let Some(connection) = self.connection.as_mut() {
            let ppp = ui.ctx().pixels_per_point();
            if let Some((mark, image_rect, at, shown, stream_width)) = connection.led_cursor(ppp) {
                let shown = shown.clone();
                paint_host_pointer(
                    ui,
                    image_rect,
                    at,
                    mark,
                    &shown,
                    Some(stream_width),
                    &mut connection.pointer_texture,
                );
            }
        }
        paint_text_input_overlays(
            ui.ctx(),
            rect,
            self.typing_mode() == TypingMode::Characters
                && self
                    .connection
                    .as_ref()
                    .and_then(Connection::host_types_text)
                    == Some(true),
            self.preedit.as_deref(),
            self.notice.as_ref().map(|(_, text)| text.as_str()),
        );
        if self.notice.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }
        if self.fullscreen {
            self.connection_bar(ui.ctx());
        }
        let Some(connection) = &self.connection else {
            return;
        };
        if self.show_stream_stats {
            let displays_note = self.displays_origin.session_note();
            let lines = self
                .stream_stats_text
                .get_or_refresh(std::time::Instant::now(), || {
                    connection.stream_stats_lines(displays_note)
                });
            paint_stream_stats(ui, rect, lines);
        }
        self.record_support_snapshot(connection);
        if self.show_tablet_monitor {
            self.paint_tablet_monitor(ui, rect, connection);
        }
        if self.show_negotiated_truth {
            let state = connection
                .media()
                .shared()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (requested_video, requested_selection, requested_intent) = connection.requested();
            let truth = state.server_hello.as_ref().map(|hello| {
                arcen_deck_core::session_truth::NegotiatedTruth::from_hello_with_selection(
                    hello,
                    requested_video,
                    requested_selection,
                    None,
                )
            });
            let summary = self.services.platform_summary();
            paint_negotiated_truth_panel(
                ui,
                rect,
                truth.as_ref(),
                requested_intent,
                &DecodeTruth {
                    hardware: state.decoder_hardware_accelerated,
                    backend: state.decoder_backend_name,
                    platform: &summary,
                    path: if state.decoder_hardware_accelerated == Some(true) {
                        "hardware"
                    } else {
                        "unknown"
                    },
                },
            );
        }
    }

    /// What Stream Statistics and Negotiated Truth show now, kept for a
    /// support bundle made after the session (built at most every two
    /// seconds; `support_bundle::snapshot`).
    fn record_support_snapshot(&self, connection: &Connection) {
        use arcen_deck_core::session_truth::{
            NegotiatedTruth, negotiated_truth_summary, truth_badge_text,
        };
        arcen_deck_core::support_bundle::snapshot::record_session_snapshot(|| {
            let (truth, decoder_refusal) = {
                let state = connection
                    .media()
                    .shared()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (requested_video, requested_selection, _) = connection.requested();
                let truth = state.server_hello.as_ref().map(|hello| {
                    let truth = NegotiatedTruth::from_hello_with_selection(
                        hello,
                        requested_video,
                        requested_selection,
                        None,
                    );
                    vec![
                        negotiated_truth_summary(&truth),
                        truth_badge_text(&truth, state.decoder_hardware_accelerated),
                    ]
                });
                (truth, state.decoder_refusal.clone())
            };
            arcen_deck_core::support_bundle::snapshot::SessionSnapshot {
                captured_unix_seconds: 0,
                stream_statistics: connection
                    .stream_stats_lines(self.displays_origin.session_note()),
                negotiated_truth: truth.unwrap_or_default(),
                health: Vec::new(),
                decoder_refusal,
                telemetry: arcen_deck_core::support_bundle::snapshot::telemetry_lines(
                    connection.telemetry(),
                ),
            }
        });
    }

    /// View ▸ Tablet Monitor, from what this shell knows of the pen so far:
    /// the hello's negotiation and the device's presence. Pen samples and
    /// their counters arrive with the input stream's pen capture.
    fn paint_tablet_monitor(&self, ui: &egui::Ui, rect: egui::Rect, connection: &Connection) {
        use arcen_deck_core::settings::{
            pen_negotiation_result, protocol_capability, provisional_tablet_mode_active,
            tablet_mode_allows_typed_pen,
        };
        use arcen_input::CapabilityAvailability as Local;
        use arcen_protocol::messages::InputCapabilityAvailability as Wire;
        let requested = self.settings.tablet_mode_requested;
        let state = connection
            .media()
            .shared()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hello = state
            .server_hello
            .as_ref()
            .map(|hello| (hello.input_protocol_version, hello.input_capabilities.pen));
        let local = hello.map_or(Wire::Unknown, |_| {
            protocol_capability(arcen_deck_core::services::typed_pen_capability(
                tablet_mode_allows_typed_pen(requested),
                self.services.captures_typed_pen(),
            ))
        });
        let (active, negotiated) = hello.map_or((requested, false), |(version, host)| {
            (
                provisional_tablet_mode_active(requested, version, local, host),
                pen_negotiation_result(requested, version, host),
            )
        });
        paint_tablet_monitor_panel(
            ui,
            rect,
            &TabletMonitorFacts {
                tablet_presence: self.services.tablet_presence(),
                local_pen_offered: local,
                host_pen: hello.map_or(Wire::Unknown, |(_, host)| host),
                pen_negotiated: negotiated,
                typed_pen_allowed: tablet_mode_allows_typed_pen(active),
                runtime_installed: false,
                mode_requested: requested,
                mode_active: active,
                mode_reason: "",
                last_event: None,
                events_sent: 0,
                overflow_resets: 0,
                producer_dropped: 0,
                dispatcher_edges: 0,
                motion_forwarded: 0,
                dispatcher_overflow: 0,
                probe_pressure: Local::Unknown,
                probe_tilt: Local::Unknown,
                probe_rotation: Local::Unknown,
                probe_eraser: Local::Unknown,
                probe_barrel_buttons: Local::Unknown,
                last_error: None,
            },
        );
    }

    /// Ends the session and returns to Home with `status`.
    fn disconnect(&mut self, status: &str) {
        // A password never outlives its attempt on Home (the shared Connect
        // already took it; this covers any other way in).
        zeroize::Zeroize::zeroize(&mut self.direct.password);
        self.disconnect_quietly();
        // A deliberate disconnect is never resumed (the Mac's
        // `manual_cancel`).
        if let Some(controller) = &mut self.reconnect {
            controller.manual_cancel();
        }
        self.reconnect = None;
        self.reconnect_options = None;
        self.active_connection = None;
        self.login_window_handover = None;
        self.held = None;
        status.clone_into(&mut self.status);
        self.screen = Screen::Home;
    }

    fn disconnect_quietly(&mut self) {
        self.quit_hold = None;
        // A display recovery's retry took its credentials already.
        self.recovery_auth = None;
        self.clipboard.detach();
        self.preedit = None;
        self.deferred_auth = None;
        self.submitted_auth = None;
        self.host_clipboard = None;
        if let Some(connection) = self.connection.take() {
            connection.close();
        }
    }

    /// Whether a session is up (the menu bar enables its commands).
    pub(super) const fn in_session(&self) -> bool {
        self.connection.is_some()
            || matches!(self.screen, Screen::Reconnecting(..) | Screen::SigningIn)
    }

    /// The window was minimised or restored.
    pub(super) fn set_visible(&self, visible: bool) {
        if let Some(connection) = &self.connection {
            connection.set_visible(visible);
        }
    }

    /// Applies Export's or Import's outcome as a Home outcome.
    fn apply_profile_outcome(
        &mut self,
        outcome: arcen_deck_core::screens::profile::ProfileOutcome,
    ) {
        if let Some(notice) = outcome.home_notice() {
            self.home_notice = Some(notice);
        }
        if let Some(store) = outcome.store {
            self.reconcile(store);
        }
        if let Some(status) = outcome.status {
            self.status = status;
        }
        if let Some(id) = outcome.select {
            self.show_add_form = false;
            self.editing_index = None;
            arcen_deck_core::screens::profile::select(
                &mut self.direct,
                &mut self.sign_in,
                &self.saved,
                &id,
            );
        }
    }

    fn reconcile(&mut self, store: arcen_deck_core::settings::SavedConnectionsStore) {
        self.saved = store.connections;
        self.last_selected = store.last_selected_connection_id;
    }

    fn settings_screen(&mut self, ui: &mut egui::Ui) {
        let monitors = self.monitors.clone();
        let host_clipboard_notice = self.host_clipboard_notice();
        let facts = SettingsFacts {
            hdr_screen: self.services.any_display_is_hdr(),
            notch: None,
            monitors: &monitors,
            host_clipboard_notice,
            connections: &self.saved,
            native_tablet: self
                .native_tablet
                .control(self.services.as_ref(), DeckPlatform::Pc),
            decode: self.decode_report.get().cloned(),
            support_bundle: Some(self.support_bundle.view(self.services.as_ref())),
            connection_displays: self.last_selected.as_ref().and_then(|id| {
                let saved = self.saved.iter().find(|saved| &saved.id == id)?;
                Some((
                    saved.name.clone(),
                    saved.settings.overrides.displays_mode.own()?,
                ))
            }),
            canvas_available: arcen_deck_core::canvas::CanvasRequest::for_sink::<
                crate::media::WindowSink,
            >(true, self.settings.canvas_cache_mib)
                != arcen_deck_core::canvas::CanvasRequest::Unavailable,
            system_shortcut_permission: None,
        };
        let mut values = SettingsValues::from_client_settings(&self.settings, self.settings_tab);
        let actions = render_settings(ui, &mut values, &facts);
        self.settings_tab = values.tab;
        // The tablet mode is one choice for every connection, saved with
        // the rest of Settings (0.20.7).
        if values.apply_to(&mut self.settings) {
            self.settings_changed();
        }
        for action in actions {
            match action {
                SettingsAction::Close => self.screen = Screen::Home,
                SettingsAction::FollowDisplaysSetting => {
                    if let Some(id) = self.last_selected.clone() {
                        match self.repository.set_displays_mode(&id, None) {
                            Ok(store) => self.reconcile(store),
                            Err(error) => {
                                self.status = format!(
                                    "The connection still keeps its own Displays choice: {error}"
                                );
                            }
                        }
                    }
                }
                SettingsAction::RefreshDisplays => {
                    self.monitors = super::displays::settings_monitors();
                }
                SettingsAction::ShowLogFolder => {
                    if let Some(logs) = self.services.directories().logs
                        && !crate::logging::show_in_folder(&logs)
                    {
                        self.status =
                            format!("The log folder could not be shown: {}", logs.display());
                    }
                }
                SettingsAction::NativeTabletComponent => {
                    let ctx = ui.ctx().clone();
                    self.native_tablet
                        .clicked(Arc::clone(&self.services), move || ctx.request_repaint());
                }
                SettingsAction::OpenColourCheck | SettingsAction::AllowSystemShortcutPermission => {
                }
                SettingsAction::SupportBundle(action) => {
                    let ctx = ui.ctx().clone();
                    let wake_ctx = ctx.clone();
                    let services = Arc::clone(&self.services);
                    let displays = move || services.displays();
                    self.support_bundle.apply(
                        action,
                        &arcen_deck_core::screens::support_bundle::SupportBundleContext {
                            services: Arc::clone(&self.services),
                            component: crate::platform::support::component(),
                            log_level: self.settings.log_level,
                            displays: &displays,
                            wake: Arc::new(move || wake_ctx.request_repaint()),
                            ctx: &ctx,
                        },
                    );
                }
            }
        }
    }
}

/// Reads the decoder's report once on its own thread (it may run D3D11
/// probe decodes) and keeps it for the Settings screen.
/// When a session's system shortcuts go to the host: its connection's own
/// switch over Settings' (shared `ConnectionOverrides::system_shortcut_capture`).
fn session_keyboard_capture(
    overrides: &ConnectionOverrides,
    settings: &ClientSettings,
) -> arcen_input::keyboard::KeyboardCapturePolicy {
    overrides
        .system_shortcut_capture(
            SystemShortcutCapture::from_settings(settings),
            SystemShortcutRoute::PcKeyboardHook,
        )
        .keyboard_capture
}

fn probe_decode_report(
    services: &Arc<dyn DeckServices>,
) -> Arc<std::sync::OnceLock<arcen_deck_core::decode_report::DecodeReport>> {
    let cell = Arc::new(std::sync::OnceLock::new());
    let filled = Arc::clone(&cell);
    let services = Arc::clone(services);
    let _ = std::thread::Builder::new()
        .name("arcen-decode-report".to_owned())
        .spawn(move || {
            let _ = filled.set(services.decode_report());
        });
    cell
}

/// The window's background on every launcher screen and Settings (a
/// session leaves it clear for the video).
pub(super) fn launcher_root_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new()
        .fill(arcen_deck_core::screens::theme::colours(ui).bg)
        .inner_margin(0.0)
}
