// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One connection of the Windows shell: its `ConnectOptions` from the shared
//! connection plan, the session engine, and core's media composition on the
//! window's composition tree (`crate::media`). What the session asks of the
//! user (certificate, disclaimer, credentials) is read here and drawn by the
//! launcher with the shared screens.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use arcen_deck_core::DeckServices;
use arcen_deck_core::client_telemetry::ClientTelemetry;
use arcen_deck_core::connect_plan::{
    apply_streaming_settings, keep_profile_decodable, profile_video,
};
use arcen_deck_core::connection_draft::ConnectionDraft;
use arcen_deck_core::input::overlay_pointer::OverlayPointerRegions;
use arcen_deck_core::input::{SessionInput, host_input_facts};
use arcen_deck_core::multi_window::activation::{
    MULTI_WINDOW_ENTER_TIMEOUT, MultiWindowSessionState, begin_multi_window_entry,
    teardown_required,
};
use arcen_deck_core::multi_window::runtime::{
    ActiveDisplayInfo, MultiWindowEnterAttempt, MultiWindowEnterPoll, MultiWindowPlan,
    ViewportBindObservation,
};
use arcen_deck_core::multi_window::session::{
    choose_root_monitor, monitor_fps_text, reorder_for_root_monitor,
};
use arcen_deck_core::present::rate::PresentedRate;
use arcen_deck_core::reconnect::ResumeAttempt;
use arcen_deck_core::runtime::multi_monitor::{
    RequestedMultiMonitorSelection, deck_supported_carriers,
};
use arcen_deck_core::runtime::session::{
    AuthSubmission, ConnectOptions, DisconnectReason, SessionAuthentication, SessionCommand,
    SessionEnd, StreamProfile, TerminalDisconnect, spawn_resume_session, spawn_session,
    spawn_session_opt_in,
};
use arcen_deck_core::runtime::tls::{CertInfo, TlsTrustConfig};
use arcen_deck_core::screens::host_pointer::{HostPointerTexture, host_pointer_point};
use arcen_deck_core::settings::pen_negotiation_result;
use arcen_deck_core::settings::{ClientSettings, DisplaysMode};
use arcen_input::cursor::DeckCursor;
use arcen_input::deck_events::{DeckInputEvent, DeckInputSink, LedCursor};
use arcen_input::deck_pen::ImageRect;
use arcen_input::host_pointer::LedPointerMark;
use arcen_input::keyboard::{
    DeckShortcut, KeyboardCapturePolicy, KeyboardSettings, PcKeyboardSettings,
};
use arcen_input::text::{HostTextTruth, OptionKeyPreference, TypingMode};
use arcen_media::SessionMonitorId;
use arcen_protocol::messages::{AuthRequest, DisplayUpdateResultMsg, TabletModeMsg};
use windows::Win32::Foundation::HWND;

use crate::input::WindowInput;
use crate::present::composition::SharedTree;

use super::displays::DisplayPlan;
use crate::media::{MediaSession, MediaStart, VideoSurface};

/// What a pass of the session asks the shell to do.
#[derive(Debug)]
pub(super) enum SessionNews {
    /// Nothing new.
    Quiet,
    /// The host's certificate is not trusted: ask the user.
    CertificateUntrusted(CertInfo),
    /// A host this Deck pinned presented this certificate instead.
    CertificateChanged(CertInfo),
    /// The host asks for credentials (possibly after a disclaimer).
    AuthRequired(AuthRequest),
    /// The host authenticated the attachment (with its resume grant, if
    /// any): the reconnect controller records it before it is accepted.
    Authenticated(SessionAuthentication),
    /// The desktop is up; `login_window` when it is the host's sign-in
    /// screen.
    Streaming { login_window: bool },
    /// The host's desk changed during the session: the note to show, and
    /// whether people at the host may see or use the session.
    DeskPrivacy { note: &'static str, warning: bool },
    /// The session ended, with why.
    Ended(SessionEnd),
}

/// How a connection attaches.
#[derive(Debug)]
pub(super) enum Attach {
    /// A fresh sign-in; with a holder nonce the host is asked for a resume
    /// grant (the Mac's `spawn_session_opt_in`).
    Fresh { holder_nonce: Option<String> },
    /// Resumes a lost session with its grant, by `deadline`.
    Resume {
        attempt: ResumeAttempt,
        deadline: std::time::Instant,
    },
}

/// What the session's input hands back to the Deck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LocalInput {
    /// The input method's composition (`None`: it ended).
    Preedit(Option<String>),
    /// A Deck chord: the shell applies its menu command.
    Shortcut(DeckShortcut),
    /// Esc or Enter in a session window under a Help window: it closes.
    DismissModal,
    /// The keyboard ended pointer lock (Esc held, focus lost).
    PointerLockReleased,
}

/// Receives the events the Deck keeps, on the window's thread.
struct LocalSink {
    news: Arc<Mutex<Vec<LocalInput>>>,
    led: Arc<Mutex<Option<LedCursor>>>,
    ctx: egui::Context,
}

impl DeckInputSink for LocalSink {
    fn input(&self, event: DeckInputEvent) {
        let news = match event {
            DeckInputEvent::Shortcut(shortcut) => LocalInput::Shortcut(shortcut),
            DeckInputEvent::PointerLockReleased { .. } => LocalInput::PointerLockReleased,
            DeckInputEvent::Preedit(text) => LocalInput::Preedit(text),
            DeckInputEvent::DismissModal => LocalInput::DismissModal,
            _ => return,
        };
        self.news
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(news);
        self.ctx.request_repaint();
    }

    fn led_cursor(&self, led: Option<LedCursor>) {
        *self.led.lock().unwrap_or_else(PoisonError::into_inner) = led;
        self.ctx.request_repaint();
    }
}

/// The key of a session window in the shared multi-window engine: the root
/// window is 0, each further window its monitor's session monitor id.
pub(super) type WindowKey = u16;

/// The root window's key.
pub(super) const ROOT_WINDOW: WindowKey = 0;

fn window_key(monitor: SessionMonitorId) -> WindowKey {
    monitor.get()
}

/// A Match My Layout session's windows (the Mac's `MultiWindowSessionState`
/// plus the monitor the root window presents).
pub(super) struct MultiWindow {
    state: MultiWindowSessionState<WindowKey>,
    root_monitor: SessionMonitorId,
    started: std::time::Instant,
    /// The secondary windows whose input this session has.
    inputs: Vec<(HWND, SessionMonitorId)>,
}

/// What one pass of a multi-window entry decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MultiWindowVerdict {
    /// Still opening, or committed and well.
    Steady,
    /// Every window is on its display: input and presentation start.
    Committed,
    /// The layout cannot be shown as negotiated: the session ends.
    Failed(String),
}

/// What the shell observed of one session window this pass.
#[derive(Debug, Clone, Copy)]
pub(super) struct WindowObservation {
    pub key: WindowKey,
    pub observation: ViewportBindObservation,
}

/// A live connection.
pub(super) struct Connection {
    media: MediaSession,
    /// The session's event→wire path (shared with the macOS Deck).
    input: SessionInput,
    local_input: Arc<Mutex<Vec<LocalInput>>>,
    /// The host-led cursor to draw, while the host leads.
    led_cursor: Arc<Mutex<Option<LedCursor>>>,
    /// The cursor the host shows (for the host-led cursor's image).
    shown_cursor: DeckCursor,
    /// The host-led cursor's uploaded image.
    pub(super) pointer_texture: HostPointerTexture,
    /// The window whose input this session has, and where the picture is
    /// in it.
    input_window: Option<(HWND, Option<ImageRect>, (u32, u32))>,
    /// Where the Deck's own UI takes the session windows' input now
    /// (`Launcher::publish_local_ui`), kept for a secondary window attached
    /// later.
    local_ui: OverlayPointerRegions,
    /// The host's input facts, once the input is attached.
    host_facts: Option<arcen_input::deck_events::HostInputFacts>,
    /// What the pen's barrel buttons click on this host (from its hello and
    /// the pen settings), for every window's input.
    pen_buttons: arcen_input::pen_buttons::PenButtonMap,
    streaming: bool,
    /// The host's last `desk_privacy_update` this window was told of.
    desk_update_seen: Option<arcen_deck_core::session_truth::DeskPrivacyStatus>,
    /// Frames confirmed on glass, as each UI pass saw them.
    presented: PresentedRate,
    /// Frames submitted, as each UI pass saw them: the estimate Stream
    /// Statistics shows when DWM confirms nothing (as the Mac's
    /// `presented_frame_times_estimated`).
    submitted: PresentedRate,
    /// When the presenters last took a frame.
    last_frame_at: Option<std::time::Instant>,
    max_fps: u32,
    exact_requested: bool,
    /// Re-reports the display the window moved to, from what this
    /// attachment's `AuthResponse` stated (a resume states the first
    /// connection's), once its host follows (`display_follow`).
    display_follow: arcen_deck_core::display_follow::StreamDisplayFollower,
    /// What this connection asked for (after
    /// `keep_profile_decodable`), for Negotiated Truth.
    requested_video: arcen_media::VideoConfiguration,
    requested_selection: arcen_protocol::messages::VideoSelectionIntent,
    requested_encode_intent: String,
    /// The session's telemetry counters (support bundle snapshot).
    telemetry: Arc<ClientTelemetry>,
    /// Match My Layout's windows, once the host applied a layout.
    multi: Option<MultiWindow>,
    /// The host showed some Deck displays at another size or not at all
    /// (`arcen_deck_core::multi_window::layout_note`).
    layout_note: Option<String>,
    /// The frames each monitor's window presented, as each UI pass saw
    /// them (per-display rates of a multi-window session).
    monitor_presents: Mutex<std::collections::BTreeMap<SessionMonitorId, PresentedRate>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("streaming", &self.streaming)
            .finish_non_exhaustive()
    }
}

/// The options a draft connects with: the shared plan applied to this
/// shell's settings and displays, for the attempt's Displays `mode` and the
/// Match My Layout topology its preflight approved.
#[allow(
    clippy::too_many_arguments,
    reason = "one attempt's inputs, each from a different owner"
)]
pub(super) fn connect_options(
    draft: &ConnectionDraft,
    settings: &ClientSettings,
    mode: DisplaysMode,
    approved: Option<RequestedMultiMonitorSelection>,
    replace_incompatible_desktop: bool,
    services: &Arc<dyn DeckServices>,
    tls: Option<TlsTrustConfig>,
    telemetry: &Arc<ClientTelemetry>,
) -> (ConnectOptions, DisplayPlan) {
    let mut profile = StreamProfile::default();
    apply_streaming_settings(
        &mut profile,
        settings.performance_mode,
        settings.color_fidelity,
    );
    // Grading or HDR this GPU cannot decode is never asked for.
    let _ = keep_profile_decodable(&mut profile, &services.decode_capabilities());
    let mut options = ConnectOptions {
        display_refresh: None,
        services: Arc::clone(services),
        host: draft.host.clone(),
        port: draft.port,
        use_tls: draft.use_tls,
        username: String::new(),
        password: String::new(),
        timeout: Duration::from_secs(30),
        // A pin the command line fixed wins over the security setting.
        tls: draft.tls_trust.clone().or(tls).unwrap_or_default(),
        profile,
        monitors: Vec::new(),
        displays_mode: mode.as_wire().to_string(),
        multi_monitor_topology: None,
        replace_incompatible_desktop,
        timezone: services.timezone(),
        cursor_preference: settings.cursor_preference,
        clipboard_enabled: settings.clipboard_enabled,
        microphone_enabled: settings.microphone_enabled,
        tablet_input_enabled: settings.tablet_input_enabled,
        tablet_mode_requested: settings.tablet_mode_requested,
        telemetry: Arc::clone(telemetry),
        exact_still_image: settings.exact_still_image,
        keep_desk_private: settings.keep_desk_private,
        // From the eight-bit presenter's canvas executor: nothing is
        // advertised until it has one (ADR 0028).
        canvas: arcen_deck_core::canvas::CanvasRequest::for_sink::<crate::media::WindowSink>(
            settings.canvas,
            settings.canvas_cache_mib,
        ),
    };
    let plan = super::displays::apply_display_plan(&mut options, settings, mode, approved);
    // Only the root window's presenter executes the canvas: a secondary
    // window does not register one with the media worker yet, so a
    // multi-window session asks for full frames (ADR 0028 §9).
    if options.multi_monitor_topology.is_some() {
        options.canvas = arcen_deck_core::canvas::CanvasRequest::Unavailable;
    }
    (options, plan)
}

impl Connection {
    /// Starts the session engine and its media on the window's surface.
    pub(super) fn start(
        options: ConnectOptions,
        surface: &VideoSurface,
        ctx: &egui::Context,
        generation: u64,
        attach: Attach,
    ) -> Result<Self, String> {
        let telemetry = Arc::clone(&options.telemetry);
        let max_fps = options.profile.max_fps;
        let exact_requested = options.exact_still_image;
        let display_follow = arcen_deck_core::display_follow::StreamDisplayFollower::new(
            options.display_refresh.as_ref(),
        );
        let requested_video = profile_video(&options.profile)
            .unwrap_or_else(arcen_media::VideoConfiguration::legacy_h264);
        let requested_selection = options.profile.video_selection;
        let requested_encode_intent = options.profile.encode_intent.clone();
        let resuming = matches!(attach, Attach::Resume { .. });
        let handle = match attach {
            Attach::Fresh {
                holder_nonce: Some(nonce),
            } => spawn_session_opt_in(options, nonce),
            Attach::Fresh { holder_nonce: None } => spawn_session(options),
            Attach::Resume { attempt, deadline } => {
                spawn_resume_session(options, attempt, deadline)
            }
        };
        let media = MediaSession::start(
            handle,
            surface,
            Arc::new(super::MediaWake(ctx.clone())),
            MediaStart {
                generation,
                resuming,
                telemetry: Arc::clone(&telemetry),
                audio_player: Box::new(|| {
                    Box::new(crate::audio::output::Player::awaiting_stream_result())
                }),
                requested_video: Some(requested_video),
            },
        )?;
        let local_input = Arc::new(Mutex::new(Vec::new()));
        let led_cursor = Arc::new(Mutex::new(None));
        let input = SessionInput::new(
            media.commands().clone(),
            Arc::new(LocalSink {
                news: Arc::clone(&local_input),
                led: Arc::clone(&led_cursor),
                ctx: ctx.clone(),
            }),
        );
        Ok(Self {
            media,
            input,
            local_input,
            led_cursor,
            shown_cursor: DeckCursor::Default,
            pointer_texture: None,
            input_window: None,
            local_ui: OverlayPointerRegions::default(),
            host_facts: None,
            pen_buttons: arcen_input::pen_buttons::PenButtonMap::default(),
            streaming: false,
            desk_update_seen: None,
            presented: PresentedRate::default(),
            submitted: PresentedRate::default(),
            last_frame_at: None,
            max_fps,
            exact_requested,
            display_follow,
            requested_video,
            requested_selection,
            requested_encode_intent,
            telemetry,
            multi: None,
            layout_note: None,
            monitor_presents: Mutex::default(),
        })
    }

    /// What this connection asked for: the video, its selection and the
    /// encode intent (Negotiated Truth compares the host's answer to it).
    pub(super) fn requested(
        &self,
    ) -> (
        arcen_media::VideoConfiguration,
        arcen_protocol::messages::VideoSelectionIntent,
        &str,
    ) {
        (
            self.requested_video,
            self.requested_selection,
            &self.requested_encode_intent,
        )
    }

    /// One UI pass: presenter outcomes, then what the session asks for.
    pub(super) fn poll(&mut self) -> SessionNews {
        let pass = self.media.poll();
        let now = std::time::Instant::now();
        let confirmed = pass.eight_bit.frames_confirmed + pass.ten_bit.frames_confirmed;
        self.presented.record(now, confirmed);
        let submitted = pass.eight_bit.frames_submitted + pass.ten_bit.frames_submitted;
        self.submitted.record(now, submitted);
        if confirmed > 0 || submitted > 0 {
            self.last_frame_at = Some(now);
        }
        if let Some(root) = self.multi_window_root_monitor() {
            // New frames the root presenter submitted, as each further
            // window counts its own: a frame is submitted in one pass and
            // confirmed in a later one, so counting both would count it
            // twice, and a re-present is no new frame.
            let new_frames =
                pass.eight_bit.new_frames_submitted() + pass.ten_bit.new_frames_submitted();
            self.note_presented(root, new_frames);
        }
        let mut state = self
            .media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(info) = state.certificate_untrusted.take() {
            return SessionNews::CertificateUntrusted(info);
        }
        if let Some(info) = state.certificate_changed.take() {
            return SessionNews::CertificateChanged(info);
        }
        if let Some(request) = state.pending_auth.take() {
            return SessionNews::AuthRequired(request);
        }
        if let Some(authentication) = state.pending_authentication.take() {
            return SessionNews::Authenticated(authentication);
        }
        if state.closed {
            let end = state.end.clone().unwrap_or_else(|| SessionEnd {
                reason: DisconnectReason::Terminal(TerminalDisconnect::Protocol),
                message: state
                    .error
                    .clone()
                    .unwrap_or_else(|| "Disconnected".to_owned()),
                observed_at: std::time::Instant::now(),
            });
            return SessionNews::Ended(end);
        }
        if !self.streaming
            && let Some(hello) = &state.server_hello
        {
            self.streaming = true;
            return SessionNews::Streaming {
                login_window: hello.login_window,
            };
        }
        if self.streaming
            && state.desk_privacy_update.is_some()
            && state.desk_privacy_update != self.desk_update_seen
        {
            self.desk_update_seen = state.desk_privacy_update;
            if let Some((note, warning)) = state
                .desk_privacy_update
                .and_then(|status| status.deck_note().map(|note| (note, status.is_warning())))
            {
                return SessionNews::DeskPrivacy { note, warning };
            }
        }
        SessionNews::Quiet
    }

    /// Sends a session message (a `display_update`) to the host.
    pub(super) fn send_json(&self, value: serde_json::Value) -> bool {
        self.media
            .commands()
            .send(SessionCommand::Json(value))
            .is_ok()
    }

    /// The host's answer to the last `display_update`, once.
    pub(super) fn take_display_update_result(&self) -> Option<DisplayUpdateResultMsg> {
        self.media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .display_update_result
            .take()
    }

    /// Whether the host resizes its display for this session
    /// (`supports_display_update` in its hello).
    pub(super) fn host_supports_display_update(&self) -> bool {
        self.media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .server_hello
            .as_ref()
            .is_some_and(|hello| hello.supports_display_update)
    }

    /// The follower that re-reports the display the window moved to.
    pub(super) fn display_follow(
        &mut self,
    ) -> &mut arcen_deck_core::display_follow::StreamDisplayFollower {
        &mut self.display_follow
    }

    /// Whether the host follows the session window to another display
    /// (`StreamRateMsg::follows_display_moves`), for a single stream.
    pub(super) fn host_follows_display_moves(&self) -> bool {
        self.media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .server_hello
            .as_ref()
            .is_some_and(|hello| {
                arcen_deck_core::display_follow::host_follows_display_moves(
                    hello,
                    hello.multi_monitor_v1().ok().flatten().is_some(),
                )
            })
    }

    /// Whether a frame reached the glass in the last four seconds (the
    /// Mac's fresh-frame gate for display updates).
    pub(super) fn frame_fresh(&self) -> bool {
        self.last_frame_at
            .is_some_and(|at| at.elapsed() < Duration::from_secs(4))
    }

    /// Lets the session continue after the host authenticated it.
    pub(super) fn accept_authentication(&self) -> bool {
        self.media
            .commands()
            .send(SessionCommand::AcceptAuthentication)
            .is_ok()
    }

    /// Releases the windows' input (the connection is lost; its last
    /// picture stays under the reconnect overlay until it is dropped).
    pub(super) fn release_input(&mut self) {
        if let Some((hwnd, ..)) = self.input_window.take() {
            crate::input::with_input(hwnd, |input| input.set_session(None));
            crate::input::detach(hwnd);
        }
        if let Some(multi) = self.multi.as_mut() {
            for (hwnd, _) in multi.inputs.drain(..) {
                crate::input::with_input(hwnd, |input| input.set_session(None));
                crate::input::detach(hwnd);
            }
        }
    }

    /// Turns the hello's applied multi-monitor layout, if any, into a
    /// multi-window entry (the Mac's `begin_multi_window_if_applicable`):
    /// validated, with region input required, the root window on the
    /// highest-refresh display, the per-monitor decode armed and the region
    /// input set. `requested` is the layout this attempt asked for.
    ///
    /// # Errors
    ///
    /// The host applied a layout this Deck cannot present as negotiated:
    /// the session must end (never a silent fall back to one display).
    pub(super) fn begin_multi_window(
        &mut self,
        requested: Option<&arcen_media::RequestedMonitorTopology>,
    ) -> Result<(), String> {
        if self.multi.is_some() {
            return Ok(());
        }
        let hello = {
            let state = self
                .media
                .shared()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(hello) = state.server_hello.clone() else {
                return Ok(());
            };
            hello
        };
        let entry =
            begin_multi_window_entry(&hello, deck_supported_carriers(), ROOT_WINDOW, window_key)
                .map_err(|error| format!("invalid multi-monitor topology ({error})"))?;
        let Some((validated, _negotiated_plan)) = entry else {
            return Ok(());
        };
        self.layout_note =
            arcen_deck_core::multi_window::layout_note::hello_layout_note(requested, &hello);
        if let Some(note) = self.layout_note.as_ref() {
            tracing::info!(target: "arcen::display", note = %note, "host kept its own layout");
        }
        if !arcen_protocol::messages::supports_region_input_v1(
            hello.input_protocol_version,
            hello.input_capabilities,
        ) {
            return Err("Match My Layout requires direct region input".to_owned());
        }
        let displays = crate::display::enumerate();
        let refresh: std::collections::BTreeMap<u32, u32> = displays
            .iter()
            .map(|display| (display.id, display.refresh_hz))
            .collect();
        let root_monitor = choose_root_monitor(&validated, &refresh)
            .or_else(|| validated.primary_monitor_id())
            .ok_or_else(|| "the applied layout has no monitor".to_owned())?;
        let validated = reorder_for_root_monitor(validated, root_monitor);
        let plan = MultiWindowPlan::build(
            &validated.monitor_ids(),
            &validated.display_ids(),
            ROOT_WINDOW,
            window_key,
        )
        .map_err(|error| format!("invalid root-reordered multi-monitor plan ({error})"))?;
        self.input
            .set_region_topology(&validated, requested, true)
            .map_err(|error| format!("invalid region topology ({error})"))?;
        self.media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .arm_multi_monitor_decode(&validated, Some(root_monitor), &displays);
        tracing::info!(
            target: "arcen::display",
            generation = validated.generation.get(),
            monitors = validated.monitors.len(),
            root_monitor_id = root_monitor.get(),
            root_display_id = plan.primary_display_id(),
            "beginning production multi-window entry",
        );
        self.multi = Some(MultiWindow {
            state: MultiWindowSessionState::Active {
                validated,
                attempt: MultiWindowEnterAttempt::new(plan, Duration::ZERO),
                committed: false,
            },
            root_monitor,
            started: std::time::Instant::now(),
            inputs: Vec::new(),
        });
        Ok(())
    }

    /// The windows a multi-window session needs: the root window's display
    /// and each further monitor with its display. `None` outside one.
    pub(super) fn multi_window_plan(&self) -> Option<(u32, Vec<(SessionMonitorId, u32)>)> {
        let MultiWindowSessionState::Active { attempt, .. } = &self.multi.as_ref()?.state else {
            return None;
        };
        let plan = attempt.plan();
        Some((
            plan.primary_display_id(),
            plan.assignments()
                .iter()
                .map(|assignment| (assignment.session_monitor_id, assignment.display_id))
                .collect(),
        ))
    }

    /// The monitor the root window presents in a multi-window session.
    pub(super) fn multi_window_root_monitor(&self) -> Option<SessionMonitorId> {
        self.multi.as_ref().map(|multi| multi.root_monitor)
    }

    /// Whether every window of a multi-window session is confirmed.
    pub(super) fn multi_window_committed(&self) -> bool {
        self.multi
            .as_ref()
            .is_some_and(|multi| multi.state.is_committed())
    }

    /// One pass of the multi-window entry, from what the shell observed of
    /// each window and the displays attached now (the Mac's
    /// `drive_multi_window` decisions): confirmed on the exact displays,
    /// timed out, aborted, or torn down after commit (a display gone, a
    /// window closed).
    pub(super) fn observe_multi_window(
        &mut self,
        observations: &[WindowObservation],
        live: &[ActiveDisplayInfo],
        any_close_requested: bool,
    ) -> MultiWindowVerdict {
        let Some(multi) = self.multi.as_mut() else {
            return MultiWindowVerdict::Steady;
        };
        let MultiWindowSessionState::Active {
            validated,
            attempt,
            committed,
        } = &mut multi.state
        else {
            return MultiWindowVerdict::Steady;
        };
        for window in observations {
            attempt.record_observation(window.key, window.observation, live);
        }
        if *committed {
            let live_ids: Vec<u32> = live.iter().map(|display| display.display_id).collect();
            if let Some(reason) = attempt.abort_reason() {
                return MultiWindowVerdict::Failed(reason.to_string());
            }
            if teardown_required(validated, &live_ids, any_close_requested) {
                return MultiWindowVerdict::Failed(
                    "a display of the layout or one of its windows went away".to_owned(),
                );
            }
            return MultiWindowVerdict::Steady;
        }
        match attempt.poll(multi.started.elapsed(), MULTI_WINDOW_ENTER_TIMEOUT) {
            MultiWindowEnterPoll::StillWaiting => MultiWindowVerdict::Steady,
            MultiWindowEnterPoll::Confirmed => {
                *committed = true;
                tracing::info!(
                    target: "arcen::display",
                    monitors = validated.monitors.len(),
                    "multi-window entry committed",
                );
                MultiWindowVerdict::Committed
            }
            MultiWindowEnterPoll::TimedOut { unconfirmed } => MultiWindowVerdict::Failed(format!(
                "the windows of {unconfirmed:?} were not confirmed on their displays in time"
            )),
            MultiWindowEnterPoll::Aborted(reason) => {
                MultiWindowVerdict::Failed(format!("multi-window entry aborted: {reason}"))
            }
        }
    }

    /// The secondary monitors' newest decoded frames, each ready for its own
    /// window's presenter, once the layout is committed (nothing is shown
    /// before every window is on its display, as on the Mac).
    pub(super) fn take_secondary_frames(
        &self,
    ) -> Vec<(
        SessionMonitorId,
        arcen_deck_core::present::PresenterPipeline,
        arcen_deck_core::present::PresentableFrame<crate::decode::D3d11Picture>,
    )> {
        if !self.multi_window_committed() {
            return Vec::new();
        }
        let mut state = self
            .media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let frames = std::mem::take(&mut state.secondary_frames);
        frames
            .into_iter()
            .filter_map(|(monitor, frame)| {
                let (pipeline, presentable) = state.presentable_secondary_frame(&frame)?;
                Some((monitor, pipeline, presentable))
            })
            .collect()
    }

    /// Counts new frames a secondary window's presenters submitted, and
    /// those they replaced before a present: the per-display statistics
    /// and the session's presented frames (the decoded count covers every
    /// monitor).
    pub(super) fn note_secondary_presented(
        &self,
        monitor: SessionMonitorId,
        frames: u64,
        superseded: u64,
    ) {
        self.media.record_secondary_presented(frames, superseded);
        self.note_presented(monitor, frames);
    }

    /// One window's new frames for `monitor`, once: the shared per-monitor
    /// count the media worker logs and the local history Stream
    /// Statistics shows.
    fn note_presented(&self, monitor: SessionMonitorId, frames: u64) {
        if frames > 0 {
            self.media
                .shared()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .note_monitor_presented(monitor, frames);
        }
        let now = std::time::Instant::now();
        let mut presents = self
            .monitor_presents
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        presents.entry(monitor).or_default().record(now, frames);
    }

    /// The per-display frame rates of a committed multi-window session
    /// (the shared text), or `None` outside one.
    fn monitor_fps_text(&self) -> Option<String> {
        let multi = self.multi.as_ref()?;
        let MultiWindowSessionState::Active {
            validated,
            committed: true,
            ..
        } = &multi.state
        else {
            return None;
        };
        let now = std::time::Instant::now();
        let presents = self
            .monitor_presents
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let text = monitor_fps_text(validated, |monitor| {
            presents.get(&monitor).map(|rate| rate.rate(now))
        });
        (!text.is_empty()).then_some(text)
    }

    /// Gives the desktop a secondary window's keyboard, mouse and pen, for
    /// the monitor it presents (region input through the shared runtime).
    pub(super) fn attach_secondary_input(
        &mut self,
        hwnd: HWND,
        monitor: SessionMonitorId,
        keyboard: KeyboardSettings,
        option_keys: OptionKeyPreference,
        grab: KeyboardCapturePolicy,
    ) -> Result<(), String> {
        let Some(facts) = self.host_facts else {
            return Err("the session's input is not attached".to_owned());
        };
        let Some(multi) = self.multi.as_mut() else {
            return Ok(());
        };
        if multi.inputs.iter().any(|(attached, _)| *attached == hwnd) {
            return Ok(());
        }
        crate::input::attach(
            hwnd,
            WindowInput::new(hwnd, self.input.sink(Some(monitor)), grab),
        )
        .map_err(|error| format!("input: {error}"))?;
        let keys = PcKeyboardSettings::from_settings(&keyboard, option_keys);
        let pen_buttons = self.pen_buttons;
        crate::input::with_input(hwnd, |input| {
            input.set_session(Some((facts, keys)));
            input.set_pen_buttons(pen_buttons);
            input.set_fullscreen(true);
            input.set_host_cursor_shown(Some(self.shown_cursor.clone()), false);
        });
        // Opened while a Help window is: held by it from the start.
        publish_local_ui(None, [hwnd], self.local_ui);
        multi.inputs.push((hwnd, monitor));
        Ok(())
    }

    /// Keeps a secondary window's input on its picture (the shown fit of
    /// its own composition tree), and tells the session where the window
    /// is, for a drag that crosses into it.
    pub(super) fn sync_secondary_input(
        &self,
        hwnd: HWND,
        monitor: SessionMonitorId,
        tree: &SharedTree,
    ) {
        let Some((fit, source)) = tree.lock().ok().and_then(|tree| tree.shown_fit()) else {
            return;
        };
        let rect = ImageRect {
            left: f64::from(fit.offset_x),
            top: f64::from(fit.offset_y),
            width: f64::from(fit.scale) * f64::from(source.0),
            height: f64::from(fit.scale) * f64::from(source.1),
        };
        if let Some(placed) = crate::input::with_input(hwnd, |input| {
            input.set_cursor_scale(fit.scale);
            input.set_image_rect(Some(rect));
            input.screen_placement()
        }) {
            self.input.set_picture_surface(monitor, placed);
        }
    }

    /// Sends credentials (or a disclaimer's acceptance with them).
    pub(super) fn submit(&self, submission: AuthSubmission) -> bool {
        self.media
            .commands()
            .send(SessionCommand::SubmitAuth(submission))
            .is_ok()
    }

    /// Declines the host's disclaimer.
    pub(super) fn decline_disclaimer(&self) {
        let _ = self
            .media
            .commands()
            .send(SessionCommand::DeclineDisclaimer);
    }

    /// The session's telemetry counters.
    pub(super) fn telemetry(&self) -> &ClientTelemetry {
        &self.telemetry
    }

    /// The media state for Stream Statistics and the overlays.
    pub(super) const fn media(&self) -> &MediaSession {
        &self.media
    }

    /// Stream Statistics' lines, from the same shared inputs as the Mac's,
    /// plus the decoder's refusal when it refused the stream.
    ///
    /// `displays_note` is the launcher's word on this session's displays
    /// (shared `SessionDisplaysOrigin::session_note`), shown beside the
    /// host's own layout line.
    pub(super) fn stream_stats_lines(&self, displays_note: Option<&str>) -> Vec<String> {
        // Confirmed presents, else submitted ones, as the Mac does: frames
        // over the real time they were seen in, each counted once.
        let now = std::time::Instant::now();
        let confirmed = self.presented.rate(now);
        let presented_fps = if confirmed > 0.0 {
            confirmed
        } else {
            self.submitted.rate(now)
        };
        let state = self
            .media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let axes = state
            .server_hello
            .as_ref()
            .map(arcen_deck_core::session_truth::ActiveContract::from_hello);
        // As on the Mac: the host's offer from its hello, the hub's verdict
        // for the primary monitor.
        let refine = arcen_deck_core::screens::statistics::exact_status_line(
            self.exact_requested,
            state
                .server_hello
                .as_ref()
                .and_then(|hello| hello.lossless_refine.as_ref()),
            || {
                state.refine_hub.as_ref().map_or_else(String::new, |hub| {
                    hub.status(arcen_media::refine::PRIMARY_MONITOR).text
                })
            },
        );
        let mut lines = arcen_media::stats_overlay::stats_overlay_lines(
            &arcen_media::stats_overlay::StatsOverlayInputs {
                path: state
                    .host_health
                    .as_ref()
                    .and_then(|stats| stats.path.as_ref()),
                presented_fps,
                max_fps: arcen_deck_core::screens::statistics::served_max_fps(
                    state.server_hello.as_ref(),
                    self.max_fps,
                ),
                axes: axes.as_ref(),
                // As on the Mac: the served-pipeline update, else the
                // hello's active pipeline.
                pipeline: state
                    .served_pipeline
                    .as_ref()
                    .map(|served| served.served.token())
                    .or_else(|| {
                        state
                            .server_hello
                            .as_ref()
                            .and_then(|hello| hello.active_pipeline.as_ref())
                            .map(arcen_protocol::messages::ServedStreamPipeline::token)
                    })
                    .map(arcen_deck_core::screens::statistics::stream_pipeline_label),
                refine: Some(refine.as_str()),
                // The served-pipeline update's backend, else the hello's.
                encoder: state
                    .served_pipeline
                    .as_ref()
                    .and_then(|served| served.encoder.as_deref())
                    .or_else(|| {
                        state
                            .server_hello
                            .as_ref()
                            .map(|hello| hello.encoder_backend.as_str())
                    }),
            },
        )
        .to_vec();
        if let Some(refusal) = &state.decoder_refusal {
            lines.push(format!("Decoder: {refusal}"));
        }
        lines.extend(state.decoder_behind.line());
        lines.extend(state.canvas_status_line());
        lines.push(self.media.decode_device_line().to_owned());
        let host_displays = state
            .host_health
            .as_ref()
            .and_then(|stats| arcen_media::stats_overlay::host_display_rates_line(&stats.monitors));
        let display_note = state.server_hello.as_ref().and_then(|hello| {
            arcen_deck_core::display_note::display_negotiation_note(&hello.device_capabilities)
        });
        let desk_note = state.server_hello.as_ref().and_then(|hello| {
            arcen_deck_core::session_truth::desk_privacy_note_now(hello, state.desk_privacy_update)
        });
        drop(state);
        if let Some(text) = self.monitor_fps_text() {
            lines.push(format!("Displays fps: {text}"));
        }
        lines.extend(self.layout_note.clone());
        lines.extend(displays_note.map(str::to_owned));
        lines.extend(display_note);
        lines.extend(desk_note.map(str::to_owned));
        lines.extend(host_displays);
        lines
    }

    /// The host-layout line ([`Self::begin_multi_window`]), if any.
    pub(super) fn layout_note(&self) -> Option<&str> {
        self.layout_note.as_deref()
    }

    /// What the host did with the requested display size and UI scale,
    /// when it did not do exactly that (shared
    /// `arcen_deck_core::display_note`, the Mac's words).
    pub(super) fn display_note(&self) -> Option<String> {
        let state = self
            .media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.server_hello.as_ref().and_then(|hello| {
            arcen_deck_core::display_note::display_negotiation_note(&hello.device_capabilities)
        })
    }

    /// What the desk at the host shows while attached (shared
    /// `arcen_deck_core::session_truth::desk_privacy_note`, the Mac's words).
    pub(super) fn desk_note(&self) -> Option<&'static str> {
        let state = self
            .media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.server_hello.as_ref().and_then(|hello| {
            arcen_deck_core::session_truth::desk_privacy_note_now(hello, state.desk_privacy_update)
        })
    }

    /// Minimised or occluded: no presents.
    pub(super) fn set_visible(&self, visible: bool) {
        self.media.set_visible(visible);
    }

    /// Gives the desktop `hwnd`'s keyboard, mouse and pen once it streams:
    /// the host's facts from its hello, this connection's keyboard
    /// settings, the typed pen as negotiated.
    pub(super) fn attach_input(
        &mut self,
        hwnd: HWND,
        keyboard: KeyboardSettings,
        option_keys: OptionKeyPreference,
        (tablet_mode, pen_buttons): (TabletModeMsg, arcen_input::pen_buttons::PenButtonSettings),
        grab: KeyboardCapturePolicy,
    ) -> Result<(), String> {
        if self.input_window.is_some() {
            return Ok(());
        }
        let hello = self
            .media
            .shared()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .server_hello
            .clone()
            .ok_or_else(|| "the host's hello has not arrived".to_owned())?;
        let source = (hello.screen_width, hello.screen_height);
        // A multi-window session's input runs on its region topology,
        // which `begin_multi_window` set; one window has the legacy frame.
        let root_monitor = self.multi_window_root_monitor();
        if root_monitor.is_none() {
            self.input
                .set_legacy_frame(source.0, source.1)
                .map_err(|error| error.to_string())?;
        }
        self.input.set_typed_pen(pen_negotiation_result(
            tablet_mode,
            hello.input_protocol_version,
            hello.input_capabilities.pen,
        ));
        // A touchpad pinch goes where the host takes gestures (the Mac rule).
        self.input
            .set_gestures(arcen_protocol::messages::supports_gestures_v1(
                hello.input_protocol_version,
                hello.input_capabilities,
            ));
        crate::input::attach(
            hwnd,
            WindowInput::new(hwnd, self.input.sink(root_monitor), grab),
        )
        .map_err(|error| format!("input: {error}"))?;
        let facts = host_input_facts(&hello);
        let keys = PcKeyboardSettings::from_settings(&keyboard, option_keys);
        let pen_buttons = arcen_deck_core::input::pen_button_map(&hello, pen_buttons);
        tracing::info!(
            target: arcen_deck_core::log_target::INPUT,
            host = ?pen_buttons.host,
            barrel = pen_buttons.settings.barrel.token(),
            "pen buttons"
        );
        self.pen_buttons = pen_buttons;
        crate::input::with_input(hwnd, |input| {
            input.set_session(Some((facts, keys)));
            input.set_pen_buttons(pen_buttons);
        });
        self.input_window = Some((hwnd, None, source));
        self.host_facts = Some(facts);
        Ok(())
    }

    /// Whether the host types text sent from the Deck (`None`: not known
    /// yet).
    pub(super) fn host_types_text(&self) -> Option<bool> {
        self.host_facts.map(|facts| facts.text_commit)
    }

    /// The typing mode in force for this session's keyboard.
    pub(super) fn set_typing(&self, typing: TypingMode) {
        let Some(facts) = self.host_facts else {
            return;
        };
        let _ = self.with_window_input(|input| {
            input.set_typing(
                typing,
                HostTextTruth {
                    text_commit: facts.text_commit,
                    unicode: facts.unicode_text,
                },
            );
        });
    }

    /// Keeps the window's input on the picture: its rectangle and scale
    /// from the video's fit, and the host's cursor image and position from
    /// the session's cursor lane. `bridged_pen`: the Native tablet carries
    /// the pen.
    pub(super) fn sync_window_input(&mut self, tree: &SharedTree, bridged_pen: bool) {
        let Some((hwnd, current, source)) = self.input_window.as_mut() else {
            return;
        };
        let hwnd = *hwnd;
        let (cursor, pointer, keymap) = {
            let mut state = self
                .media
                .shared()
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (
                state.pending_host_cursor.take(),
                state.pending_host_pointer.take(),
                state.host_keymap.take(),
            )
        };
        if let Some(keymap) = keymap
            && crate::input::with_input(hwnd, |input| input.set_host_keymap(&keymap)).is_none()
        {
            // The input was busy with a message: try again next pass.
            self.media
                .shared()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .host_keymap
                .get_or_insert(keymap);
        }
        if let Some(cursor) = &cursor {
            self.shown_cursor = cursor.clone();
        }
        if let (Some(cursor), Some(multi)) = (&cursor, &self.multi) {
            for (secondary, _) in &multi.inputs {
                crate::input::with_input(*secondary, |input| {
                    input.set_host_cursor_shown(Some(cursor.clone()), bridged_pen);
                });
            }
        }
        if cursor.is_some() || pointer.is_some() {
            crate::input::with_input(hwnd, |input| {
                if let Some(cursor) = cursor {
                    input.set_host_cursor_shown(Some(cursor), bridged_pen);
                }
                if let Some(position) = pointer {
                    input.host_pointer_position(position);
                }
            });
        }
        // Geometry only from the picture a presenter shows now, in that
        // picture's own size (a host may change resolution mid-session).
        let Some((fit, shown_source)) = tree.lock().ok().and_then(|tree| tree.shown_fit()) else {
            return;
        };
        if shown_source != *source
            && (self.multi.is_some()
                || self
                    .input
                    .set_legacy_frame(shown_source.0, shown_source.1)
                    .is_ok())
        {
            *source = shown_source;
        }
        let rect = ImageRect {
            left: f64::from(fit.offset_x),
            top: f64::from(fit.offset_y),
            width: f64::from(fit.scale) * f64::from(source.0),
            height: f64::from(fit.scale) * f64::from(source.1),
        };
        let changed = *current != Some(rect);
        *current = Some(rect);
        let placed = crate::input::with_input(hwnd, |input| {
            input.set_cursor_scale(fit.scale);
            if changed {
                input.set_image_rect(Some(rect));
            }
            input.screen_placement()
        });
        // A multi-window session's drag follows the pointer into whichever
        // window it is over: the session knows where the root one is.
        if let (Some(root), Some(placed)) = (self.multi_window_root_monitor(), placed) {
            self.input.set_picture_surface(root, placed);
        }
    }

    /// The host-led cursor to draw now: its mark, where on the picture (in
    /// `pixels_per_point` egui points), the cursor the host shows, and the
    /// stream's width. `None` while the Deck's own pointer leads.
    pub(super) fn led_cursor(
        &self,
        pixels_per_point: f32,
    ) -> Option<(LedPointerMark, egui::Rect, egui::Pos2, &DeckCursor, usize)> {
        let led = (*self
            .led_cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner))?;
        let (_, Some(image), source) = self.input_window? else {
            return None;
        };
        #[allow(clippy::cast_possible_truncation, reason = "window geometry")]
        let rect = egui::Rect::from_min_size(
            egui::pos2(image.left as f32, image.top as f32),
            egui::vec2(image.width as f32, image.height as f32),
        ) / pixels_per_point;
        let at = host_pointer_point(rect, led.position.fraction()?)?;
        Some((
            led.mark,
            rect,
            at,
            &self.shown_cursor,
            usize::try_from(source.0).ok()?,
        ))
    }

    /// Applies `apply` to the window's input, if this session has it.
    /// The keyboard switch's chord, for the session window and every
    /// secondary window.
    pub(super) fn set_switch_chord(&self, switch: arcen_input::keyboard::KeyboardSwitchChord) {
        let _ = self.with_window_input(|input| input.set_switch_chord(switch));
        if let Some(multi) = &self.multi {
            for (secondary, _) in &multi.inputs {
                crate::input::with_input(*secondary, |input| input.set_switch_chord(switch));
            }
        }
    }

    /// Where the Deck's own UI takes the session windows' input
    /// (`arcen_deck_core::input::overlay_pointer`): the connection bar in
    /// the root window only, a modal Help window in every one.
    pub(super) fn set_local_ui(&mut self, regions: OverlayPointerRegions) {
        self.local_ui = regions;
        let secondaries = self
            .multi
            .iter()
            .flat_map(|multi| multi.inputs.iter().map(|(hwnd, _)| *hwnd));
        publish_local_ui(
            self.input_window.map(|(hwnd, ..)| hwnd),
            secondaries,
            regions,
        );
    }

    /// Whether any session window's input holds a key it took for a Deck
    /// chord (Alt+F4 held in a secondary window as well as the root one).
    pub(super) fn holds_deck_key(&self, qt_key: u32) -> bool {
        self.with_window_input(|input| input.holds_deck_key(qt_key)) == Some(true)
            || self.multi.as_ref().is_some_and(|multi| {
                multi.inputs.iter().any(|(hwnd, _)| {
                    crate::input::with_input(*hwnd, |input| input.holds_deck_key(qt_key))
                        == Some(true)
                })
            })
    }

    pub(super) fn with_window_input<T>(
        &self,
        apply: impl FnOnce(&mut WindowInput) -> T,
    ) -> Option<T> {
        let (hwnd, ..) = self.input_window?;
        crate::input::with_input(hwnd, apply)
    }

    /// What the input handed back since the last call.
    pub(super) fn take_local_input(&self) -> Vec<LocalInput> {
        std::mem::take(
            &mut *self
                .local_input
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Ends the session; dropping the connection discards queued frames and
    /// clears the video visual.
    pub(super) fn close(self) {
        self.media.close();
    }
}

impl Drop for Connection {
    /// The window's input outlives no session: what it held is released
    /// and its window procedure restored.
    fn drop(&mut self) {
        self.release_input();
    }
}

/// Publishes the Deck UI's regions to a session's windows: all of them to
/// the root window (`primary`), what a secondary window shares
/// (`OverlayPointerRegions::for_secondary`) to each secondary one.
fn publish_local_ui(
    primary: Option<HWND>,
    secondaries: impl IntoIterator<Item = HWND>,
    regions: OverlayPointerRegions,
) {
    if let Some(hwnd) = primary {
        crate::input::with_input(hwnd, |input| input.set_overlay_pointer(regions));
    }
    for hwnd in secondaries {
        crate::input::with_input(hwnd, |input| {
            input.set_overlay_pointer(regions.for_secondary());
        });
    }
}

#[cfg(test)]
mod local_ui_tests {
    use std::sync::{Arc, Mutex};

    use arcen_deck_core::input::overlay_pointer::OverlayPointerRegions;
    use arcen_input::deck_pen::ImageRect;
    use arcen_input::keyboard::{KeyboardCapturePolicy, PcKeyboardSettings};
    use arcen_protocol::messages::HostPlatform;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
        WS_POPUP,
    };
    use windows::core::w;

    use super::publish_local_ui;
    use crate::input::{HostInputFacts, InputEvent, InputSink, WindowInput, attach, with_input};

    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<InputEvent>>>);

    impl InputSink for Sent {
        fn input(&self, event: InputEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// A hidden session window with its input attached, as the root window
    /// and each Match My Layout secondary window have one.
    fn session_window(sent: &Sent) -> HWND {
        // SAFETY: a plain hidden popup of the system's static class, on this
        // thread, destroyed by the test.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("local ui test"),
                WS_POPUP,
                0,
                0,
                400,
                300,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a test window");
        let mut input = WindowInput::new(hwnd, Box::new(sent.clone()), KeyboardCapturePolicy::Host);
        let facts = HostInputFacts {
            platform: HostPlatform::Windows,
            extended_keys: true,
            precise_scroll: true,
            text_commit: true,
            unicode_text: true,
        };
        input.set_session(Some((facts, PcKeyboardSettings::default())));
        input.set_image_rect(Some(ImageRect {
            left: 0.0,
            top: 0.0,
            width: 400.0,
            height: 300.0,
        }));
        attach(hwnd, input).expect("input attached");
        hwnd
    }

    /// Whether a click at (200, 20) and the letter A reach the host.
    fn host_gets(hwnd: HWND, sent: &Sent) -> (bool, bool) {
        let _ = std::mem::take(&mut *sent.0.lock().unwrap());
        let click = with_input(hwnd, |input| {
            input.handle(WM_LBUTTONDOWN, WPARAM(1), LPARAM((20 << 16) | 200))
        })
        .flatten()
        .is_some();
        let clicked = std::mem::take(&mut *sent.0.lock().unwrap())
            .iter()
            .any(|event| matches!(event, InputEvent::Button { pressed: true, .. }));
        let _ = with_input(hwnd, |input| {
            input.handle(WM_KEYDOWN, WPARAM(0x41), LPARAM(0x001E_0001))
        });
        let typed = std::mem::take(&mut *sent.0.lock().unwrap())
            .iter()
            .any(|event| matches!(event, InputEvent::Keys(_)));
        let _ = with_input(hwnd, |input| input.release_modifiers());
        (click && clicked, typed)
    }

    #[test]
    fn a_help_window_holds_every_session_window_and_the_bar_only_the_root() {
        let (root_sent, secondary_sent, late_sent) =
            (Sent::default(), Sent::default(), Sent::default());
        let root = session_window(&root_sent);
        let secondary = session_window(&secondary_sent);
        // The fullscreen bar's menu is open over the root window: the root's
        // pointer is the UI's, a secondary window's click still the host's.
        let bar_menu = OverlayPointerRegions {
            owned: Some(ImageRect {
                left: 0.0,
                top: 0.0,
                width: 400.0,
                height: 30.0,
            }),
            whole_window: true,
            ..OverlayPointerRegions::default()
        };
        publish_local_ui(Some(root), [secondary], bar_menu);
        assert!(!host_gets(root, &root_sent).0);
        assert!(host_gets(secondary, &secondary_sent).0);
        // Help ▸ About is open: no session window's click or key reaches
        // the host, nor one of a secondary window opened meanwhile.
        let help = OverlayPointerRegions {
            modal: true,
            ..OverlayPointerRegions::default()
        };
        publish_local_ui(Some(root), [secondary], help);
        let late = session_window(&late_sent);
        publish_local_ui(None, [late], help);
        for (hwnd, sent) in [
            (root, &root_sent),
            (secondary, &secondary_sent),
            (late, &late_sent),
        ] {
            assert_eq!(host_gets(hwnd, sent), (false, false));
        }
        // Esc in a secondary window (the one with the keyboard) closes Help.
        let _ = std::mem::take(&mut *secondary_sent.0.lock().unwrap());
        let _ = with_input(secondary, |input| {
            input.handle(WM_KEYDOWN, WPARAM(0x1B), LPARAM(0x0001_0001))
        });
        assert_eq!(
            std::mem::take(&mut *secondary_sent.0.lock().unwrap()),
            vec![InputEvent::DismissModal]
        );
        let _ = with_input(secondary, |input| {
            input.handle(WM_KEYUP, WPARAM(0x1B), LPARAM(0xC001_0001_u32 as isize))
        });
        // Closed: the host has them all again.
        publish_local_ui(
            Some(root),
            [secondary, late],
            OverlayPointerRegions::default(),
        );
        for (hwnd, sent) in [
            (root, &root_sent),
            (secondary, &secondary_sent),
            (late, &late_sent),
        ] {
            assert!(host_gets(hwnd, sent).0);
        }
        for hwnd in [root, secondary, late] {
            // SAFETY: the windows created above, on this thread (their
            // input detaches on WM_NCDESTROY).
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }
}
