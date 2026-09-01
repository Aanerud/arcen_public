//! Media worker — owns the streaming hot path on its own OS thread.
//!
//! The session's receive/decode/audio pipeline must NOT live on the egui UI
//! thread: macOS throttles repaints for unfocused windows and pauses them
//! entirely when the display sleeps, and any UI hitch backs the stream up
//! (soak testing showed wire age climbing to 11 s once the display dimmed).
//! The worker consumes lightweight SessionEvents continuously and drains the
//! bounded media inbox on `MediaReady`. It feeds audio immediately, requests
//! recovery after an ingress drop, and publishes the latest frame plus
//! telemetry through a mutex the UI reads at its own pace.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::observability::ClientTelemetry;
use crate::pipeline::audio::PcmAudioPlayer;
use crate::pipeline::frame_queue::{
    IncomingMediaBatch, IncomingMediaReceiver, IncomingMediaTelemetry,
};
use crate::pipeline::monitor_router::{MonitorFrameRouter, MonitorRoute, RouteOutcome};
use crate::pipeline::video_decoder::{DecodedVideoFrame, NativeVideoDecoder, SessionColor};
use crate::protocol::messages::{
    msg_type, AudioStreamResultMsg, AuthRequest, CursorModeResultMsg, CursorShapeMsg,
    DisplayUpdateResultMsg, HealthPongMsg, HealthStatsMsg, ServerHelloMsg, TabletModeResultMsg,
    AUDIO_STREAM_RESULT, CURSOR_MODE_RESULT, CURSOR_SHAPE, DISPLAY_UPDATE_RESULT, HEALTH_PONG,
    HEALTH_STATS, TABLET_MODE_RESULT,
};
use crate::protocol::VideoHeader;
use crate::transport::tls::CertInfo;
use crate::transport::websocket::{
    FullFrameRequestGate, SessionAuthentication, SessionCommandSender, SessionEnd, SessionEvent,
};
use crate::ui::session_truth::ActiveContract;
use arcen_media::{ColorPrimaries, TransferCharacteristics};

const TELEMETRY_WINDOW: Duration = Duration::from_secs(2);
/// How often the worker emits an INFO "stream healthy" heartbeat while frames
/// are flowing. At INFO this is the sysadmin's "OK working" signal; DEBUG adds
/// the per-event drop/keyframe/decode detail around it.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// One negotiated monitor's own media counters.
///
/// Every counter the legacy single-decoder path keeps session-wide is
/// duplicated here *per monitor*, because a multi-monitor session's
/// aggregates cannot attribute a fault to the viewport that actually
/// suffered it. The 2026-08-11 pier-windows.example.internal field test is the exact case:
/// the host encoded a steady 60 fps per monitor in under 0.5 ms while the
/// client presented ~29 fps, and nothing in the client's own telemetry
/// could say *which* of the two viewports was starved -- every counter was
/// a session-wide sum.
///
/// Kept as plain counters (never a rate) so the heartbeat can derive
/// windowed rates itself without this type owning a clock.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MonitorMediaCounters {
    /// Packets admitted to this monitor's slot, including those skipped
    /// while it waits for its own keyframe.
    pub frames_received: u64,
    /// Packets that produced a genuinely fresh decoded frame.
    pub frames_decoded: u64,
    /// Packets this monitor's admission fence or decoder rejected.
    pub frames_rejected: u64,
    /// Fresh frames that replaced a still-unpresented frame for this
    /// monitor -- decode kept up but presentation did not.
    pub presentation_superseded: u64,
    /// Whether this monitor's own slot is currently waiting for a keyframe.
    pub waiting_for_keyframe: bool,
    /// Most recent route+decode duration for this monitor.
    pub last_decode_ms: f64,
    /// Sum of every route+decode duration, for a mean without retaining
    /// per-frame samples.
    pub decode_ms_total: f64,
}

impl MonitorMediaCounters {
    /// Mean route+decode milliseconds across every fresh frame this monitor
    /// has decoded, or `0.0` before its first.
    #[must_use]
    pub fn average_decode_ms(&self) -> f64 {
        if self.frames_decoded == 0 {
            return 0.0;
        }
        self.decode_ms_total / self.frames_decoded as f64
    }
}

#[derive(Default)]
pub struct SharedMediaState {
    pub certificate_untrusted: Option<CertInfo>,
    pub pending_auth: Option<AuthRequest>,
    pub pending_authentication: Option<SessionAuthentication>,
    /// Newest decoded frame, taken (and uploaded) by the UI thread.
    pub latest_frame: Option<DecodedVideoFrame>,
    /// The primary/root decoder's own backend label, snapshotted alongside
    /// every frame it publishes (see `publish_decoded_frame`). Empty before
    /// any frame has decoded. Independent of `encoder_backend`/`encoder_class`
    /// on `server_hello`: a session can be hardware-encoded and
    /// software-decoded, or the reverse, and neither side can see the
    /// other's answer -- see `NativeVideoDecoder::backend_name`.
    pub decoder_backend_name: &'static str,
    /// `NativeVideoDecoder::is_hardware_accelerated`'s own answer,
    /// snapshotted alongside every frame the primary/root decoder publishes.
    /// `None` before any frame has decoded, exactly like the decoder's own
    /// pre-session answer.
    pub decoder_hardware_accelerated: Option<bool>,
    pub server_hello: Option<ServerHelloMsg>,
    pub cursor_mode_result: Option<CursorModeResultMsg>,
    pub tablet_mode_result: Option<TabletModeResultMsg>,
    pub pending_cursor_shape: Option<arcen_protocol::messages::CursorShapeKind>,
    pub microphone_active: bool,
    pub display_update_result: Option<DisplayUpdateResultMsg>,
    pub broker_hello: bool,
    pub host_health: Option<HealthStatsMsg>,
    pub last_health_pong: Option<HealthPongMsg>,
    pub video_frames_seen: u64,
    pub video_frames_decoded: u64,
    pub presentation_superseded_frames: u64,
    pub video_batch_high_water: usize,
    pub audio_batch_high_water: usize,
    pub audio_frames_seen: u64,
    pub audio_bytes_accepted: u64,
    pub last_audio_queued_ms: usize,
    pub audio_playback_underruns: u64,
    pub audio_buffer_trim_events: u64,
    pub audio_buffer_trimmed_samples: u64,
    pub last_audio_feed_gap_ms: u64,
    pub max_audio_feed_gap_ms: u64,
    pub audio_decode_failures: u64,
    pub audio_concealed_frames: u64,
    pub audio_queue_phase: String,
    /// Last frame as observed on the wire, before decoder dispatch.
    ///
    /// Kept separate from the UI thread's decoder summary so the Connection
    /// Health panel cannot alternate between the host/wire contract and the
    /// VideoToolbox result under one "Client decoder" label.
    pub last_wire_video_summary: String,
    pub last_audio_summary: String,
    pub last_decode_error: Option<String>,
    pub last_decode_ms: f64,
    pub decode_ms_samples: VecDeque<f64>,
    pub video_packet_times: VecDeque<Instant>,
    pub last_wire_frame_age_ms: Option<i32>,
    pub waiting_for_keyframe: bool,
    pub inbox: IncomingMediaTelemetry,
    pub ingress_idr_requests: u64,
    pub malformed_media_packets: u64,
    pub generation: u64,
    pub end: Option<SessionEnd>,
    pub closed: bool,
    pub error: Option<String>,
    /// One-way UI-thread -> worker-thread arm: `None` for every legacy and
    /// primary-only session (the overwhelming default). Set at most once
    /// per connection as soon as the UI thread has validated the applied
    /// `multi_monitor_v1` topology (carrier + 1..=4 roster) -- i.e. at
    /// *topology acceptance*, deliberately BEFORE any native window has
    /// bound/fullscreened its target display and before presentation/input
    /// ever commits. Decoder isolation must start this early so a
    /// multi-monitor host can never have any of its per-monitor packets
    /// reach the single legacy decoder while windows are still being
    /// bound: see `crate::ui::app::ArcenApp::begin_multi_window_if_applicable`,
    /// which sets this, and the *separate*, later
    /// `MultiWindowSessionState::Active`'s own `committed: bool` UI-side
    /// gate, which continues to guard presentation/input independently of
    /// this field. The worker reads this to build its additive per-monitor
    /// `MonitorFrameRouter` for wire `monitor_id != 0` traffic; it must
    /// never be unset or replaced again for this shared state's lifetime
    /// (topology changes are a reconnect-required signal, not a live
    /// mutation -- see `crate::ui::multi_window_session`). If the window
    /// transaction that follows ever fails (timeout/abort/mismatch), the
    /// whole session disconnects (`crate::ui::app::ArcenApp::disconnect`),
    /// which drops every `Arc<Mutex<SharedMediaState>>` reference and lets
    /// the worker thread's own `secondary_router` local variable -- built
    /// from this field -- drop with it; there is no partial-retry path that
    /// would need a more explicit teardown.
    pub multi_monitor_decode_roster: Option<(
        arcen_media::TopologyGeneration,
        arcen_media::RegionMediaRoster,
    )>,
    /// Latest decoded frame for every negotiated *secondary* monitor (i.e.
    /// every committed monitor id other than the roster's primary, whose
    /// frame continues to flow through `latest_frame` unchanged). Empty
    /// until `multi_monitor_decode_roster` is set and at least one
    /// secondary frame has decoded. Bounded to one latest frame per monitor,
    /// mirroring `latest_frame`'s single-slot bound -- and, like
    /// `latest_frame`, meant to be *taken* (`BTreeMap::remove`) by the UI
    /// thread rather than cloned in place: `ArcenApp::drive_multi_window`
    /// removes each monitor's entry as soon as it reads it, so an ordinary
    /// repaint with no fresh decode in between sees `None` here and keeps
    /// painting its own already-uploaded `secondary_textures` entry instead
    /// of re-uploading the same RGBA buffer to the GPU every frame.
    pub secondary_frames: BTreeMap<arcen_media::SessionMonitorId, DecodedVideoFrame>,
    /// Per-monitor media counters for every negotiated monitor, including
    /// the roster's primary.
    ///
    /// Empty for every legacy/primary-only session; populated only by the
    /// multi-monitor routed decode path. Bounded by the roster itself
    /// (`arcen_media::MAX_MULTI_MONITOR_COUNT`), so this map can never grow
    /// beyond four entries.
    pub monitor_media: BTreeMap<arcen_media::SessionMonitorId, MonitorMediaCounters>,
}

impl SharedMediaState {
    pub fn fresh(generation: u64, waiting_for_keyframe: bool) -> Self {
        Self {
            generation,
            waiting_for_keyframe,
            ..Self::default()
        }
    }
}

pub fn spawn_media_worker(
    mut events: mpsc::UnboundedReceiver<SessionEvent>,
    media: IncomingMediaReceiver,
    commands: SessionCommandSender,
    shared: Arc<Mutex<SharedMediaState>>,
    repaint: egui::Context,
    telemetry: Arc<ClientTelemetry>,
) {
    std::thread::Builder::new()
        .name("media-worker".to_string())
        .spawn(move || {
            // Created on this thread on purpose: the VT session and the CPAL
            // stream are not Send; the worker owns them for its lifetime.
            let mut decoder = NativeVideoDecoder::new();
            let mut audio = PcmAudioPlayer::new();
            let mut full_frame_requests = FullFrameRequestGate::default();
            let mut pending_ingress_idr = false;
            let mut last_heartbeat = Instant::now();
            // Additive per-monitor router for wire `monitor_id != 0` traffic.
            // Stays `None` (and `decoder` above remains the entire decode
            // path, byte for byte unchanged) for every legacy and
            // primary-only session -- the default today and for the
            // foreseeable future, since it is only ever built once the UI
            // thread has armed decoder isolation for a validated
            // multi-window topology (see
            // `SharedMediaState::multi_monitor_decode_roster`). Built at
            // most once per connection: a one-way `None` -> `Some`
            // transition mirroring "no live topology mutation".
            let mut secondary_router: Option<MonitorFrameRouter> = None;
            // Seeded by `ServerHello` and applied to every decoder, including
            // the per-monitor ones the router builds later in the session.
            let mut session_color = SessionColor::default();
            tracing::info!(target: crate::logging::target::VIDEO, "media worker started");

            loop {
                if full_frame_requests
                    .retry_after()
                    .is_some_and(|delay| delay.is_zero())
                    && full_frame_requests.send_due(&commands)
                    && pending_ingress_idr
                {
                    shared
                        .lock()
                        .expect("media state poisoned")
                        .ingress_idr_requests += 1;
                }
                let event = if full_frame_requests.is_pending() {
                    match events.try_recv() {
                        Ok(event) => Some(event),
                        Err(mpsc::error::TryRecvError::Empty) => {
                            if full_frame_requests.send_due(&commands) {
                                if pending_ingress_idr {
                                    shared
                                        .lock()
                                        .expect("media state poisoned")
                                        .ingress_idr_requests += 1;
                                }
                            } else {
                                std::thread::sleep(
                                    full_frame_requests
                                        .retry_after()
                                        .unwrap_or_default()
                                        .min(Duration::from_millis(10)),
                                );
                            }
                            continue;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => None,
                    }
                } else {
                    events.blocking_recv()
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    SessionEvent::CertificateUntrusted(info) => {
                        let mut state = shared.lock().expect("media state poisoned");
                        state.certificate_untrusted = Some(info);
                        state.closed = true;
                        drop(state);
                        repaint.request_repaint();
                        return;
                    }
                    SessionEvent::AuthRequired(request) => {
                        shared.lock().expect("media state poisoned").pending_auth = Some(request);
                        repaint.request_repaint();
                    }
                    SessionEvent::Authenticated(authentication) => {
                        shared
                            .lock()
                            .expect("media state poisoned")
                            .pending_authentication = Some(authentication);
                        repaint.request_repaint();
                    }
                    SessionEvent::ServerHello(hello) => {
                        tracing::info!(
                            target: crate::logging::target::SESSION,
                            server = %hello.server_name,
                            version = %hello.version,
                            encoder = %hello.encoder_backend,
                            width = hello.screen_width,
                            height = hello.screen_height,
                            audio = hello.supports_audio,
                            yuv444 = hello.supports_yuv444,
                            "server hello",
                        );
                        // Teach every decoder the two colour axes no packet
                        // header carries. Taken from the host's own
                        // `active_*` caps rather than from what this Deck
                        // asked for, so a downgraded session is presented
                        // as what actually arrives. `None` (a host that
                        // does not report the axis at all) keeps the
                        // BT.709 SDR default rather than guessing.
                        let active = ActiveContract::from_hello(&hello);
                        {
                            let mut state = shared.lock().expect("media state poisoned");
                            state.server_hello = Some(hello);
                        }
                        session_color = SessionColor {
                            primaries: active.primaries.unwrap_or(ColorPrimaries::Bt709),
                            transfer: active.transfer.unwrap_or(TransferCharacteristics::Bt709),
                        };
                        tracing::info!(
                            target: crate::logging::target::VIDEO,
                            primaries = session_color.primaries.token(),
                            transfer = session_color.transfer.token(),
                            hdr = matches!(
                                session_color.transfer,
                                TransferCharacteristics::Pq | TransferCharacteristics::Hlg
                            ),
                            "deck resolved session colour from host caps",
                        );
                        decoder.set_session_color(session_color);
                        if let Some(router) = secondary_router.as_mut() {
                            router.set_session_color(session_color);
                        }
                        full_frame_requests.request();
                        let _ = full_frame_requests.send_due(&commands);
                        repaint.request_repaint();
                    }
                    SessionEvent::BrokerHello(_) => {
                        tracing::info!(
                            target: crate::logging::target::SESSION,
                            "broker hello",
                        );
                        shared.lock().expect("media state poisoned").broker_hello = true;
                        repaint.request_repaint();
                    }
                    SessionEvent::Json(value) => {
                        let mut state = shared.lock().expect("media state poisoned");
                        match msg_type(&value) {
                            Some(HEALTH_STATS) => {
                                if let Ok(stats) = serde_json::from_value::<HealthStatsMsg>(value) {
                                    state.host_health = Some(stats);
                                }
                            }
                            Some(HEALTH_PONG) => {
                                if let Ok(pong) = serde_json::from_value::<HealthPongMsg>(value) {
                                    state.last_health_pong = Some(pong);
                                }
                            }
                            Some(CURSOR_MODE_RESULT) => {
                                if let Ok(result) =
                                    serde_json::from_value::<CursorModeResultMsg>(value)
                                {
                                    state.cursor_mode_result = Some(result);
                                }
                            }
                            Some(CURSOR_SHAPE) => {
                                if let Ok(msg) = serde_json::from_value::<CursorShapeMsg>(value) {
                                    tracing::debug!(
                                        target: crate::logging::target::SESSION,
                                        shape = ?msg.shape,
                                        "cursor shape received from host"
                                    );
                                    state.pending_cursor_shape = Some(msg.shape);
                                }
                            }
                            Some(TABLET_MODE_RESULT) => {
                                if let Ok(result) =
                                    serde_json::from_value::<TabletModeResultMsg>(value)
                                {
                                    state.tablet_mode_result = Some(result);
                                }
                            }
                            Some(DISPLAY_UPDATE_RESULT) => {
                                if let Ok(result) =
                                    serde_json::from_value::<DisplayUpdateResultMsg>(value)
                                {
                                    state.display_update_result = Some(result);
                                }
                            }
                            Some(AUDIO_STREAM_RESULT) => {
                                drop(state);
                                if let Ok(result) =
                                    serde_json::from_value::<AudioStreamResultMsg>(value)
                                {
                                    audio.set_stream_result(&result);
                                }
                            }
                            _ => {}
                        }
                    }
                    SessionEvent::MicrophoneActive(active) => {
                        shared
                            .lock()
                            .expect("media state poisoned")
                            .microphone_active = active;
                        repaint.request_repaint();
                    }
                    SessionEvent::MediaReady => {
                        maybe_commit_secondary_router(
                            &shared,
                            &mut secondary_router,
                            session_color,
                        );
                        handle_media_batch(
                            &shared,
                            &mut decoder,
                            &mut secondary_router,
                            &mut audio,
                            &commands,
                            &mut full_frame_requests,
                            &mut pending_ingress_idr,
                            media.take_batch(),
                            &telemetry,
                            &repaint,
                        );
                        maybe_heartbeat(&shared, &mut last_heartbeat);
                        repaint.request_repaint();
                    }
                    SessionEvent::Ended(end) => {
                        let error = Some(end.message.clone());
                        shared.lock().expect("media state poisoned").end = Some(end);
                        finish(&shared, &repaint, error);
                        return;
                    }
                }
            }
            finish(&shared, &repaint, None);
        })
        .expect("failed to spawn media worker");
}

fn finish(shared: &Arc<Mutex<SharedMediaState>>, repaint: &egui::Context, error: Option<String>) {
    match &error {
        Some(err) => tracing::error!(
            target: crate::logging::target::SESSION,
            error = %err,
            "media worker stopped on error",
        ),
        None => tracing::info!(
            target: crate::logging::target::SESSION,
            "media worker stopped (session closed)",
        ),
    }
    {
        let mut state = shared.lock().expect("media state poisoned");
        state.closed = true;
        state.microphone_active = false;
        state.error = error;
    }
    repaint.request_repaint();
}

/// Emit an INFO "stream healthy" heartbeat at most every HEARTBEAT_INTERVAL.
/// This is the light-level "OK working" signal a sysadmin greps for; DEBUG
/// carries the per-event drop/keyframe/decode detail between heartbeats.
fn maybe_heartbeat(shared: &Arc<Mutex<SharedMediaState>>, last: &mut Instant) {
    if last.elapsed() < HEARTBEAT_INTERVAL {
        return;
    }
    *last = Instant::now();
    let state = shared.lock().expect("media state poisoned");
    tracing::info!(
        target: crate::logging::target::VIDEO,
        frames_seen = state.video_frames_seen,
        frames_decoded = state.video_frames_decoded,
        presentation_superseded_frames = state.presentation_superseded_frames,
        video_batch_high_water = state.video_batch_high_water,
        audio_batch_high_water = state.audio_batch_high_water,
        audio_frames = state.audio_frames_seen,
        decode_ms = state.last_decode_ms,
        wire_age_ms = ?state.last_wire_frame_age_ms,
        waiting_for_keyframe = state.waiting_for_keyframe,
        video_queue_drops = state.inbox.video_dropped_packets,
        video_queue_superseded = state.inbox.video_superseded_packets,
        video_loss_epochs = state.inbox.video_loss_epochs,
        video_queue_high_water = state.inbox.video_high_water_depth,
        video_queue_high_water_bytes = state.inbox.video_high_water_bytes,
        video_queue_overflow_events = state.inbox.video_queue_overflow_events,
        video_overflow_last_oldest_age_ms = state.inbox.video_overflow_last_oldest_age_ms,
        video_overflow_max_oldest_age_ms = state.inbox.video_overflow_max_oldest_age_ms,
        video_overflow_last_burst_packets = state.inbox.video_overflow_last_burst_packets,
        video_enqueue_burst_high_water = state.inbox.video_enqueue_burst_high_water,
        audio_queue_drops = state.inbox.audio_dropped_packets,
        audio_queue_high_water = state.inbox.audio_high_water_depth,
        audio_queue_high_water_bytes = state.inbox.audio_high_water_bytes,
        audio_enqueue_burst_high_water = state.inbox.audio_enqueue_burst_high_water,
        audio_playback_underruns = state.audio_playback_underruns,
        audio_buffer_trim_events = state.audio_buffer_trim_events,
        audio_buffer_trimmed_samples = state.audio_buffer_trimmed_samples,
        audio_feed_gap_ms = state.last_audio_feed_gap_ms,
        audio_feed_gap_max_ms = state.max_audio_feed_gap_ms,
        ingress_idr_requests = state.ingress_idr_requests,
        malformed_video_packets = state.inbox.malformed_video_packets,
        malformed_audio_packets = state.inbox.malformed_audio_packets,
        "stream healthy",
    );
    // One bounded record per negotiated monitor (never more than
    // `MAX_MULTI_MONITOR_COUNT`), emitted only for multi-monitor sessions.
    // Without this, a session-wide aggregate cannot say which viewport was
    // starved -- the exact gap that left the pier-windows.example.internal stutter
    // undiagnosable from client telemetry alone.
    for (monitor_id, counters) in &state.monitor_media {
        tracing::info!(
            target: crate::logging::target::VIDEO,
            monitor_id = monitor_id.get(),
            frames_received = counters.frames_received,
            frames_decoded = counters.frames_decoded,
            frames_rejected = counters.frames_rejected,
            presentation_superseded = counters.presentation_superseded,
            waiting_for_keyframe = counters.waiting_for_keyframe,
            decode_ms = counters.last_decode_ms,
            average_decode_ms = counters.average_decode_ms(),
            "per-monitor stream health",
        );
    }
}

/// One-way arm: builds the additive per-monitor `secondary_router` the
/// first (and only) time the UI thread's
/// `SharedMediaState::multi_monitor_decode_roster` is observed to be
/// `Some` -- set at topology acceptance, deliberately before window
/// binding/presentation commit, so decoder isolation begins as early as
/// possible. Idempotent and cheap when already built or still `None` (the
/// default, legacy path). Building can fail only if the UI thread armed an
/// invalid roster, which its own validation
/// (`crate::ui::multi_window_session`) must never do; on that defensive
/// failure the worker logs and stays on the legacy-only path rather than
/// panicking, since the legacy `decoder` above is entirely unaffected either
/// way.
fn maybe_commit_secondary_router(
    shared: &Arc<Mutex<SharedMediaState>>,
    secondary_router: &mut Option<MonitorFrameRouter>,
    session_color: SessionColor,
) {
    if secondary_router.is_some() {
        return;
    }
    let roster = shared
        .lock()
        .expect("media state poisoned")
        .multi_monitor_decode_roster
        .clone();
    let Some((generation, media_roster)) = roster else {
        return;
    };
    match MonitorFrameRouter::new_with_media_roster(generation, &media_roster) {
        Ok(router) => {
            tracing::info!(
                target: crate::logging::target::VIDEO,
                generation = generation.get(),
                monitors = media_roster.plans().len(),
                "media worker committed to negotiated multi-monitor routing",
            );
            // The router is built lazily, long after `ServerHello` resolved
            // the session's colour, so its freshly created per-monitor
            // decoders have to be told too -- otherwise every secondary
            // display silently presents an HDR session as BT.709 SDR.
            let mut router = router;
            router.set_session_color(session_color);
            *secondary_router = Some(router);
        }
        Err(error) => {
            tracing::error!(
                target: crate::logging::target::VIDEO,
                %error,
                "UI thread committed an invalid multi-monitor roster; staying on legacy routing",
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_media_batch(
    shared: &Arc<Mutex<SharedMediaState>>,
    decoder: &mut NativeVideoDecoder,
    secondary_router: &mut Option<MonitorFrameRouter>,
    audio: &mut PcmAudioPlayer,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
    batch: IncomingMediaBatch,
    telemetry: &ClientTelemetry,
    repaint: &egui::Context,
) {
    {
        let now = Instant::now();
        let mut state = shared.lock().expect("media state poisoned");
        state.video_frames_seen = batch.telemetry.video_received;
        state.audio_frames_seen = batch.telemetry.audio_received;
        state.video_batch_high_water = state.video_batch_high_water.max(batch.video.len());
        state.audio_batch_high_water = state.audio_batch_high_water.max(batch.audio.len());
        state.malformed_media_packets = batch.telemetry.malformed_packets;
        state.inbox = batch.telemetry;
        telemetry.record_media(
            state.video_frames_seen,
            state.video_frames_decoded,
            state.inbox.video_dropped_packets,
            Duration::from_secs_f64((state.last_decode_ms / 1_000.0).max(0.0)),
        );
        if let Some(error) = &batch.malformed_error {
            state.last_decode_error = Some(format!("media packet error: {error}"));
        }
        for _ in &batch.video {
            push_instant_sample(&mut state.video_packet_times, now);
        }
        if let Some((header, payload)) = batch.video.last() {
            state.last_wire_frame_age_ms = Some(wire_frame_age_ms(header.timestamp_ms));
            state.last_wire_video_summary = format!(
                "{:?} {:?} {:?} monitor={} ts={} payload={} bytes",
                header.frame_type,
                header.codec,
                header.chroma,
                header.monitor_id,
                header.timestamp_ms,
                payload.len()
            );
        } else if batch.video_discontinuity {
            state.last_wire_video_summary = format!(
                "Video queue loss epoch {} · dropped {} packets / {} bytes",
                batch.telemetry.video_loss_epochs,
                batch.telemetry.video_dropped_packets,
                batch.telemetry.video_dropped_bytes,
            );
        }
    }

    for (header, payload) in batch.audio {
        let status = audio.feed(header, &payload);
        let mut state = shared.lock().expect("media state poisoned");
        let previous_underruns = state.audio_playback_underruns;
        let previous_trim_events = state.audio_buffer_trim_events;
        let previous_trimmed_samples = state.audio_buffer_trimmed_samples;
        state.audio_bytes_accepted += status.accepted_bytes as u64;
        state.last_audio_queued_ms = status.queued_ms;
        state.audio_playback_underruns = status.playback_underruns;
        state.audio_buffer_trim_events = status.buffer_trim_events;
        state.audio_buffer_trimmed_samples = status.buffer_trimmed_samples;
        state.last_audio_feed_gap_ms = status.feed_gap_ms;
        state.max_audio_feed_gap_ms = status.max_feed_gap_ms;
        state.audio_decode_failures = status.decode_failures;
        state.audio_concealed_frames = status.concealed_frames;
        state.audio_queue_phase = status.queue_phase.to_string();
        state.last_audio_summary = if let Some(note) = status.note.as_deref() {
            format!(
                "Audio {} queued={} ms accepted={} bytes underruns={} trims={} gap={} ms phase={} · {}",
                status.backend,
                status.queued_ms,
                status.accepted_bytes,
                status.playback_underruns,
                status.buffer_trim_events,
                status.feed_gap_ms,
                status.queue_phase,
                note
            )
        } else {
            format!(
                "Audio {} queued={} ms accepted={} bytes underruns={} trims={} gap={} ms phase={}",
                status.backend,
                status.queued_ms,
                status.accepted_bytes,
                status.playback_underruns,
                status.buffer_trim_events,
                status.feed_gap_ms,
                status.queue_phase,
            )
        };
        drop(state);
        if status.playback_underruns > previous_underruns {
            tracing::warn!(
                target: crate::logging::target::AUDIO,
                playback_underruns = status.playback_underruns,
                queued_ms = status.queued_ms,
                feed_gap_ms = status.feed_gap_ms,
                feed_gap_max_ms = status.max_feed_gap_ms,
                decode_failures = status.decode_failures,
                concealed_frames = status.concealed_frames,
                "CoreAudio playback queue underrun; rebuffering"
            );
        }
        if status.buffer_trim_events > previous_trim_events {
            tracing::debug!(
                target: crate::logging::target::AUDIO,
                trim_events = status.buffer_trim_events,
                trimmed_samples = status.buffer_trimmed_samples,
                trimmed_samples_delta = status
                    .buffer_trimmed_samples
                    .saturating_sub(previous_trimmed_samples),
                queued_ms = status.queued_ms,
                "CoreAudio playback queue trimmed excess latency"
            );
        }
    }

    if batch.video_discontinuity {
        for monitor_id in &batch.video_discontinuity_monitor_ids {
            if *monitor_id == 0 {
                decoder.notify_discontinuity();
            } else if let Some(router) = secondary_router.as_mut() {
                if !router.notify_route_discontinuity(*monitor_id) {
                    tracing::debug!(
                        target: crate::logging::target::VIDEO,
                        monitor_id,
                        "video discontinuity referenced an uncommitted monitor route",
                    );
                }
            }
        }
        let mut state = shared.lock().expect("media state poisoned");
        state.waiting_for_keyframe = true;
        drop(state);
        tracing::debug!(
            target: crate::logging::target::VIDEO,
            loss_epochs = batch.telemetry.video_loss_epochs,
            dropped_packets = batch.telemetry.video_dropped_packets,
            dropped_bytes = batch.telemetry.video_dropped_bytes,
            queue_overflow_events = batch.telemetry.video_queue_overflow_events,
            overflow_oldest_age_ms = batch.telemetry.video_overflow_last_oldest_age_ms,
            overflow_burst_packets = batch.telemetry.video_overflow_last_burst_packets,
            enqueue_burst_high_water = batch.telemetry.video_enqueue_burst_high_water,
            "video inbox discontinuity; waiting for replacement keyframe",
        );
    }
    if batch.idr_needed {
        *pending_ingress_idr = true;
        full_frame_requests.request();
        if full_frame_requests.send_due(commands) {
            shared
                .lock()
                .expect("media state poisoned")
                .ingress_idr_requests += 1;
        }
    }

    if decode_batch(
        shared,
        decoder,
        secondary_router,
        commands,
        full_frame_requests,
        &batch.video,
        telemetry,
        repaint,
    ) {
        full_frame_requests.cancel_pending();
        *pending_ingress_idr = false;
    }
}

/// Decode every video packet in arrival order (P-frames reference their
/// predecessors), but if the batch contains a keyframe, start at the LAST
/// keyframe — everything before it is stale history the IDR supersedes.
///
/// Once a real multi-window session has committed (`secondary_router` is
/// `Some`), this legacy body is bypassed entirely in favor of
/// [`decode_secondary_packets`] -- a committed roster only ever admits
/// nonzero wire `monitor_id`s (see [`MonitorRoute`]), so no packet in this
/// mode is legacy traffic for `decoder` to see. Defense in depth: even while
/// still on this legacy body (no router committed yet, including every
/// primary-only session with Match My Layout never requested, or still
/// mid-negotiation/mid-transaction before commit), any packet that DOES
/// carry a nonzero `monitor_id` -- a host sending multi-monitor traffic this
/// build hasn't admitted a roster for yet, or ever, for that legacy session
/// -- is dropped before it ever reaches `decoder`, never silently decoded
/// into the single legacy texture.
#[allow(clippy::too_many_arguments)]
fn decode_batch(
    shared: &Arc<Mutex<SharedMediaState>>,
    decoder: &mut NativeVideoDecoder,
    secondary_router: &mut Option<MonitorFrameRouter>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    packets: &[(VideoHeader, Vec<u8>)],
    telemetry: &ClientTelemetry,
    repaint: &egui::Context,
) -> bool {
    if packets.is_empty() {
        return false;
    }
    if let Some(router) = secondary_router.as_mut() {
        // Release-candidate media finding #1: the shared full-frame-request/
        // pending-ingress-IDR gate must only ever be cancelled once *every*
        // admitted monitor has recovered from its own keyframe wait
        // (`SecondaryDecodeOutcome::all_recovered`), never merely because
        // this batch happened to decode a fresh frame for *one* of them
        // (`decoded_any`) -- see that type's own doc for why.
        let outcome = decode_secondary_packets(
            shared,
            router,
            commands,
            full_frame_requests,
            packets,
            telemetry,
            repaint,
        );
        return outcome.all_recovered;
    }
    let waiting_for_keyframe = shared
        .lock()
        .expect("media state poisoned")
        .waiting_for_keyframe;
    let Some(start) = first_decodable_index(packets, waiting_for_keyframe) else {
        full_frame_requests.request();
        let _ = full_frame_requests.send_due(commands);
        return false;
    };
    if start > 0 {
        // Caught up by jumping to the freshest keyframe; everything before it
        // was stale backlog. At DEBUG this quantifies the catch-up drop.
        tracing::debug!(
            target: crate::logging::target::VIDEO,
            dropped = start,
            batch = packets.len(),
            "skip-to-keyframe: dropped stale backlog",
        );
    }

    let mut decoded_keyframe = false;
    for (header, payload) in &packets[start..] {
        if header.monitor_id != 0 {
            // No multi-monitor router is committed yet (still legacy/
            // primary-only), so a nonzero wire `monitor_id` here means a
            // host is sending multi-monitor traffic this build has not
            // (yet, or ever) admitted a roster for. Never let it reach the
            // single legacy decoder/texture: that would silently corrupt
            // primary-only presentation with a different monitor's frames.
            // Drop it and ask for a full frame once the legacy path is
            // ready to resume from a clean keyframe.
            tracing::warn!(
                target: crate::logging::target::VIDEO,
                monitor_id = header.monitor_id,
                "dropping multi-monitor packet on the legacy single-decoder path; no committed \
                 router yet",
            );
            full_frame_requests.request();
            let _ = full_frame_requests.send_due(commands);
            continue;
        }
        let decode_start = Instant::now();
        match decoder.decode(header, payload) {
            Ok(Some(frame)) => {
                let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
                decoded_keyframe |= header.is_keyframe();
                publish_decoded_frame(
                    shared,
                    frame,
                    decode_ms,
                    decoder.backend_name(),
                    decoder.is_hardware_accelerated(),
                    telemetry,
                    repaint,
                );
            }
            Ok(None) => {
                if decoder.wants_keyframe() {
                    decoded_keyframe = false;
                    tracing::debug!(
                        target: crate::logging::target::VIDEO,
                        "decoder wants keyframe; requesting full frame",
                    );
                    shared
                        .lock()
                        .expect("media state poisoned")
                        .waiting_for_keyframe = true;
                    full_frame_requests.request();
                    let _ = full_frame_requests.send_due(commands);
                }
            }
            Err(error) => {
                decoded_keyframe = false;
                decoder.notify_discontinuity();
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    %error,
                    "video decode error",
                );
                let mut state = shared.lock().expect("media state poisoned");
                state.last_decode_error = Some(error.to_string());
                state.waiting_for_keyframe = true;
                drop(state);
                full_frame_requests.request();
                let _ = full_frame_requests.send_due(commands);
            }
        }
    }
    decoded_keyframe
}

/// Whether a freshly decoded `route` belongs in `shared.latest_frame` (the
/// roster's explicit negotiated primary, presented on root) rather than
/// `shared.secondary_frames` (every other negotiated monitor).
/// `primary_monitor_id` must be `router.primary_monitor_id()` -- the
/// router's own explicit `MonitorFrameRouter::new`-time primary -- never
/// re-derived from any sorted/iteration order, so a primary/secondary pair
/// like primary `7`/secondary `1` is never misclassified by numeric id.
/// Pulled out as its own pure function so this exact selection is
/// independently unit-testable without needing a real decodable video
/// payload.
fn is_primary_route(
    route: MonitorRoute,
    primary_monitor_id: Option<arcen_media::SessionMonitorId>,
) -> bool {
    matches!(route, MonitorRoute::Negotiated(id) if Some(id) == primary_monitor_id)
}

/// Outcome of routing/decoding one batch of packets through a committed
/// `secondary_router`: separates whether *any* fresh frame decoded this
/// batch (a telemetry-only signal -- at least one monitor produced new
/// output, currently consumed only by this module's own DEBUG logging just
/// above its construction) from whether *every* admitted monitor has now
/// recovered from its own keyframe wait (the router-wide
/// [`MonitorFrameRouter::all_recovered`] signal, which `decode_batch` reads
/// to decide whether it is safe to cancel the shared gate).
/// `handle_media_batch`/`decode_batch` must gate cancelling the shared
/// full-frame-request/pending-ingress-IDR ask on `all_recovered`, never on
/// `decoded_any`: a single monitor producing a fresh frame while a sibling
/// monitor is still waiting for its own keyframe must never cancel that
/// shared gate, since there is no per-monitor full-frame request on the wire
/// -- cancelling early would strand the sibling waiting forever with
/// nothing left to re-ask the host for. Both fields are kept on this type
/// (rather than collapsing to just `all_recovered`) so this distinction
/// stays explicit and independently testable at the type level, matching
/// [`RouteOutcome`]'s own "never conflate fresh-decode with cached-lookup"
/// contract one level up.
#[allow(
    dead_code,
    reason = "decoded_any is read via the local variable that seeds it, not through this \
              struct's own field, for its one current DEBUG-log use; kept as a named field \
              anyway so the type documents both halves of the outcome explicitly"
)]
struct SecondaryDecodeOutcome {
    decoded_any: bool,
    all_recovered: bool,
}

/// Routes every packet in `packets` through `router`'s per-monitor decode
/// slots (`MonitorFrameRouter::route_and_decode`), publishing each
/// successfully decoded frame to `shared.latest_frame` (the roster's primary
/// monitor -- so root's existing single-texture presentation path keeps
/// working unchanged) or `shared.secondary_frames` (every other negotiated
/// monitor). Returns a [`SecondaryDecodeOutcome`] -- see its own doc for why
/// `decoded_any` and `all_recovered` are deliberately distinct fields, never
/// conflated into one bool the way the legacy path's return value is.
///
/// Unlike the legacy path there is no batch-level "skip to the last
/// keyframe" optimization: each monitor's own `NativeVideoDecoder` (inside
/// its `MonitorSlot`) independently tracks whether it wants a keyframe, and
/// any per-monitor admission/decode failure falls back to the same global
/// full-frame-request gate the legacy path uses (multi-monitor-v1's wire has
/// no per-monitor full-frame request today). This is an intentional,
/// honestly-scoped simplification for this additive, still env-gated-off
/// path; the legacy hot path above is completely unaffected.
fn decode_secondary_packets(
    shared: &Arc<Mutex<SharedMediaState>>,
    router: &mut MonitorFrameRouter,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    packets: &[(VideoHeader, Vec<u8>)],
    telemetry: &ClientTelemetry,
    repaint: &egui::Context,
) -> SecondaryDecodeOutcome {
    // The router's own explicit negotiated primary -- never
    // `router.monitor_ids().next()`, which iterates in ascending numeric
    // order and would silently treat the *smallest* admitted id as "the
    // primary" whenever the real negotiated primary is not the smallest
    // (e.g. primary `7` alongside secondary `1`).
    let primary_monitor_id = router.primary_monitor_id();
    let mut decoded_any = false;
    for (header, payload) in packets {
        let monitor_id = arcen_media::SessionMonitorId::new(header.monitor_id).ok();
        let started = Instant::now();
        let outcome = router.route_and_decode(header, payload);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        match outcome {
            Ok(RouteOutcome::FreshFrame) => {
                let route = MonitorRoute::from_wire_monitor_id(header.monitor_id);
                // `FreshFrame` guarantees this call just set the slot's
                // cached frame; look it up as an explicit, separate
                // *presentation* read rather than trusting the route result
                // itself to carry freshness -- a stale cached frame must
                // never be mistaken for one this call produced.
                let frame = match route {
                    MonitorRoute::Negotiated(id) => router.latest_frame(id).cloned(),
                    MonitorRoute::LegacyPrimary => router.latest_frame_legacy_primary().cloned(),
                };
                let Some(frame) = frame else {
                    // Unreachable: `FreshFrame` is only ever returned after
                    // the slot's `latest_frame` was just populated.
                    continue;
                };
                decoded_any = true;
                let mut state = shared.lock().expect("media state poisoned");
                let superseded = if is_primary_route(route, primary_monitor_id) {
                    state.latest_frame.replace(frame).is_some()
                } else {
                    match route {
                        MonitorRoute::Negotiated(id) => {
                            state.secondary_frames.insert(id, frame).is_some()
                        }
                        MonitorRoute::LegacyPrimary => {
                            // Unreachable once committed: a committed
                            // roster is always all-`Negotiated`
                            // (`MonitorFrameRouter::new` never admits wire
                            // id 0). Defensive no-op if it ever somehow
                            // occurred.
                            false
                        }
                    }
                };
                // The routed path publishes into `latest_frame`/
                // `secondary_frames` directly rather than through
                // `publish_decoded_frame`, so every shared counter that
                // function maintains has to be updated here too. Omitting
                // this is what made the pier-windows.example.internal session report
                // `frames_decoded: 0` and `waiting_for_keyframe: true` for
                // its entire 126-second lifetime while it was in fact
                // decoding and presenting ~29 fps.
                record_monitor_decode(&mut state, monitor_id, elapsed_ms, superseded);
                if is_primary_route(route, primary_monitor_id) {
                    if let Some(primary_id) = primary_monitor_id {
                        state.decoder_backend_name =
                            router.decoder_backend_name(primary_id).unwrap_or("");
                        state.decoder_hardware_accelerated =
                            router.decoder_hardware_accelerated(primary_id).flatten();
                    }
                }
                let (seen, decoded, dropped) = (
                    state.video_frames_seen,
                    state.video_frames_decoded,
                    state.inbox.video_dropped_packets,
                );
                drop(state);
                telemetry.record_media(
                    seen,
                    decoded,
                    dropped,
                    Duration::from_secs_f64((elapsed_ms / 1_000.0).max(0.0)),
                );
                repaint.request_repaint();
            }
            Ok(RouteOutcome::NoOutputYet) => {
                let mut state = shared.lock().expect("media state poisoned");
                monitor_counters(&mut state, monitor_id, |counters| {
                    counters.frames_received = counters.frames_received.saturating_add(1);
                });
            }
            Err(error) => {
                {
                    let mut state = shared.lock().expect("media state poisoned");
                    monitor_counters(&mut state, monitor_id, |counters| {
                        counters.frames_received = counters.frames_received.saturating_add(1);
                        counters.frames_rejected = counters.frames_rejected.saturating_add(1);
                    });
                }
                tracing::debug!(
                    target: crate::logging::target::VIDEO,
                    %error,
                    monitor_id = header.monitor_id,
                    "multi-monitor frame routing/decode rejected; requesting full frame",
                );
                full_frame_requests.request();
                let _ = full_frame_requests.send_due(commands);
            }
        }
    }
    let all_recovered = router.all_recovered();
    // Mirror every slot's own keyframe-recovery state into the shared
    // aggregate, so the "stream healthy" heartbeat reports the routed
    // path's real state instead of the stale `true` it inherited from
    // session start and never cleared.
    {
        let mut state = shared.lock().expect("media state poisoned");
        state.waiting_for_keyframe = !all_recovered;
        for monitor_id in router.monitor_ids() {
            let waiting = router.waiting_for_keyframe(monitor_id).unwrap_or(false);
            state
                .monitor_media
                .entry(monitor_id)
                .or_default()
                .waiting_for_keyframe = waiting;
        }
    }
    if decoded_any && !all_recovered {
        // At least one monitor produced fresh output this batch, but
        // another admitted monitor is still waiting for its own keyframe --
        // exactly the state the shared full-frame-request/pending-ingress-
        // IDR gate must stay armed for (see `SecondaryDecodeOutcome`'s own
        // doc). DEBUG-only: this is expected and self-healing once the
        // host's next full frame lands, not a warning-worthy condition.
        tracing::debug!(
            target: crate::logging::target::VIDEO,
            "multi-monitor batch decoded fresh output for some monitors while at least one \
             sibling is still awaiting its own keyframe; keeping the full-frame request armed",
        );
    }
    SecondaryDecodeOutcome {
        decoded_any,
        all_recovered,
    }
}

/// Applies `update` to one monitor's counters, or does nothing when the
/// packet carried a wire id with no negotiated `SessionMonitorId` (wire id
/// 0, the legacy primary, which the routed path never admits).
fn monitor_counters(
    state: &mut SharedMediaState,
    monitor_id: Option<arcen_media::SessionMonitorId>,
    update: impl FnOnce(&mut MonitorMediaCounters),
) {
    let Some(monitor_id) = monitor_id else {
        return;
    };
    update(state.monitor_media.entry(monitor_id).or_default());
}

/// Records one fresh routed decode into both this monitor's own counters and
/// the session-wide aggregates `publish_decoded_frame` maintains for the
/// legacy path, so the two paths report the same shape of truth.
fn record_monitor_decode(
    state: &mut SharedMediaState,
    monitor_id: Option<arcen_media::SessionMonitorId>,
    decode_ms: f64,
    superseded: bool,
) {
    state.last_decode_ms = decode_ms;
    push_ms_sample(&mut state.decode_ms_samples, decode_ms);
    state.last_decode_error = None;
    state.video_frames_decoded = state.video_frames_decoded.saturating_add(1);
    if superseded {
        state.presentation_superseded_frames =
            state.presentation_superseded_frames.saturating_add(1);
    }
    monitor_counters(state, monitor_id, |counters| {
        counters.frames_received = counters.frames_received.saturating_add(1);
        counters.frames_decoded = counters.frames_decoded.saturating_add(1);
        counters.last_decode_ms = decode_ms;
        counters.decode_ms_total += decode_ms;
        if superseded {
            counters.presentation_superseded = counters.presentation_superseded.saturating_add(1);
        }
    });
}

fn publish_decoded_frame(
    shared: &Arc<Mutex<SharedMediaState>>,
    frame: DecodedVideoFrame,
    decode_ms: f64,
    decoder_backend_name: &'static str,
    decoder_hardware_accelerated: Option<bool>,
    telemetry: &ClientTelemetry,
    repaint: &egui::Context,
) {
    let mut state = shared.lock().expect("media state poisoned");
    state.last_decode_ms = decode_ms;
    push_ms_sample(&mut state.decode_ms_samples, decode_ms);
    state.last_decode_error = None;
    state.waiting_for_keyframe = false;
    state.video_frames_decoded = state.video_frames_decoded.saturating_add(1);
    state.decoder_backend_name = decoder_backend_name;
    state.decoder_hardware_accelerated = decoder_hardware_accelerated;
    if state.latest_frame.replace(frame).is_some() {
        state.presentation_superseded_frames =
            state.presentation_superseded_frames.saturating_add(1);
    }
    telemetry.record_media(
        state.video_frames_seen,
        state.video_frames_decoded,
        state.inbox.video_dropped_packets,
        Duration::from_secs_f64((decode_ms / 1_000.0).max(0.0)),
    );
    // Release the lock before requesting a repaint so the UI thread can
    // immediately take the frame without contending on the mutex.
    drop(state);
    repaint.request_repaint();
}

fn first_decodable_index(
    packets: &[(VideoHeader, Vec<u8>)],
    waiting_for_keyframe: bool,
) -> Option<usize> {
    let keyframe = packets.iter().rposition(|(header, _)| header.is_keyframe());
    if waiting_for_keyframe {
        keyframe
    } else {
        Some(keyframe.unwrap_or(0))
    }
}

fn push_instant_sample(samples: &mut VecDeque<Instant>, now: Instant) {
    samples.push_back(now);
    while samples
        .front()
        .is_some_and(|front| now.duration_since(*front) > TELEMETRY_WINDOW)
    {
        samples.pop_front();
    }
}

fn push_ms_sample(samples: &mut VecDeque<f64>, value: f64) {
    samples.push_back(value);
    while samples.len() > 120 {
        samples.pop_front();
    }
}

fn wire_frame_age_ms(timestamp_ms: u32) -> i32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    now.wrapping_sub(timestamp_ms) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChromaSubsampling, FrameType, VideoCodec, VIDEO_KEYFRAME_FLAG};

    #[test]
    fn rejected_keyframe_rearms_full_frame_recovery() {
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (sender, mut received) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(sender);
        let mut gate = FullFrameRequestGate::default();
        let mut decoder = NativeVideoDecoder::new();
        let telemetry = ClientTelemetry::default();
        decoder.notify_discontinuity();
        let packets = vec![(
            VideoHeader {
                frame_type: FrameType::VideoH264,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: VIDEO_KEYFRAME_FLAG,
                timestamp_ms: 1,
                monitor_id: 0,
                topology_generation: 0,
                stream_epoch: 0,
            },
            vec![0xff],
        )];

        let mut secondary_router: Option<MonitorFrameRouter> = None;
        assert!(!decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &packets,
            &telemetry,
            &egui::Context::default(),
        ));
        assert!(received.try_recv().is_ok());
        assert!(
            shared
                .lock()
                .expect("media state poisoned")
                .waiting_for_keyframe
        );
    }

    fn packet(flags: u8, frame_type: FrameType) -> (VideoHeader, Vec<u8>) {
        (
            VideoHeader {
                frame_type,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags,
                timestamp_ms: 1,
                monitor_id: 0,
                topology_generation: 0,
                stream_epoch: 0,
            },
            vec![1],
        )
    }

    #[test]
    fn resumed_media_state_is_fresh_and_waits_for_keyframe() {
        let state = SharedMediaState::fresh(42, true);
        assert_eq!(state.generation, 42);
        assert!(state.waiting_for_keyframe);
        assert!(state.latest_frame.is_none());
        assert_eq!(state.video_frames_seen, 0);
        assert_eq!(state.presentation_superseded_frames, 0);
        assert_eq!(state.audio_frames_seen, 0);
        assert_eq!(state.inbox, IncomingMediaTelemetry::default());
        // A reconnect must never carry a stale hardware/software decode
        // verdict over from the previous connection attempt.
        assert_eq!(state.decoder_backend_name, "");
        assert_eq!(state.decoder_hardware_accelerated, None);
    }

    #[test]
    fn decoded_frames_are_published_individually_without_building_a_backlog() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            video_frames_seen: 2,
            ..SharedMediaState::default()
        }));
        let telemetry = ClientTelemetry::default();
        let frame = |timestamp_ms| DecodedVideoFrame {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
            timestamp_ms,
            pixel_format: "rgba".to_string(),
            backend: "test",
            native: None,
        };

        publish_decoded_frame(
            &shared,
            frame(1),
            1.0,
            "videotoolbox-bgra-hw",
            Some(true),
            &telemetry,
            &egui::Context::default(),
        );
        publish_decoded_frame(
            &shared,
            frame(2),
            1.0,
            "videotoolbox-bgra-hw",
            Some(true),
            &telemetry,
            &egui::Context::default(),
        );

        let state = shared.lock().expect("media state poisoned");
        assert_eq!(state.video_frames_decoded, 2);
        assert_eq!(state.presentation_superseded_frames, 1);
        assert_eq!(state.decoder_backend_name, "videotoolbox-bgra-hw");
        assert_eq!(state.decoder_hardware_accelerated, Some(true));
        assert_eq!(
            state.latest_frame.as_ref().map(|frame| frame.timestamp_ms),
            Some(2)
        );
    }

    #[test]
    fn resumed_decoder_discards_delta_frames_until_fresh_idr() {
        let delta_only = vec![packet(0, FrameType::VideoH264)];
        assert_eq!(first_decodable_index(&delta_only, true), None);

        let with_idr = vec![
            packet(0, FrameType::VideoH264),
            packet(VIDEO_KEYFRAME_FLAG, FrameType::VideoH264),
            packet(0, FrameType::VideoH264),
        ];
        assert_eq!(first_decodable_index(&with_idr, true), Some(1));
    }

    #[test]
    fn is_primary_route_uses_the_routers_explicit_primary_not_the_smallest_id() {
        // Audit finding: root's `latest_frame`/texture must key off the
        // roster's *explicit* negotiated primary, never whichever admitted
        // id happens to sort smallest. Primary `7` alongside secondary `1`
        // is exactly the case that a "smallest id wins" bug would flip.
        let primary = arcen_media::SessionMonitorId::new(7).expect("nonzero");
        let secondary = arcen_media::SessionMonitorId::new(1).expect("nonzero");

        assert!(is_primary_route(
            MonitorRoute::Negotiated(primary),
            Some(primary)
        ));
        assert!(!is_primary_route(
            MonitorRoute::Negotiated(secondary),
            Some(primary)
        ));
        assert!(!is_primary_route(
            MonitorRoute::LegacyPrimary,
            Some(primary)
        ));
        // No explicit primary recorded at all (only ever true for
        // `MonitorFrameRouter::single_monitor`'s legacy-only router, which
        // `decode_secondary_packets` never reaches since `secondary_router`
        // stays `None` for it) never misclassifies anything as primary.
        assert!(!is_primary_route(MonitorRoute::Negotiated(primary), None));
    }

    #[test]
    fn decode_secondary_packets_reads_the_routers_explicit_primary_id_seven_not_one() {
        // End-to-end proof with the audit's own primary id 7 / secondary id
        // 1 pair: builds the exact kind of router
        // `maybe_commit_secondary_router` builds for this roster (primary
        // first, per `ValidatedAppliedTopology::monitor_ids()`'s contract),
        // and confirms `router.primary_monitor_id()` -- what
        // `decode_secondary_packets` actually reads before routing any
        // frame -- is `7`, never the numerically smaller `1` a sorted/min-id
        // bug would have produced instead.
        let primary = arcen_media::SessionMonitorId::new(7).expect("nonzero");
        let secondary = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let router = MonitorFrameRouter::new(generation, &[primary, secondary])
            .expect("a two-monitor roster with primary first is valid");

        assert_eq!(router.primary_monitor_id(), Some(primary));
        assert!(is_primary_route(
            MonitorRoute::Negotiated(primary),
            router.primary_monitor_id()
        ));
        assert!(!is_primary_route(
            MonitorRoute::Negotiated(secondary),
            router.primary_monitor_id()
        ));
    }

    #[test]
    fn decode_batch_only_cancels_the_shared_gate_once_every_admitted_monitor_recovers() {
        // Release-candidate media finding #1: `decode_batch`'s "safe to
        // cancel the shared full-frame-request/pending-ingress-IDR gate"
        // signal must track `MonitorFrameRouter::all_recovered` (every
        // admitted monitor), never merely `decoded_any` (something decoded
        // *somewhere* this batch). Every packet fed to `decode_batch` below
        // targets either a still-waiting monitor (skipped before ever
        // reaching the real decoder) or an unrouted monitor id (rejected at
        // admission, before touching any slot) -- so `decoded_any` stays
        // `false` for this entire test. The cancel decision nonetheless
        // flips from `false` to `true` and back to `false` purely by
        // driving each monitor's own recovery gate via
        // `MonitorFrameRouter::force_recovered_for_test`, proving the
        // wiring is recovery-state-driven, not decode-this-batch-driven.
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (sender, _received) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(sender);
        let mut gate = FullFrameRequestGate::default();
        let mut decoder = NativeVideoDecoder::new();
        let telemetry = ClientTelemetry::default();
        let repaint = egui::Context::default();

        let sid1 = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let sid2 = arcen_media::SessionMonitorId::new(2).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let router = MonitorFrameRouter::new(generation, &[sid1, sid2])
            .expect("a two-monitor roster is valid");
        let mut secondary_router = Some(router);

        let delta_for = |monitor_id: u16| -> Vec<(VideoHeader, Vec<u8>)> {
            vec![(
                VideoHeader {
                    frame_type: FrameType::RegionVideoH264,
                    codec: VideoCodec::H264,
                    chroma: ChromaSubsampling::Yuv420,
                    flags: 0,
                    timestamp_ms: 1,
                    monitor_id,
                    topology_generation: 1,
                    stream_epoch: 1,
                },
                vec![9, 9, 9],
            )]
        };
        // Unrouted (not part of this 2-monitor roster): rejected at
        // admission before touching monitor 1 or monitor 2's own slot, so it
        // is safe to use as a non-empty "no-op" batch once both monitors are
        // already in the exact recovery state a phase wants to assert.
        let unrouted_noop = delta_for(99);

        // Phase 1: monitor 1 already recovered, monitor 2 has not. A delta
        // packet for monitor 2 is skipped (still waiting), never re-arming
        // anything, so it must stay armed (not safe to cancel).
        secondary_router
            .as_mut()
            .expect("router present")
            .force_recovered_for_test(MonitorRoute::Negotiated(sid1));
        assert!(
            !decode_batch(
                &shared,
                &mut decoder,
                &mut secondary_router,
                &commands,
                &mut gate,
                &delta_for(2),
                &telemetry,
                &repaint,
            ),
            "one recovered monitor alongside a still-waiting sibling must never cancel the \
             shared gate",
        );

        // Phase 2: monitor 2 also recovers. No packet in this batch decodes
        // anything (`decoded_any` is still `false`), yet it is now safe to
        // cancel purely because every admitted monitor has recovered.
        secondary_router
            .as_mut()
            .expect("router present")
            .force_recovered_for_test(MonitorRoute::Negotiated(sid2));
        assert!(
            decode_batch(
                &shared,
                &mut decoder,
                &mut secondary_router,
                &commands,
                &mut gate,
                &unrouted_noop,
                &telemetry,
                &repaint,
            ),
            "once every admitted monitor has recovered, cancelling the shared gate must be safe \
             even when nothing decoded this exact batch",
        );

        // Phase 3: a discontinuity re-arms every admitted monitor's own
        // recovery gate. The very next batch must keep the shared gate
        // armed again, exactly mirroring phase 1.
        secondary_router
            .as_mut()
            .expect("router present")
            .notify_discontinuity();
        assert!(
            !decode_batch(
                &shared,
                &mut decoder,
                &mut secondary_router,
                &commands,
                &mut gate,
                &unrouted_noop,
                &telemetry,
                &repaint,
            ),
            "a discontinuity resets every admitted monitor's recovery gate; the shared gate must \
             re-arm too",
        );
    }

    /// The routed multi-monitor path publishes into
    /// `latest_frame`/`secondary_frames` directly instead of through
    /// `publish_decoded_frame`, and originally updated none of the shared
    /// counters that function maintains. The 2026-08-11 pier-windows.example.internal session
    /// is the exact consequence: 126 seconds of a visibly streaming desktop
    /// reported `frames_decoded: 0`, `decode_ms: 0.0` and a permanently
    /// stuck `waiting_for_keyframe: true`, which made the presentation
    /// stutter impossible to attribute to a monitor from client telemetry.
    ///
    /// Drives real packets through `decode_batch`'s routed path and asserts
    /// the session aggregates and the per-monitor counters both move.
    #[test]
    fn routed_multi_monitor_decode_records_aggregate_and_per_monitor_counters() {
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (sender, _received) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(sender);
        let mut gate = FullFrameRequestGate::default();
        let mut decoder = NativeVideoDecoder::new();
        let telemetry = ClientTelemetry::default();
        let repaint = egui::Context::default();

        let sid1 = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let sid2 = arcen_media::SessionMonitorId::new(2).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let mut secondary_router = Some(
            MonitorFrameRouter::new(generation, &[sid1, sid2])
                .expect("a two-monitor roster is valid"),
        );

        // Rejected at admission (unrouted id), which must still be counted
        // against that monitor rather than vanishing.
        let unrouted = vec![(
            VideoHeader {
                frame_type: FrameType::RegionVideoH264,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: 0,
                timestamp_ms: 1,
                monitor_id: 3,
                topology_generation: 1,
                stream_epoch: 1,
            },
            vec![9, 9, 9],
        )];
        decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &unrouted,
            &telemetry,
            &repaint,
        );

        let state = shared.lock().expect("media state poisoned");
        let sid3 = arcen_media::SessionMonitorId::new(3).expect("nonzero");
        let rejected = state
            .monitor_media
            .get(&sid3)
            .copied()
            .expect("an unrouted monitor's rejection is still attributed to it");
        assert_eq!(rejected.frames_rejected, 1);
        assert_eq!(rejected.frames_decoded, 0);
        // Every admitted monitor's own recovery state is mirrored into the
        // shared map, so a stalled viewport is identifiable by id.
        assert_eq!(
            state
                .monitor_media
                .get(&sid1)
                .map(|c| c.waiting_for_keyframe),
            Some(true),
        );
        assert_eq!(
            state
                .monitor_media
                .get(&sid2)
                .map(|c| c.waiting_for_keyframe),
            Some(true),
        );
        assert!(
            state.waiting_for_keyframe,
            "the session aggregate must reflect the routed path's real recovery state",
        );
    }

    /// `record_monitor_decode` is the exact routine the routed path uses to
    /// keep the session aggregates and this monitor's own counters in step,
    /// including the presentation-supersession signal that distinguishes
    /// "decode kept up but presentation did not" from ordinary throughput.
    #[test]
    fn recording_a_routed_decode_moves_both_aggregate_and_monitor_counters() {
        let mut state = SharedMediaState::default();
        state.waiting_for_keyframe = true;
        let sid = arcen_media::SessionMonitorId::new(2).expect("nonzero");

        record_monitor_decode(&mut state, Some(sid), 4.0, false);
        record_monitor_decode(&mut state, Some(sid), 6.0, true);

        assert_eq!(state.video_frames_decoded, 2);
        assert_eq!(state.presentation_superseded_frames, 1);
        assert!((state.last_decode_ms - 6.0).abs() < f64::EPSILON);

        let counters = state.monitor_media.get(&sid).copied().expect("recorded");
        assert_eq!(counters.frames_decoded, 2);
        assert_eq!(counters.frames_received, 2);
        assert_eq!(counters.presentation_superseded, 1);
        assert!((counters.average_decode_ms() - 5.0).abs() < f64::EPSILON);

        // A packet with no negotiated id (wire id 0) must not fabricate an
        // entry, but must still move the session aggregate.
        record_monitor_decode(&mut state, None, 1.0, false);
        assert_eq!(state.video_frames_decoded, 3);
        assert_eq!(state.monitor_media.len(), 1);
    }
}
