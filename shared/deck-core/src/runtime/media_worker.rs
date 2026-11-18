// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Media worker — owns the streaming hot path on its own OS thread.
//!
//! The session's receive/decode/audio pipeline must NOT live on the egui UI
//! thread: macOS throttles repaints for unfocused windows and pauses them
//! entirely when the display sleeps, and any UI hitch backs the stream up
//! (soak testing showed wire age climbing to 11 s once the display dimmed).
//! The worker consumes lightweight `SessionEvent`s continuously and drains the
//! bounded media inbox on `MediaReady`. It feeds audio immediately, requests
//! recovery after an ingress drop, and publishes the latest frame plus
//! telemetry through a mutex the UI reads at its own pace.
//!
//! Moved from the macOS Deck (ADR 0024 extraction step 5). The platform
//! enters through [`MediaPlatform`]: its decoder
//! ([`VideoDecoderBackend`], created on this thread by a
//! [`VideoDecoderFactory`]), its two dedicated presenters
//! ([`VideoPresenterSink`]) and the status vocabulary its UI reads. The
//! decode, routing, recovery and fallback rules are the same on every Deck:
//! a reconnect gets a fresh worker, decoder and inbox and waits for a fresh
//! keyframe, and a teardown discards queued frames.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::expect_used,
    clippy::float_cmp,
    clippy::inconsistent_struct_constructor,
    clippy::match_same_arms,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::single_match_else,
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::too_many_lines,
    reason = "the media worker moved verbatim from the macOS Deck (ADR 0024 step 5); \
              these lints are tightened in follow-ups, never by changing behaviour in the move"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

use crate::client_telemetry::ClientTelemetry;
use crate::decode::{
    DecodedVideoFrame, SessionColor, VideoDecodeError, VideoDecoderBackend, VideoDecoderFactory,
};
use crate::present::{
    PresentOutcome, PresentableFrame, PresenterTelemetryDelta, RefineFrameTag, RepaintRequester,
    VideoPresenterSink,
};
use crate::runtime::audio_worker::AudioPlayback;
use crate::runtime::frame_queue::{
    IncomingMediaBatch, IncomingMediaReceiver, IncomingMediaTelemetry,
};
use crate::runtime::monitor_router::{MonitorFrameRouter, MonitorRoute, RouteError, RouteOutcome};
use crate::runtime::session::{
    DisconnectReason, FullFrameRequestGate, SessionAuthentication, SessionCommandSender,
    SessionEnd, SessionEvent, TerminalDisconnect,
};
use crate::runtime::tls::CertInfo;
use arcen_media::{PresentationWindow, TransferCharacteristics, classify_presentation_window};
use arcen_protocol::VideoHeader;
use arcen_protocol::messages::{
    AUDIO_STREAM_RESULT, AuthRequest, CURSOR_MODE_RESULT, CURSOR_SHAPE, CursorModeResultMsg,
    CursorShapeMsg, DISPLAY_UPDATE_RESULT, DisplayUpdateResultMsg, HEALTH_PONG, HEALTH_STATS,
    HOST_KEYMAP, HealthPongMsg, HealthStatsMsg, HostKeymapMsg, SERVED_PIPELINE, ServedPipelineMsg,
    ServerHelloMsg, TABLET_MODE_RESULT, TEXT_COMMIT_RESULT, TabletModeResultMsg,
    TextCommitResultMsg, msg_type,
};
use arcen_telemetry::{WireDelayEstimator, rounded_percentiles_ms};

const TELEMETRY_WINDOW: Duration = Duration::from_secs(2);
/// How often the worker emits an INFO "stream healthy" heartbeat while frames
/// are flowing. At INFO this is the sysadmin's "OK working" signal; DEBUG adds
/// the per-event drop/keyframe/decode detail around it.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// A dedicated presenter's status as the platform's UI keeps it.
///
/// The media worker reasons in [`PresentOutcome`]; a platform whose UI keeps
/// a richer status (the macOS Deck names the exact Metal failure) converts
/// both ways without loss. A platform without one uses [`PresentOutcome`]
/// itself.
pub trait PresenterStatus:
    Copy
    + Default
    + std::fmt::Debug
    + PartialEq
    + Send
    + 'static
    + From<PresentOutcome>
    + Into<PresentOutcome>
{
}

impl<T> PresenterStatus for T where
    T: Copy
        + Default
        + std::fmt::Debug
        + PartialEq
        + Send
        + 'static
        + From<PresentOutcome>
        + Into<PresentOutcome>
{
}

/// The platform half of the media worker: one type per Deck that names its
/// decoder, its two dedicated presenters and the statuses its UI reads.
///
/// The presenters are shared with the UI thread (attach, resize,
/// visibility, teardown) behind a mutex; the worker only calls
/// [`VideoPresenterSink::present`] on them.
pub trait MediaPlatform: 'static {
    /// The decoder's native picture.
    type NativeFrame: Clone + std::fmt::Debug + Send + 'static;
    /// The decoder. Every instance, the root's and each monitor's, comes
    /// from the worker's [`VideoDecoderFactory`].
    type Decoder: VideoDecoderBackend<NativeFrame = Self::NativeFrame>;
    /// Auto and Speed.
    type EightBitPresenter: VideoPresenterSink<NativeFrame = Self::NativeFrame> + Send + 'static;
    /// Grading and HDR.
    type TenBitPresenter: VideoPresenterSink<NativeFrame = Self::NativeFrame> + Send + 'static;
    /// The eight-bit presenter's status as the UI keeps it.
    type EightBitStatus: PresenterStatus;
    /// The ten-bit presenter's status as the UI keeps it.
    type TenBitStatus: PresenterStatus;
}

/// Keeps an established `Presented` status across a transient miss, in the
/// platform's own status vocabulary.
fn preserve_established<S: PresenterStatus>(
    current: S,
    incoming: PresentOutcome,
    has_presented_picture: bool,
) -> S {
    let current: PresentOutcome = current.into();
    S::from(current.preserve_established_on_transient(incoming, has_presented_picture))
}

#[derive(Debug, PartialEq, Eq)]
enum WorkerLoopStep {
    Event,
    PendingPolled,
    Disconnected,
}

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
    /// Fresh frames the Deck intentionally did not present because the
    /// target display had not reached its next refresh opportunity.
    pub frames_superseded_by_refresh: u64,
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

/// Local display identity a negotiated monitor is presented on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MonitorPresentationTarget {
    /// The local `CGDirectDisplayID` receiving this monitor.
    pub deck_display_id: u32,
    /// Human-readable Deck display name, or a stable fallback.
    pub deck_display_name: String,
    /// Nominal refresh of the local Deck display receiving this monitor.
    pub display_refresh_hz: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MonitorRateSnapshot {
    frames_received: u64,
    frames_presented: u64,
    frames_rejected: u64,
    frames_superseded_by_refresh: u64,
}

impl MonitorRateSnapshot {
    fn from_counts(counters: &MonitorMediaCounters, frames_presented: u64) -> Self {
        Self {
            frames_received: counters.frames_received,
            frames_presented,
            frames_rejected: counters.frames_rejected,
            frames_superseded_by_refresh: counters.frames_superseded_by_refresh,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MonitorWindowRates {
    fps_received: f64,
    fps_presented: f64,
    frames_dropped: u64,
    frames_superseded_by_refresh: u64,
    frames_dropped_before_presentation: u64,
}

/// What the UI shows about the newest natively presented frame.
#[derive(Debug, Clone)]
pub struct NativePresentationMetadata {
    pub width: usize,
    pub height: usize,
    pub timestamp_ms: u32,
    pub pixel_format: String,
    pub backend: &'static str,
    pub video: arcen_media::VideoConfiguration,
}

/// A presenter fallback asked the Pier for a full frame; armed until the
/// fallback path has shown a picture.
#[derive(Debug, Default)]
pub struct PresentationRecovery {
    pending: bool,
}

impl PresentationRecovery {
    fn arm(&mut self, gate: &mut FullFrameRequestGate) {
        self.pending = true;
        gate.request();
    }

    const fn is_pending(&self) -> bool {
        self.pending
    }

    fn clear_after_fallback_handoff(&mut self) -> bool {
        if !self.pending {
            return false;
        }
        self.pending = false;
        true
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct LatestFrameMailbox<T> {
    slot: Option<T>,
    accepted: u64,
    dropped: u64,
}

#[cfg(test)]
impl<T> LatestFrameMailbox<T> {
    pub(crate) fn submit(&mut self, frame: T) {
        if self.slot.replace(frame).is_some() {
            self.dropped = self.dropped.saturating_add(1);
        }
        self.accepted = self.accepted.saturating_add(1);
    }

    pub(crate) fn take(&mut self) -> Option<T> {
        self.slot.take()
    }

    pub(crate) const fn accepted(&self) -> u64 {
        self.accepted
    }

    pub(crate) const fn dropped(&self) -> u64 {
        self.dropped
    }
}

fn monitor_stream_window(
    elapsed: Duration,
    previous: Option<MonitorRateSnapshot>,
    current: MonitorRateSnapshot,
    display_refresh_hz: u32,
) -> MonitorWindowRates {
    let previous = previous.unwrap_or_default();
    let report = classify_presentation_window(PresentationWindow {
        frames_received: current
            .frames_received
            .saturating_sub(previous.frames_received),
        frames_presented_by_client: current
            .frames_presented
            .saturating_sub(previous.frames_presented),
        frames_rejected: current
            .frames_rejected
            .saturating_sub(previous.frames_rejected),
        frames_superseded_by_refresh: current
            .frames_superseded_by_refresh
            .saturating_sub(previous.frames_superseded_by_refresh),
        display_refresh_hz,
        elapsed,
    });
    MonitorWindowRates {
        fps_received: report.fps_received,
        fps_presented: report.fps_presented,
        frames_dropped: report.frames_dropped,
        frames_superseded_by_refresh: report.frames_superseded_by_refresh,
        frames_dropped_before_presentation: report.frames_dropped_before_presentation,
    }
}

/// What the media worker publishes for the UI thread, behind one mutex.
pub struct SharedMediaState<M: MediaPlatform> {
    /// The audio worker's own status cell (ADR 0014): audio playout never
    /// takes this state's lock, the UI copies the snapshot from here.
    pub audio_playback: crate::runtime::audio_worker::AudioPlaybackCell,
    pub certificate_untrusted: Option<CertInfo>,
    pub pending_auth: Option<AuthRequest>,
    pub pending_authentication: Option<SessionAuthentication>,
    pub connection_status_hint: Option<String>,
    /// Newest decoded frame, taken (and uploaded) by the UI thread.
    pub latest_frame: Option<DecodedVideoFrame<M::NativeFrame>>,
    /// Main-thread-attached root 8-bit presenter, rendered from the media
    /// worker when Auto/Speed zero-copy presentation is available.
    pub eight_bit_video_presenter: Option<Arc<Mutex<M::EightBitPresenter>>>,
    pub eight_bit_layer_ready: bool,
    pub eight_bit_layer_has_picture: bool,
    pub eight_bit_layer_disabled: bool,
    pub eight_bit_presentation_status: M::EightBitStatus,
    /// Main-thread-attached root Grading/HDR presenter, rendered from the
    /// media worker when the native `xf44`/RGB10A2 path is available.
    pub ten_bit_video_presenter: Option<Arc<Mutex<M::TenBitPresenter>>>,
    /// Exact still image: this connection's hub. Ten-bit frames carry the
    /// fence the video lane named for them, so the presenter composites
    /// exactly the tiles valid at that fence.
    pub refine_hub: Option<arcen_media::refine::hub::DeckRefineHub>,
    pub ten_bit_layer_ready: bool,
    pub ten_bit_layer_has_picture: bool,
    pub ten_bit_layer_disabled: bool,
    pub ten_bit_presentation_status: M::TenBitStatus,
    pub dedicated_presenter_drops: u64,
    pub dedicated_presenter_skips: u64,
    pub layer_present_ms_samples: VecDeque<f64>,
    pub latest_native_frame_metadata: Option<NativePresentationMetadata>,
    pub last_native_frame_size: Option<[usize; 2]>,
    pub presentation_recovery: PresentationRecovery,
    /// The primary/root decoder's own backend label, snapshotted alongside
    /// every frame it publishes (see `publish_decoded_frame`). Empty before
    /// any frame has decoded. Independent of `encoder_backend`/`encoder_class`
    /// on `server_hello`: a session can be hardware-encoded and
    /// software-decoded, or the reverse, and neither side can see the
    /// other's answer -- see `VideoDecoderBackend::backend_name`.
    pub decoder_backend_name: &'static str,
    /// `VideoDecoderBackend::is_hardware_accelerated`'s own answer,
    /// snapshotted alongside every frame the primary/root decoder publishes.
    /// `None` before any frame has decoded, exactly like the decoder's own
    /// pre-session answer.
    pub decoder_hardware_accelerated: Option<bool>,
    pub server_hello: Option<ServerHelloMsg>,
    pub served_pipeline: Option<ServedPipelineMsg>,
    pub served_source_fps: Option<u32>,
    pub cursor_mode_result: Option<CursorModeResultMsg>,
    pub tablet_mode_result: Option<TabletModeResultMsg>,
    pub pending_cursor_shape: Option<arcen_protocol::messages::CursorShapeKind>,
    /// Host reports on committed text the UI has not shown yet.
    pub text_commit_results: Vec<TextCommitResultMsg>,
    /// The host's newest keyboard-layout notice, not yet applied.
    pub host_keymap: Option<HostKeymapMsg>,
    /// The host's cursor image state, when it changed since the UI last
    /// took it (`arcen_input::cursor::DeckCursorState`).
    pub pending_host_cursor: Option<arcen_input::cursor::DeckCursor>,
    /// The newest host pointer position not yet taken by the UI.
    pub pending_host_pointer: Option<arcen_protocol::cursor_position::CursorPosition>,
    pub microphone_active: bool,
    pub display_update_result: Option<DisplayUpdateResultMsg>,
    pub host_health: Option<HealthStatsMsg>,
    /// The host displays last reported limited (`HealthStatsMsg.monitors`),
    /// with their reason, so each change is logged once.
    pub host_limited_displays: Vec<(u16, String)>,
    pub last_health_pong: Option<HealthPongMsg>,
    /// When the host last answered a health ping: the session is alive
    /// even while every host monitor is still and sends no frames.
    pub last_health_pong_at: Option<Instant>,
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
    /// platform decoder result under one "Client decoder" label.
    pub last_wire_video_summary: String,
    pub last_audio_summary: String,
    pub last_decode_error: Option<String>,
    /// Why the decoder refused this connection's stream
    /// ([`VideoDecodeError::Unsupported`]: a profile this machine did not
    /// prove, or a picture that contradicts its wire contract). Unlike
    /// `last_decode_error` a later decode does not clear it: Stream
    /// Statistics keeps saying why video stopped (per connection; a
    /// reconnect starts from a fresh state).
    pub decoder_refusal: Option<String>,
    pub last_decode_ms: f64,
    pub decode_ms_samples: VecDeque<f64>,
    pub video_packet_times: VecDeque<Instant>,
    pub last_wire_clock_age_ms: Option<i32>,
    pub last_wire_delay_ms: Option<i32>,
    pub wire_delay_estimator: WireDelayEstimator,
    /// Recent raw clock ages and offset-free delays at arrival. Clock age uses
    /// the host wall clock and therefore includes inter-machine clock offset;
    /// delay subtracts the session rolling minimum for decisions and overlays.
    pub video_wire_clock_age_samples: VecDeque<f64>,
    pub video_wire_delay_samples: VecDeque<f64>,
    pub audio_wire_clock_age_samples: VecDeque<f64>,
    pub audio_wire_delay_samples: VecDeque<f64>,
    pub waiting_for_keyframe: bool,
    pub inbox: IncomingMediaTelemetry,
    pub ingress_idr_requests: u64,
    pub malformed_media_packets: u64,
    /// Number of media-ready events that explicitly woke the UI.
    pub ui_wake_media_ready: u64,
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
    /// bound: see `arcen_deck::ui::app::ArcenApp::begin_multi_window_if_applicable`,
    /// which sets this, and the *separate*, later
    /// `MultiWindowSessionState::Active`'s own `committed: bool` UI-side
    /// gate, which continues to guard presentation/input independently of
    /// this field. The worker reads this to build its additive per-monitor
    /// `MonitorFrameRouter` for wire `monitor_id != 0` traffic; it must
    /// never be unset or replaced again for this shared state's lifetime
    /// (topology changes are a reconnect-required signal, not a live
    /// mutation -- see `arcen_deck::ui::multi_window_session`). If the window
    /// transaction that follows ever fails (timeout/abort/mismatch), the
    /// whole session disconnects (`arcen_deck::ui::app::ArcenApp::disconnect`),
    /// which drops every `Arc<Mutex<SharedMediaState>>` reference and lets
    /// the worker thread's own `secondary_router` local variable -- built
    /// from this field -- drop with it; there is no partial-retry path that
    /// would need a more explicit teardown.
    pub multi_monitor_decode_roster: Option<(
        arcen_media::TopologyGeneration,
        arcen_media::RegionMediaRoster,
    )>,
    /// Negotiated monitor presented by the Deck root viewport. Defaults to
    /// the roster primary when absent; multi-window presentation may move
    /// root to another local display to avoid pacing every viewport on a
    /// slow primary display.
    pub multi_monitor_root_monitor_id: Option<arcen_media::SessionMonitorId>,
    /// Latest decoded frame for every negotiated non-root monitor (i.e.
    /// every committed monitor id other than `multi_monitor_root_monitor_id`
    /// or, when absent, the roster primary). Empty
    /// until `multi_monitor_decode_roster` is set and at least one
    /// secondary frame has decoded. Bounded to one latest frame per monitor,
    /// mirroring `latest_frame`'s single-slot bound -- and, like
    /// `latest_frame`, meant to be *taken* (`BTreeMap::remove`) by the UI
    /// thread rather than cloned in place: `ArcenApp::drive_multi_window`
    /// removes each monitor's entry as soon as it reads it, so an ordinary
    /// repaint with no fresh decode in between sees `None` here and keeps
    /// painting its own already-uploaded `secondary_textures` entry instead
    /// of re-uploading the same RGBA buffer to the GPU every frame.
    pub secondary_frames:
        BTreeMap<arcen_media::SessionMonitorId, DecodedVideoFrame<M::NativeFrame>>,
    /// Per-monitor media counters for every negotiated monitor, including
    /// the roster's primary.
    ///
    /// Empty for every legacy/primary-only session; populated only by the
    /// multi-monitor routed decode path. Bounded by the roster itself
    /// (`arcen_media::MAX_MULTI_MONITOR_COUNT`), so this map can never grow
    /// beyond four entries.
    pub monitor_media: BTreeMap<arcen_media::SessionMonitorId, MonitorMediaCounters>,
    /// How often this connection's decoder fell behind the stream and the
    /// video skipped ahead ([`crate::runtime::decode_lag`]).
    pub decoder_behind: crate::runtime::decode_lag::DecoderBehind,
    /// Recent offset-free video wire-delay samples by negotiated monitor.
    pub monitor_wire_delay_samples: BTreeMap<arcen_media::SessionMonitorId, VecDeque<f64>>,
    /// Cumulative UI presentations by monitor. Decode/receive counters are
    /// worker-owned, but presentation happens on the UI thread.
    pub monitor_presented_frames: BTreeMap<arcen_media::SessionMonitorId, u64>,
    /// The local Deck display each negotiated monitor is presented on.
    pub monitor_presentation_targets:
        BTreeMap<arcen_media::SessionMonitorId, MonitorPresentationTarget>,
    /// The root monitor of a multi-monitor session goes to the dedicated
    /// eight-bit presenter, as a single-monitor session's frames do, rather
    /// than to `latest_frame` (set by a Deck with no egui video path).
    pub routed_primary_dedicated_eight_bit: bool,
    /// The shared multi-stream timeline (ADR 0026): decoded frames of a
    /// committed multi-monitor session wait here until frames captured with
    /// them on the other monitors are ready too, within a bound.
    pub frame_sync: crate::runtime::frame_sync::FrameSync<M::NativeFrame>,
}

impl<M: MediaPlatform> std::fmt::Debug for SharedMediaState<M> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedMediaState")
            .field("generation", &self.generation)
            .field("closed", &self.closed)
            .field("waiting_for_keyframe", &self.waiting_for_keyframe)
            .field("video_frames_seen", &self.video_frames_seen)
            .field("video_frames_decoded", &self.video_frames_decoded)
            .finish_non_exhaustive()
    }
}

// Written out: a derive would demand `Default` of the platform's presenters
// and pictures, which no field needs.
#[allow(
    clippy::default_trait_access,
    reason = "one uniform line per field of a long state struct"
)]
impl<M: MediaPlatform> Default for SharedMediaState<M> {
    fn default() -> Self {
        Self {
            audio_playback: Default::default(),
            certificate_untrusted: Default::default(),
            pending_auth: Default::default(),
            pending_authentication: Default::default(),
            connection_status_hint: Default::default(),
            latest_frame: Default::default(),
            eight_bit_video_presenter: Default::default(),
            eight_bit_layer_ready: Default::default(),
            eight_bit_layer_has_picture: Default::default(),
            eight_bit_layer_disabled: Default::default(),
            eight_bit_presentation_status: Default::default(),
            ten_bit_video_presenter: Default::default(),
            refine_hub: Default::default(),
            ten_bit_layer_ready: Default::default(),
            ten_bit_layer_has_picture: Default::default(),
            ten_bit_layer_disabled: Default::default(),
            ten_bit_presentation_status: Default::default(),
            dedicated_presenter_drops: Default::default(),
            dedicated_presenter_skips: Default::default(),
            layer_present_ms_samples: Default::default(),
            latest_native_frame_metadata: Default::default(),
            last_native_frame_size: Default::default(),
            presentation_recovery: Default::default(),
            decoder_backend_name: Default::default(),
            decoder_hardware_accelerated: Default::default(),
            server_hello: Default::default(),
            served_pipeline: Default::default(),
            served_source_fps: Default::default(),
            cursor_mode_result: Default::default(),
            tablet_mode_result: Default::default(),
            pending_cursor_shape: Default::default(),
            text_commit_results: Default::default(),
            host_keymap: Default::default(),
            pending_host_cursor: Default::default(),
            pending_host_pointer: Default::default(),
            microphone_active: Default::default(),
            display_update_result: Default::default(),
            host_health: Default::default(),
            host_limited_displays: Vec::new(),
            last_health_pong: Default::default(),
            last_health_pong_at: None,
            video_frames_seen: Default::default(),
            video_frames_decoded: Default::default(),
            presentation_superseded_frames: Default::default(),
            video_batch_high_water: Default::default(),
            audio_batch_high_water: Default::default(),
            audio_frames_seen: Default::default(),
            audio_bytes_accepted: Default::default(),
            last_audio_queued_ms: Default::default(),
            audio_playback_underruns: Default::default(),
            audio_buffer_trim_events: Default::default(),
            audio_buffer_trimmed_samples: Default::default(),
            last_audio_feed_gap_ms: Default::default(),
            max_audio_feed_gap_ms: Default::default(),
            audio_decode_failures: Default::default(),
            audio_concealed_frames: Default::default(),
            audio_queue_phase: Default::default(),
            last_wire_video_summary: Default::default(),
            last_audio_summary: Default::default(),
            last_decode_error: Default::default(),
            decoder_refusal: Default::default(),
            last_decode_ms: Default::default(),
            decode_ms_samples: Default::default(),
            video_packet_times: Default::default(),
            last_wire_clock_age_ms: Default::default(),
            last_wire_delay_ms: Default::default(),
            wire_delay_estimator: Default::default(),
            video_wire_clock_age_samples: Default::default(),
            video_wire_delay_samples: Default::default(),
            audio_wire_clock_age_samples: Default::default(),
            audio_wire_delay_samples: Default::default(),
            waiting_for_keyframe: Default::default(),
            inbox: Default::default(),
            ingress_idr_requests: Default::default(),
            malformed_media_packets: Default::default(),
            ui_wake_media_ready: Default::default(),
            generation: Default::default(),
            end: Default::default(),
            closed: Default::default(),
            error: Default::default(),
            multi_monitor_decode_roster: Default::default(),
            multi_monitor_root_monitor_id: Default::default(),
            secondary_frames: Default::default(),
            monitor_media: Default::default(),
            decoder_behind: Default::default(),
            monitor_wire_delay_samples: Default::default(),
            monitor_presented_frames: Default::default(),
            monitor_presentation_targets: Default::default(),
            routed_primary_dedicated_eight_bit: false,
            frame_sync: Default::default(),
        }
    }
}

impl<M: MediaPlatform> SharedMediaState<M> {
    /// Arms per-monitor decode for a validated multi-window layout, once
    /// (a later call never overwrites the first arm): the media roster the
    /// worker routes by, the monitor the root window presents
    /// (`latest_frame`; the others land in `secondary_frames`), and each
    /// monitor's local display (name and refresh, from `displays`) for the
    /// per-display statistics.
    pub fn arm_multi_monitor_decode(
        &mut self,
        validated: &crate::multi_window::session::ValidatedAppliedTopology,
        root_monitor_id: Option<arcen_media::SessionMonitorId>,
        displays: &[arcen_protocol::messages::ClientMonitor],
    ) {
        if self.multi_monitor_decode_roster.is_some() {
            return;
        }
        let display_targets: BTreeMap<u32, (String, u32)> = displays
            .iter()
            .map(|display| (display.id, (display.name.clone(), display.refresh_hz)))
            .collect();
        self.multi_monitor_decode_roster = Some((validated.generation, validated.media_roster()));
        self.multi_monitor_root_monitor_id = root_monitor_id;
        self.monitor_presentation_targets = validated
            .monitors
            .iter()
            .map(|monitor| {
                (
                    monitor.session_monitor_id,
                    MonitorPresentationTarget {
                        deck_display_id: monitor.display_id,
                        deck_display_name: display_targets
                            .get(&monitor.display_id)
                            .map(|(name, _)| name)
                            .cloned()
                            .unwrap_or_else(|| format!("Display {}", monitor.display_id)),
                        display_refresh_hz: display_targets
                            .get(&monitor.display_id)
                            .map_or(0, |(_, refresh_hz)| *refresh_hz),
                    },
                )
            })
            .collect();
    }

    /// A secondary monitor's decoded frame as its own window's dedicated
    /// presenter takes it: the pipeline (eight- or ten-bit, the primary's
    /// routing rule) and the frame with the served pipeline's timing. `None`
    /// when the frame carries no native picture for a dedicated presenter.
    #[must_use]
    pub fn presentable_secondary_frame(
        &self,
        frame: &DecodedVideoFrame<M::NativeFrame>,
    ) -> Option<(
        crate::present::PresenterPipeline,
        PresentableFrame<M::NativeFrame>,
    )> {
        let native = frame.native.as_ref()?;
        let pipeline = if should_use_dedicated_ten_bit_layer(frame, false) {
            crate::present::PresenterPipeline::TenBit
        } else if should_use_dedicated_eight_bit_layer(frame, false) {
            crate::present::PresenterPipeline::EightBit
        } else {
            return None;
        };
        Some((
            pipeline,
            PresentableFrame {
                picture: native.picture.clone(),
                contract: native.video,
                arrival: Instant::now(),
                source_fps: None,
                refine: None,
                timing: present_timing_for_state(self),
            },
        ))
    }

    /// The state of a new connection attempt: nothing decoded, and
    /// `waiting_for_keyframe` when it resumes a session.
    #[must_use]
    pub fn fresh(generation: u64, waiting_for_keyframe: bool) -> Self {
        Self {
            generation,
            waiting_for_keyframe,
            ..Self::default()
        }
    }
}

/// Starts the media worker for one connection attempt. It owns a fresh
/// root decoder (from `decoders`, created on the worker thread) and starts
/// the audio worker with `audio_player`; both end with the session.
///
/// # Panics
///
/// Panics if the OS cannot spawn the thread, as the macOS Deck always has.
#[allow(
    clippy::too_many_arguments,
    reason = "the session's channels, the shared state and the platform's decoder and player"
)]
pub fn spawn_media_worker<M, F>(
    mut events: mpsc::UnboundedReceiver<SessionEvent>,
    media: IncomingMediaReceiver,
    commands: SessionCommandSender,
    shared: Arc<Mutex<SharedMediaState<M>>>,
    repaint: Arc<dyn RepaintRequester>,
    telemetry: Arc<ClientTelemetry>,
    decoders: F,
    audio_player: impl FnOnce() -> Box<dyn AudioPlayback> + Send + 'static,
) where
    M: MediaPlatform,
    F: VideoDecoderFactory<Decoder = M::Decoder>,
{
    std::thread::Builder::new()
        .name("media-worker".to_string())
        .spawn(move || {
            let repaint: &dyn RepaintRequester = repaint.as_ref();
            // Created on this thread on purpose: the platform decoder and
            // audio stream need not be Send; the worker owns them for its
            // lifetime.
            let mut decoder = decoders.create();
            // Host audio plays on its own worker, fed only by the audio
            // lane; this worker never decodes or plays audio.
            let audio_queue = media.audio_lane().clone();
            let _audio_queue_guard = CloseAudioQueueOnDrop(audio_queue.clone());
            let audio_status = shared
                .lock()
                .expect("media state poisoned")
                .audio_playback
                .clone();
            if let Err(error) = crate::runtime::audio_worker::spawn_audio_worker(
                audio_queue,
                audio_status,
                audio_player,
            ) {
                tracing::error!(
                    target: crate::log_target::AUDIO,
                    %error,
                    "audio worker could not start; host audio is unavailable"
                );
            }
            let mut full_frame_requests = FullFrameRequestGate::default();
            let mut host_cursor = arcen_input::cursor::DeckCursorState::default();
            let mut host_pointer = arcen_input::host_pointer::DeckHostPointer::default();
            let mut pending_ingress_idr = false;
            let mut last_heartbeat = Instant::now();
            let mut monitor_rate_snapshots: BTreeMap<
                arcen_media::SessionMonitorId,
                MonitorRateSnapshot,
            > = BTreeMap::new();
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
            let mut secondary_router: Option<MonitorFrameRouter<M::Decoder>> = None;
            // Seeded by `ServerHello` and applied to every decoder, including
            // the per-monitor ones the router builds later in the session.
            let mut session_color = SessionColor::default();
            let mut decode_latency_policy = arcen_media::video::DecodeLatencyPolicy::default();
            let mut decode_lag = crate::runtime::decode_lag::DecodeLagGuard::default();
            tracing::info!(target: crate::log_target::VIDEO, "media worker started");

            loop {
                let mut event = None;
                release_held_frames(
                    &shared,
                    secondary_router.as_ref(),
                    &commands,
                    &mut full_frame_requests,
                    &mut pending_ingress_idr,
                    &telemetry,
                    repaint,
                );
                let hold_wait = crate::runtime::frame_sync::wait_until_due(
                    shared
                        .lock()
                        .expect("media state poisoned")
                        .frame_sync
                        .next_due(),
                    Instant::now(),
                    HELD_FRAME_POLL,
                );
                let step = if let Some(wait) = hold_wait {
                    // A frame waits for its instant: read events without
                    // blocking so it is released on time.
                    if full_frame_requests.is_pending() {
                        let _ = send_due_full_frame_request(
                            &shared,
                            &commands,
                            &mut full_frame_requests,
                            pending_ingress_idr,
                        );
                    }
                    match events.try_recv() {
                        Ok(next) => {
                            event = Some(next);
                            WorkerLoopStep::Event
                        }
                        Err(mpsc::error::TryRecvError::Empty) => {
                            if !wait.is_zero() {
                                std::thread::sleep(wait);
                            }
                            WorkerLoopStep::PendingPolled
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            WorkerLoopStep::Disconnected
                        }
                    }
                } else {
                    service_worker_loop_step(
                        &shared,
                        &commands,
                        &mut full_frame_requests,
                        pending_ingress_idr,
                        |blocking| {
                            if blocking {
                                Ok(events.blocking_recv())
                            } else {
                                events.try_recv().map(Some)
                            }
                        },
                        std::thread::sleep,
                        &mut event,
                    )
                };
                match step {
                    WorkerLoopStep::PendingPolled => continue,
                    WorkerLoopStep::Disconnected => break,
                    WorkerLoopStep::Event => {}
                }
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
                            target: crate::log_target::SESSION,
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
                        session_color = SessionColor::from_hello(&hello);
                        decode_latency_policy = hello
                            .active_pipeline
                            .as_ref()
                            .and_then(arcen_media::video::PipelineId::from_served_wire)
                            .map_or_else(arcen_media::video::DecodeLatencyPolicy::default, |id| {
                                arcen_media::video::pipeline_contract(id).decode_latency
                            });
                        {
                            let mut state = shared.lock().expect("media state poisoned");
                            state.server_hello = Some(hello);
                        }
                        tracing::info!(
                            target: crate::log_target::VIDEO,
                            primaries = session_color.primaries.token(),
                            transfer = session_color.transfer.token(),
                            hdr = matches!(
                                session_color.transfer,
                                TransferCharacteristics::Pq | TransferCharacteristics::Hlg
                            ),
                            "deck resolved session colour from host caps",
                        );
                        decoder.set_session_color(session_color);
                        decoder.set_decode_latency_policy(decode_latency_policy);
                        if let Some(router) = secondary_router.as_mut() {
                            router.set_session_color(session_color);
                            router.set_decode_latency_policy(decode_latency_policy);
                        }
                        full_frame_requests.request();
                        let _ = full_frame_requests.send_due(&commands);
                        repaint.request_repaint();
                    }
                    SessionEvent::Json(value) => {
                        let mut state = shared.lock().expect("media state poisoned");
                        // Results the UI acts on wake it; health samples
                        // wait for its next pass.
                        let mut wake = true;
                        match msg_type(&value) {
                            Some(HEALTH_STATS) => {
                                if let Ok(stats) = serde_json::from_value::<HealthStatsMsg>(value) {
                                    let served_fps = served_fps_from_health(&stats);
                                    note_host_limited_displays(
                                        &mut state.host_limited_displays,
                                        &stats,
                                    );
                                    state.host_health = Some(stats);
                                    if served_fps.is_some() {
                                        state.served_source_fps = served_fps;
                                    }
                                }
                                wake = false;
                            }
                            Some(HEALTH_PONG) => {
                                if let Ok(pong) = serde_json::from_value::<HealthPongMsg>(value) {
                                    state.last_health_pong = Some(pong);
                                    state.last_health_pong_at = Some(Instant::now());
                                }
                                wake = false;
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
                                        target: crate::log_target::SESSION,
                                        shape = ?msg.shape,
                                        "cursor shape received from host"
                                    );
                                    state.pending_cursor_shape = Some(msg.shape);
                                }
                            }
                            Some(TEXT_COMMIT_RESULT) => {
                                if let Ok(result) =
                                    serde_json::from_value::<TextCommitResultMsg>(value)
                                {
                                    if state.text_commit_results.len() < 32 {
                                        state.text_commit_results.push(result);
                                    }
                                }
                            }
                            Some(HOST_KEYMAP) => {
                                if let Ok(keymap) = serde_json::from_value::<HostKeymapMsg>(value) {
                                    state.host_keymap = Some(keymap);
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
                            // The session loop hands `audio_stream_result`
                            // to the audio worker itself, on receipt.
                            Some(AUDIO_STREAM_RESULT) => wake = false,
                            Some(SERVED_PIPELINE) => {
                                if let Ok(message) =
                                    serde_json::from_value::<ServedPipelineMsg>(value)
                                {
                                    state.served_pipeline = Some(message);
                                }
                            }
                            _ => wake = false,
                        }
                        if wake {
                            repaint.request_repaint();
                        }
                    }
                    SessionEvent::CursorUpdate(bytes) => match host_cursor.apply(&bytes) {
                        Ok(true) => {
                            shared
                                .lock()
                                .expect("media state poisoned")
                                .pending_host_cursor = Some(host_cursor.current());
                            repaint.request_repaint();
                        }
                        Ok(false) => {}
                        Err(error) => tracing::debug!(
                            target: crate::log_target::SESSION,
                            %error,
                            "host cursor record dropped"
                        ),
                    },
                    SessionEvent::CursorPosition(bytes) => match host_pointer.apply(&bytes) {
                        Ok(Some(position)) => {
                            shared
                                .lock()
                                .expect("media state poisoned")
                                .pending_host_pointer = Some(position);
                            repaint.request_repaint();
                        }
                        Ok(None) => {}
                        Err(error) => tracing::debug!(
                            target: crate::log_target::SESSION,
                            %error,
                            "host pointer record dropped"
                        ),
                    },
                    SessionEvent::CursorLaneReset => {
                        host_cursor.reset_lane();
                        host_pointer.reset_lane();
                    }
                    SessionEvent::MicrophoneActive(active) => {
                        shared
                            .lock()
                            .expect("media state poisoned")
                            .microphone_active = active;
                        repaint.request_repaint();
                    }
                    SessionEvent::MediaReady => {
                        {
                            let mut state = shared.lock().expect("media state poisoned");
                            state.ui_wake_media_ready = state.ui_wake_media_ready.saturating_add(1);
                        }
                        maybe_commit_secondary_router(
                            &shared,
                            &mut secondary_router,
                            session_color,
                            decode_latency_policy,
                            &decoders,
                        );
                        let mut batch = media.take_batch();
                        skip_video_if_behind(&shared, &media, &mut decode_lag, &mut batch);
                        handle_media_batch(
                            &shared,
                            &mut decoder,
                            &mut secondary_router,
                            &commands,
                            &mut full_frame_requests,
                            &mut pending_ingress_idr,
                            batch,
                            &telemetry,
                            repaint,
                        );
                        maybe_heartbeat(&shared, &mut last_heartbeat, &mut monitor_rate_snapshots);
                        maybe_report_timeline(&shared, &telemetry);
                        // No wake here: a decoded frame wakes the UI where it
                        // is published, and waking for every batch woke it
                        // ~50 times a second for audio alone.
                    }
                    SessionEvent::PresentationRecoveryRequested => {
                        request_presentation_recovery(&shared, &commands, &mut full_frame_requests);
                        repaint.request_repaint();
                    }
                    SessionEvent::ConnectionStatusHint(hint) => {
                        shared
                            .lock()
                            .expect("media state poisoned")
                            .connection_status_hint = hint;
                        repaint.request_repaint();
                    }
                    SessionEvent::Ended(end) => {
                        let error = session_end_worker_error(&end);
                        shared.lock().expect("media state poisoned").end = Some(end);
                        finish(&shared, repaint, error);
                        return;
                    }
                }
            }
            finish(&shared, repaint, None);
        })
        .expect("failed to spawn media worker");
}

/// Applies the dedicated presenters' late outcomes (a device loss, a
/// failure the presenter thread reached after `present` returned) on the UI
/// thread: a fallback tears the presenter down, disables its pipeline for
/// the connection and asks for recovery; otherwise the established status
/// survives a transient miss. Returns `true` when the caller must request a
/// full frame (`SessionEvent::PresentationRecoveryRequested`), as the macOS
/// Deck's `apply_async_*_status` do.
///
/// Never holds the media state's lock and a presenter's at the same time,
/// like the media worker.
///
/// # Panics
///
/// If the media state or a presenter mutex is poisoned, as everywhere in
/// the media worker.
pub fn apply_presenter_async_statuses<M: MediaPlatform>(
    shared: &Mutex<SharedMediaState<M>>,
) -> bool {
    let (eight_bit, ten_bit) = {
        let state = shared.lock().expect("media state poisoned");
        (
            state.eight_bit_video_presenter.clone(),
            state.ten_bit_video_presenter.clone(),
        )
    };
    let mut needs_recovery = false;
    if let Some(presenter) = eight_bit {
        if let Some(outcome) = take_async_outcome(&presenter) {
            let has_picture = presenter_has_picture(&presenter);
            let mut state = shared.lock().expect("media state poisoned");
            let established = outcome_of(state.eight_bit_presentation_status).is_presented()
                && !state.eight_bit_layer_disabled
                && has_picture;
            let status =
                preserve_established(state.eight_bit_presentation_status, outcome, established);
            state.eight_bit_presentation_status = status;
            let status = outcome_of(status);
            if status.is_fallback() {
                state.eight_bit_layer_disabled = true;
                state.eight_bit_layer_has_picture = false;
                needs_recovery = true;
            } else {
                state.eight_bit_layer_has_picture =
                    status.is_presented() && !state.eight_bit_layer_disabled && has_picture;
            }
        }
    }
    if let Some(presenter) = ten_bit {
        if let Some(outcome) = take_async_outcome(&presenter) {
            let has_picture = presenter_has_picture(&presenter);
            let mut state = shared.lock().expect("media state poisoned");
            let established = outcome_of(state.ten_bit_presentation_status).is_presented()
                && !state.ten_bit_layer_disabled
                && has_picture;
            let status =
                preserve_established(state.ten_bit_presentation_status, outcome, established);
            state.ten_bit_presentation_status = status;
            let status = outcome_of(status);
            if status.is_fallback() {
                state.ten_bit_layer_disabled = true;
                state.ten_bit_layer_has_picture = false;
                needs_recovery = true;
            } else {
                state.ten_bit_layer_has_picture =
                    status.is_presented() && !state.ten_bit_layer_disabled && has_picture;
            }
        }
    }
    needs_recovery
}

/// One UI pass's presenter statistics, per pipeline.
#[derive(Debug, Clone, Default)]
pub struct PresenterPass {
    /// The eight-bit presenter's statistics since the last pass.
    pub eight_bit: PresenterTelemetryDelta,
    /// The ten-bit presenter's statistics since the last pass.
    pub ten_bit: PresenterTelemetryDelta,
}

/// Once per UI pass: takes the presenters' statistics and records in the
/// shared state whether each pipeline has an established picture on glass
/// (presented, not disabled, and the presenter confirms a picture), which
/// is what lets the media worker keep a `Presented` status across a
/// transient miss. The macOS Deck's `drain_dedicated_presenter_telemetry`
/// and `refresh_dedicated_presenter_picture_state` do the same for its
/// own fields. Returns the statistics for the window's own counters.
///
/// # Panics
///
/// If the media state or a presenter mutex is poisoned.
pub fn drain_presenters<M: MediaPlatform>(shared: &Mutex<SharedMediaState<M>>) -> PresenterPass {
    let (eight_bit, ten_bit) = {
        let state = shared.lock().expect("media state poisoned");
        (
            state.eight_bit_video_presenter.clone(),
            state.ten_bit_video_presenter.clone(),
        )
    };
    let eight_bit = eight_bit.map(|presenter| {
        let presenter = presenter.lock().expect("presenter poisoned");
        (
            presenter.take_telemetry(),
            presenter.has_presented_picture(),
        )
    });
    let ten_bit = ten_bit.map(|presenter| {
        let presenter = presenter.lock().expect("presenter poisoned");
        (
            presenter.take_telemetry(),
            presenter.has_presented_picture(),
        )
    });
    let (eight_bit, eight_bit_picture) = eight_bit.unwrap_or_default();
    let (ten_bit, ten_bit_picture) = ten_bit.unwrap_or_default();
    let mut state = shared.lock().expect("media state poisoned");
    state.dedicated_presenter_drops = state
        .dedicated_presenter_drops
        .saturating_add(ten_bit.drops.saturating_add(eight_bit.drops));
    state.dedicated_presenter_skips = state
        .dedicated_presenter_skips
        .saturating_add(ten_bit.skips.saturating_add(eight_bit.skips));
    state.ten_bit_layer_has_picture = outcome_of(state.ten_bit_presentation_status).is_presented()
        && !state.ten_bit_layer_disabled
        && ten_bit_picture;
    state.eight_bit_layer_has_picture = outcome_of(state.eight_bit_presentation_status)
        .is_presented()
        && !state.eight_bit_layer_disabled
        && eight_bit_picture;
    PresenterPass { eight_bit, ten_bit }
}

fn outcome_of<S: PresenterStatus>(status: S) -> PresentOutcome {
    status.into()
}

/// A presenter's late outcome; a fallback tears it down first.
fn take_async_outcome<P: VideoPresenterSink>(presenter: &Mutex<P>) -> Option<PresentOutcome> {
    let mut presenter = presenter.lock().expect("presenter poisoned");
    let outcome = presenter.take_async_status()?;
    if outcome.is_fallback() {
        presenter.teardown();
    }
    Some(outcome)
}

fn presenter_has_picture<P: VideoPresenterSink>(presenter: &Mutex<P>) -> bool {
    presenter
        .lock()
        .expect("presenter poisoned")
        .has_presented_picture()
}

/// Copies the audio worker's snapshot into the fields the UI and heartbeat
/// read. Takes the audio cell's lock briefly; the audio worker never takes
/// the media state's lock, so the order cannot deadlock.
pub fn mirror_audio_playback<M: MediaPlatform>(state: &mut SharedMediaState<M>) {
    let audio = crate::runtime::audio_worker::audio_playback_snapshot(&state.audio_playback);
    state.audio_frames_seen = audio.frames_seen;
    state.audio_batch_high_water = audio.batch_high_water;
    state.audio_bytes_accepted = audio.bytes_accepted;
    state.last_audio_queued_ms = audio.queued_ms;
    state.audio_playback_underruns = audio.playback_underruns;
    state.audio_buffer_trim_events = audio.buffer_trim_events;
    state.audio_buffer_trimmed_samples = audio.buffer_trimmed_samples;
    state.last_audio_feed_gap_ms = audio.feed_gap_ms;
    state.max_audio_feed_gap_ms = audio.max_feed_gap_ms;
    state.audio_decode_failures = audio.decode_failures;
    state.audio_concealed_frames = audio.concealed_frames;
    state.audio_queue_phase = audio.queue_phase;
    if !audio.summary.is_empty() {
        state.last_audio_summary = audio.summary;
    }
    state.audio_wire_clock_age_samples = audio.wire_clock_age_samples;
    state.audio_wire_delay_samples = audio.wire_delay_samples;
    state.inbox.audio_dropped_packets = audio.queue_overflow_drops;
    state.inbox.audio_high_water_depth = audio.queue_high_water;
    state.inbox.malformed_audio_packets = audio.malformed_frames;
}

/// Ends the audio worker with the media worker, however it returns.
struct CloseAudioQueueOnDrop(crate::runtime::audio_lane::AudioLaneReceiver);

impl Drop for CloseAudioQueueOnDrop {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn session_end_worker_error(end: &SessionEnd) -> Option<String> {
    match end.reason {
        DisconnectReason::Terminal(
            TerminalDisconnect::Manual | TerminalDisconnect::GracefulHostClose,
        ) => None,
        _ => Some(end.message.clone()),
    }
}

fn finish<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    repaint: &dyn RepaintRequester,
    error: Option<String>,
) {
    match &error {
        Some(err) => tracing::error!(
            target: crate::log_target::SESSION,
            error = %err,
            "media worker stopped on error",
        ),
        None => tracing::info!(
            target: crate::log_target::SESSION,
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

fn service_worker_loop_step<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: bool,
    mut recv: impl FnMut(bool) -> Result<Option<SessionEvent>, mpsc::error::TryRecvError>,
    sleep: impl FnOnce(Duration),
    event: &mut Option<SessionEvent>,
) -> WorkerLoopStep {
    if full_frame_requests
        .retry_after()
        .is_some_and(|delay| delay.is_zero())
    {
        let _ =
            send_due_full_frame_request(shared, commands, full_frame_requests, pending_ingress_idr);
    }
    if full_frame_requests.is_pending() {
        match recv(false) {
            Ok(Some(next)) => {
                *event = Some(next);
                WorkerLoopStep::Event
            }
            Ok(None) => WorkerLoopStep::Disconnected,
            Err(mpsc::error::TryRecvError::Empty) => {
                if !send_due_full_frame_request(
                    shared,
                    commands,
                    full_frame_requests,
                    pending_ingress_idr,
                ) {
                    sleep(
                        full_frame_requests
                            .retry_after()
                            .unwrap_or_default()
                            .min(Duration::from_millis(10)),
                    );
                }
                WorkerLoopStep::PendingPolled
            }
            Err(mpsc::error::TryRecvError::Disconnected) => WorkerLoopStep::Disconnected,
        }
    } else {
        *event = recv(true).unwrap_or(None);
        if event.is_some() {
            WorkerLoopStep::Event
        } else {
            WorkerLoopStep::Disconnected
        }
    }
}

/// Emit an INFO "stream healthy" heartbeat at most every `HEARTBEAT_INTERVAL`.
/// This is the light-level "OK working" signal a sysadmin greps for; DEBUG
/// carries the per-event drop/keyframe/decode detail between heartbeats.
fn maybe_heartbeat<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    last: &mut Instant,
    monitor_rate_snapshots: &mut BTreeMap<arcen_media::SessionMonitorId, MonitorRateSnapshot>,
) {
    let now = Instant::now();
    let elapsed = now.duration_since(*last);
    if elapsed < HEARTBEAT_INTERVAL {
        return;
    }
    *last = now;
    let mut state = shared.lock().expect("media state poisoned");
    mirror_audio_playback(&mut state);
    tracing::info!(
        target: crate::log_target::VIDEO,
        frames_seen = state.video_frames_seen,
        frames_decoded = state.video_frames_decoded,
        presentation_superseded_frames = state.presentation_superseded_frames,
        video_batch_high_water = state.video_batch_high_water,
        audio_batch_high_water = state.audio_batch_high_water,
        audio_frames = state.audio_frames_seen,
        decode_ms = state.last_decode_ms,
        wire_clock_age_ms = ?state.last_wire_clock_age_ms,
        wire_delay_ms = ?state.last_wire_delay_ms,
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
    // Its own record: the heartbeat above is at the per-record field cap.
    // Audio heard = its wire age + what waits in the playout queue.
    let video_clock_age =
        rounded_percentiles_ms(state.video_wire_clock_age_samples.iter().copied());
    let video_delay = rounded_percentiles_ms(state.video_wire_delay_samples.iter().copied());
    let audio_clock_age =
        rounded_percentiles_ms(state.audio_wire_clock_age_samples.iter().copied());
    let audio_delay = rounded_percentiles_ms(state.audio_wire_delay_samples.iter().copied());
    tracing::info!(
        target: crate::log_target::VIDEO,
        video_clock_age_p50_ms = ?video_clock_age.map(|age| age.p50_ms),
        video_clock_age_p95_ms = ?video_clock_age.map(|age| age.p95_ms),
        video_delay_p50_ms = ?video_delay.map(|delay| delay.p50_ms),
        video_delay_p95_ms = ?video_delay.map(|delay| delay.p95_ms),
        audio_clock_age_p50_ms = ?audio_clock_age.map(|age| age.p50_ms),
        audio_clock_age_p95_ms = ?audio_clock_age.map(|age| age.p95_ms),
        audio_delay_p50_ms = ?audio_delay.map(|delay| delay.p50_ms),
        audio_delay_p95_ms = ?audio_delay.map(|delay| delay.p95_ms),
        audio_queued_ms = state.last_audio_queued_ms,
        audio_heard_ms = ?audio_delay.map(|delay| delay.p50_ms + state.last_audio_queued_ms as i64),
        "stream delay",
    );
    // One bounded record per negotiated monitor (never more than
    // `MAX_MULTI_MONITOR_COUNT`), emitted only for multi-monitor sessions.
    // Without this, a session-wide aggregate cannot say which viewport was
    // starved -- the exact gap that left the pier-windows.example.internal stutter
    // undiagnosable from client telemetry alone.
    for (monitor_id, counters) in &state.monitor_media {
        let presented = state
            .monitor_presented_frames
            .get(monitor_id)
            .copied()
            .unwrap_or_default();
        let current = MonitorRateSnapshot::from_counts(counters, presented);
        let target = state.monitor_presentation_targets.get(monitor_id);
        let rates = monitor_stream_window(
            elapsed,
            monitor_rate_snapshots.get(monitor_id).copied(),
            current,
            target.map_or(0, |target| target.display_refresh_hz),
        );
        monitor_rate_snapshots.insert(*monitor_id, current);
        let video_delay = state
            .monitor_wire_delay_samples
            .get(monitor_id)
            .and_then(|samples| rounded_percentiles_ms(samples.iter().copied()));
        tracing::info!(
            target: crate::log_target::VIDEO,
            monitor_id = monitor_id.get(),
            video_delay_p50_ms = ?video_delay.map(|delay| delay.p50_ms),
            video_delay_p95_ms = ?video_delay.map(|delay| delay.p95_ms),
            fps_received = rates.fps_received,
            fps_presented = rates.fps_presented,
            frames_dropped = rates.frames_dropped,
            display_refresh_hz = target.map_or(0, |target| target.display_refresh_hz),
            frames_superseded_by_refresh = rates.frames_superseded_by_refresh,
            frames_dropped_before_presentation = rates.frames_dropped_before_presentation,
            deck_display_id = target.map_or(0, |target| target.deck_display_id),
            deck_display_name = target.map_or("", |target| target.deck_display_name.as_str()),
            "per-monitor stream delay",
        );
        tracing::info!(
            target: crate::log_target::VIDEO,
            monitor_id = monitor_id.get(),
            frames_received = counters.frames_received,
            frames_decoded = counters.frames_decoded,
            frames_rejected = counters.frames_rejected,
            presentation_superseded = counters.presentation_superseded,
            frames_superseded_by_refresh = counters.frames_superseded_by_refresh,
            waiting_for_keyframe = counters.waiting_for_keyframe,
            decode_ms = counters.last_decode_ms,
            average_decode_ms = counters.average_decode_ms(),
            "per-monitor stream health",
        );
    }
    monitor_rate_snapshots.retain(|monitor_id, _| state.monitor_media.contains_key(monitor_id));
}

/// One-way arm: builds the additive per-monitor `secondary_router` the
/// first (and only) time the UI thread's
/// `SharedMediaState::multi_monitor_decode_roster` is observed to be
/// `Some` -- set at topology acceptance, deliberately before window
/// binding/presentation commit, so decoder isolation begins as early as
/// possible. Idempotent and cheap when already built or still `None` (the
/// default, legacy path). Building can fail only if the UI thread armed an
/// invalid roster, which its own validation
/// (`arcen_deck::ui::multi_window_session`) must never do; on that defensive
/// failure the worker logs and stays on the legacy-only path rather than
/// panicking, since the legacy `decoder` above is entirely unaffected either
/// way.
fn maybe_commit_secondary_router<M, F>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    secondary_router: &mut Option<MonitorFrameRouter<M::Decoder>>,
    session_color: SessionColor,
    decode_latency_policy: arcen_media::video::DecodeLatencyPolicy,
    decoders: &F,
) where
    M: MediaPlatform,
    F: VideoDecoderFactory<Decoder = M::Decoder>,
{
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
    match MonitorFrameRouter::new_with_media_roster_from_factory(
        generation,
        &media_roster,
        decoders,
    ) {
        Ok(router) => {
            tracing::info!(
                target: crate::log_target::VIDEO,
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
            router.set_decode_latency_policy(decode_latency_policy);
            *secondary_router = Some(router);
        }
        Err(error) => {
            tracing::error!(
                target: crate::log_target::VIDEO,
                %error,
                "UI thread committed an invalid multi-monitor roster; staying on legacy routing",
            );
        }
    }
}

/// An interactive contract whose decoder fell behind the stream skips the
/// batch's video to the next keyframe of each monitor it carries none for
/// ([`crate::runtime::decode_lag`]): the picture stays with the sound
/// instead of drifting behind it until the inbox overflows.
fn skip_video_if_behind<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    media: &IncomingMediaReceiver,
    guard: &mut crate::runtime::decode_lag::DecodeLagGuard,
    batch: &mut IncomingMediaBatch,
) {
    let interactive = {
        let state = shared.lock().expect("media state poisoned");
        !present_timing_for_state(&state).waits_for_vsync()
    };
    let crate::runtime::decode_lag::DecodeLag::SkipAhead { waited } =
        guard.judge(batch.oldest_video_wait, interactive)
    else {
        return;
    };
    let keyed: std::collections::BTreeSet<u16> = batch
        .video
        .iter()
        .filter(|(header, _)| header.is_keyframe())
        .map(|(header, _)| header.monitor_id)
        .collect();
    let behind: Vec<u16> = batch
        .video
        .iter()
        .map(|(header, _)| header.monitor_id)
        .filter(|monitor_id| !keyed.contains(monitor_id))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if behind.is_empty() {
        return;
    }
    let (mut packets, mut bytes) = (0_u64, 0_u64);
    batch.video.retain(|(header, payload)| {
        let skip = behind.contains(&header.monitor_id);
        if skip {
            packets += 1;
            bytes += payload.len() as u64;
        }
        !skip
    });
    media.skip_to_keyframe(&behind, (packets, bytes));
    for monitor_id in &behind {
        if !batch.video_discontinuity_monitor_ids.contains(monitor_id) {
            batch.video_discontinuity_monitor_ids.push(*monitor_id);
        }
        if !batch.idr_needed_monitor_ids.contains(monitor_id) {
            batch.idr_needed_monitor_ids.push(*monitor_id);
        }
    }
    batch.video_discontinuity = true;
    batch.idr_needed = true;
    let skips = {
        let mut state = shared.lock().expect("media state poisoned");
        state.decoder_behind.note(waited);
        state.decoder_behind.skips
    };
    tracing::warn!(
        target: crate::log_target::VIDEO,
        waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        monitors = ?behind,
        skipped_packets = packets,
        skips,
        "the decoder fell behind the stream: video skipped ahead to the next keyframe",
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_media_batch<M, D>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    decoder: &mut D,
    secondary_router: &mut Option<MonitorFrameRouter<M::Decoder>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
    batch: IncomingMediaBatch,
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
) where
    M: MediaPlatform,
    D: VideoDecoderBackend<NativeFrame = M::NativeFrame>,
{
    {
        let now = Instant::now();
        let mut state = shared.lock().expect("media state poisoned");
        state.video_frames_seen = batch.telemetry.video_received;
        state.video_batch_high_water = state.video_batch_high_water.max(batch.video.len());
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
        for (header, _) in &batch.video {
            let clock_age = wire_clock_age_ms(header.timestamp_ms);
            let delay = state.wire_delay_estimator.observe(clock_age);
            if let Ok(monitor_id) = arcen_media::SessionMonitorId::new(header.monitor_id) {
                push_ms_sample(
                    state
                        .monitor_wire_delay_samples
                        .entry(monitor_id)
                        .or_default(),
                    f64::from(delay),
                );
            }
        }
        if let Some((header, payload)) = batch.video.last() {
            let clock_age = wire_clock_age_ms(header.timestamp_ms);
            let delay = state.wire_delay_estimator.observe(clock_age);
            state.last_wire_clock_age_ms = Some(clock_age);
            state.last_wire_delay_ms = Some(delay);
            push_ms_sample(
                &mut state.video_wire_clock_age_samples,
                f64::from(clock_age),
            );
            push_ms_sample(&mut state.video_wire_delay_samples, f64::from(delay));
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

    if batch.video_discontinuity {
        for monitor_id in &batch.video_discontinuity_monitor_ids {
            if *monitor_id == 0 {
                decoder.notify_discontinuity();
            } else if let Some(router) = secondary_router.as_mut() {
                if !router.notify_route_discontinuity(*monitor_id) {
                    tracing::debug!(
                        target: crate::log_target::VIDEO,
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
            target: crate::log_target::VIDEO,
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
        let _ = send_due_full_frame_request(
            shared,
            commands,
            full_frame_requests,
            *pending_ingress_idr,
        );
    }

    if decode_batch(
        shared,
        decoder,
        secondary_router,
        commands,
        full_frame_requests,
        pending_ingress_idr,
        &batch.video,
        telemetry,
        repaint,
    ) {
        keep_or_cancel_full_frame_gate_after_recovery(
            shared,
            full_frame_requests,
            pending_ingress_idr,
        );
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
fn decode_batch<M, PrimaryDecoder, RouteDecoder>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    decoder: &mut PrimaryDecoder,
    secondary_router: &mut Option<MonitorFrameRouter<RouteDecoder>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
    packets: &[(VideoHeader, Vec<u8>)],
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
) -> bool
where
    M: MediaPlatform,
    PrimaryDecoder: VideoDecoderBackend<NativeFrame = M::NativeFrame>,
    RouteDecoder: VideoDecoderBackend<NativeFrame = M::NativeFrame>,
{
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
            pending_ingress_idr,
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
            target: crate::log_target::VIDEO,
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
                target: crate::log_target::VIDEO,
                monitor_id = header.monitor_id,
                "dropping multi-monitor packet on the legacy single-decoder path; no committed \
                 router yet",
            );
            full_frame_requests.request();
            let _ = full_frame_requests.send_due(commands);
            continue;
        }
        let prefer_native_biplanar = {
            let state = shared.lock().expect("media state poisoned");
            (state.eight_bit_video_presenter.is_some()
                && state.eight_bit_layer_ready
                && !state.eight_bit_layer_disabled)
                || (state.ten_bit_video_presenter.is_some()
                    && state.ten_bit_layer_ready
                    && !state.ten_bit_layer_disabled)
        };
        let native_preference_changed = decoder.set_prefer_native_frames(prefer_native_biplanar);
        request_decoder_recovery_for_native_preference_change(
            native_preference_changed,
            shared,
            commands,
            full_frame_requests,
        );
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
                    commands,
                    full_frame_requests,
                    pending_ingress_idr,
                );
            }
            Ok(None) => {
                if decoder.wants_keyframe() {
                    decoded_keyframe = false;
                    tracing::debug!(
                        target: crate::log_target::VIDEO,
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
                    target: crate::log_target::VIDEO,
                    %error,
                    "video decode error",
                );
                let mut state = shared.lock().expect("media state poisoned");
                state.last_decode_error = Some(error.to_string());
                note_decoder_refusal(&mut state, &error, None);
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
/// keyframe" optimization: each monitor's own decoder (inside
/// its `MonitorSlot`) independently tracks whether it wants a keyframe, and
/// any per-monitor admission/decode failure falls back to the same global
/// full-frame-request gate the legacy path uses (multi-monitor-v1's wire has
/// no per-monitor full-frame request today). This is an intentional,
/// honestly-scoped simplification for this additive, still env-gated-off
/// path; the legacy hot path above is completely unaffected.
#[allow(clippy::too_many_arguments)]
fn decode_secondary_packets<M, RouteDecoder>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    router: &mut MonitorFrameRouter<RouteDecoder>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
    packets: &[(VideoHeader, Vec<u8>)],
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
) -> SecondaryDecodeOutcome
where
    M: MediaPlatform,
    RouteDecoder: VideoDecoderBackend<NativeFrame = M::NativeFrame>,
{
    // The monitor the Deck root viewport presents. Defaults to the router's
    // explicit negotiated primary, but multi-window presentation can choose
    // a faster local display for root so a slow primary display cannot pace
    // every immediate secondary viewport.
    let root_monitor_id = shared
        .lock()
        .expect("media state poisoned")
        .multi_monitor_root_monitor_id
        .or_else(|| router.primary_monitor_id());
    let mut decoded_any = false;
    for (header, payload) in packets {
        let monitor_id = arcen_media::SessionMonitorId::new(header.monitor_id).ok();
        let route = MonitorRoute::from_wire_monitor_id(header.monitor_id);
        let root_route = is_primary_route(route, root_monitor_id);
        if root_route {
            if let Some(root_id) = root_monitor_id {
                let prefer_native_biplanar = {
                    let state = shared.lock().expect("media state poisoned");
                    state.ten_bit_video_presenter.is_some()
                        && state.ten_bit_layer_ready
                        && !state.ten_bit_layer_disabled
                };
                let changed = router.set_prefer_native_biplanar(root_id, prefer_native_biplanar);
                request_decoder_recovery_for_native_preference_change(
                    changed,
                    shared,
                    commands,
                    full_frame_requests,
                );
            }
        }
        let started = Instant::now();
        let outcome = router.route_and_decode(header, payload);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
        match outcome {
            Ok(RouteOutcome::FreshFrame) => {
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
                // Every negotiated monitor's frame passes the shared
                // timeline (ADR 0026); one that must wait for the other
                // monitors is published later by `release_held_frames`.
                let (frame, early) = match route {
                    MonitorRoute::Negotiated(id) => shared
                        .lock()
                        .expect("media state poisoned")
                        .frame_sync
                        .admit(
                            id,
                            frame,
                            Instant::now(),
                            crate::runtime::frame_sync::now_wire_ms(),
                        ),
                    MonitorRoute::LegacyPrimary => (Some(frame), None),
                };
                if let Some(early) = early {
                    let early_root = Some(early.monitor) == root_monitor_id;
                    publish_released_frame(
                        shared,
                        early.monitor,
                        early_root,
                        early.frame,
                        telemetry,
                        repaint,
                        commands,
                        full_frame_requests,
                        pending_ingress_idr,
                    );
                }
                let superseded = match frame {
                    Some(frame) => publish_routed_frame(
                        shared,
                        route,
                        root_route,
                        frame,
                        telemetry,
                        repaint,
                        commands,
                        full_frame_requests,
                        pending_ingress_idr,
                    ),
                    None => false,
                };
                let mut state = shared.lock().expect("media state poisoned");
                // The routed path publishes into `latest_frame`/
                // `secondary_frames` directly rather than through
                // `publish_decoded_frame`, so every shared counter that
                // function maintains has to be updated here too. Omitting
                // this is what made the pier-windows.example.internal session report
                // `frames_decoded: 0` and `waiting_for_keyframe: true` for
                // its entire 126-second lifetime while it was in fact
                // decoding and presenting ~29 fps.
                record_monitor_decode(&mut state, monitor_id, elapsed_ms, superseded);
                if is_primary_route(route, root_monitor_id) {
                    if let Some(root_id) = root_monitor_id {
                        state.decoder_backend_name =
                            router.decoder_backend_name(root_id).unwrap_or("");
                        state.decoder_hardware_accelerated =
                            router.decoder_hardware_accelerated(root_id).flatten();
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
                    if let RouteError::Decode(decode_error) = &error {
                        note_decoder_refusal(&mut state, decode_error, monitor_id);
                    }
                }
                tracing::debug!(
                    target: crate::log_target::VIDEO,
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
            target: crate::log_target::VIDEO,
            "multi-monitor batch decoded fresh output for some monitors while at least one \
             sibling is still awaiting its own keyframe; keeping the full-frame request armed",
        );
    }
    SecondaryDecodeOutcome {
        decoded_any,
        all_recovered,
    }
}

/// Records a refusal ([`VideoDecodeError::Unsupported`]) for Stream
/// Statistics and the log, once per distinct reason.
fn note_decoder_refusal<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
    error: &VideoDecodeError,
    monitor_id: Option<arcen_media::SessionMonitorId>,
) {
    let VideoDecodeError::Unsupported(reason) = error else {
        return;
    };
    let refusal = match monitor_id {
        Some(monitor_id) => format!("monitor {}: {reason}", monitor_id.get()),
        None => reason.clone(),
    };
    if state.decoder_refusal.as_deref() != Some(refusal.as_str()) {
        tracing::warn!(
            target: crate::log_target::VIDEO,
            refusal = %refusal,
            "decoder refused the stream",
        );
        state.decoder_refusal = Some(refusal);
    }
}

/// Applies `update` to one monitor's counters, or does nothing when the
/// packet carried a wire id with no negotiated `SessionMonitorId` (wire id
/// 0, the legacy primary, which the routed path never admits).
fn monitor_counters<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
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
/// How long the worker sleeps at most between event polls while a frame
/// waits for its presentation instant.
const HELD_FRAME_POLL: Duration = Duration::from_millis(2);

/// Publishes one decoded frame of a routed (multi-monitor) session: the
/// root monitor's through the root presentation path, every other monitor's
/// into `secondary_frames`. Returns whether it replaced an unpresented frame.
#[allow(clippy::too_many_arguments)]
fn publish_routed_frame<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    route: MonitorRoute,
    root_route: bool,
    frame: DecodedVideoFrame<M::NativeFrame>,
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
) -> bool {
    if root_route {
        publish_routed_primary_frame(
            shared,
            frame,
            telemetry,
            repaint,
            commands,
            full_frame_requests,
            pending_ingress_idr,
        )
        .0
    } else {
        let mut state = shared.lock().expect("media state poisoned");
        match route {
            MonitorRoute::Negotiated(id) => state.secondary_frames.insert(id, frame).is_some(),
            MonitorRoute::LegacyPrimary => false,
        }
    }
}

/// Publishes a frame the timeline held, counting a frame it replaced as
/// superseded (its decode was counted when it decoded).
#[allow(clippy::too_many_arguments)]
fn publish_released_frame<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    monitor: arcen_media::SessionMonitorId,
    root_route: bool,
    frame: DecodedVideoFrame<M::NativeFrame>,
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
) {
    let superseded = publish_routed_frame(
        shared,
        MonitorRoute::Negotiated(monitor),
        root_route,
        frame,
        telemetry,
        repaint,
        commands,
        full_frame_requests,
        pending_ingress_idr,
    );
    if superseded {
        let mut state = shared.lock().expect("media state poisoned");
        state.presentation_superseded_frames =
            state.presentation_superseded_frames.saturating_add(1);
        monitor_counters(&mut state, Some(monitor), |counters| {
            counters.presentation_superseded = counters.presentation_superseded.saturating_add(1);
        });
    }
    repaint.request_repaint();
}

/// Publishes every held frame whose presentation instant has come.
fn release_held_frames<M, RouteDecoder>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    router: Option<&MonitorFrameRouter<RouteDecoder>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
) where
    M: MediaPlatform,
    RouteDecoder: VideoDecoderBackend<NativeFrame = M::NativeFrame>,
{
    let (due, root) = {
        let mut state = shared.lock().expect("media state poisoned");
        if state.frame_sync.next_due().is_none() {
            return;
        }
        let due = state
            .frame_sync
            .take_due(Instant::now(), crate::runtime::frame_sync::now_wire_ms());
        (due, state.multi_monitor_root_monitor_id)
    };
    let root = root.or_else(|| router.and_then(MonitorFrameRouter::primary_monitor_id));
    for held in due {
        publish_released_frame(
            shared,
            held.monitor,
            Some(held.monitor) == root,
            held.frame,
            telemetry,
            repaint,
            commands,
            full_frame_requests,
            pending_ingress_idr,
        );
    }
}

/// Logs the timeline's skew report when one is due.
fn maybe_report_timeline<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    telemetry: &ClientTelemetry,
) {
    let clock = telemetry.pier_clock();
    let report = shared
        .lock()
        .expect("media state poisoned")
        .frame_sync
        .take_report(crate::runtime::frame_sync::now_wire_ms(), clock);
    if let Some(report) = report {
        crate::runtime::frame_sync::log_report(&report);
    }
}

fn record_monitor_decode<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DedicatedLayerRoute {
    Egui,
    EightBit,
    TenBit,
}

pub(crate) fn dedicated_layer_route_for_video(
    video: Option<arcen_media::VideoConfiguration>,
    has_rgba: bool,
    eight_bit_disabled: bool,
    ten_bit_disabled: bool,
) -> DedicatedLayerRoute {
    if has_rgba {
        return DedicatedLayerRoute::Egui;
    }
    let Some(video) = video else {
        return DedicatedLayerRoute::Egui;
    };
    if !eight_bit_disabled
        && video.bit_depth == arcen_media::BitDepth::Eight
        && video.chroma == arcen_media::ChromaSubsampling::Yuv420
    {
        if matches!(
            video.transfer,
            arcen_media::TransferCharacteristics::Pq | arcen_media::TransferCharacteristics::Hlg
        ) {
            tracing::warn!(
                target: crate::log_target::VIDEO,
                transfer = video.transfer.token(),
                "host tagged an 8-bit stream as HDR; presenting on the SDR 8-bit path with EDR off"
            );
        }
        return DedicatedLayerRoute::EightBit;
    }
    if !ten_bit_disabled
        && video.bit_depth >= arcen_media::BitDepth::Ten
        && video.chroma == arcen_media::ChromaSubsampling::Yuv444
    {
        return DedicatedLayerRoute::TenBit;
    }
    DedicatedLayerRoute::Egui
}

pub(crate) fn should_use_dedicated_eight_bit_layer<N>(
    frame: &DecodedVideoFrame<N>,
    disabled: bool,
) -> bool {
    let video = frame.native.as_ref().map(|native| native.video);
    dedicated_layer_route_for_video(video, !frame.rgba.is_empty(), disabled, true)
        == DedicatedLayerRoute::EightBit
}

pub(crate) fn should_use_dedicated_ten_bit_layer<N>(
    frame: &DecodedVideoFrame<N>,
    disabled: bool,
) -> bool {
    let video = frame.native.as_ref().map(|native| native.video);
    dedicated_layer_route_for_video(video, !frame.rgba.is_empty(), true, disabled)
        == DedicatedLayerRoute::TenBit
}

fn record_primary_decode_state<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
    decode_ms: f64,
    decoder_backend_name: &'static str,
    decoder_hardware_accelerated: Option<bool>,
    telemetry: &ClientTelemetry,
) {
    state.last_decode_ms = decode_ms;
    push_ms_sample(&mut state.decode_ms_samples, decode_ms);
    state.last_decode_error = None;
    state.waiting_for_keyframe = false;
    state.video_frames_decoded = state.video_frames_decoded.saturating_add(1);
    state.decoder_backend_name = decoder_backend_name;
    state.decoder_hardware_accelerated = decoder_hardware_accelerated;
    telemetry.record_media(
        state.video_frames_seen,
        state.video_frames_decoded,
        state.inbox.video_dropped_packets,
        Duration::from_secs_f64((decode_ms / 1_000.0).max(0.0)),
    );
}

fn record_layer_submission<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
    present_elapsed: Duration,
) {
    push_ms_sample(
        &mut state.layer_present_ms_samples,
        present_elapsed.as_secs_f64() * 1000.0,
    );
}

fn publish_native_metadata<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
    frame: &DecodedVideoFrame<M::NativeFrame>,
    video: arcen_media::VideoConfiguration,
) -> bool {
    let size = [frame.width, frame.height];
    let changed = state.last_native_frame_size != Some(size);
    state.last_native_frame_size = Some(size);
    state.latest_native_frame_metadata = Some(NativePresentationMetadata {
        width: frame.width,
        height: frame.height,
        timestamp_ms: frame.timestamp_ms,
        pixel_format: frame.pixel_format.clone(),
        backend: frame.backend,
        video,
    });
    changed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TenBitFailureFallback {
    PublishRgbaFallback,
    KeepExistingFallback,
}

fn ten_bit_failure_fallback(has_rgba: bool) -> TenBitFailureFallback {
    if has_rgba {
        TenBitFailureFallback::PublishRgbaFallback
    } else {
        TenBitFailureFallback::KeepExistingFallback
    }
}

/// The refinement tag of a decoded ten-bit frame: the fence its access
/// unit carried, if the video lane named one.
fn refine_tag<N>(
    hub: Option<&arcen_media::refine::hub::DeckRefineHub>,
    frame: &DecodedVideoFrame<N>,
) -> Option<RefineFrameTag> {
    let hub = hub?;
    let (monitor_id, fence) = hub.frame_tag(frame.timestamp_ms)?;
    Some(RefineFrameTag {
        hub: hub.clone(),
        monitor_id,
        fence,
    })
}

fn publish_ten_bit_failure_fallback<M: MediaPlatform>(
    state: &mut SharedMediaState<M>,
    frame: DecodedVideoFrame<M::NativeFrame>,
) {
    // Exact tiles are never fed to the eight-bit path (ADR 0016 R7).
    if let Some(hub) = state.refine_hub.as_ref() {
        hub.set_monitor_enabled(arcen_media::refine::PRIMARY_MONITOR, false);
    }
    state.ten_bit_layer_disabled = true;
    clear_native_layer_history(state);
    let counts_as_fallback_present = !frame.rgba.is_empty();
    if ten_bit_failure_fallback(!frame.rgba.is_empty())
        == TenBitFailureFallback::PublishRgbaFallback
        && state.latest_frame.replace(frame).is_some()
    {
        state.presentation_superseded_frames =
            state.presentation_superseded_frames.saturating_add(1);
    }

    if counts_as_fallback_present {
        state.presentation_recovery.clear_after_fallback_handoff();
    }
}

fn clear_native_layer_history<M: MediaPlatform>(state: &mut SharedMediaState<M>) {
    state.layer_present_ms_samples.clear();
    state.eight_bit_layer_has_picture = false;
    state.ten_bit_layer_has_picture = false;
}

fn request_presentation_recovery<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
) {
    let sent = {
        let mut state = shared.lock().expect("media state poisoned");
        state.waiting_for_keyframe = true;
        state.presentation_recovery.arm(full_frame_requests);
        full_frame_requests.send_due(commands)
    };
    if sent {
        shared
            .lock()
            .expect("media state poisoned")
            .ingress_idr_requests += 1;
    }
}

fn presentation_recovery_pending<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
) -> bool {
    shared
        .lock()
        .expect("media state poisoned")
        .presentation_recovery
        .is_pending()
}

fn keep_or_cancel_full_frame_gate_after_recovery<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: &mut bool,
) {
    if presentation_recovery_pending(shared) {
        full_frame_requests.request();
        *pending_ingress_idr = false;
    } else {
        full_frame_requests.cancel_pending();
        *pending_ingress_idr = false;
    }
}

fn send_due_full_frame_request<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    pending_ingress_idr: bool,
) -> bool {
    if !full_frame_requests.send_due(commands) {
        return false;
    }
    let should_count = pending_ingress_idr || presentation_recovery_pending(shared);
    if should_count {
        shared
            .lock()
            .expect("media state poisoned")
            .ingress_idr_requests += 1;
    }
    true
}

fn request_decoder_recovery_for_native_preference_change<M: MediaPlatform>(
    changed: bool,
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
) {
    if !changed {
        return;
    }
    {
        let mut state = shared.lock().expect("media state poisoned");
        state.waiting_for_keyframe = true;
    }
    full_frame_requests.request();
    let _ = send_due_full_frame_request(shared, commands, full_frame_requests, true);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutedPrimaryPresentation {
    DedicatedEightBit,
    DedicatedTenBit,
    EguiFallback,
    DedicatedFailure,
}

/// Logs each change of the host displays the Pier reports below what their
/// content owed (`HealthStatsMsg.monitors`), with the reason: WARN when one
/// falls behind, INFO when every display keeps up again.
fn note_host_limited_displays(previous: &mut Vec<(u16, String)>, stats: &HealthStatsMsg) {
    let limited: Vec<(u16, String)> = stats
        .monitors
        .iter()
        .filter_map(|monitor| {
            monitor
                .limited_by_token()
                .map(|token| (monitor.session_monitor_id, token.to_owned()))
        })
        .collect();
    if limited == *previous {
        return;
    }
    match arcen_media::stats_overlay::host_display_rates_line(&stats.monitors) {
        Some(line) if !limited.is_empty() => tracing::warn!(
            target: crate::log_target::VIDEO,
            displays = %line,
            "the host reports a display below its rate"
        ),
        _ => tracing::info!(
            target: crate::log_target::VIDEO,
            "every host display keeps up with its rate again"
        ),
    }
    *previous = limited;
}

fn served_fps_from_health(stats: &HealthStatsMsg) -> Option<u32> {
    let fps = if stats.fps_target.is_finite() && stats.fps_target > 0.0 {
        stats.fps_target
    } else if stats.fps_actual.is_finite() && stats.fps_actual > 0.0 {
        stats.fps_actual
    } else {
        return None;
    };
    Some(fps.round().clamp(1.0, 240.0) as u32)
}

fn source_fps_for_state<M: MediaPlatform>(state: &SharedMediaState<M>) -> Option<u32> {
    state.served_source_fps
}

/// When the presenters show a frame: the shared timing of the pipeline the
/// host serves (its `served_pipeline` message, else its hello).
pub fn present_timing_for_state<M: MediaPlatform>(
    state: &SharedMediaState<M>,
) -> arcen_media::video::PresentTiming {
    let served = state
        .served_pipeline
        .as_ref()
        .map(|served| &served.served)
        .or_else(|| {
            state
                .server_hello
                .as_ref()
                .and_then(|hello| hello.active_pipeline.as_ref())
        });
    arcen_media::video::operational_present_timing(served)
}

fn routed_primary_presentation_for_frame<N>(
    frame: &DecodedVideoFrame<N>,
    ten_bit_disabled: bool,
) -> RoutedPrimaryPresentation {
    if should_use_dedicated_ten_bit_layer(frame, ten_bit_disabled) {
        RoutedPrimaryPresentation::DedicatedTenBit
    } else {
        RoutedPrimaryPresentation::EguiFallback
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_routed_primary_frame<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    frame: DecodedVideoFrame<M::NativeFrame>,
    _telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    _pending_ingress_idr: &mut bool,
) -> (bool, RoutedPrimaryPresentation) {
    let presentation = {
        let state = shared.lock().expect("media state poisoned");
        routed_primary_presentation_for_frame(&frame, state.ten_bit_layer_disabled)
    };
    if presentation == RoutedPrimaryPresentation::DedicatedTenBit {
        let (presenter, source_fps, refine, timing) = {
            let state = shared.lock().expect("media state poisoned");
            (
                state.ten_bit_video_presenter.clone(),
                source_fps_for_state(&state),
                refine_tag(state.refine_hub.as_ref(), &frame),
                present_timing_for_state(&state),
            )
        };
        if let (Some(presenter), Some(native)) = (presenter, frame.native.as_ref()) {
            let layer_frame = PresentableFrame {
                picture: native.picture.clone(),
                contract: native.video,
                arrival: Instant::now(),
                source_fps,
                refine,
                timing,
            };
            let present_start = Instant::now();
            let status = presenter
                .lock()
                .expect("10-bit presenter poisoned")
                .present(&layer_frame);
            let present_elapsed = present_start.elapsed();
            let mut state = shared.lock().expect("media state poisoned");
            state.ten_bit_presentation_status = preserve_established(
                state.ten_bit_presentation_status,
                status,
                state.ten_bit_layer_has_picture,
            );
            let metadata_changed = if status.is_presented() || status.is_transient_drop_or_skip() {
                publish_native_metadata(&mut state, &frame, native.video)
            } else {
                false
            };
            if status.is_presented() {
                record_layer_submission(&mut state, present_elapsed);
                if metadata_changed {
                    drop(state);
                    repaint.request_repaint();
                }
                return (false, RoutedPrimaryPresentation::DedicatedTenBit);
            }
            if status.is_fallback() {
                publish_ten_bit_failure_fallback(&mut state, frame);
                drop(state);
                request_presentation_recovery(shared, commands, full_frame_requests);
                repaint.request_repaint();
                return (false, RoutedPrimaryPresentation::DedicatedFailure);
            }
            if metadata_changed {
                drop(state);
                repaint.request_repaint();
            }
            return (false, RoutedPrimaryPresentation::DedicatedTenBit);
        }
    }

    // A Deck whose root window shows eight-bit video only through its
    // dedicated presenter (the Windows Deck has no egui video path) takes
    // the root monitor of a multi-monitor session there too.
    let eight_bit = {
        let state = shared.lock().expect("media state poisoned");
        (state.routed_primary_dedicated_eight_bit
            && should_use_dedicated_eight_bit_layer(&frame, state.eight_bit_layer_disabled))
        .then(|| {
            (
                state.eight_bit_video_presenter.clone(),
                source_fps_for_state(&state),
                present_timing_for_state(&state),
            )
        })
    };
    if let Some((Some(presenter), source_fps, timing)) = eight_bit
        && let Some(native) = frame.native.as_ref()
    {
        let layer_frame = PresentableFrame {
            picture: native.picture.clone(),
            contract: native.video,
            arrival: Instant::now(),
            source_fps,
            refine: None,
            timing,
        };
        let present_start = Instant::now();
        let status = presenter
            .lock()
            .expect("8-bit presenter poisoned")
            .present(&layer_frame);
        let present_elapsed = present_start.elapsed();
        let mut state = shared.lock().expect("media state poisoned");
        state.eight_bit_presentation_status = preserve_established(
            state.eight_bit_presentation_status,
            status,
            state.eight_bit_layer_has_picture,
        );
        let metadata_changed = if status.is_presented() || status.is_transient_drop_or_skip() {
            publish_native_metadata(&mut state, &frame, native.video)
        } else {
            false
        };
        if status.is_presented() {
            record_layer_submission(&mut state, present_elapsed);
        }
        if status.is_fallback() {
            state.eight_bit_layer_disabled = true;
            clear_native_layer_history(&mut state);
            drop(state);
            request_presentation_recovery(shared, commands, full_frame_requests);
            repaint.request_repaint();
            return (false, RoutedPrimaryPresentation::DedicatedFailure);
        }
        if metadata_changed {
            drop(state);
            repaint.request_repaint();
        }
        return (false, RoutedPrimaryPresentation::DedicatedEightBit);
    }

    let mut state = shared.lock().expect("media state poisoned");
    let counts_as_fallback_present = !frame.rgba.is_empty();
    let superseded = state.latest_frame.replace(frame).is_some();
    if counts_as_fallback_present {
        state.presentation_recovery.clear_after_fallback_handoff();
    }
    drop(state);
    repaint.request_repaint();
    (superseded, RoutedPrimaryPresentation::EguiFallback)
}

#[allow(clippy::too_many_arguments)]
fn publish_decoded_frame<M: MediaPlatform>(
    shared: &Arc<Mutex<SharedMediaState<M>>>,
    frame: DecodedVideoFrame<M::NativeFrame>,
    decode_ms: f64,
    decoder_backend_name: &'static str,
    decoder_hardware_accelerated: Option<bool>,
    telemetry: &ClientTelemetry,
    repaint: &dyn RepaintRequester,
    commands: &SessionCommandSender,
    full_frame_requests: &mut FullFrameRequestGate,
    _pending_ingress_idr: &mut bool,
) {
    enum DedicatedRoute<M: MediaPlatform> {
        EightBit(Arc<Mutex<M::EightBitPresenter>>),
        TenBit(Arc<Mutex<M::TenBitPresenter>>),
        Egui,
    }

    let mut waits_for_layer = false;
    let (route, source_fps, timing): (DedicatedRoute<M>, _, _) = {
        let state = shared.lock().expect("media state poisoned");
        let source_fps = source_fps_for_state(&state);
        let timing = present_timing_for_state(&state);
        // A native frame goes to its layer only once the UI attached that
        // layer. The decoder hands out native frames while either layer is
        // attached: after a sign-in hand-over the held picture's eight-bit
        // layer is, and the desktop's first ten-bit frame would otherwise
        // find its own layer missing and turn the session's presentation
        // to eight bits for good. On the UI path it tells the UI which
        // layer to attach, and a fresh full frame is asked for: nothing
        // shows this one, and the next may be a keepalive a second away.
        let route = if should_use_dedicated_eight_bit_layer(&frame, state.eight_bit_layer_disabled)
        {
            match state.eight_bit_video_presenter.as_ref() {
                Some(presenter) if state.eight_bit_layer_ready => {
                    DedicatedRoute::EightBit(presenter.clone())
                }
                Some(_) => {
                    waits_for_layer = true;
                    DedicatedRoute::Egui
                }
                None => DedicatedRoute::Egui,
            }
        } else if should_use_dedicated_ten_bit_layer(&frame, state.ten_bit_layer_disabled) {
            match state.ten_bit_video_presenter.as_ref() {
                Some(presenter) if state.ten_bit_layer_ready => {
                    DedicatedRoute::TenBit(presenter.clone())
                }
                Some(_) => {
                    waits_for_layer = true;
                    DedicatedRoute::Egui
                }
                None => DedicatedRoute::Egui,
            }
        } else {
            DedicatedRoute::Egui
        };
        (route, source_fps, timing)
    };

    match route {
        DedicatedRoute::EightBit(presenter) => {
            if let Some(native) = frame.native.as_ref() {
                {
                    let mut state = shared.lock().expect("media state poisoned");
                    record_primary_decode_state(
                        &mut state,
                        decode_ms,
                        decoder_backend_name,
                        decoder_hardware_accelerated,
                        telemetry,
                    );
                }
                let layer_frame = PresentableFrame {
                    picture: native.picture.clone(),
                    contract: native.video,
                    arrival: Instant::now(),
                    source_fps,
                    refine: None,
                    timing,
                };
                let present_start = Instant::now();
                let status = presenter
                    .lock()
                    .expect("8-bit presenter poisoned")
                    .present(&layer_frame);
                let present_elapsed = present_start.elapsed();
                let mut state = shared.lock().expect("media state poisoned");
                state.eight_bit_presentation_status = preserve_established(
                    state.eight_bit_presentation_status,
                    status,
                    state.eight_bit_layer_has_picture,
                );
                let metadata_changed =
                    if status.is_presented() || status.is_transient_drop_or_skip() {
                        publish_native_metadata(&mut state, &frame, native.video)
                    } else {
                        false
                    };
                if status.is_presented() {
                    record_layer_submission(&mut state, present_elapsed);
                    if metadata_changed {
                        drop(state);
                        repaint.request_repaint();
                    }
                    return;
                }
                if status.is_fallback() {
                    state.eight_bit_layer_disabled = true;
                    clear_native_layer_history(&mut state);
                    drop(state);
                    request_presentation_recovery(shared, commands, full_frame_requests);
                    repaint.request_repaint();
                    return;
                }
                if metadata_changed {
                    drop(state);
                    repaint.request_repaint();
                }
                return;
            }
        }
        DedicatedRoute::TenBit(presenter) => {
            if let Some(native) = frame.native.as_ref() {
                let refine = {
                    let mut state = shared.lock().expect("media state poisoned");
                    record_primary_decode_state(
                        &mut state,
                        decode_ms,
                        decoder_backend_name,
                        decoder_hardware_accelerated,
                        telemetry,
                    );
                    refine_tag(state.refine_hub.as_ref(), &frame)
                };
                let layer_frame = PresentableFrame {
                    picture: native.picture.clone(),
                    contract: native.video,
                    arrival: Instant::now(),
                    source_fps,
                    refine,
                    timing,
                };
                let present_start = Instant::now();
                let status = presenter
                    .lock()
                    .expect("10-bit presenter poisoned")
                    .present(&layer_frame);
                let present_elapsed = present_start.elapsed();
                let mut state = shared.lock().expect("media state poisoned");
                state.ten_bit_presentation_status = preserve_established(
                    state.ten_bit_presentation_status,
                    status,
                    state.ten_bit_layer_has_picture,
                );
                let metadata_changed =
                    if status.is_presented() || status.is_transient_drop_or_skip() {
                        publish_native_metadata(&mut state, &frame, native.video)
                    } else {
                        false
                    };
                if status.is_presented() {
                    record_layer_submission(&mut state, present_elapsed);
                    if metadata_changed {
                        drop(state);
                        repaint.request_repaint();
                    }
                    return;
                }
                if status.is_fallback() {
                    publish_ten_bit_failure_fallback(&mut state, frame);
                    drop(state);
                    request_presentation_recovery(shared, commands, full_frame_requests);
                    repaint.request_repaint();
                    return;
                }
                if metadata_changed {
                    drop(state);
                    repaint.request_repaint();
                }
                return;
            }
        }
        DedicatedRoute::Egui => {}
    }

    let mut state = shared.lock().expect("media state poisoned");
    record_primary_decode_state(
        &mut state,
        decode_ms,
        decoder_backend_name,
        decoder_hardware_accelerated,
        telemetry,
    );
    if state.latest_frame.replace(frame).is_some() {
        state.presentation_superseded_frames =
            state.presentation_superseded_frames.saturating_add(1);
    }
    if !state
        .latest_frame
        .as_ref()
        .is_some_and(|frame| frame.rgba.is_empty())
    {
        state.presentation_recovery.clear_after_fallback_handoff();
    }
    // Release the lock before requesting a repaint so the UI thread can
    // immediately take the frame without contending on the mutex.
    drop(state);
    repaint.request_repaint();
    if waits_for_layer {
        full_frame_requests.request();
        let _ = full_frame_requests.send_due(commands);
    }
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

fn wire_clock_age_ms(timestamp_ms: u32) -> i32 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32;
    now.wrapping_sub(timestamp_ms) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{NativeDecodedVideoFrame, NativePictureFormat, VideoDecodeError};
    use crate::present::{LayerColourState, PresenterFailure, PresenterFailureStage};
    use crate::present::{PresenterPipeline, PresenterTelemetryDelta};
    use crate::runtime::session::SessionCommand;
    use crate::runtime::test_support::ContractDecoder;
    use arcen_protocol::{ChromaSubsampling, FrameType, VIDEO_KEYFRAME_FLAG, VideoCodec};

    /// A native picture the tests can create on any platform.
    #[derive(Debug, Clone)]
    struct TestPicture;

    type DecodedVideoFrame = crate::decode::DecodedVideoFrame<TestPicture>;
    type TestDecoder = ContractDecoder<TestPicture>;
    type SharedMediaState = super::SharedMediaState<TestMedia>;

    /// The failure an unattached presenter reports (the macOS presenter's
    /// "no root layer").
    const NO_SURFACE: PresenterFailure = PresenterFailure {
        stage: PresenterFailureStage::NoWindow,
        detail: "no video surface attached",
    };

    /// A dedicated presenter with no window, as the platform presenters
    /// behave before the UI attaches them: a hidden surface skips the frame
    /// (and keeps it for the restore), a visible one cannot present and
    /// falls back.
    #[derive(Debug, Default)]
    struct TestPresenter {
        visible: bool,
        async_status: Option<PresentOutcome>,
        on_glass: bool,
        torn_down: bool,
    }

    impl VideoPresenterSink for TestPresenter {
        type NativeFrame = TestPicture;

        fn pipeline(&self) -> PresenterPipeline {
            PresenterPipeline::TenBit
        }

        fn present(&mut self, _frame: &PresentableFrame<TestPicture>) -> PresentOutcome {
            if self.visible {
                PresentOutcome::Fallback(NO_SURFACE)
            } else {
                PresentOutcome::SkippedOccluded
            }
        }

        fn represent_last(&mut self) -> Option<PresentOutcome> {
            None
        }

        fn set_surface_visible(&mut self, visible: bool) {
            self.visible = visible;
        }

        fn take_async_status(&mut self) -> Option<PresentOutcome> {
            self.async_status.take()
        }

        fn has_presented_picture(&self) -> bool {
            self.on_glass
        }

        fn take_telemetry(&self) -> PresenterTelemetryDelta {
            PresenterTelemetryDelta::default()
        }

        fn colour_state(&self) -> Option<LayerColourState> {
            None
        }

        fn teardown(&mut self) {
            self.on_glass = false;
            self.torn_down = true;
        }
    }

    #[derive(Debug, Default)]
    struct TestMedia;

    impl MediaPlatform for TestMedia {
        type NativeFrame = TestPicture;
        type Decoder = TestDecoder;
        type EightBitPresenter = TestPresenter;
        type TenBitPresenter = TestPresenter;
        type EightBitStatus = PresentOutcome;
        type TenBitStatus = PresentOutcome;
    }

    /// Both Decks' presenters take each frame's timing from the pipeline the
    /// host serves, through this worker.
    #[test]
    fn frames_carry_the_served_pipelines_presentation_timing() {
        use arcen_media::video::PresentTiming;
        use arcen_protocol::messages::{ServedPipelineMsg, ServedStreamPipeline};
        let mut state = SharedMediaState::default();
        assert_eq!(present_timing_for_state(&state), PresentTiming::Newest);
        for (served, timing) in [
            (ServedStreamPipeline::Auto, PresentTiming::Newest),
            (ServedStreamPipeline::Speed, PresentTiming::Tearing),
            (ServedStreamPipeline::Grading, PresentTiming::Vsync),
            (ServedStreamPipeline::Hdr, PresentTiming::Vsync),
        ] {
            state.served_pipeline = Some(ServedPipelineMsg::new(served, "hardware", None));
            assert_eq!(present_timing_for_state(&state), timing);
        }
    }

    struct NoRepaint;

    impl RepaintRequester for NoRepaint {
        fn request_repaint(&self) {}
    }

    /// A multi-monitor session's root frames reach the dedicated eight-bit
    /// presenter on a Deck that asks for it (the Windows Deck has no egui
    /// video path); otherwise they stay on `latest_frame` (the macOS Deck).
    #[test]
    fn a_routed_root_frame_goes_to_the_dedicated_eight_bit_presenter_when_asked() {
        for dedicated in [false, true] {
            let shared = Arc::new(Mutex::new(SharedMediaState::default()));
            {
                let mut state = shared.lock().unwrap();
                state.eight_bit_video_presenter =
                    Some(Arc::new(Mutex::new(TestPresenter::default())));
                state.routed_primary_dedicated_eight_bit = dedicated;
            }
            let (sender, _receiver) = mpsc::unbounded_channel();
            let commands = SessionCommandSender::for_test(sender);
            let mut gate = FullFrameRequestGate::default();
            let (_, presentation) = publish_routed_primary_frame(
                &shared,
                native_eight_bit_frame_sized(1, 1920, 1080),
                &ClientTelemetry::default(),
                &NoRepaint,
                &commands,
                &mut gate,
                &mut false,
            );
            let state = shared.lock().unwrap();
            if dedicated {
                assert_eq!(presentation, RoutedPrimaryPresentation::DedicatedEightBit);
                assert!(state.latest_frame.is_none());
                assert_eq!(
                    state.eight_bit_presentation_status,
                    PresentOutcome::SkippedOccluded
                );
            } else {
                assert_eq!(presentation, RoutedPrimaryPresentation::EguiFallback);
                assert!(state.latest_frame.is_some());
            }
        }
    }

    #[test]
    fn per_monitor_window_rates_use_counter_deltas() {
        let previous = MonitorRateSnapshot {
            frames_received: 100,
            frames_presented: 90,
            frames_rejected: 7,
            frames_superseded_by_refresh: 0,
        };
        let current = MonitorRateSnapshot {
            frames_received: 250,
            frames_presented: 140,
            frames_rejected: 12,
            frames_superseded_by_refresh: 0,
        };

        let rates = monitor_stream_window(Duration::from_secs(5), Some(previous), current, 0);

        assert!((rates.fps_received - 30.0).abs() < f64::EPSILON);
        assert!((rates.fps_presented - 10.0).abs() < f64::EPSILON);
        assert_eq!(rates.frames_dropped, 100);
        assert_eq!(rates.frames_superseded_by_refresh, 0);
        assert_eq!(rates.frames_dropped_before_presentation, 100);
    }

    #[test]
    fn manual_session_end_is_not_a_media_worker_error() {
        let end = SessionEnd {
            reason: DisconnectReason::Terminal(TerminalDisconnect::Manual),
            message: "Disconnected".to_string(),
            observed_at: Instant::now(),
        };
        assert_eq!(session_end_worker_error(&end), None);
    }

    #[test]
    fn abnormal_session_end_remains_a_media_worker_error() {
        let end = SessionEnd {
            reason: DisconnectReason::Transient(
                crate::runtime::session::TransientTransportError::ConnectionReset,
            ),
            message: "transport reset".to_string(),
            observed_at: Instant::now(),
        };
        assert_eq!(
            session_end_worker_error(&end),
            Some("transport reset".to_string())
        );
    }

    #[test]
    fn per_monitor_window_rates_guard_missing_previous_and_zero_elapsed() {
        let current = MonitorRateSnapshot {
            frames_received: 50,
            frames_presented: 25,
            frames_rejected: 3,
            frames_superseded_by_refresh: 0,
        };

        let rates = monitor_stream_window(Duration::ZERO, None, current, 60);

        assert!(rates.fps_received.abs() < f64::EPSILON);
        assert!(rates.fps_presented.abs() < f64::EPSILON);
        assert_eq!(rates.frames_dropped, 50);
        assert_eq!(rates.frames_superseded_by_refresh, 0);
        assert_eq!(rates.frames_dropped_before_presentation, 50);
    }

    #[test]
    fn per_monitor_window_rates_attribute_slow_display_supersession_to_refresh() {
        let current = MonitorRateSnapshot {
            frames_received: 300,
            frames_presented: 300,
            frames_rejected: 0,
            frames_superseded_by_refresh: 0,
        };

        let rates = monitor_stream_window(Duration::from_secs(5), None, current, 30);

        assert_eq!(rates.fps_received, 60.0);
        assert_eq!(rates.fps_presented, 30.0);
        assert_eq!(rates.frames_dropped, 150);
        assert_eq!(rates.frames_superseded_by_refresh, 150);
        assert_eq!(rates.frames_dropped_before_presentation, 0);
    }

    #[test]
    fn a_decoder_behind_an_interactive_stream_skips_its_batch_to_the_next_keyframe() {
        use crate::runtime::frame_queue::{MediaPacket, incoming_media_inbox};
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (inbox, media) = incoming_media_inbox();
        let delta = |timestamp_ms| MediaPacket::Video {
            header: VideoHeader {
                frame_type: FrameType::VideoH264,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: 0,
                timestamp_ms,
                monitor_id: 0,
                topology_generation: 0,
                stream_epoch: 0,
            },
            payload: vec![0; 3],
        };
        let mut keyframe = delta(1);
        if let MediaPacket::Video { header, .. } = &mut keyframe {
            header.flags = VIDEO_KEYFRAME_FLAG;
        }
        let mut guard = crate::runtime::decode_lag::DecodeLagGuard::default();
        let mut late_batch = |packets: Vec<MediaPacket>, waited_ms| {
            for packet in packets {
                let _ = inbox.enqueue(packet);
            }
            let mut batch = media.take_batch();
            batch.oldest_video_wait = Some(Duration::from_millis(waited_ms));
            skip_video_if_behind(&shared, &media, &mut guard, &mut batch);
            batch
        };

        let first = late_batch(vec![keyframe, delta(2)], 130);
        assert_eq!(first.video.len(), 2);
        assert!(!first.video_discontinuity);
        let second = late_batch(vec![delta(3), delta(4)], 170);
        assert!(second.video.is_empty());
        assert!(second.video_discontinuity && second.idr_needed);
        assert_eq!(second.video_discontinuity_monitor_ids, vec![0]);
        assert_eq!(
            shared
                .lock()
                .expect("media state poisoned")
                .decoder_behind
                .skips,
            1
        );
        // Deltas are dropped on arrival until the keyframe it asked for.
        assert!(!inbox.enqueue(delta(5)).accepted);
    }

    #[test]
    fn rejected_keyframe_rearms_full_frame_recovery() {
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (sender, mut received) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(sender);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let mut decoder = TestDecoder::default();
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

        let mut secondary_router: Option<MonitorFrameRouter<TestDecoder>> = None;
        assert!(!decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &packets,
            &telemetry,
            &NoRepaint,
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

    #[derive(Debug)]
    enum ScriptedDecode {
        Frame(DecodedVideoFrame),
        NoOutput,
        Refuse(&'static str),
    }

    #[derive(Debug, Default)]
    struct ScriptedVideoDecoder {
        outcomes: VecDeque<ScriptedDecode>,
        prefer_native_biplanar: bool,
        prefer_history: Vec<bool>,
        wants_keyframe: bool,
    }

    impl ScriptedVideoDecoder {
        fn new(outcomes: impl Into<VecDeque<ScriptedDecode>>) -> Self {
            Self {
                outcomes: outcomes.into(),
                ..Self::default()
            }
        }

        fn prefers_native_biplanar(&self) -> bool {
            self.prefer_native_biplanar
        }
    }

    impl VideoDecoderBackend for ScriptedVideoDecoder {
        type NativeFrame = TestPicture;

        fn set_session_color(&mut self, _session_color: SessionColor) {}

        fn set_prefer_native_frames(&mut self, prefer: bool) -> bool {
            if self.prefer_native_biplanar == prefer {
                return false;
            }
            self.prefer_native_biplanar = prefer;
            self.prefer_history.push(prefer);
            self.wants_keyframe = true;
            true
        }

        fn set_decode_latency_policy(&mut self, _policy: arcen_media::video::DecodeLatencyPolicy) {}

        fn decode(
            &mut self,
            _header: &VideoHeader,
            _payload: &[u8],
        ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
            match self
                .outcomes
                .pop_front()
                .unwrap_or(ScriptedDecode::NoOutput)
            {
                ScriptedDecode::Frame(frame) => {
                    self.wants_keyframe = false;
                    Ok(Some(frame))
                }
                ScriptedDecode::NoOutput => Ok(None),
                ScriptedDecode::Refuse(reason) => {
                    self.wants_keyframe = true;
                    Err(VideoDecodeError::Unsupported(reason.to_owned()))
                }
            }
        }

        fn backend_name(&self) -> &'static str {
            "scripted"
        }

        fn is_hardware_accelerated(&self) -> Option<bool> {
            Some(false)
        }

        fn wants_keyframe(&self) -> bool {
            self.wants_keyframe
        }

        fn notify_discontinuity(&mut self) {
            self.wants_keyframe = true;
        }
    }

    fn rgba_frame(timestamp_ms: u32, tag: u8) -> DecodedVideoFrame {
        DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: vec![tag; 16],
            timestamp_ms,
            pixel_format: "rgba".to_string(),
            backend: "scripted",
            native: None,
        }
    }

    fn ten_bit_video() -> arcen_media::VideoConfiguration {
        arcen_media::VideoConfiguration {
            chroma: arcen_media::ChromaSubsampling::Yuv444,
            bit_depth: arcen_media::BitDepth::Ten,
            range: arcen_media::ColorRange::Full,
            matrix: arcen_media::ColorMatrix::Bt709,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            ..arcen_media::VideoConfiguration::legacy_h264()
        }
    }

    fn native_ten_bit_frame(timestamp_ms: u32) -> DecodedVideoFrame {
        native_ten_bit_frame_sized(timestamp_ms, 2, 2)
    }

    fn native_ten_bit_frame_sized(
        timestamp_ms: u32,
        width: usize,
        height: usize,
    ) -> DecodedVideoFrame {
        DecodedVideoFrame {
            width,
            height,
            rgba: Vec::new(),
            timestamp_ms,
            pixel_format: "xf44-full-iosurface".to_string(),
            backend: "scripted-native",
            native: Some(NativeDecodedVideoFrame {
                format: NativePictureFormat::BiPlanar444Ten,
                picture: TestPicture,
                video: ten_bit_video(),
            }),
        }
    }

    fn native_eight_bit_frame_sized(
        timestamp_ms: u32,
        width: usize,
        height: usize,
    ) -> DecodedVideoFrame {
        let video = arcen_media::VideoConfiguration::legacy_h264();
        DecodedVideoFrame {
            width,
            height,
            rgba: Vec::new(),
            timestamp_ms,
            pixel_format: "420v-iosurface".to_string(),
            backend: "scripted-native",
            native: Some(NativeDecodedVideoFrame {
                format: NativePictureFormat::Nv12,
                picture: TestPicture,
                video,
            }),
        }
    }

    fn legacy_video_packet(timestamp_ms: u32, keyframe: bool) -> (VideoHeader, Vec<u8>) {
        (
            VideoHeader {
                frame_type: FrameType::VideoH264,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: if keyframe { VIDEO_KEYFRAME_FLAG } else { 0 },
                timestamp_ms,
                monitor_id: 0,
                topology_generation: 0,
                stream_epoch: 0,
            },
            vec![u8::from(keyframe)],
        )
    }

    fn routed_video_packet(
        monitor_id: arcen_media::SessionMonitorId,
        generation: arcen_media::TopologyGeneration,
        timestamp_ms: u32,
        keyframe: bool,
    ) -> (VideoHeader, Vec<u8>) {
        (
            VideoHeader {
                frame_type: FrameType::RegionVideoH264,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: if keyframe { VIDEO_KEYFRAME_FLAG } else { 0 },
                timestamp_ms,
                monitor_id: monitor_id.get(),
                topology_generation: generation.get(),
                stream_epoch: 1,
            },
            vec![monitor_id.get() as u8, u8::from(keyframe)],
        )
    }

    fn visible_ten_bit_presenter() -> Arc<Mutex<TestPresenter>> {
        let mut presenter = TestPresenter::default();
        presenter.set_surface_visible(true);
        Arc::new(Mutex::new(presenter))
    }

    fn drain_commands(rx: &mut mpsc::UnboundedReceiver<SessionCommand>) -> usize {
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        count
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
    fn served_fps_uses_actual_health_target_not_pipeline_ceiling() {
        let state = SharedMediaState {
            served_source_fps: Some(15),
            served_pipeline: Some(ServedPipelineMsg::new(
                arcen_protocol::messages::ServedStreamPipeline::Auto,
                "hardware",
                None,
            )),
            ..SharedMediaState::default()
        };
        assert_eq!(source_fps_for_state(&state), Some(15));
        assert_ne!(
            source_fps_for_state(&state),
            Some(arcen_media::video::contract(arcen_media::video::StreamingPreset::Auto).max_fps)
        );
    }

    #[test]
    fn served_fps_from_health_prefers_target_over_ceiling() {
        let stats = HealthStatsMsg {
            fps_target: 15.0,
            fps_actual: 14.8,
            ..HealthStatsMsg::default()
        };
        assert_eq!(served_fps_from_health(&stats), Some(15));
    }

    #[test]
    fn dedicated_layer_route_keeps_pipelines_separate() {
        let eight_bit = arcen_media::VideoConfiguration::legacy_h264();
        let grading = arcen_media::VideoConfiguration {
            chroma: arcen_media::ChromaSubsampling::Yuv444,
            bit_depth: arcen_media::BitDepth::Ten,
            range: arcen_media::ColorRange::Full,
            matrix: arcen_media::ColorMatrix::Bt709,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            ..arcen_media::VideoConfiguration::legacy_h264()
        };
        let hdr = arcen_media::VideoConfiguration {
            primaries: arcen_media::ColorPrimaries::Bt2020,
            transfer: arcen_media::TransferCharacteristics::Pq,
            ..grading
        };
        let invalid_eight_bit_hdr = arcen_media::VideoConfiguration {
            transfer: arcen_media::TransferCharacteristics::Pq,
            ..eight_bit
        };

        assert_eq!(
            dedicated_layer_route_for_video(Some(eight_bit), false, false, false),
            DedicatedLayerRoute::EightBit
        );
        assert_eq!(
            dedicated_layer_route_for_video(Some(invalid_eight_bit_hdr), false, false, false),
            DedicatedLayerRoute::EightBit,
            "8-bit HDR tags are a logged contract violation, not a reason to widen the path"
        );
        assert_eq!(
            dedicated_layer_route_for_video(Some(grading), false, false, false),
            DedicatedLayerRoute::TenBit
        );
        assert_eq!(
            dedicated_layer_route_for_video(Some(hdr), false, false, false),
            DedicatedLayerRoute::TenBit,
            "PQ/BT.2020 HDR uses the same 10-bit layer, with EDR decided by colour config"
        );
    }

    #[test]
    fn dedicated_layer_route_falls_back_when_disabled_or_rgba_is_present() {
        let grading = arcen_media::VideoConfiguration {
            chroma: arcen_media::ChromaSubsampling::Yuv444,
            bit_depth: arcen_media::BitDepth::Ten,
            range: arcen_media::ColorRange::Full,
            matrix: arcen_media::ColorMatrix::Bt709,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            ..arcen_media::VideoConfiguration::legacy_h264()
        };

        assert_eq!(
            dedicated_layer_route_for_video(Some(grading), false, false, true),
            DedicatedLayerRoute::Egui
        );
        assert_eq!(
            dedicated_layer_route_for_video(Some(grading), true, false, false),
            DedicatedLayerRoute::Egui,
            "CPU RGBA payloads stay on the existing egui fallback path"
        );
    }

    #[test]
    fn ten_bit_failure_keeps_existing_fallback_for_empty_native_frames() {
        assert_eq!(
            ten_bit_failure_fallback(false),
            TenBitFailureFallback::KeepExistingFallback
        );
        assert_eq!(
            ten_bit_failure_fallback(true),
            TenBitFailureFallback::PublishRgbaFallback
        );
    }

    fn drive_sync_presenter_failure_recovery() -> (
        Arc<Mutex<SharedMediaState>>,
        SessionCommandSender,
        mpsc::UnboundedReceiver<SessionCommand>,
        FullFrameRequestGate,
        bool,
        ScriptedVideoDecoder,
    ) {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(visible_ten_bit_presenter()),
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let mut gate = FullFrameRequestGate::with_interval(Duration::from_millis(5));
        let mut pending_ingress_idr = false;
        let mut decoder = ScriptedVideoDecoder::new(VecDeque::from([
            ScriptedDecode::Frame(native_ten_bit_frame(1)),
            ScriptedDecode::Frame(rgba_frame(2, 7)),
        ]));
        let mut secondary_router: Option<MonitorFrameRouter<ScriptedVideoDecoder>> = None;

        gate.request();
        assert!(gate.send_due(&commands));
        assert_eq!(drain_commands(&mut commands_rx), 1);

        let failed_native_keyframe = vec![legacy_video_packet(1, true)];
        assert!(decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &failed_native_keyframe,
            &telemetry,
            &repaint,
        ));
        keep_or_cancel_full_frame_gate_after_recovery(&shared, &mut gate, &mut pending_ingress_idr);
        assert!(gate.is_pending());
        assert!(presentation_recovery_pending(&shared));
        assert!(
            shared
                .lock()
                .expect("media state poisoned")
                .ten_bit_layer_disabled
        );

        let mut event = None;
        assert_eq!(
            service_worker_loop_step(
                &shared,
                &commands,
                &mut gate,
                false,
                |_| Err(mpsc::error::TryRecvError::Empty),
                |_| {},
                &mut event,
            ),
            WorkerLoopStep::PendingPolled
        );
        assert_eq!(drain_commands(&mut commands_rx), 0);
        std::thread::sleep(Duration::from_millis(6));
        assert_eq!(
            service_worker_loop_step(
                &shared,
                &commands,
                &mut gate,
                false,
                |_| Err(mpsc::error::TryRecvError::Empty),
                |_| {},
                &mut event,
            ),
            WorkerLoopStep::PendingPolled
        );
        assert_eq!(drain_commands(&mut commands_rx), 1);

        let fallback_keyframe = vec![legacy_video_packet(2, true)];
        assert!(decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &fallback_keyframe,
            &telemetry,
            &repaint,
        ));
        keep_or_cancel_full_frame_gate_after_recovery(&shared, &mut gate, &mut pending_ingress_idr);
        assert!(!decoder.prefers_native_biplanar());
        assert!(!presentation_recovery_pending(&shared));
        assert!(!gate.is_pending());
        assert!(
            !shared
                .lock()
                .expect("media state poisoned")
                .latest_frame
                .as_ref()
                .expect("fallback frame")
                .rgba
                .is_empty()
        );

        (
            shared,
            commands,
            commands_rx,
            gate,
            pending_ingress_idr,
            decoder,
        )
    }

    #[test]
    fn warmed_gate_sync_presenter_failure_in_batch_holds_until_post_failure_rgba() {
        let (_shared, _commands, _commands_rx, _gate, _pending_ingress_idr, decoder) =
            drive_sync_presenter_failure_recovery();
        assert_eq!(decoder.prefer_history, [true, false]);
    }

    #[test]
    fn decode_batch_presentation_recovery_counts_native_failure_and_rgba_fallback_decodes() {
        let (shared, _commands, _commands_rx, _gate, _pending_ingress_idr, _decoder) =
            drive_sync_presenter_failure_recovery();
        let state = shared.lock().expect("media state poisoned");
        assert_eq!(state.video_frames_decoded, 2);
        assert_eq!(
            state.latest_frame.as_ref().map(|frame| frame.timestamp_ms),
            Some(2)
        );
    }

    #[test]
    fn host_throttled_presentation_recovery_retries_by_worker_servicing() {
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::with_interval(Duration::from_millis(20));
        let mut event = None;

        request_presentation_recovery(&shared, &commands, &mut gate);
        assert!(commands_rx.try_recv().is_ok());
        assert!(gate.is_pending());
        assert_eq!(
            service_worker_loop_step(
                &shared,
                &commands,
                &mut gate,
                false,
                |_| Err(mpsc::error::TryRecvError::Empty),
                |_| {},
                &mut event,
            ),
            WorkerLoopStep::PendingPolled
        );
        assert!(commands_rx.try_recv().is_err());
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(
            service_worker_loop_step(
                &shared,
                &commands,
                &mut gate,
                false,
                |_| Err(mpsc::error::TryRecvError::Empty),
                |_| {},
                &mut event,
            ),
            WorkerLoopStep::PendingPolled
        );
        assert!(commands_rx.try_recv().is_ok());
    }

    #[test]
    fn routed_root_native_switch_delta_then_keyframe_recovers_through_router() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(Arc::new(Mutex::new(TestPresenter::default()))),
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::with_interval(Duration::from_millis(1));
        let mut pending_ingress_idr = false;
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let root = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let mut router =
            MonitorFrameRouter::new_with_decoder_factory_for_test(generation, &[root], |_| {
                ScriptedVideoDecoder::new(VecDeque::from([
                    ScriptedDecode::NoOutput,
                    ScriptedDecode::Frame(rgba_frame(2, 9)),
                ]))
            })
            .expect("single routed root is valid");
        router.force_recovered_for_test(MonitorRoute::Negotiated(root));
        gate.request();
        assert!(gate.send_due(&commands));
        assert_eq!(drain_commands(&mut commands_rx), 1);
        gate.cancel_pending();
        let delta = vec![(
            VideoHeader {
                frame_type: FrameType::RegionVideoH264,
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                flags: 0,
                timestamp_ms: 1,
                monitor_id: root.get(),
                topology_generation: generation.get(),
                stream_epoch: 1,
            },
            vec![9, 9, 9],
        )];

        let outcome = decode_secondary_packets(
            &shared,
            &mut router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &delta,
            &telemetry,
            &repaint,
        );

        assert!(!outcome.all_recovered);
        assert!(gate.is_pending());
        assert_eq!(drain_commands(&mut commands_rx), 0);
        assert!(!presentation_recovery_pending(&shared));

        std::thread::sleep(Duration::from_millis(1));
        let mut event = None;
        assert_eq!(
            service_worker_loop_step(
                &shared,
                &commands,
                &mut gate,
                true,
                |_| Err(mpsc::error::TryRecvError::Empty),
                |_| {},
                &mut event,
            ),
            WorkerLoopStep::PendingPolled
        );
        assert_eq!(drain_commands(&mut commands_rx), 1);

        let keyframe = vec![routed_video_packet(root, generation, 2, true)];
        let outcome = decode_secondary_packets(
            &shared,
            &mut router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &keyframe,
            &telemetry,
            &repaint,
        );
        assert!(outcome.all_recovered);
        keep_or_cancel_full_frame_gate_after_recovery(&shared, &mut gate, &mut pending_ingress_idr);
        assert!(!gate.is_pending());
        assert!(
            !shared
                .lock()
                .expect("media state poisoned")
                .latest_frame
                .as_ref()
                .expect("root frame")
                .rgba
                .is_empty()
        );
    }

    #[test]
    fn routed_root_presentation_failure_reverts_to_rgba_fallback() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(visible_ten_bit_presenter()),
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let root = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let mut router =
            MonitorFrameRouter::new_with_decoder_factory_for_test(generation, &[root], |_| {
                ScriptedVideoDecoder::new(VecDeque::from([
                    ScriptedDecode::Frame(native_ten_bit_frame(1)),
                    ScriptedDecode::Frame(rgba_frame(2, 3)),
                ]))
            })
            .expect("single routed root is valid");

        let native_keyframe = vec![routed_video_packet(root, generation, 1, true)];
        let outcome = decode_secondary_packets(
            &shared,
            &mut router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &native_keyframe,
            &telemetry,
            &repaint,
        );
        assert!(outcome.all_recovered);
        keep_or_cancel_full_frame_gate_after_recovery(&shared, &mut gate, &mut pending_ingress_idr);
        assert!(gate.is_pending());
        assert!(presentation_recovery_pending(&shared));
        assert!(
            shared
                .lock()
                .expect("media state poisoned")
                .ten_bit_layer_disabled
        );
        assert_eq!(drain_commands(&mut commands_rx), 1);

        let fallback_keyframe = vec![routed_video_packet(root, generation, 2, true)];
        let outcome = decode_secondary_packets(
            &shared,
            &mut router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &fallback_keyframe,
            &telemetry,
            &repaint,
        );
        assert!(outcome.all_recovered);
        keep_or_cancel_full_frame_gate_after_recovery(&shared, &mut gate, &mut pending_ingress_idr);

        let decoder = router
            .decoder_for_route_for_test(MonitorRoute::Negotiated(root))
            .expect("root decoder");
        assert!(!decoder.prefers_native_biplanar());
        let state = shared.lock().expect("media state poisoned");
        assert!(!state.presentation_recovery.is_pending());
        assert!(
            !state
                .latest_frame
                .as_ref()
                .expect("fallback frame")
                .rgba
                .is_empty()
        );
    }

    fn assert_native_metadata_size(
        shared: &Arc<Mutex<SharedMediaState>>,
        width: usize,
        height: usize,
        timestamp_ms: u32,
    ) {
        let metadata = shared
            .lock()
            .expect("media state poisoned")
            .latest_native_frame_metadata
            .clone()
            .expect("native metadata");
        assert_eq!(metadata.width, width);
        assert_eq!(metadata.height, height);
        assert_eq!(metadata.timestamp_ms, timestamp_ms);
    }

    #[test]
    fn hidden_root_eight_bit_frame_publishes_native_metadata_before_restore() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            eight_bit_video_presenter: Some(Arc::new(Mutex::new(TestPresenter::default()))),
            eight_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let mut decoder = ScriptedVideoDecoder::new(VecDeque::from([ScriptedDecode::Frame(
            native_eight_bit_frame_sized(21, 7, 5),
        )]));
        let mut secondary_router: Option<MonitorFrameRouter<ScriptedVideoDecoder>> = None;

        assert!(decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &[legacy_video_packet(21, true)],
            &telemetry,
            &repaint,
        ));

        assert_native_metadata_size(&shared, 7, 5, 21);
    }

    #[test]
    fn hidden_root_ten_bit_frame_publishes_native_metadata_before_restore() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(Arc::new(Mutex::new(TestPresenter::default()))),
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let mut decoder = ScriptedVideoDecoder::new(VecDeque::from([ScriptedDecode::Frame(
            native_ten_bit_frame_sized(22, 9, 6),
        )]));
        let mut secondary_router: Option<MonitorFrameRouter<ScriptedVideoDecoder>> = None;

        assert!(decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &[legacy_video_packet(22, true)],
            &telemetry,
            &repaint,
        ));

        assert_native_metadata_size(&shared, 9, 6, 22);
    }

    #[test]
    fn hidden_routed_primary_frame_publishes_native_metadata_before_restore() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(Arc::new(Mutex::new(TestPresenter::default()))),
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let root = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let mut router =
            MonitorFrameRouter::new_with_decoder_factory_for_test(generation, &[root], |_| {
                ScriptedVideoDecoder::new(VecDeque::from([ScriptedDecode::Frame(
                    native_ten_bit_frame_sized(23, 11, 8),
                )]))
            })
            .expect("single routed root is valid");

        let outcome = decode_secondary_packets(
            &shared,
            &mut router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &[routed_video_packet(root, generation, 23, true)],
            &telemetry,
            &repaint,
        );

        assert!(outcome.all_recovered);
        assert_native_metadata_size(&shared, 11, 8, 23);
    }

    #[test]
    fn presentation_obligation_never_cancels_while_routed_sibling_awaits_keyframe() {
        let root = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let sibling = arcen_media::SessionMonitorId::new(2).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let shared = Arc::new(Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(visible_ten_bit_presenter()),
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        }));
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let mut decoder = TestDecoder::default();
        let mut secondary_router = Some(
            MonitorFrameRouter::new_with_decoder_factory_for_test(
                generation,
                &[root, sibling],
                |route| match route {
                    MonitorRoute::Negotiated(id) if id == root => {
                        ScriptedVideoDecoder::new(VecDeque::from([
                            ScriptedDecode::Frame(native_ten_bit_frame(1)),
                            ScriptedDecode::Frame(rgba_frame(2, 4)),
                        ]))
                    }
                    _ => ScriptedVideoDecoder::new(VecDeque::from([ScriptedDecode::Frame(
                        rgba_frame(3, 5),
                    )])),
                },
            )
            .expect("two-monitor router is valid"),
        );

        assert!(!decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &[routed_video_packet(root, generation, 1, true)],
            &telemetry,
            &repaint,
        ));
        assert!(gate.is_pending());
        assert!(presentation_recovery_pending(&shared));

        assert!(!decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &[routed_video_packet(root, generation, 2, true)],
            &telemetry,
            &repaint,
        ));
        assert!(!presentation_recovery_pending(&shared));
        assert!(gate.is_pending());

        assert!(decode_batch(
            &shared,
            &mut decoder,
            &mut secondary_router,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
            &[routed_video_packet(sibling, generation, 3, true)],
            &telemetry,
            &repaint,
        ));
        keep_or_cancel_full_frame_gate_after_recovery(&shared, &mut gate, &mut pending_ingress_idr);
        assert!(!gate.is_pending());
    }

    #[test]
    fn post_recovery_idle_worker_blocks_without_requests() {
        let (shared, commands, mut commands_rx, mut gate, _pending_ingress_idr, _decoder) =
            drive_sync_presenter_failure_recovery();
        assert_eq!(drain_commands(&mut commands_rx), 0);
        for _ in 0..3 {
            let mut event = None;
            assert_eq!(
                service_worker_loop_step(
                    &shared,
                    &commands,
                    &mut gate,
                    false,
                    |blocking| {
                        assert!(blocking, "idle recovered stream must block, not poll");
                        Ok(None)
                    },
                    |_| panic!("idle recovered stream must not sleep-poll"),
                    &mut event,
                ),
                WorkerLoopStep::Disconnected
            );
            assert_eq!(drain_commands(&mut commands_rx), 0);
        }
    }

    #[test]
    fn ten_bit_async_failure_does_not_replace_fallback_with_empty_native_frame() {
        let fallback = DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: vec![0, 0, 0, 255],
            timestamp_ms: 7,
            pixel_format: "rgba".to_string(),
            backend: "test",
            native: None,
        };
        let empty_native = DecodedVideoFrame {
            width: 2,
            height: 2,
            rgba: Vec::new(),
            timestamp_ms: 8,
            pixel_format: "xf44-full-iosurface".to_string(),
            backend: "videotoolbox-iosurface",
            native: None,
        };
        let mut state = SharedMediaState {
            latest_frame: Some(fallback),
            ..SharedMediaState::default()
        };

        publish_ten_bit_failure_fallback(&mut state, empty_native);

        assert!(state.ten_bit_layer_disabled);
        assert_eq!(
            state.latest_frame.as_ref().map(|frame| frame.timestamp_ms),
            Some(7),
            "an async 10-bit layer failure must not publish an empty-RGBA frame over the fallback"
        );
    }

    /// After a sign-in hand-over the held picture keeps the eight-bit layer
    /// attached, so the decoder hands out native frames before the
    /// desktop's ten-bit layer exists. That frame goes to the UI, which then
    /// attaches the ten-bit layer; it must not find the layer missing and
    /// turn the session to eight bits.
    #[test]
    fn a_ten_bit_frame_before_its_layer_is_attached_reaches_the_ui_not_a_fallback() {
        let mut ten_bit_presenter = TestPresenter::default();
        ten_bit_presenter.set_surface_visible(true);
        let shared = Arc::new(Mutex::new(SharedMediaState {
            eight_bit_video_presenter: Some(Arc::new(Mutex::new(TestPresenter::default()))),
            eight_bit_layer_ready: true,
            ten_bit_video_presenter: Some(Arc::new(Mutex::new(ten_bit_presenter))),
            ten_bit_layer_ready: false,
            ..SharedMediaState::default()
        }));
        let telemetry = ClientTelemetry::default();
        let (commands_tx, mut commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        publish_decoded_frame(
            &shared,
            native_ten_bit_frame(5),
            1.0,
            "videotoolbox-iosurface",
            Some(true),
            &telemetry,
            &NoRepaint,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
        );
        let state = shared.lock().expect("media state poisoned");
        assert!(!state.ten_bit_layer_disabled);
        assert!(!state.ten_bit_presentation_status.is_fallback());
        assert!(
            state
                .latest_frame
                .as_ref()
                .is_some_and(|frame| frame.native.is_some() && frame.timestamp_ms == 5),
            "the UI sees the ten-bit frame and attaches its layer"
        );
        drop(state);
        let mut full_frame_asked = false;
        while let Ok(command) = commands_rx.try_recv() {
            if let SessionCommand::Json(value) = command {
                full_frame_asked |= value["type"] == arcen_protocol::messages::REQUEST_FULL_FRAME;
            }
        }
        assert!(full_frame_asked, "a fresh frame follows for the new layer");
    }

    #[test]
    fn decoded_frames_are_published_individually_without_building_a_backlog() {
        let shared = Arc::new(Mutex::new(SharedMediaState {
            video_frames_seen: 2,
            ..SharedMediaState::default()
        }));
        let telemetry = ClientTelemetry::default();
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
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
            &NoRepaint,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
        );
        publish_decoded_frame(
            &shared,
            frame(2),
            1.0,
            "videotoolbox-bgra-hw",
            Some(true),
            &telemetry,
            &NoRepaint,
            &commands,
            &mut gate,
            &mut pending_ingress_idr,
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
        let router = MonitorFrameRouter::<TestDecoder>::new(generation, &[primary, secondary])
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
        let mut pending_ingress_idr = false;
        let mut decoder = TestDecoder::default();
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;

        let sid1 = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let sid2 = arcen_media::SessionMonitorId::new(2).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let router = MonitorFrameRouter::<TestDecoder>::new(generation, &[sid1, sid2])
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
                &mut pending_ingress_idr,
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
                &mut pending_ingress_idr,
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
                &mut pending_ingress_idr,
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
        let mut pending_ingress_idr = false;
        let mut decoder = TestDecoder::default();
        let telemetry = ClientTelemetry::default();
        let repaint = NoRepaint;

        let sid1 = arcen_media::SessionMonitorId::new(1).expect("nonzero");
        let sid2 = arcen_media::SessionMonitorId::new(2).expect("nonzero");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let mut secondary_router = Some(
            MonitorFrameRouter::<TestDecoder>::new(generation, &[sid1, sid2])
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
            &mut pending_ingress_idr,
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
        let mut state = SharedMediaState {
            waiting_for_keyframe: true,
            ..SharedMediaState::default()
        };
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

    /// A decoder with no `Default`: like the Windows decoder, it can only be
    /// made by its platform factory (which holds the device).
    #[derive(Debug)]
    struct FactoryOnlyDecoder(TestDecoder);

    impl VideoDecoderBackend for FactoryOnlyDecoder {
        type NativeFrame = TestPicture;

        fn set_session_color(&mut self, session_color: SessionColor) {
            self.0.set_session_color(session_color);
        }

        fn set_prefer_native_frames(&mut self, prefer: bool) -> bool {
            self.0.set_prefer_native_frames(prefer)
        }

        fn set_decode_latency_policy(&mut self, policy: arcen_media::video::DecodeLatencyPolicy) {
            self.0.set_decode_latency_policy(policy);
        }

        fn decode(
            &mut self,
            header: &VideoHeader,
            payload: &[u8],
        ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
            self.0.decode(header, payload)
        }

        fn backend_name(&self) -> &'static str {
            "factory-only"
        }

        fn is_hardware_accelerated(&self) -> Option<bool> {
            None
        }

        fn wants_keyframe(&self) -> bool {
            self.0.wants_keyframe()
        }

        fn notify_discontinuity(&mut self) {
            self.0.notify_discontinuity();
        }
    }

    #[derive(Debug, Default)]
    struct CountingFactory(std::sync::atomic::AtomicUsize);

    impl VideoDecoderFactory for CountingFactory {
        type Decoder = FactoryOnlyDecoder;

        fn create(&self) -> FactoryOnlyDecoder {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            FactoryOnlyDecoder(TestDecoder::default())
        }
    }

    #[derive(Debug, Default)]
    struct FactoryOnlyMedia;

    impl MediaPlatform for FactoryOnlyMedia {
        type NativeFrame = TestPicture;
        type Decoder = FactoryOnlyDecoder;
        type EightBitPresenter = TestPresenter;
        type TenBitPresenter = TestPresenter;
        type EightBitStatus = PresentOutcome;
        type TenBitStatus = PresentOutcome;
    }

    /// R10: every monitor's decoder comes from the platform's factory, so a
    /// decoder that needs the factory's device (no `Default`) serves
    /// multi-monitor sessions too.
    #[test]
    fn every_monitor_decoder_comes_from_the_platform_factory() {
        let plan = |id: u16| {
            arcen_media::RegionMediaPlan::new(
                arcen_media::SessionMonitorId::new(id).expect("nonzero"),
                arcen_media::MediaStreamEpoch::new(1).expect("nonzero epoch"),
                arcen_media::video::EncoderBackend::NativeNvenc,
                arcen_media::VideoConfiguration::legacy_h264(),
                1920,
                1080,
                60,
                arcen_media::BitrateBudgetKbps::nominal_for_geometry(1920, 1080, 60),
            )
            .expect("valid region plan")
        };
        let roster = arcen_media::RegionMediaRoster::new(vec![plan(7), plan(1), plan(3)])
            .expect("valid roster");
        let generation = arcen_media::TopologyGeneration::new(1).expect("nonzero generation");
        let shared = Arc::new(Mutex::new(super::SharedMediaState::<FactoryOnlyMedia> {
            multi_monitor_decode_roster: Some((generation, roster)),
            ..super::SharedMediaState::default()
        }));
        let factory = CountingFactory::default();
        let root = factory.create();
        assert_eq!(root.backend_name(), "factory-only");
        let mut secondary_router = None;

        maybe_commit_secondary_router(
            &shared,
            &mut secondary_router,
            SessionColor::default(),
            arcen_media::video::DecodeLatencyPolicy::default(),
            &factory,
        );

        let router = secondary_router.expect("router committed");
        assert_eq!(
            router.primary_monitor_id(),
            arcen_media::SessionMonitorId::new(7).ok()
        );
        assert_eq!(
            factory.0.load(std::sync::atomic::Ordering::Relaxed),
            4,
            "the root decoder and one decoder per monitor, all from the factory"
        );
    }

    /// decoder's request: a refusal (a stream that contradicts its contract
    /// or a profile this machine did not prove) stays in Stream Statistics
    /// after later frames decode, instead of flashing past as the last
    /// decode error.
    #[test]
    fn a_decoder_refusal_stays_visible_after_later_frames_decode() {
        let shared = Arc::new(Mutex::new(SharedMediaState::default()));
        let (commands_tx, _commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(commands_tx);
        let mut gate = FullFrameRequestGate::default();
        let mut pending_ingress_idr = false;
        let telemetry = ClientTelemetry::default();
        let mut decoder = ScriptedVideoDecoder::new(VecDeque::from([
            ScriptedDecode::Refuse("PQ but the session says Bt709"),
            ScriptedDecode::Frame(rgba_frame(2, 1)),
        ]));
        let mut secondary_router: Option<MonitorFrameRouter<ScriptedVideoDecoder>> = None;

        for timestamp_ms in [1, 2] {
            let _ = decode_batch(
                &shared,
                &mut decoder,
                &mut secondary_router,
                &commands,
                &mut gate,
                &mut pending_ingress_idr,
                &[legacy_video_packet(timestamp_ms, true)],
                &telemetry,
                &NoRepaint,
            );
        }

        let state = shared.lock().expect("media state poisoned");
        assert_eq!(state.last_decode_error, None);
        assert_eq!(
            state.decoder_refusal.as_deref(),
            Some("PQ but the session says Bt709")
        );
        assert!(SharedMediaState::fresh(2, true).decoder_refusal.is_none());
    }

    #[test]
    fn a_late_presenter_failure_disables_its_pipeline_and_asks_for_recovery() {
        let ten_bit = Arc::new(Mutex::new(TestPresenter {
            visible: true,
            on_glass: true,
            async_status: Some(PresentOutcome::DroppedFrame),
            ..TestPresenter::default()
        }));
        let shared = Mutex::new(SharedMediaState {
            ten_bit_video_presenter: Some(Arc::clone(&ten_bit)),
            ten_bit_presentation_status: PresentOutcome::Presented,
            ten_bit_layer_ready: true,
            ..SharedMediaState::default()
        });

        assert!(!apply_presenter_async_statuses(&shared));
        {
            let state = shared.lock().expect("media state poisoned");
            assert_eq!(
                state.ten_bit_presentation_status,
                PresentOutcome::Presented,
                "a transient drop keeps the picture that is on glass"
            );
            assert!(state.ten_bit_layer_has_picture);
        }

        ten_bit.lock().expect("presenter").async_status =
            Some(PresentOutcome::Fallback(PresenterFailure {
                stage: PresenterFailureStage::DeviceLost,
                detail: "device removed",
            }));
        assert!(apply_presenter_async_statuses(&shared));
        let state = shared.lock().expect("media state poisoned");
        assert!(state.ten_bit_presentation_status.is_fallback());
        assert!(state.ten_bit_layer_disabled);
        assert!(!state.ten_bit_layer_has_picture);
        drop(state);
        assert!(ten_bit.lock().expect("presenter").torn_down);
        assert!(!apply_presenter_async_statuses(&shared), "reported once");
    }

    #[test]
    fn a_pass_records_which_pipeline_has_an_established_picture() {
        let eight_bit = Arc::new(Mutex::new(TestPresenter {
            on_glass: true,
            ..TestPresenter::default()
        }));
        let ten_bit = Arc::new(Mutex::new(TestPresenter {
            on_glass: true,
            ..TestPresenter::default()
        }));
        let shared = Mutex::new(SharedMediaState {
            eight_bit_video_presenter: Some(Arc::clone(&eight_bit)),
            eight_bit_presentation_status: PresentOutcome::Presented,
            ten_bit_video_presenter: Some(Arc::clone(&ten_bit)),
            ten_bit_presentation_status: PresentOutcome::Presented,
            ten_bit_layer_disabled: true,
            ..SharedMediaState::default()
        });

        let _ = drain_presenters(&shared);

        let state = shared.lock().expect("media state poisoned");
        assert!(state.eight_bit_layer_has_picture);
        assert!(
            !state.ten_bit_layer_has_picture,
            "a disabled pipeline never counts as established"
        );
    }

    #[test]
    fn latest_frame_mailbox_is_bounded_and_latest_wins() {
        let mut mailbox = LatestFrameMailbox::default();
        mailbox.submit(1);
        mailbox.submit(2);
        mailbox.submit(3);
        assert_eq!(mailbox.accepted(), 3);
        assert_eq!(mailbox.dropped(), 2);
        assert_eq!(mailbox.take(), Some(3));
        assert_eq!(mailbox.take(), None);
    }
}
