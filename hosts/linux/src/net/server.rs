//! The TLS/WebSocket listener and per-connection relay — the Rust port of
//! `server/main.py::run_server` + `handle_client` + the `SessionRuntime` frame
//! pump/dispatch, minus the Python.
//!
//! Stage 1 scope: `--no-auth` only. The host sends `server_hello` first, spawns
//! one `capenc` child per connection, frames its Annex-B access units with the
//! 10-byte wire header, and relays them under the bounded, drop-oldest,
//! IDR-on-drop [`FrameQueue`] backpressure policy. Inbound control messages
//! (`request_full_frame`, `client_hello`, `quality_settings`, `health_ping`,
//! WS ping) are dispatched; resolution ingest + PAM land in later stages.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
#[cfg(feature = "wss-compat")]
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
#[cfg(feature = "wss-compat")]
use tokio_tungstenite::accept_hdr_async_with_config;
#[cfg(feature = "wss-compat")]
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::{debug, error, info, trace, warn, Instrument};

use crate::cli::{AudioUserMode, AuthMode, Config, InputMode};
use crate::clipboard::{
    spawn_clipboard_agent, ClipboardAgentIo, ClipboardItem, ClipboardNegotiation,
    ClipboardWriterQueue,
};
use crate::display::nvctrl::{self, MetaModeGuard, NvControl};
use crate::display::topology::LinuxTopologyPlan;
use crate::eventlog;
use crate::input::region_adapter::RegionInputAdapter;
use crate::input::{InputController, InputError, InputStats};
#[cfg(feature = "wss-compat")]
use crate::logging::target::TLS;
use crate::logging::target::{AUDIO, AUTH, CAPENC, DISPLAY, HEALTH, INPUT, MEDIA, NET, SESSION};
use crate::logging::LogController;
use crate::media::annexb::NalCodec;
use crate::media::audio::{self as audiocap, AudioConfig};
use crate::media::capenc::{self, IdrRequester, ResolvedMediaPlan};
use crate::media::encoder_admission;
use crate::media::multi_capenc::{
    self, CapencHandle, MonitorPipelineTemplate, MultiCapencSupervisor,
};
use crate::media::{self};
use crate::session::audio::{AudioFrameEncoder, AudioQueue};
use crate::session::auth;
use crate::session::client::FrameQueue;
use crate::session::handshake::{build_server_hello, negotiate_client_transport};
use crate::session::launcher::{
    unlock_logind_session_id, AuthenticatedLauncher, LauncherConfig, LauncherError,
};
use crate::session::lifecycle::{
    HeldDisplayResources, LifecycleError, SessionLease, SessionRegistry,
};
use crate::session::monitor_mux::{MonitorMux, VideoSource};
use crate::session::multi_monitor;
use crate::session::resume::{
    self, DirectSessionSocket, OwnerCommand, ResumeBindings, TopologyBinding,
};
use crate::session::timezone::validate_zoneinfo_timezone;
use crate::session_admission::{SessionAdmissionLease, SessionAdmissionRuntime};
use crate::LifecycleEmitter;
use arcen_identity::{
    ActiveHostSessionId, DisclaimerAcceptance, HostIdentity, LogindSessionId, NativePrincipal,
    PreparedDisclaimer,
};
use arcen_input::{
    mutual_capability, negotiate_tablet_mode, CapabilityAvailability as InputCapabilityTruth,
    InputSequenceTracker, TabletMode as InputTabletMode,
};
use arcen_media::audio::{
    AudioPolicy, ConfiguredAudioPolicy, MicrophoneFrameDecision, MicrophoneFrameOrder,
    MicrophoneIngestOutcome, MicrophonePolicy, MicrophoneStats, MicrophoneStatsTracker,
    ResolvedAudioStream, MICROPHONE_STATS_INTERVAL,
};
use arcen_media::clipboard::{ClipboardFlow, ClipboardKind};
use arcen_media::video::{
    color_contract_is_servable, resolve_client_color_request_with_matrix_caps, ClientColorRequest,
    ColorCeiling, ColorMatrixCapabilities, EncoderBackend, EncoderRequest,
};
use arcen_media::SessionMonitorId;
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent,
    TransferCharacteristics, VideoCodec, VideoConfiguration,
};
use arcen_protocol::messages::{
    msg_type, supports_region_input_v1, AuthRequest, AuthResponse, AuthResult, ClientHelloMsg,
    ClipboardDataMsg, CursorMode, CursorModeReason, CursorModeResultMsg, DisplayUpdateMsg,
    DisplayUpdateResultMsg, HealthPingMsg, HealthPongMsg, InputCapabilityAvailability, KeyEventMsg,
    KeyResetModifiersMsg, MicrophoneStreamStopMsg, MouseButtonMsg, MouseMoveMsg,
    MouseMoveRelativeMsg, MouseScrollMsg, PenEventMsg, QualitySettings, RegionInputValidationError,
    RegionPenEventMsg, RegionPointerButtonMsg, RegionPointerEnterMsg, RegionPointerLeaveMsg,
    RegionPointerMotionMsg, RegionPointerScrollMsg, RequestFullFrameMsg, ResumeErrorCode,
    ServerMultiMonitorMsg, TabletModeCapabilitiesMsg, TabletModeMsg, TabletModeReason,
    TabletModeResultMsg, AUTH_METHOD_RESUME, AUTH_REQUEST, AUTH_RESULT, CLIENT_HELLO,
    CLIPBOARD_DATA, DISPLAY_UPDATE, HEALTH_PING, HEALTH_PONG, KEY_RESET_MODIFIERS,
    MICROPHONE_STREAM_STOP, MOUSE_MOVE_RELATIVE, MOUSE_SCROLL, PEN_EVENT,
    REGION_INPUT_PROTOCOL_VERSION, REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER,
    REGION_POINTER_LEAVE, REGION_POINTER_MOTION, REGION_POINTER_SCROLL, REQUEST_FULL_FRAME,
};
use arcen_protocol::{
    decode_clipboard_chunk, decode_microphone_frame, encode_clipboard_chunk, AudioCodec,
    ClipboardChunkHeader, FrameType,
};
use arcen_session::direct_reconnect::{
    DirectReconnect, MonotonicMillis, ReconnectEvent, ReconnectPolicy, ReconnectState,
};
use arcen_session::restore_lease::IanaTimeZone;
use arcen_telemetry::{
    CorrelationId, FieldValue, LifecycleEventKind, OperationalProfile, QosSample, QosTargets,
    StructuredFields, TelemetryTarget,
};
use serde::de::DeserializeOwned;
use zeroize::Zeroize;

use super::tls;

fn microphone_generation_from_entropy(bytes: [u8; 4]) -> Option<u32> {
    let generation = u32::from_ne_bytes(bytes);
    (generation != 0).then_some(generation)
}

fn next_microphone_generation() -> Result<u32, String> {
    for _ in 0..4 {
        let mut bytes = [0_u8; 4];
        getrandom::getrandom(&mut bytes)
            .map_err(|error| format!("generate microphone attachment generation: {error}"))?;
        if let Some(generation) = microphone_generation_from_entropy(bytes) {
            return Ok(generation);
        }
    }
    Err("OS randomness repeatedly produced a zero microphone generation".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicrophoneIngressRejection {
    Malformed,
    Codec,
    Sequence(MicrophoneFrameDecision),
}

impl MicrophoneIngressRejection {
    const fn terminates_stream(self) -> bool {
        matches!(self, Self::Sequence(MicrophoneFrameDecision::Discontinuity))
    }
}

struct MicrophoneIngressValidator {
    codec: AudioCodec,
    order: MicrophoneFrameOrder,
}

impl MicrophoneIngressValidator {
    fn new(codec: AudioCodec, generation: u32) -> Self {
        Self {
            codec,
            order: MicrophoneFrameOrder::new(generation),
        }
    }

    fn validate(&mut self, frame: &[u8]) -> Result<(), MicrophoneIngressRejection> {
        if frame.len()
            > arcen_protocol::MICROPHONE_HEADER_SIZE + arcen_protocol::MICROPHONE_PCM_BYTES
        {
            return Err(MicrophoneIngressRejection::Malformed);
        }
        let (header, _payload) =
            decode_microphone_frame(frame).map_err(|_| MicrophoneIngressRejection::Malformed)?;
        if header.codec != self.codec {
            return Err(MicrophoneIngressRejection::Codec);
        }
        let mut provisional_order = self.order;
        let decision = provisional_order.observe(header);
        if matches!(
            decision,
            MicrophoneFrameDecision::First
                | MicrophoneFrameDecision::OnTime
                | MicrophoneFrameDecision::Gap { .. }
        ) {
            self.order = provisional_order;
            Ok(())
        } else {
            Err(MicrophoneIngressRejection::Sequence(decision))
        }
    }
}

fn log_linux_transport_stats(
    session_log_id: &CorrelationId,
    generation: u32,
    stats: MicrophoneStats,
    final_snapshot: bool,
    duration: Duration,
    stop_reason: &'static str,
) {
    info!(
        target: AUDIO,
        event = if final_snapshot {
            "mic_linux_transport_teardown_summary"
        } else {
            "mic_linux_transport_stats"
        },
        sid = %session_log_id,
        generation,
        final_snapshot,
        duration_ms = duration.as_millis().min(u128::from(u64::MAX)) as u64,
        stop_reason,
        received_frames = stats.received_frames,
        received_bytes = stats.received_bytes,
        accepted_frames = stats.accepted_frames,
        accepted_bytes = stats.accepted_bytes,
        transport_backpressure_drops = stats.transport_backpressure_drops,
        duplicate_frames = stats.duplicate_frames,
        late_frames = stats.late_frames,
        wrong_generation_frames = stats.wrong_generation_frames,
        discontinuities = stats.discontinuities,
        rejected_discontinuities = stats.rejected_discontinuities,
        protocol_errors = stats.decoder_errors,
        helper_failures = stats.backend_underruns,
        telemetry_drops = stats.telemetry_drops,
        "Linux microphone transport statistics"
    );
}

const MICROPHONE_TELEMETRY_QUEUE_DEPTH: usize = 4;

#[derive(Clone)]
struct LinuxTransportTelemetrySink {
    sender: std::sync::mpsc::SyncSender<LinuxTransportTelemetrySnapshot>,
    dropped: Arc<AtomicU64>,
}

struct LinuxTransportTelemetrySnapshot {
    stats: MicrophoneStats,
    final_snapshot: bool,
    duration: Duration,
    stop_reason: &'static str,
}

impl LinuxTransportTelemetrySink {
    fn try_snapshot(
        &self,
        mut stats: MicrophoneStats,
        final_snapshot: bool,
        duration: Duration,
        stop_reason: &'static str,
    ) {
        let carried_drops = self.dropped.swap(0, Ordering::Relaxed);
        stats.telemetry_drops = stats.telemetry_drops.saturating_add(carried_drops);
        let snapshot = LinuxTransportTelemetrySnapshot {
            stats,
            final_snapshot,
            duration,
            stop_reason,
        };
        if let Err(error) = self.sender.try_send(snapshot) {
            let lost = match error {
                std::sync::mpsc::TrySendError::Full(snapshot)
                | std::sync::mpsc::TrySendError::Disconnected(snapshot) => {
                    snapshot.stats.telemetry_drops.saturating_add(1)
                }
            };
            self.dropped.fetch_add(lost, Ordering::Relaxed);
        }
    }
}

fn spawn_linux_transport_telemetry(
    session_log_id: CorrelationId,
    generation: u32,
) -> LinuxTransportTelemetrySink {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<LinuxTransportTelemetrySnapshot>(
        MICROPHONE_TELEMETRY_QUEUE_DEPTH,
    );
    let dropped = Arc::new(AtomicU64::new(0));
    let _ = std::thread::Builder::new()
        .name("arcen-linux-mic-telemetry".to_string())
        .spawn(move || {
            while let Ok(snapshot) = receiver.recv() {
                log_linux_transport_stats(
                    &session_log_id,
                    generation,
                    snapshot.stats,
                    snapshot.final_snapshot,
                    snapshot.duration,
                    snapshot.stop_reason,
                );
            }
        });
    LinuxTransportTelemetrySink { sender, dropped }
}

const fn microphone_stop_reason(reason: SessionEndReason) -> &'static str {
    match reason {
        SessionEndReason::ClientClosed => "client_closed",
        SessionEndReason::ProtocolError => "protocol_error",
        SessionEndReason::ReadLivenessTimeout => "read_timeout",
        SessionEndReason::WriterEnded => "writer_ended",
        SessionEndReason::TransportError => "transport_error",
        SessionEndReason::HostShutdown => "host_shutdown",
        SessionEndReason::MediaEnded => "media_ended",
        _ => "session_end",
    }
}

/// The host only receives authentication, monitor metadata, and input/control
/// JSON. One MiB leaves ample room for 16 base64 EDIDs while capping the 16
/// pre-authentication connections at about 16 MiB. Outbound media is not
/// constrained by tungstenite's inbound message/frame limits.
const MAX_INBOUND_WS_MESSAGE: usize =
    arcen_protocol::CLIPBOARD_HEADER_SIZE + arcen_protocol::CHUNK_BYTES;

/// Bounds the number of `/dev/uhid` virtual devices one experimental-raw-hid
/// session may create, independent of the wire-level descriptor/report size
/// bounds already enforced in `arcen_protocol::decode_hid_device_added` /
/// `decode_hid_report`. Five supported vendors × a couple of interfaces each
/// leaves comfortable headroom without letting a hostile/buggy peer flood the
/// kernel with virtual devices.
#[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
const MAX_EXPERIMENTAL_RAW_HID_DEVICES: usize = 8;

/// `request_full_frame` IDR guard (mirrors `session_runtime.py`'s 0.5 s throttle
/// — a client stuck waiting for a decodable keyframe repeats the request every
/// frame; without this each repeat forces a fresh IDR).
const FULL_FRAME_IDR_GUARD: Duration = Duration::from_millis(500);

/// Periodic health_pong cadence (matches `health_loop.py`'s ~2 s beat).
const HEALTH_BEAT: Duration = Duration::from_secs(2);
const CONTROL_CHANNEL_CAPACITY: usize = 8;
/// Minimum interval between applied mid-session stream resizes (WIRE.md: the
/// host applies at most one resize per second; the watch channel coalesces
/// anything faster, latest wins).
const RESIZE_MIN_INTERVAL: Duration = Duration::from_secs(1);
const CRITICAL_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const INTERACTIVE_AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WEBSOCKET_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PAM_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const PAM_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const PAM_OPERATION_CAPACITY: usize = 4;
const PREAUTH_CAPACITY: usize = 16;
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const READ_LIVENESS_TIMEOUT: Duration = Duration::from_secs(30);
// Deadline for the initial client_hello / quality_settings handshake
// receives. Kept as a named constant (rather than inline) so tests can pass
// a short explicit override to `receive_client_hello`/`receive_quality_settings`
// instead of waiting out the real deadline.
const HANDSHAKE_RECEIVE_TIMEOUT: Duration = Duration::from_secs(15);
// Covers the serialized bounded microphone, media, and display restoration steps.
const SHUTDOWN_DISPLAY_DRAIN_TIMEOUT: Duration = Duration::from_secs(130);

#[derive(Clone)]
struct ControlSender {
    tx: mpsc::Sender<WriterControl>,
    dropped: Arc<AtomicU64>,
}

enum WriterControl {
    Message(Message),
    Barrier(oneshot::Sender<()>),
}

impl ControlSender {
    fn channel(capacity: usize) -> (Self, mpsc::Receiver<WriterControl>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    /// Health and transport pong messages are recoverable and may be dropped
    /// when a non-reading client fills the bounded control mailbox.
    fn send_best_effort(&self, message: Message, kind: &'static str) -> bool {
        match self.tx.try_send(WriterControl::Message(message)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                trace!(target: HEALTH, kind, dropped, "control mailbox full — coalescing/dropping");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    async fn send_required(&self, message: Message, kind: &'static str) -> bool {
        self.send_required_with_timeout(message, kind, CRITICAL_CONTROL_TIMEOUT)
            .await
    }

    async fn send_required_with_timeout(
        &self,
        message: Message,
        kind: &'static str,
        timeout: Duration,
    ) -> bool {
        match tokio::time::timeout(timeout, self.tx.send(WriterControl::Message(message))).await {
            Ok(Ok(())) => true,
            Ok(Err(_)) => {
                warn!(target: NET, kind, "required control mailbox closed");
                false
            }
            Err(_) => {
                warn!(
                    target: NET,
                    kind,
                    timeout_ms = timeout.as_millis() as u64,
                    "required control mailbox send timed out"
                );
                false
            }
        }
    }

    async fn barrier(&self) -> bool {
        let (complete, wait) = oneshot::channel();
        if !matches!(
            tokio::time::timeout(
                CRITICAL_CONTROL_TIMEOUT,
                self.tx.send(WriterControl::Barrier(complete)),
            )
            .await,
            Ok(Ok(()))
        ) {
            return false;
        }
        matches!(
            tokio::time::timeout(CRITICAL_CONTROL_TIMEOUT, wait).await,
            Ok(Ok(()))
        )
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

async fn send_critical_control<S>(sink: &mut S, message: Message, kind: &'static str) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_critical_control_with_timeout(sink, message, kind, CRITICAL_CONTROL_TIMEOUT).await
}

/// Close frame carrying a human-readable reason so the client can surface WHY
/// the host aborted (a bare `Close(None)` reads as a cryptic protocol error).
fn close_with_reason(reason: &str) -> Message {
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    // Close reasons ride in the control frame; keep them comfortably under the
    // 125-byte control-payload limit (2 bytes are the close code).
    let mut reason = reason.to_string();
    let mut cut = 120.min(reason.len());
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    reason.truncate(cut);
    Message::Close(Some(CloseFrame {
        code: CloseCode::Error,
        reason: reason.into(),
    }))
}

async fn send_critical_control_with_timeout<S>(
    sink: &mut S,
    message: Message,
    kind: &'static str,
    timeout: Duration,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_ws_with_timeout(sink, message, kind, timeout).await
}

async fn send_ws_with_timeout<S>(
    sink: &mut S,
    message: Message,
    kind: &'static str,
    timeout: Duration,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    match tokio::time::timeout(timeout, sink.send(message)).await {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            warn!(target: NET, %error, kind, "WebSocket send failed");
            false
        }
        Err(_) => {
            warn!(target: NET, kind, "WebSocket send timed out");
            false
        }
    }
}

async fn next_with_liveness<R>(
    stream: &mut R,
    timeout: Duration,
) -> Result<
    Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    tokio::time::error::Elapsed,
>
where
    R: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(timeout, stream.next()).await
}

async fn send_auth_result<S>(sink: &mut S, success: bool, message: &str) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_auth_result_with_resume(sink, success, message, None, None, false, None).await
}

#[allow(clippy::too_many_arguments)]
async fn send_auth_result_with_resume<S>(
    sink: &mut S,
    success: bool,
    message: &str,
    resume_grant: Option<&arcen_identity::DirectResumeGrantToken>,
    resume_window_secs: Option<u32>,
    resumed: bool,
    error_code: Option<ResumeErrorCode>,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_auth_result_with_resume_timeout(
        sink,
        success,
        message,
        resume_grant,
        resume_window_secs,
        resumed,
        error_code,
        CRITICAL_CONTROL_TIMEOUT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_auth_result_with_resume_timeout<S>(
    sink: &mut S,
    success: bool,
    message: &str,
    resume_grant: Option<&arcen_identity::DirectResumeGrantToken>,
    resume_window_secs: Option<u32>,
    resumed: bool,
    error_code: Option<ResumeErrorCode>,
    timeout: Duration,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let result = AuthResult {
        msg_type: AUTH_RESULT.to_string(),
        success,
        message: message.to_string(),
        resume_grant: resume_grant.map(|grant| grant.expose_for_transport().to_string()),
        resume_window_secs,
        resumed,
        error_code,
    };
    match serde_json::to_string(&result) {
        Ok(json) => {
            send_critical_control_with_timeout(sink, Message::Text(json), "auth_result", timeout)
                .await
        }
        Err(error) => {
            warn!(target: AUTH, %error, "failed to serialize auth_result");
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_successor_auth_result_or_drain<S>(
    sink: &mut S,
    message: &str,
    successor_grant: &arcen_identity::DirectResumeGrantToken,
    window_secs: u32,
    resumed: bool,
    timeout: Duration,
    registry: &resume::ResumeRegistry,
    active_session_id: &ActiveHostSessionId,
) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    if send_auth_result_with_resume_timeout(
        sink,
        true,
        message,
        Some(successor_grant),
        Some(window_secs),
        resumed,
        None,
        timeout,
    )
    .await
    {
        return true;
    }
    if let Err(error) = registry.begin_drain(active_session_id) {
        tracing::error!(
            target: SESSION,
            ?error,
            "successor delivery failure could not drain resume authority"
        );
    }
    false
}

async fn send_resume_rejection<S>(sink: &mut S, message: &str, error_code: ResumeErrorCode) -> bool
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_auth_result_with_resume(sink, false, message, None, None, false, Some(error_code)).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthenticationRoute {
    ResumeRegistry,
    Pam,
}

fn authentication_route(response: &AuthResponse) -> AuthenticationRoute {
    if response.method == AUTH_METHOD_RESUME {
        AuthenticationRoute::ResumeRegistry
    } else {
        AuthenticationRoute::Pam
    }
}

fn clear_resume_secrets(response: &mut AuthResponse) {
    response.credential.zeroize();
    if let Some(holder_nonce) = &mut response.resume_holder_nonce {
        holder_nonce.zeroize();
    }
    if let Some(grant) = &mut response.resume_grant {
        grant.zeroize();
    }
    response.resume_holder_nonce = None;
    response.resume_grant = None;
}

fn build_auth_request(
    disclaimer: Option<&PreparedDisclaimer>,
    resume_supported: bool,
    detached_resume: bool,
    multi_monitor_v1: Option<arcen_protocol::messages::AuthMultiMonitorOfferMsg>,
) -> AuthRequest {
    AuthRequest {
        msg_type: AUTH_REQUEST.to_string(),
        auth_methods: vec!["pam".to_string()],
        challenge: String::new(),
        salt: String::new(),
        auth_mode: Some("pam".to_string()),
        disclaimer: if detached_resume {
            None
        } else {
            disclaimer.map(|value| value.text().to_string())
        },
        multi_monitor_v1,
    }
    .with_resume_support(resume_supported)
}

/// Builds this connection's `multi_monitor_v1` operator gate from `cfg`.
///
/// `cfg.multi_monitor.heads` (duplicate/unrecognized head rejection) and the
/// `cfg.encoder` vs. `cfg.multi_monitor.advertise_enabled` policy conflict
/// (`Auto`/`WindowsMediaFoundation` cannot ever back an advertised offer)
/// are both already validated and rejected at CLI/config-load time (see
/// `cli::parse`'s end-of-parse `validate-config` checks), so a build
/// failure here should never happen in practice; it is still handled
/// defensively (fully disabled, logged once) rather than panicking, in case
/// a future hot-reload path ever bypasses that earlier validation.
fn multi_monitor_gate(cfg: &Config) -> multi_monitor::MultiMonitorGate {
    let gate =
        multi_monitor::MultiMonitorGate::from_config(&cfg.multi_monitor, cfg.encoder).unwrap_or_else(
        |error| {
            warn!(
                target: SESSION,
                %error,
                "multi_monitor_v1 head configuration is invalid; disabling advertisement for this connection"
            );
            multi_monitor::MultiMonitorGate::disabled()
        },
    );
    info!(
        target: SESSION,
        advertise_enabled = cfg.multi_monitor.advertise_enabled,
        configured_heads = cfg.multi_monitor.heads.len(),
        effective_max_monitors = gate.inventory.as_ref().map_or(0, |inventory| inventory.len()),
        nvenc_session_limit = ?cfg.multi_monitor.nvenc_session_limit,
        nvenc_capacity_policy = if cfg.multi_monitor.nvenc_session_limit.is_some() {
            "operator_ceiling_then_runtime_probe"
        } else {
            "runtime_probe"
        },
        allow_software_fallback = cfg.multi_monitor.allow_software_fallback,
        encoder = ?cfg.encoder,
        "effective Linux multi-monitor admission policy"
    );
    gate
}

struct AuthenticatedConnection {
    response: AuthResponse,
    display_plan: auth::SessionDisplayPlan,
    multi_monitor_outcome: multi_monitor::MultiMonitorOutcome,
    launcher: AuthenticatedLauncher,
    session_config: Config,
}

fn config_with_initial_video_request(
    config: &Config,
    response: &AuthResponse,
) -> Result<Config, String> {
    let mut resolved = config.clone();
    if let Some(request) = response.initial_video.as_ref() {
        resolved.apply_initial_video_request(request)?;
    }
    Ok(resolved)
}

#[derive(Debug)]
struct AcknowledgedDisclaimer {
    disclaimer: Arc<PreparedDisclaimer>,
    accepted_at: u64,
}

fn validated_requested_timezone(cfg: &Config, response: &AuthResponse) -> Option<IanaTimeZone> {
    if cfg.auth_mode != AuthMode::Pam || !cfg.timezone_redirection {
        return None;
    }
    let Some(requested) = response.timezone.as_deref() else {
        warn!(
            target: SESSION,
            "timezone redirection enabled but client timezone is absent; continuing without redirection"
        );
        return None;
    };
    match validate_zoneinfo_timezone(&cfg.zoneinfo_root, requested) {
        Ok(timezone) => Some(timezone),
        Err(error) => {
            warn!(
                target: SESSION,
                %error,
                "client timezone failed host zoneinfo validation; continuing without redirection"
            );
            None
        }
    }
}

fn resolve_session_log_id(value: Option<&str>) -> Result<(CorrelationId, bool), String> {
    if let Some(value) = value {
        if let Ok(id) = CorrelationId::parse_uuid(value) {
            return Ok((id, false));
        }
    }
    Ok((fallback_session_log_id()?, true))
}

fn fallback_session_log_id() -> Result<CorrelationId, String> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("generate session log id fallback: {error}"))?;
    Ok(CorrelationId::from_uuid_v4_bytes(bytes))
}

/// Emits `SESSION_AUTH_FAIL` (1101) at one final PAM/validation/timeout
/// refusal boundary. Never carries the client's username, credential, or a
/// raw PAM/transport error string in structured `fields` — the claimed
/// username is never authenticated at this boundary, so top-level `user`
/// always stays `None` here. `peer_addr`, unlike identity, is a transport
/// fact known regardless of auth outcome, so it is still supplied when the
/// caller has one.
fn emit_session_auth_fail(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    peer_addr: Option<&str>,
    stage: &'static str,
    reason_class: &'static str,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("auth_method", FieldValue::String("pam".to_string()));
    let _ = fields.insert("stage", FieldValue::String(stage.to_string()));
    let _ = fields.insert("reason_class", FieldValue::String(reason_class.to_string()));
    let context =
        emitter.session_context(session_log_id, None, peer_addr.map(str::to_string), None);
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::SessionAuthFail,
        context,
        fields,
    );
}

/// Emits `SESSION_AUTH_OK` (1100) once PAM, uid/gid bind, and logind
/// graphical-session readiness are all confirmed. Carries the now-
/// authenticated top-level `user` and the connection's `peer_addr`; the
/// nested `fields` set never repeats identity.
fn emit_session_auth_ok(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer_addr: Option<&str>,
    os_session_id: Option<i64>,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("auth_method", FieldValue::String("pam".to_string()));
    let _ = fields.insert(
        "identity_binding",
        FieldValue::String("uid_gid_logind_bound".to_string()),
    );
    if let Some(os_session_id) = os_session_id {
        let _ = fields.insert("os_session_id", FieldValue::Integer(os_session_id));
    }
    let context = emitter.session_context(
        session_log_id,
        Some(user.to_string()),
        peer_addr.map(str::to_string),
        None,
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::SessionAuthOk,
        context,
        fields,
    );
}

/// Safe, closed reason-class mapping for [`LauncherError`]. Never carries
/// its `Display` text (which may include raw PAM/child diagnostics) into a
/// native lifecycle event.
fn launcher_error_reason_class(error: &LauncherError) -> &'static str {
    match error {
        LauncherError::BinaryUnavailable => "binary_unavailable",
        LauncherError::Rejected => "rejected",
        LauncherError::Protocol => "protocol_error",
        LauncherError::Timeout => "timeout",
        LauncherError::Io(_) => "io_error",
        LauncherError::Identity(_) => "identity_invalid",
        LauncherError::Agent(_) => "agent_error",
        LauncherError::PamInitialization => "pam_init_failed",
        LauncherError::PamSession => "pam_session_failed",
        LauncherError::LogindSession => "logind_session_missing",
        LauncherError::RootRequired => "root_required",
        LauncherError::XorgConfig => "xorg_config_invalid",
        LauncherError::XorgStart => "xorg_start_failed",
        LauncherError::XorgVerify => "xorg_verify_failed",
        LauncherError::XorgRelease => "xorg_release_failed",
        LauncherError::LogindActivation => "logind_activation_failed",
        LauncherError::LogindUnlock => "logind_unlock_failed",
        LauncherError::Deskside => "deskside_refused",
    }
}

async fn receive_auth_response<S>(
    socket: &mut S,
    cfg: &Config,
    resume_supported: bool,
    detached_resume: bool,
    emitter: &LifecycleEmitter,
    remote_host: &str,
) -> Result<(AuthResponse, CorrelationId), ()>
where
    S: futures_util::Sink<Message>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    S::Error: std::fmt::Display,
{
    let multi_monitor_offer = multi_monitor::build_offer(&multi_monitor_gate(cfg));
    let request = build_auth_request(
        cfg.disclaimer.as_deref(),
        resume_supported,
        detached_resume,
        multi_monitor_offer,
    );
    let request_json = serde_json::to_string(&request).map_err(|error| {
        warn!(target: AUTH, %error, "failed to serialize auth_request");
    })?;
    if !send_critical_control(socket, Message::Text(request_json), "auth_request").await {
        emit_session_auth_fail(
            emitter,
            eventlog::random_correlation_id(),
            Some(remote_host),
            "send_auth_request",
            "transport_error",
        );
        return Err(());
    }

    let auth_response_timeout = if cfg.disclaimer.is_some() {
        INTERACTIVE_AUTH_RESPONSE_TIMEOUT
    } else {
        AUTH_RESPONSE_TIMEOUT
    };
    let incoming = tokio::time::timeout(auth_response_timeout, socket.next())
        .await
        .map_err(|_| {
            warn!(target: AUTH, "timed out waiting for auth_response");
            emit_session_auth_fail(
                emitter,
                eventlog::random_correlation_id(),
                Some(remote_host),
                "await_auth_response",
                "timeout",
            );
        })?;
    let text = match incoming {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(_)) => {
            warn!(target: AUTH, "expected text auth_response");
            send_auth_result(socket, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                eventlog::random_correlation_id(),
                Some(remote_host),
                "await_auth_response",
                "protocol_error",
            );
            return Err(());
        }
        Some(Err(error)) => {
            warn!(target: AUTH, %error, "WebSocket failed while waiting for auth_response");
            emit_session_auth_fail(
                emitter,
                eventlog::random_correlation_id(),
                Some(remote_host),
                "await_auth_response",
                "transport_error",
            );
            return Err(());
        }
        None => {
            warn!(target: AUTH, "client disconnected before auth_response");
            emit_session_auth_fail(
                emitter,
                eventlog::random_correlation_id(),
                Some(remote_host),
                "await_auth_response",
                "client_disconnected",
            );
            return Err(());
        }
    };

    let response = match serde_json::from_str::<AuthResponse>(&text) {
        Ok(response) => response,
        Err(error) => {
            warn!(target: AUTH, %error, "invalid auth_response");
            send_auth_result(socket, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                eventlog::random_correlation_id(),
                Some(remote_host),
                "parse_auth_response",
                "protocol_error",
            );
            return Err(());
        }
    };
    let (session_log_id, replaced) = resolve_session_log_id(response.session_log_id.as_deref())
        .map_err(|error| {
            warn!(target: SESSION, %error, "failed to resolve session log id");
            emit_session_auth_fail(
                emitter,
                eventlog::random_correlation_id(),
                Some(remote_host),
                "resolve_session_log_id",
                "internal_error",
            );
        })?;
    if replaced {
        warn!(
            target: SESSION,
            sid = %session_log_id,
            "client session log id was absent or invalid; generated host fallback"
        );
    }
    drop(text);
    Ok((response, session_log_id))
}

async fn authenticate_pam_response<S>(
    sink: &mut S,
    cfg: &Config,
    pam_slots: Arc<Semaphore>,
    remote_host: &str,
    emitter: &LifecycleEmitter,
    mut response: AuthResponse,
    session_log_id: CorrelationId,
) -> Result<AuthenticatedConnection, ()>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let acknowledged_disclaimer =
        match validate_disclaimer_acknowledgment(cfg.disclaimer.clone(), &response) {
            Ok(acknowledged) => acknowledged,
            Err(reason_class) => {
                warn!(target: AUTH, "rejected disclaimer acknowledgment");
                send_auth_result(sink, false, "Authentication failed").await;
                emit_session_auth_fail(
                    emitter,
                    session_log_id,
                    Some(remote_host),
                    "validate_disclaimer",
                    reason_class,
                );
                return Err(());
            }
        };
    if let Err(error) = auth::validate_pam_response(&response) {
        warn!(target: AUTH, %error, "rejected invalid PAM auth_response");
        send_auth_result(sink, false, "Authentication failed").await;
        emit_session_auth_fail(
            emitter,
            session_log_id,
            Some(remote_host),
            "validate_request",
            "invalid_request",
        );
        return Err(());
    }
    let display_plan = match auth::session_display_plan(&response) {
        Ok(plan) => plan,
        Err(error) => {
            warn!(target: AUTH, %error, "rejected unsupported client display request");
            send_auth_result(sink, false, &error).await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "validate_display_request",
                "invalid_request",
            );
            return Err(());
        }
    };
    let session_config = match config_with_initial_video_request(cfg, &response) {
        Ok(config) => config,
        Err(error) => {
            warn!(
                target: AUTH,
                %error,
                "auth-time video request rejected before display or encoder creation"
            );
            send_auth_result(sink, false, &format!("Video request rejected: {error}")).await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "validate_video_request",
                "invalid_request",
            );
            return Err(());
        }
    };
    if let Some(requested) = response.multi_monitor_v1.as_ref() {
        let full_color_required = requested
            .requested_topology()
            .monitors()
            .iter()
            .filter(|monitor| {
                monitor.quality_intent
                    == arcen_protocol::messages::MonitorQualityIntentMsg::FullColorRequired
            })
            .count();
        if full_color_required > 0 && session_config.chroma != "yuv444" {
            let error = format!(
                "multi-monitor quality admission failed: {full_color_required} display(s) require \
                 full-color 4:4:4, but this host profile is configured for yuv420"
            );
            warn!(target: AUTH, %error, "rejected multi-monitor quality request");
            send_auth_result(sink, false, &error).await;
            return Err(());
        }
    }
    let multi_monitor_gate = multi_monitor_gate(&session_config);
    let multi_monitor_offer = multi_monitor::build_offer(&multi_monitor_gate);
    let multi_monitor_outcome = multi_monitor::admit_requested_topology(
        &multi_monitor_gate,
        multi_monitor_offer.as_ref(),
        response.multi_monitor_v1.as_ref(),
    );
    if let multi_monitor::MultiMonitorOutcome::Planned { plan, .. } = &multi_monitor_outcome {
        if let Err(error) = multi_capenc::validate_monitor_resource_policy(
            plan,
            session_config.multi_monitor.nvenc_session_limit,
            session_config.multi_monitor.allow_software_fallback
                && session_config.exact_pins_allow_software_h264(),
        ) {
            let error = format!("multi-monitor resource admission failed: {error}");
            warn!(target: AUTH, %error, "rejected multi-monitor resource request");
            send_auth_result(sink, false, &error).await;
            return Err(());
        }
    }

    let username = response.username.clone();
    let password = std::mem::take(&mut response.credential);
    let pam_permit = match tokio::time::timeout(PAM_QUEUE_TIMEOUT, pam_slots.acquire_owned()).await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            warn!(target: AUTH, "PAM operation semaphore closed");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "acquire_pam_slot",
                "pam_unavailable",
            );
            return Err(());
        }
        Err(_) => {
            warn!(target: AUTH, "PAM operation capacity exhausted");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "acquire_pam_slot",
                "pam_busy",
            );
            return Err(());
        }
    };
    let launcher_binary = match cfg.resolve_session_launcher_binary() {
        Some(binary) => binary,
        None => {
            warn!(target: AUTH, "privileged session-launcher binary is unavailable");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "resolve_binaries",
                "binary_unavailable",
            );
            return Err(());
        }
    };
    let agent_binary = match cfg.resolve_session_agent_binary() {
        Some(binary) => binary,
        None => {
            warn!(target: AUTH, "session-agent binary is unavailable");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "resolve_binaries",
                "binary_unavailable",
            );
            return Err(());
        }
    };
    let launcher_config = LauncherConfig {
        binary: launcher_binary,
        pam_service: cfg.pam_service.clone(),
        display: cfg.session_display.clone(),
        gpu_head: cfg.session_gpu_head.clone(),
        xorg_binary: cfg.xorg_bin.clone(),
        xorg_config_template: cfg.xorg_config_template.clone(),
        runtime_root: cfg.session_runtime_root.clone(),
        agent_binary,
        desktop: cfg.desktop_session.clone(),
        deskside: cfg.deskside.clone(),
    };
    let launcher_result = tokio::time::timeout(
        PAM_OPERATION_TIMEOUT,
        AuthenticatedLauncher::authenticate(
            &launcher_config,
            &username,
            password,
            remote_host,
            pam_permit,
            session_log_id.clone(),
        ),
    )
    .await;
    match launcher_result {
        Err(_) => {
            warn!(target: AUTH, %username, service = %cfg.pam_service, "PAM authentication timed out");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "pam_authenticate",
                "timeout",
            );
            Err(())
        }
        Ok(Ok(mut launcher)) => {
            launcher.set_timezone(validated_requested_timezone(cfg, &response));
            info!(
                target: AUTH,
                username = %launcher.identity.username,
                uid = launcher.identity.uid,
                service = %cfg.pam_service,
                "session launcher accepted PAM authentication for OS user"
            );
            if let Some(acknowledged) = &acknowledged_disclaimer {
                let acceptance =
                    DisclaimerAcceptance::new(&acknowledged.disclaimer, acknowledged.accepted_at);
                info!(
                    target: SESSION,
                    sid = %session_log_id,
                    disclaimer_locale = acceptance.locale().as_str(),
                    disclaimer_sha256 = %acceptance.digest().to_lower_hex(),
                    disclaimer_accepted_at = acceptance.accepted_at_epoch_seconds(),
                    success = true,
                    "disclaimer acceptance recorded"
                );
            }
            Ok(AuthenticatedConnection {
                response,
                display_plan,
                multi_monitor_outcome,
                launcher,
                session_config,
            })
        }

        Ok(Err(LauncherError::Rejected)) => {
            warn!(target: AUTH, %username, service = %cfg.pam_service, "PAM authentication rejected");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "pam_authenticate",
                "rejected",
            );
            Err(())
        }
        Ok(Err(error)) => {
            warn!(target: AUTH, %username, service = %cfg.pam_service, %error, "PAM authentication error");
            send_auth_result(sink, false, "Authentication failed").await;
            emit_session_auth_fail(
                emitter,
                session_log_id,
                Some(remote_host),
                "pam_authenticate",
                launcher_error_reason_class(&error),
            );
            Err(())
        }
    }
}

fn validate_disclaimer_acknowledgment(
    disclaimer: Option<Arc<PreparedDisclaimer>>,
    response: &AuthResponse,
) -> Result<Option<AcknowledgedDisclaimer>, &'static str> {
    let Some(disclaimer) = disclaimer else {
        return Ok(None);
    };
    let Some(acknowledgment) = response.disclaimer_acceptance_sha256.as_deref() else {
        return Err("acknowledgment_missing");
    };
    match disclaimer.matches_acknowledgment(acknowledgment) {
        Ok(true) => {}
        Ok(false) => return Err("acknowledgment_mismatch"),
        Err(_) => return Err("acknowledgment_invalid"),
    }
    let accepted_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "clock_invalid")?
        .as_secs();
    Ok(Some(AcknowledgedDisclaimer {
        disclaimer,
        accepted_at,
    }))
}

/// Typed classification of why the frame relay/dispatch race ended,
/// replacing the previous reason-discarding `tokio::select!` outcome so the
/// teardown path can emit `SESSION_END` (clean) or `SESSION_INTERRUPTED`
/// (everything else) with an accurate stage/reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEndReason {
    /// The client sent an explicit WebSocket close frame.
    ClientClosed,
    /// The client's read-liveness deadline elapsed.
    ReadLivenessTimeout,
    /// The WebSocket transport failed or ended without a close frame.
    TransportError,
    /// The client sent an invalid in-session control message.
    ProtocolError,
    /// The capenc frame channel closed unexpectedly.
    MediaEnded,
    /// The outbound sender failed to write to the client.
    WriterEnded,
    /// Pier shutdown or a terminal resume-authority command ended the attachment.
    HostShutdown,
    /// Attached grant generation or signing failed and requires terminal drain.
    ResumeAuthorityFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttachmentEnd {
    reason: SessionEndReason,
    transport_loss_observed_at: Option<MonotonicMillis>,
    /// `true` only once this attachment reached the point where the session
    /// is genuinely usable: applied `ServerHello` (with its multi-monitor
    /// capability, when committed) sent, `ClientHello`/initial quality
    /// settings accepted, and the frame pump/mux/sender/dispatcher started
    /// serving — i.e. every earlier admission/capenc/READY/capability/mux
    /// construction step already succeeded. `false` for every early return
    /// out of `run_attachment` (constructed via [`Self::terminal`]) that
    /// happens before that point, including every multi-monitor
    /// capenc/READY/applied-capability/mux-construction failure. The
    /// non-resumable caller uses this to distinguish "this one attempt hit
    /// an ordinary mid/end-of-session condition" from "a committed,
    /// fixed-for-the-desktop's-lifetime multi-monitor topology could not
    /// even be brought up" — the latter must never be treated as safe to
    /// `disconnect`-and-retry with the exact same broken plan.
    reached_usable: bool,
}

impl AttachmentEnd {
    const fn terminal(reason: SessionEndReason) -> Self {
        Self {
            reason,
            transport_loss_observed_at: None,
            reached_usable: false,
        }
    }
}

impl SessionEndReason {
    const fn is_clean(self) -> bool {
        matches!(self, Self::ClientClosed)
    }

    const fn stage(self) -> &'static str {
        match self {
            Self::ClientClosed => "client_close",
            Self::ReadLivenessTimeout => "read_liveness",
            Self::TransportError => "transport",
            Self::ProtocolError => "protocol",
            Self::MediaEnded => "media",
            Self::WriterEnded => "writer",
            Self::HostShutdown => "host_shutdown",
            Self::ResumeAuthorityFailure => "resume_authority",
        }
    }

    const fn reason_class(self) -> &'static str {
        match self {
            Self::ClientClosed => "client_closed",
            Self::ReadLivenessTimeout => "read_liveness_timeout",
            Self::TransportError => "transport_error",
            Self::ProtocolError => "protocol_error",
            Self::MediaEnded => "media_ended",
            Self::WriterEnded => "writer_failed",
            Self::HostShutdown => "host_shutdown",
            Self::ResumeAuthorityFailure => "resume_authority_failure",
        }
    }
}

fn emit_session_stream_start(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    media_plan: &ResolvedMediaPlan,
    user: Option<&str>,
    peer_addr: Option<&str>,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "encoder",
        FieldValue::String(media_plan.backend.ready_token().to_string()),
    );
    let _ = fields.insert(
        "codec",
        FieldValue::String(media_plan.codec_token().to_string()),
    );
    let _ = fields.insert(
        "chroma",
        FieldValue::String(media_plan.chroma_token().to_string()),
    );
    let _ = fields.insert("width", FieldValue::Integer(i64::from(media_plan.width)));
    let _ = fields.insert("height", FieldValue::Integer(i64::from(media_plan.height)));
    let _ = fields.insert("fps", FieldValue::Integer(i64::from(media_plan.fps)));
    let _ = fields.insert("display_backend", FieldValue::String("nvctrl".to_string()));
    let context = emitter.session_context(
        session_log_id,
        user.map(str::to_string),
        peer_addr.map(str::to_string),
        None,
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::SessionStreamStart,
        context,
        fields,
    );
}

/// Emits `SESSION_END` (1103) for a clean client close or `SESSION_INTERRUPTED`
/// (1104) for every other termination reason, once cleanup has already
/// gathered the final frame counters.
#[allow(clippy::too_many_arguments)]
fn emit_session_end_or_interrupted(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    reason: SessionEndReason,
    duration: Duration,
    frames_sent: u64,
    frames_dropped: u64,
    user: Option<&str>,
    peer_addr: Option<&str>,
    client_network: Option<&arcen_protocol::messages::ClientNetworkSnapshotMsg>,
) {
    let duration_ms = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
    let frames_sent = i64::try_from(frames_sent).unwrap_or(i64::MAX);
    let frames_dropped = i64::try_from(frames_dropped).unwrap_or(i64::MAX);
    let context = emitter.session_context(
        session_log_id,
        user.map(str::to_string),
        peer_addr.map(str::to_string),
        None,
    );
    if reason.is_clean() {
        let mut fields = StructuredFields::default();
        let _ = fields.insert(
            "reason_class",
            FieldValue::String(reason.reason_class().to_string()),
        );
        let _ = fields.insert("duration_ms", FieldValue::Integer(duration_ms));
        let _ = fields.insert("frames_sent", FieldValue::Integer(frames_sent));
        let _ = fields.insert("frames_dropped", FieldValue::Integer(frames_dropped));
        crate::observability::insert_client_network_fields(&mut fields, client_network);
        crate::emit_lifecycle_event_with_context(
            emitter,
            LifecycleEventKind::SessionEnd,
            context,
            fields,
        );
    } else {
        let mut fields = StructuredFields::default();
        let _ = fields.insert("stage", FieldValue::String(reason.stage().to_string()));
        let _ = fields.insert(
            "reason_class",
            FieldValue::String(reason.reason_class().to_string()),
        );
        let _ = fields.insert("duration_ms", FieldValue::Integer(duration_ms));
        let _ = fields.insert("frames_sent", FieldValue::Integer(frames_sent));
        let _ = fields.insert("frames_dropped", FieldValue::Integer(frames_dropped));
        crate::observability::insert_client_network_fields(&mut fields, client_network);
        crate::emit_lifecycle_event_with_context(
            emitter,
            LifecycleEventKind::SessionInterrupted,
            context,
            fields,
        );
    }
}

/// Safe, closed reason-class mapping for a startup `std::io::Error`. Never
/// carries the raw OS error string into a lifecycle event.
fn startup_failure_reason_class(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::AddrInUse => "address_in_use",
        std::io::ErrorKind::PermissionDenied => "permission_denied",
        std::io::ErrorKind::InvalidInput => "invalid_tls_config",
        std::io::ErrorKind::NotFound => "not_found",
        _ => "io_error",
    }
}

fn emit_service_failed(emitter: &LifecycleEmitter, stage: &'static str, error: &std::io::Error) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
    let _ = fields.insert("stage", FieldValue::String(stage.to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String(startup_failure_reason_class(error).to_string()),
    );
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::ServiceFailed,
        eventlog::random_correlation_id(),
        fields,
    );
}

fn emit_service_start(emitter: &LifecycleEmitter) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
    let _ = fields.insert("version", FieldValue::String(crate::VERSION.to_string()));
    let _ = fields.insert("pid", FieldValue::Integer(i64::from(std::process::id())));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::ServiceStart,
        eventlog::random_correlation_id(),
        fields,
    );
}

fn emit_service_stop(emitter: &LifecycleEmitter, uptime: Duration) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("signal_shutdown".to_string()),
    );
    let _ = fields.insert(
        "uptime_ms",
        FieldValue::Integer(i64::try_from(uptime.as_millis()).unwrap_or(i64::MAX)),
    );
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::ServiceStop,
        eventlog::random_correlation_id(),
        fields,
    );
}

/// Bounded per-sink wait applied at every step of the post-`SERVICE_STOP`
/// shutdown drain below.
const SHUTDOWN_OBSERVABILITY_FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

/// Orderly shutdown drain (finding #3), run once, immediately after
/// `SERVICE_STOP`: a bounded flush makes `SERVICE_STOP` durable, then any
/// sink-loss deltas accumulated since the last heartbeat drain
/// (`emit_service_loss_notices`) are emitted as origin-excluding
/// `TELEMETRY_DROPPED` notices — covering a service that stops well before
/// its next periodic heartbeat, so that loss is never silently dropped
/// just because the process is exiting — and flushed in turn. A final
/// bounded delta check then confirms nothing further slipped in during
/// this exact sequence.
///
/// Every phase below runs unconditionally: a failure in one bounded phase
/// (for example, one unhealthy sink's flush erroring) never skips the
/// later phases, because a flush failure on sink A must never prevent a
/// healthy sink B from still receiving the loss notices this drain is
/// responsible for reporting. Each phase's own error is recorded, not
/// returned immediately; all recorded errors are aggregated into one
/// final `Err` (or `Ok(())` if every phase succeeded and nothing is
/// unreported).
///
/// Joining the sink worker threads themselves is deliberately not this
/// function's job: `main`'s `flush_logging_before_exit` already calls
/// `LogController::shutdown` exactly once, unconditionally, in both the
/// `Ok` and `Err` arms of its match on `serve`'s result, immediately after
/// `serve` returns — duplicating that here would join the same workers
/// twice.
///
/// # Errors
///
/// Returns every bounded-phase failure — instead of silently discarding
/// the loss — aggregated into one message: either bounded flush's
/// failure, or a sink-loss delta that remains unreported after the
/// notice-emission-and-flush sequence above. This propagates out of
/// `serve` so `main` reports `ExitCode::FAILURE` instead of a falsely
/// clean shutdown.
fn drain_final_loss_before_stop(
    emitter: &LifecycleEmitter,
    handle: &arcen_observability::ObservabilityHandle,
    timeout_per_sink: Duration,
) -> Result<(), String> {
    let mut errors = Vec::new();

    if let Err(error) = handle.flush(timeout_per_sink) {
        errors.push(format!("flush before shutdown loss drain: {error}"));
    }

    // Unconditional: a failed first flush above (recorded, not returned)
    // must not skip draining and emitting whatever loss deltas already
    // exist — including any `FlushFailure` delta the flush attempt itself
    // just produced for the unhealthy sink — to every other, healthy,
    // non-origin sink.
    emitter.emit_loss_notices();

    if let Err(error) = handle.flush(timeout_per_sink) {
        errors.push(format!("flush shutdown loss notices: {error}"));
    }

    if let Err(error) = unreported_loss_error(handle.drain_loss_deltas().len()) {
        errors.push(error);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Pure decision for `drain_final_loss_before_stop`'s last step: a nonzero
/// residual after the notice-emission-and-flush sequence means some loss
/// could not be reported before shutdown, and must surface as an explicit
/// error rather than being silently discarded.
fn unreported_loss_error(residual_count: usize) -> Result<(), String> {
    if residual_count == 0 {
        Ok(())
    } else {
        Err(format!(
            "{residual_count} sink-loss delta(s) remained unreported after the shutdown drain"
        ))
    }
}

/// Emits `EFFECTIVE_PROFILE` (1805, always delivered at `Critical`/Level0)
/// reporting the currently active `OperationalProfile` and how it was
/// resolved (`cli_override` / `config_level` / `config_legacy_verbosity` /
/// `production_default`). Called once at startup after `emit_service_start`
/// and again after every successful SIGHUP reload, so an operator can always
/// see what took effect without inferring it from verbosity of output.
/// Builds the closed `EFFECTIVE_PROFILE` field set for one `(profile,
/// source)` pair. Pulled out of `emit_effective_profile` so the field
/// mapping is directly unit-testable without constructing a `LogController`.
fn effective_profile_fields(profile: OperationalProfile, source: &str) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "profile_level",
        FieldValue::Integer(i64::from(u8::from(profile))),
    );
    let _ = fields.insert(
        "profile_name",
        FieldValue::String(profile.as_str().to_string()),
    );
    let _ = fields.insert("profile_source", FieldValue::String(source.to_string()));
    fields
}

/// Emits `EFFECTIVE_PROFILE` (1805, always delivered at `Critical`/Level0)
/// reporting the currently active `OperationalProfile` and how it was
/// resolved (`cli_override` / `config_level` / `config_legacy_verbosity` /
/// `production_default`). Called once at startup after `emit_service_start`
/// and again after every successful SIGHUP reload, so an operator can always
/// see what took effect without inferring it from verbosity of output.
fn emit_effective_profile(emitter: &LifecycleEmitter, log_controller: &LogController) {
    let (Ok(profile), Ok(source)) = (
        log_controller.profile(),
        log_controller.profile_source_name(),
    ) else {
        // Only fails if the runtime has already been shut down; nothing to
        // report in that case.
        return;
    };
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::EffectiveProfile,
        eventlog::random_correlation_id(),
        effective_profile_fields(profile, source),
    );
}

/// Maps a `HealthState` to its rank for the shared `service_health` atomic
/// (higher = worse), so concurrent sessions can be combined with a single
/// lock-free `fetch_max`.
fn health_state_rank(state: arcen_telemetry::HealthState) -> u8 {
    match state {
        arcen_telemetry::HealthState::Ok => 1,
        arcen_telemetry::HealthState::Degraded => 2,
        arcen_telemetry::HealthState::Critical => 3,
    }
}

/// Accumulates one health tick's already-computed per-tick sample deltas
/// into the Level2 (Info) 10s aggregation window.
///
/// `sample.frames_sent`/`frames_dropped`/`input_events` are each the tick's
/// own delta (`HealthObservation::sample`, already `saturating_sub`-computed
/// by `SessionHealth::observe`), never the raw cumulative `HostCounters`
/// totals — accumulating a cumulative total across every tick would make
/// the reported 10s sum grow unbounded with session lifetime instead of
/// reflecting only the last five 2s ticks.
fn accumulate_health_window(
    window_frames_sent: &mut u64,
    window_frames_dropped: &mut u64,
    window_input_events: &mut u64,
    window_worst_overall: &mut Option<arcen_telemetry::HealthState>,
    sample: &QosSample,
    overall: Option<arcen_telemetry::HealthState>,
) {
    *window_frames_sent = window_frames_sent.saturating_add(sample.frames_sent.unwrap_or(0));
    *window_frames_dropped =
        window_frames_dropped.saturating_add(sample.frames_dropped.unwrap_or(0));
    *window_input_events = window_input_events.saturating_add(sample.input_events.unwrap_or(0));
    if let Some(state) = overall {
        *window_worst_overall = Some(match *window_worst_overall {
            Some(worst) if health_state_rank(worst) >= health_state_rank(state) => worst,
            _ => state,
        });
    }
}

/// Emits a service-level `HEALTH_SNAPSHOT` (1806, always delivered at
/// `Critical`/Level0) summarizing the worst per-session overall health
/// observed since the previous 60-second tick, read-and-reset so a session
/// that has already ended cannot leave the service view stuck at a stale
/// severity. `overall_state` is `"unavailable"` — never a stand-in healthy
/// value — when no session reported a health tick in the window.
fn emit_service_health_snapshot(emitter: &LifecycleEmitter, service_health: &AtomicU8) {
    let rank = service_health.swap(0, Ordering::Relaxed);
    let overall_state = match rank {
        1 => "ok",
        2 => "degraded",
        3 => "critical",
        _ => "unavailable",
    };
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "overall_state",
        FieldValue::String(overall_state.to_string()),
    );
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::HealthSnapshot,
        eventlog::random_correlation_id(),
        fields,
    );
}

/// Drains every registered sink's complete loss deltas (queue-full,
/// queue-closed, delivery-failure, and flush-failure, each counted from the
/// previous complete drain, across the canonical file sink and the
/// journald/syslog native sink) and routes one canonical `TELEMETRY_DROPPED`
/// record per delta, excluding the delta's own origin.
///
/// Called from the service heartbeat (`serve`'s own independent timer)
/// rather than from any one session's health tick: loss is a process-level
/// fact, not a per-session one, so anchoring the drain to the service
/// cadence guarantees it happens on a fixed schedule regardless of whether
/// any session is active, and a session that ends well before the service's
/// next tick still has its counted loss reported instead of silently lost.
fn emit_service_loss_notices(emitter: &LifecycleEmitter) {
    emitter.emit_loss_notices();
}

#[cfg(feature = "wss-compat")]
type CompatibilityListener = Option<TcpListener>;
#[cfg(not(feature = "wss-compat"))]
type CompatibilityListener = ();

enum CompatibilityAccept {
    #[cfg(feature = "wss-compat")]
    Connected(TcpStream, SocketAddr),
}

#[cfg(feature = "wss-compat")]
async fn accept_compatibility(
    listener: &CompatibilityListener,
) -> std::io::Result<CompatibilityAccept> {
    match listener {
        Some(listener) => listener
            .accept()
            .await
            .map(|(stream, peer)| CompatibilityAccept::Connected(stream, peer)),
        None => std::future::pending().await,
    }
}

#[cfg(not(feature = "wss-compat"))]
async fn accept_compatibility(
    _listener: &CompatibilityListener,
) -> std::io::Result<CompatibilityAccept> {
    std::future::pending().await
}

/// Build TLS before binding, then serve QUIC connections until SIGINT/SIGTERM.
pub async fn serve(
    cfg: Arc<Config>,
    log_controller: Arc<LogController>,
    admission_runtime: Arc<SessionAdmissionRuntime>,
) -> std::io::Result<()> {
    let started_at = Instant::now();
    let emitter =
        eventlog::LifecycleEmitter::new(log_controller.handle(), eventlog::local_hostname());
    #[cfg(target_os = "linux")]
    let sighup = {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::hangup()) {
            Ok(signal) => Some(signal),
            Err(error) => {
                warn!(target: NET, %error, "SIGHUP reload is unavailable");
                None
            }
        }
    };
    let quic_port = cfg.direct_quic_port();
    let addr = format!("{}:{quic_port}", cfg.host);
    if cfg.unsafe_remote_no_auth() {
        warn!(
            target: NET,
            %addr,
            "UNSAFE DEVELOPMENT MODE: exposing an unauthenticated host to remote clients"
        );
    }
    let source = tls::TlsFileSource::from_config(&cfg).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "direct QUIC requires TLS certificate and key material",
        )
    })?;
    let tls_lifecycle = match tls::TlsLifecycle::load(source).map(Arc::new) {
        Ok(lifecycle) => {
            lifecycle.emit_startup(&emitter);
            lifecycle
        }
        Err(error) => {
            let error = std::io::Error::new(std::io::ErrorKind::InvalidInput, error.to_string());
            emit_service_failed(&emitter, "tls_config", &error);
            return Err(error);
        }
    };
    let quic_addr = crate::net::quic::resolve_bind_addr(&cfg.host, quic_port)
        .await
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, error))?;
    let quic_config = crate::net::quic::build_quic_server_config(&tls_lifecycle)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    let quic_endpoint = match crate::net::quic::bind_endpoint(quic_addr, quic_config) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            let error = std::io::Error::new(std::io::ErrorKind::AddrInUse, error);
            emit_service_failed(&emitter, "quic_bind", &error);
            return Err(error);
        }
    };
    #[cfg(feature = "wss-compat")]
    let listener: CompatibilityListener = match cfg.wss_port {
        Some(port) => {
            let compatibility_addr = format!("{}:{port}", cfg.host);
            Some(TcpListener::bind(&compatibility_addr).await.map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("bind dormant WSS compatibility listener {compatibility_addr}: {error}"),
                )
            })?)
        }
        None => None,
    };
    #[cfg(not(feature = "wss-compat"))]
    let listener: CompatibilityListener = ();
    info!(
        target: NET,
        addr = %quic_addr,
        transport = "quic",
        tls = true,
        codec = %cfg.codec,
        chroma = %cfg.chroma,
        "arcen-host QUIC endpoint bound"
    );

    // Resolve capenc up-front for a clear operator error (spawn is per-connection).
    match cfg.resolve_capenc_binary() {
        Some(p) => info!(target: CAPENC, path = %p.display(), "capenc binary resolved"),
        None => warn!(
            target: CAPENC,
            "current arcen-pier executable could not be resolved for capenc dispatch"
        ),
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let session_slot = Arc::new(Semaphore::new(1));
    let session_registry = SessionRegistry::new(cfg.disconnected_idle_lifetime)
        .map_err(|_| std::io::Error::other("initialize process-local resume authority"))?;
    let preauth_slots = Arc::new(Semaphore::new(PREAUTH_CAPACITY));
    let pam_slots = Arc::new(Semaphore::new(PAM_OPERATION_CAPACITY));
    let (connection_shutdown_tx, connection_shutdown_rx) = watch::channel(false);
    let service_health = Arc::new(AtomicU8::new(0));
    let qos_targets = Arc::new(RwLock::new(
        log_controller
            .qos_targets()
            .unwrap_or_else(|_| cfg.logging.qos_targets),
    ));
    let state = ServerState {
        cfg,
        tls_lifecycle: Arc::clone(&tls_lifecycle),
        preauth_slots,
        pam_slots,
        session_slot,
        session_registry,
        emitter: emitter.clone(),
        shutdown: connection_shutdown_rx,
        service_health: Arc::clone(&service_health),
        qos_targets: Arc::clone(&qos_targets),
        admission_runtime: Arc::clone(&admission_runtime),
    };
    let mut connections = JoinSet::new();
    let mut tls_health_interval = tokio::time::interval(Duration::from_secs(60));
    tls_health_interval.tick().await;

    #[cfg(target_os = "linux")]
    let reload_task = sighup.map(|signal| {
        tokio::spawn(sighup_reload_loop(
            signal,
            Arc::clone(&log_controller),
            Some(Arc::clone(&tls_lifecycle)),
            emitter.clone(),
            Arc::clone(&qos_targets),
        ))
    });
    #[cfg(not(target_os = "linux"))]
    let _ = &log_controller;

    // Readiness: the listener is bound and every startup dependency (TLS
    // config) is loaded — only now is the process truly "running".
    emit_service_start(&emitter);
    emit_effective_profile(&emitter, &log_controller);

    loop {
        tokio::select! {
            accepted = accept_compatibility(&listener) => {
                #[cfg(feature = "wss-compat")]
                match accepted {
                    Ok(CompatibilityAccept::Connected(stream, peer)) => {
                        let state = state.clone();
                        connections.spawn(async move {
                            if let Err(error) = handle_connection(stream, peer, state).await {
                                debug!(target: NET, %peer, %error, "compatibility connection ended with error");
                            }
                        });
                    }
                    Err(error) => warn!(target: NET, %error, "compatibility accept failed"),
                }
                #[cfg(not(feature = "wss-compat"))]
                let _ = accepted;
            }
            incoming = quic_endpoint.accept() => {
                if let Some(incoming) = incoming {
                    let state = state.clone();
                    connections.spawn(async move {
                        let peer = incoming.remote_address();
                        if let Err(error) = handle_quic_connection(incoming, state).await {
                            debug!(target: NET, %peer, %error, "QUIC connection ended with error");
                        }
                    });
                } else {
                    warn!(target: NET, "QUIC endpoint closed; stopping accept loop");
                    break;
                }
            }
            joined = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = joined {
                    warn!(target: NET, %error, "connection task failed");
                }
            }
            _ = tls_health_interval.tick() => {
                tls_lifecycle.check_and_emit_status(&emitter);
                emit_service_health_snapshot(&emitter, &service_health);
                emit_service_loss_notices(&emitter);
            }
            _ = &mut shutdown => {
                info!(target: NET, "shutdown signal received — stopping accept loop");
                break;
            }
        }
    }
    quic_endpoint.close(0u32.into(), b"host shutdown");
    quic_endpoint.wait_idle().await;
    let _ = connection_shutdown_tx.send(true);
    state.session_registry.request_shutdown();
    drain_connections_and_display(&mut connections, state.session_slot.clone()).await;
    match crate::microphone_input::wait_for_cleanup(SHUTDOWN_DISPLAY_DRAIN_TIMEOUT).await {
        crate::microphone_input::MicrophoneCleanupDrain::Drained => {}
        crate::microphone_input::MicrophoneCleanupDrain::Failed => {
            tracing::error!(
                target: AUDIO,
                "microphone cleanup finished without verified restoration; journal retained"
            );
        }
        crate::microphone_input::MicrophoneCleanupDrain::TimedOut => {
            tracing::error!(
                target: AUDIO,
                timeout_ms = SHUTDOWN_DISPLAY_DRAIN_TIMEOUT.as_millis() as u64,
                "timed out waiting for microphone lease restoration"
            );
        }
    }
    state.session_registry.shutdown().await;
    #[cfg(target_os = "linux")]
    {
        if let Some(reload_task) = reload_task {
            reload_task.abort();
            let _ = reload_task.await;
        }
    }

    emit_service_stop(&emitter, started_at.elapsed());
    drain_final_loss_before_stop(
        &emitter,
        &log_controller.handle(),
        SHUTDOWN_OBSERVABILITY_FLUSH_TIMEOUT,
    )
    .map_err(std::io::Error::other)?;
    Ok(())
}

async fn initialize_tls_before_bind<T, U, Load, Ready, Bind, BindFuture>(
    load: Load,
    ready: Ready,
    bind: Bind,
) -> std::io::Result<(T, U)>
where
    Load: FnOnce() -> std::io::Result<T>,
    Ready: FnOnce(&T),
    Bind: FnOnce() -> BindFuture,
    BindFuture: std::future::Future<Output = std::io::Result<U>>,
{
    let tls = load()?;
    ready(&tls);
    let listener = bind().await?;
    Ok((tls, listener))
}

/// Publishes the reloaded QoS/hysteresis thresholds (and the effective
/// profile they came from) to `qos_targets` whenever `logging_outcome`
/// (`LogController::handle_sighup`'s structured result) reports the
/// profile/QoS state was actually committed, independent of TLS or any
/// other combined SIGHUP maintenance outcome — including a managed-log
/// reopen or archive-cleanup failure *in the same `handle_sighup` call*,
/// which must never suppress an otherwise-successful state commit. Returns
/// `true` if the shared cell was updated.
///
/// `LogController::qos_targets` only ever reflects a state that
/// `handle_sighup` itself already committed
/// (`SighupOutcome::state_committed`), so gating on that flag alone can
/// never publish a stale or partially-applied value, and a session-visible
/// reload is never blocked by an unrelated TLS reload or reopen/cleanup
/// failure.
fn publish_qos_targets_if_logging_reloaded(
    logging_outcome: &crate::logging::SighupOutcome,
    emitter: &LifecycleEmitter,
    log_controller: &LogController,
    qos_targets: &RwLock<QosTargets>,
) -> bool {
    if !logging_outcome.state_committed() {
        return false;
    }
    emit_effective_profile(emitter, log_controller);
    match log_controller.qos_targets() {
        Ok(fresh) => {
            if let Ok(mut shared) = qos_targets.write() {
                *shared = fresh;
                true
            } else {
                warn!(
                    target: NET,
                    "qos_targets shared cell is poisoned; active sessions keep prior thresholds"
                );
                false
            }
        }
        Err(error) => {
            warn!(target: NET, %error, "failed to read reloaded qos_targets");
            false
        }
    }
}

#[cfg(target_os = "linux")]
async fn sighup_reload_loop(
    mut hangup: tokio::signal::unix::Signal,
    log_controller: Arc<LogController>,
    tls_lifecycle: Option<Arc<tls::TlsLifecycle>>,
    emitter: LifecycleEmitter,
    qos_targets: Arc<RwLock<QosTargets>>,
) {
    let mut last_error = None;
    while hangup.recv().await.is_some() {
        let worker_log_controller = Arc::clone(&log_controller);
        let worker_tls_lifecycle = tls_lifecycle.clone();
        let worker_emitter = emitter.clone();
        // `handle_sighup` is called exactly once and its structured
        // `SighupOutcome` reused for both the combined report below and the
        // QoS-targets/profile publish gate (`state_committed`, independent
        // of any reopen/archive/TLS failure), so logging's own outcome is
        // never re-derived and TLS's independent success/failure can never
        // influence it.
        let result = tokio::task::spawn_blocking(move || {
            let logging_outcome = worker_log_controller.handle_sighup();
            let logging_result_for_report = logging_outcome.error_message().map_or(Ok(()), Err);
            let combined = tls::coordinate_sighup(
                move || logging_result_for_report,
                || {
                    worker_tls_lifecycle.as_ref().map_or(Ok(()), |lifecycle| {
                        lifecycle
                            .reload(&worker_emitter)
                            .map_err(|error| error.reason_class().to_string())
                    })
                },
            );
            (logging_outcome, combined)
        })
        .await;
        match result {
            Ok((logging_outcome, combined)) => {
                let _ = publish_qos_targets_if_logging_reloaded(
                    &logging_outcome,
                    &emitter,
                    &log_controller,
                    &qos_targets,
                );
                match combined {
                    Ok(()) => {
                        last_error = None;
                        info!(target: NET, "SIGHUP logging and TLS reload completed");
                    }
                    Err(error) => {
                        if last_error.as_deref() != Some(error.as_str()) {
                            warn!(
                                target: NET,
                                %error,
                                "SIGHUP maintenance partially failed; last good state retained"
                            );
                            last_error = Some(error);
                        }
                    }
                }
            }
            Err(error) => {
                let error = format!("SIGHUP maintenance task failed: {error}");
                if last_error.as_deref() != Some(error.as_str()) {
                    warn!(target: NET, %error);
                    last_error = Some(error);
                }
            }
        }
    }
}

async fn drain_connections_and_display(
    connections: &mut JoinSet<()>,
    session_slot: Arc<Semaphore>,
) {
    drain_connections_and_display_with_timeout(
        connections,
        session_slot,
        SHUTDOWN_DISPLAY_DRAIN_TIMEOUT,
    )
    .await;
}

async fn drain_connections_and_display_with_timeout(
    connections: &mut JoinSet<()>,
    session_slot: Arc<Semaphore>,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !connections.is_empty() {
        match tokio::time::timeout_at(deadline, connections.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => {
                if !error.is_cancelled() {
                    warn!(target: NET, %error, "connection task failed during shutdown");
                }
            }
            Ok(None) => break,
            Err(_) => {
                connections.abort_all();
                while connections.join_next().await.is_some() {}
                break;
            }
        }
    }

    match tokio::time::timeout(timeout, session_slot.acquire()).await {
        Ok(Ok(permit)) => {
            drop(permit);
            info!(target: DISPLAY, "display restoration drained before shutdown");
        }
        Ok(Err(_)) => {
            warn!(target: DISPLAY, "display session semaphore closed during shutdown");
        }
        Err(_) => {
            tracing::error!(
                target: DISPLAY,
                timeout_ms = timeout.as_millis() as u64,
                "timed out waiting for display restoration during shutdown"
            );
        }
    }
}

/// Resolves when the process should shut down (SIGINT/SIGTERM on Unix, Ctrl-C
/// elsewhere).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        let mut int = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_INBOUND_WS_MESSAGE),
        max_frame_size: Some(MAX_INBOUND_WS_MESSAGE),
        ..WebSocketConfig::default()
    }
}

#[allow(clippy::result_large_err)] // Callback signature is fixed by tungstenite.
#[cfg(feature = "wss-compat")]
fn reject_browser_origin(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
    if request.headers().contains_key("origin") {
        warn!(target: NET, "rejecting browser-origin WebSocket upgrade");
        return Err(tokio_tungstenite::tungstenite::http::Response::builder()
            .status(403)
            .body(Some(
                "Browser WebSocket origins are not accepted".to_string(),
            ))
            .expect("static WebSocket rejection response"));
    }
    Ok(response)
}

/// Per-connection: optional TLS handshake, WS upgrade, then run the relay.
#[derive(Clone)]
struct ServerState {
    cfg: Arc<Config>,
    tls_lifecycle: Arc<tls::TlsLifecycle>,
    preauth_slots: Arc<Semaphore>,
    pam_slots: Arc<Semaphore>,
    session_slot: Arc<Semaphore>,
    session_registry: Arc<SessionRegistry>,
    emitter: LifecycleEmitter,
    shutdown: watch::Receiver<bool>,
    /// Worst per-session overall health observed since the last service-level
    /// `HEALTH_SNAPSHOT` (0 = none reported, 1 = ok, 2 = degraded, 3 =
    /// critical). Read-and-reset every 60s in `serve`'s main select loop so a
    /// session that already ended cannot leave the service snapshot stuck at
    /// a stale severity.
    service_health: Arc<AtomicU8>,
    /// Validated QoS/hysteresis thresholds, refreshed atomically on a
    /// successful SIGHUP reload (`sighup_reload_loop`). Every session reads
    /// this shared cell on each health tick rather than a config snapshot
    /// captured at attach time, so already-running sessions observe the
    /// same reloaded thresholds as sessions started afterward.
    qos_targets: Arc<RwLock<QosTargets>>,
    /// Capacity-one local session admission gate. Consulted on every
    /// new-session admission (see `run_ws`) and held through reconnect.
    admission_runtime: Arc<SessionAdmissionRuntime>,
}

/// Extracts a peer's `SocketAddrV6` zone/scope id, if any, before it is
/// lost by stringifying the address into `remote_host` (`IpAddr`/
/// `Ipv6Addr` carry no scope id at all — only `SocketAddrV6` does). A
/// link-local peer's network probe otherwise cannot resolve its owning
/// interface unambiguously, since the same `fe80::/10` prefix is typically
/// valid, and present, on every interface (re-review finding: preserve
/// `SocketAddrV6` scope_id through peer route resolution). Scope id `0`
/// means "unspecified"/absent per RFC 4007 and is normalized to `None`.
fn peer_scope_id(peer: SocketAddr) -> Option<u32> {
    match peer {
        SocketAddr::V6(v6) if v6.scope_id() != 0 => Some(v6.scope_id()),
        _ => None,
    }
}

#[cfg(feature = "wss-compat")]
async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    state: ServerState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    stream.set_nodelay(true).ok();
    let config = ws_config();
    let preauth_permit = state
        .preauth_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| "pre-authentication capacity exhausted")?;

    let selected_host_identity = state
        .tls_lifecycle
        .host_identity()
        .map_err(|_| "TLS host identity unavailable")?;
    let tls_stream = tokio::time::timeout(
        TLS_HANDSHAKE_TIMEOUT,
        state.tls_lifecycle.acceptor().accept(stream),
    )
    .await
    .map_err(|_| "TLS handshake timed out")??;
    debug!(target: TLS, %peer, "TLS handshake ok");
    let current_host_identity = state
        .tls_lifecycle
        .host_identity()
        .map_err(|_| "TLS host identity unavailable")?;
    if selected_host_identity != current_host_identity {
        return Err("TLS host identity changed during handshake".into());
    }
    let ws = tokio::time::timeout(
        WEBSOCKET_HANDSHAKE_TIMEOUT,
        accept_hdr_async_with_config(tls_stream, reject_browser_origin, Some(config)),
    )
    .await
    .map_err(|_| "WebSocket handshake timed out")??;
    info!(target: NET, %peer, "client connected (WS+TLS)");
    let span = tracing::info_span!("linux_connection", sid = tracing::field::Empty);
    let remote_scope_id = peer_scope_id(peer);
    run_ws(
        DirectSessionSocket::wss(ws),
        state.cfg.clone(),
        preauth_permit,
        state.pam_slots.clone(),
        state.session_slot.clone(),
        state.session_registry.clone(),
        state.emitter.clone(),
        peer.ip().to_string(),
        remote_scope_id,
        current_host_identity,
        state.shutdown.clone(),
        Arc::clone(&state.service_health),
        Arc::clone(&state.qos_targets),
        Arc::clone(&state.admission_runtime),
    )
    .instrument(span)
    .await;
    debug!(target: NET, %peer, "connection closed");
    Ok(())
}

async fn handle_quic_connection(
    incoming: quinn::Incoming,
    state: ServerState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let peer = incoming.remote_address();
    let preauth_permit = match state.preauth_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            incoming.refuse();
            return Ok(());
        }
    };
    let selected_host_identity = state
        .tls_lifecycle
        .host_identity()
        .map_err(|_| "TLS host identity unavailable")?;
    let connection = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, incoming)
        .await
        .map_err(|_| "QUIC TLS handshake timed out")??;
    let current_host_identity = state
        .tls_lifecycle
        .host_identity()
        .map_err(|_| "TLS host identity unavailable")?;
    if selected_host_identity != current_host_identity {
        connection.close(0_u32.into(), b"TLS host identity changed during handshake");
        return Err("TLS host identity changed during QUIC handshake".into());
    }
    let stream = tokio::time::timeout(
        WEBSOCKET_HANDSHAKE_TIMEOUT,
        arcen_transport::quic::accept_direct(connection),
    )
    .await
    .map_err(|_| "QUIC direct stream timed out")??;
    let feedback = stream.feedback_snapshot();
    debug!(
        target: NET,
        %peer,
        rtt_us = u64::try_from(feedback.rtt.as_micros()).unwrap_or(u64::MAX),
        current_mtu = feedback.current_mtu,
        congestion_window_bytes = feedback.congestion_window,
        "QUIC path established"
    );
    let ws = WebSocketStream::from_raw_socket(stream, Role::Server, Some(ws_config())).await;
    info!(target: NET, %peer, "client connected (QUIC+TLS)");
    let span = tracing::info_span!("linux_connection", sid = tracing::field::Empty);
    let remote_scope_id = peer_scope_id(peer);
    run_ws(
        DirectSessionSocket::quic(ws),
        state.cfg.clone(),
        preauth_permit,
        state.pam_slots.clone(),
        state.session_slot.clone(),
        state.session_registry.clone(),
        state.emitter.clone(),
        peer.ip().to_string(),
        remote_scope_id,
        current_host_identity,
        state.shutdown.clone(),
        Arc::clone(&state.service_health),
        Arc::clone(&state.qos_targets),
        Arc::clone(&state.admission_runtime),
    )
    .instrument(span)
    .await;
    debug!(target: NET, %peer, "QUIC connection closed");
    Ok(())
}

/// Releases the exact non-cloneable capacity-one local session admission lease
/// terminally, however `run_ws` returns. `run_resumable_session` loops through
/// detach/reconnect/reattach internally, so this guard remains held through the
/// bounded direct reconnect window and releases only after terminal drain.
struct SessionAdmissionGuard {
    runtime: Arc<SessionAdmissionRuntime>,
    lease: Option<SessionAdmissionLease>,
}

impl Drop for SessionAdmissionGuard {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.runtime.complete(lease);
        }
    }
}

/// Drive one WebSocket session: capenc READY → truthful server_hello → frame
/// relay + control dispatch, with graceful teardown when any half ends.
#[allow(clippy::too_many_arguments)] // Per-connection context threaded from `ServerState`.
async fn run_ws(
    mut ws: DirectSessionSocket,
    cfg: Arc<Config>,
    preauth_permit: OwnedSemaphorePermit,
    pam_slots: Arc<Semaphore>,
    session_slot: Arc<Semaphore>,
    session_registry: Arc<SessionRegistry>,
    emitter: LifecycleEmitter,
    remote_host: String,
    remote_scope_id: Option<u32>,
    current_host_identity: HostIdentity,
    mut shutdown: watch::Receiver<bool>,
    service_health: Arc<AtomicU8>,
    qos_targets: Arc<RwLock<QosTargets>>,
    admission_runtime: Arc<SessionAdmissionRuntime>,
) {
    let (authenticated, session_log_id) = if cfg.auth_mode == AuthMode::None {
        let session_log_id = match fallback_session_log_id() {
            Ok(value) => value,
            Err(error) => {
                warn!(target: SESSION, %error, "failed to generate no-auth session log id");
                return;
            }
        };
        info!(
            target: SESSION,
            sid = %session_log_id,
            "using host-generated session log id for legacy-compatible no-auth handshake"
        );
        (None, session_log_id)
    } else {
        let resume_supported = cfg.reconnect_window_secs > 0;
        let detached_resume = session_registry
            .resume()
            .resume_handshake_available()
            .unwrap_or_else(|registry_error| {
                error!(
                    target: SESSION,
                    ?registry_error,
                    "resume authority availability check failed closed"
                );
                false
            });
        let (mut response, session_log_id) = match receive_auth_response(
            &mut ws,
            &cfg,
            resume_supported,
            detached_resume,
            &emitter,
            &remote_host,
        )
        .await
        {
            Ok(value) => value,
            Err(()) => return,
        };
        tracing::Span::current().record("sid", tracing::field::display(&session_log_id));
        if authentication_route(&response) == AuthenticationRoute::ResumeRegistry {
            if !resume_supported {
                clear_resume_secrets(&mut response);
                send_resume_rejection(
                    &mut ws,
                    "session resume is unavailable",
                    ResumeErrorCode::Unsupported,
                )
                .await;
                return;
            }
            let prepared = session_registry.resume().prepare_resume(
                &response,
                &current_host_identity,
                &session_log_id,
            );
            clear_resume_secrets(&mut response);
            match prepared {
                Ok(permit) => {
                    if let Err((socket, rejection)) =
                        session_registry
                            .resume()
                            .handoff(permit, ws, session_log_id)
                    {
                        let mut socket = *socket;
                        rejection.notify_terminal_owner();
                        send_resume_rejection(&mut socket, rejection.message, rejection.code).await;
                    }
                }
                Err(rejection) => {
                    rejection.notify_terminal_owner();
                    send_resume_rejection(&mut ws, rejection.message, rejection.code).await;
                }
            }
            return;
        }
        if detached_resume {
            clear_resume_secrets(&mut response);
            send_resume_rejection(
                &mut ws,
                "active session requires resume authentication",
                ResumeErrorCode::Unsupported,
            )
            .await;
            return;
        }
        let authenticated = match authenticate_pam_response(
            &mut ws,
            &cfg,
            pam_slots,
            &remote_host,
            &emitter,
            response,
            session_log_id.clone(),
        )
        .await
        {
            Ok(value) => value,
            Err(()) => return,
        };
        (Some(authenticated), session_log_id)
    };
    tracing::Span::current().record("sid", tracing::field::display(&session_log_id));
    info!(target: SESSION, "session log correlation bound");
    drop(preauth_permit);
    let mut session_permit = match session_slot.try_acquire_owned() {
        Ok(permit) => Some(permit),
        Err(_) => {
            warn!(target: SESSION, "rejecting authenticated client: shared display is busy");
            send_critical_control(&mut ws, Message::Close(None), "session_busy_close").await;
            return;
        }
    };
    // Capacity-one local session admission: a new authenticated session must
    // hold a non-cloneable admission lease for the physical desktop it owns.
    // Denial only means another session already owns the host display/input
    // plane, so reject with the same closed posture as `session_busy_close`.
    let admission_lease = match admission_runtime.admit_new() {
        Ok(lease) => lease,
        Err(error) => {
            warn!(
                target: SESSION,
                %error,
                "rejecting new session: session admission denied"
            );
            send_critical_control(&mut ws, Message::Close(None), "session_admission_close").await;
            return;
        }
    };
    let _session_admission_guard = SessionAdmissionGuard {
        runtime: Arc::clone(&admission_runtime),
        lease: Some(admission_lease),
    };
    let negotiated_display_plan = authenticated
        .as_ref()
        .map(|authenticated| authenticated.display_plan);
    let negotiated_display_mode =
        negotiated_display_plan.map_or(auth::SessionDisplayMode::Windowed, |plan| plan.mode);
    if let Some(authenticated) = &authenticated {
        let response = &authenticated.response;
        let display_plan = authenticated.display_plan;
        info!(
            target: SESSION,
            username = %response.username,
            client_w = response.screen_width,
            client_h = response.screen_height,
            monitors = response.monitors.len(),
            displays_mode = %response.displays_mode,
            resolved_displays_mode = display_plan.mode.as_str(),
            session_w = display_plan.width,
            session_h = display_plan.height,
            "authenticated client display request received"
        );
        let multi_monitor_planned = matches!(
            &authenticated.multi_monitor_outcome,
            multi_monitor::MultiMonitorOutcome::Planned { .. }
        );
        if !multi_monitor_planned {
            if let Some(auth::SessionDisplayDegradation::MultiMonitorMatchLayout {
                requested_monitors,
            }) = display_plan.degradation
            {
                warn!(
                    target: SESSION,
                    username = %response.username,
                    requested_displays_mode = %response.displays_mode,
                    requested_monitors,
                    served_client_monitor_id = ?display_plan.served_monitor_id,
                    served_w = display_plan.width,
                    served_h = display_plan.height,
                    "multi-monitor Match-My-Layout was not admitted; serving the primary client monitor instead"
                );
            }
        }
        match &authenticated.multi_monitor_outcome {
            multi_monitor::MultiMonitorOutcome::NotRequested => {}
            multi_monitor::MultiMonitorOutcome::Degraded(reason) => {
                warn!(
                    target: SESSION,
                    username = %response.username,
                    %reason,
                    "multi_monitor_v1 requested topology was not admitted; the legacy single-primary session behavior applies instead"
                );
            }
            multi_monitor::MultiMonitorOutcome::Planned { plan, carrier } => {
                info!(
                    target: SESSION,
                    username = %response.username,
                    generation = plan.generation.get(),
                    monitors = plan.monitors.len(),
                    virtual_w = plan.virtual_width,
                    virtual_h = plan.virtual_height,
                    %carrier,
                    "multi_monitor_v1 requested topology was fully planned and will be applied"
                );
            }
        }
    }

    let mut session_config = (*cfg).clone();
    let (mut auth_response, session_lease): (Option<AuthResponse>, Option<SessionLease>) =
        match authenticated {
            Some(authenticated) => {
                let AuthenticatedConnection {
                    response,
                    launcher,
                    multi_monitor_outcome,
                    session_config: authenticated_config,
                    ..
                } = authenticated;
                let admitted_monitor_count = match &multi_monitor_outcome {
                    multi_monitor::MultiMonitorOutcome::Planned { plan, .. } => {
                        Some(plan.monitors.len())
                    }
                    multi_monitor::MultiMonitorOutcome::NotRequested
                    | multi_monitor::MultiMonitorOutcome::Degraded(_) => None,
                };
                let lease = match session_registry
                    .acquire(
                        launcher,
                        multi_monitor_outcome.into_planned(),
                        response.replace_incompatible_desktop,
                    )
                    .await
                {
                    Ok(lease) => lease,
                    Err(LifecycleError::Busy) => {
                        warn!(
                            target: SESSION,
                            username = %response.username,
                            "authenticated user cannot replace the persistent graphical session"
                        );
                        send_critical_control(
                            &mut ws,
                            close_with_reason(
                                "another session owns this desktop; retry after it disconnects",
                            ),
                            "persistent_session_busy_close",
                        )
                        .await;
                        return;
                    }
                    Err(error) => {
                        warn!(
                            target: SESSION,
                            username = %response.username,
                            %error,
                            "authenticated graphical session setup failed"
                        );
                        send_critical_control(
                            &mut ws,
                            close_with_reason(&format!("graphical session setup failed: {error}")),
                            "graphical_session_setup_close",
                        )
                        .await;
                        return;
                    }
                };
                if admitted_multi_monitor_topology_was_discarded(
                    admitted_monitor_count,
                    lease.metadata.multi_monitor_plan.as_ref(),
                ) {
                    warn!(
                        target: SESSION,
                        username = %response.username,
                        session_id = %lease.metadata.session_id,
                        reconnected = lease.metadata.reconnected,
                        admitted_monitors = admitted_monitor_count.unwrap_or(0),
                        "multi_monitor_v1 topology was admitted but this attachment reattached to a persistent desktop created without one; refusing rather than silently serving the single-primary subset"
                    );
                    send_critical_control(
                        &mut ws,
                        close_with_reason(MULTI_MONITOR_REATTACH_REFUSAL),
                        "multi_monitor_reattach_refused_close",
                    )
                    .await;
                    return;
                }
                if let Err(error) = unlock_logind_session_id(&lease.metadata.session_id).await {
                    warn!(
                        target: SESSION,
                        username = %response.username,
                        session_id = %lease.metadata.session_id,
                        %error,
                        "authenticated graphical session unlock failed"
                    );
                    send_critical_control(
                        &mut ws,
                        close_with_reason("graphical session unlock failed"),
                        "graphical_session_unlock_close",
                    )
                    .await;
                    return;
                }
                emit_session_auth_ok(
                    &emitter,
                    session_log_id.clone(),
                    &response.username,
                    Some(&remote_host),
                    lease.metadata.session_id.parse::<i64>().ok(),
                );
                session_config = authenticated_config;
                (Some(response), Some(lease))
            }
            None => (None, None),
        };

    if let Some(lease) = &session_lease {
        session_config.display = lease.metadata.display.clone();
        session_config.xauthority = lease
            .execution
            .environment
            .get("XAUTHORITY")
            .map(str::to_string);
        session_config.monitor = 1;
    }
    let cursor_preference = auth_response
        .as_ref()
        .map_or(CursorMode::Local, |response| response.cursor_preference);
    // A committed `multi_monitor_v1` plan means `DedicatedXorg` already
    // applied and RandR-verified this session's exact multi-head raster
    // (`session::xorg_multihead` + `session::randr_verify`) at admission
    // time (`SessionRegistry::acquire`), and — per the doc comment on
    // `resolve_input_raster` — that dedicated X display never holds a
    // `MetaModeGuard`. This branch is what makes that true: it must run
    // identically on a fresh attach *and* a reconnect (both reach this same
    // `run_ws` code path with the lease's persisted `multi_monitor_plan`),
    // skipping the legacy single-viewport resize entirely rather than
    // letting it clobber the verified multi-head topology with a
    // primary-monitor-only `MetaModeGuard::apply_with_hold` viewport
    // MetaMode. `display_guard` stays `None`, so
    // `HeldDisplayResources::raster_size()`/`resolution()` correctly report
    // nothing held and `resolve_input_raster` falls through to the plan's
    // own combined virtual raster. Legacy single-monitor sessions (no
    // committed plan) are unaffected and take the `else` branch unchanged.
    let multi_monitor_plan_committed = skip_metamode_resize_for_multi_monitor(
        session_lease
            .as_ref()
            .and_then(|lease| lease.metadata.multi_monitor_plan.as_ref()),
    );
    let mut display_guard = None;
    if multi_monitor_plan_committed {
        // Intentionally empty: see comment above. No MetaMode resize, no
        // `session_permit` consumption — it is carried through unchanged
        // into `HeldDisplayResources::new` below.
    } else if let Some(display_plan) = negotiated_display_plan {
        match nvctrl::requested_resolution(display_plan.width, display_plan.height) {
            Ok(Some(mut resolution)) => {
                // The session display is mutated once, here, before any encoder
                // exists. If we already know the encode path is software, the
                // requested geometry has to be fitted to what that encoder can
                // accept now, because there is no second modeset later: arming
                // 1800x1168 and only then discovering OpenH264 tops out at
                // 1920x1080 leaves capenc capturing a rectangle it must reject.
                let (fitted_width, fitted_height) = crate::media::capenc::fit_to_encoder_limits(
                    session_config.encoder,
                    resolution.width,
                    resolution.height,
                );
                if (fitted_width, fitted_height) != (resolution.width, resolution.height) {
                    info!(
                        target: CAPENC,
                        requested_w = resolution.width,
                        requested_h = resolution.height,
                        session_w = fitted_width,
                        session_h = fitted_height,
                        "session display fitted to the software encoder's limits"
                    );
                }
                session_config.width = fitted_width;
                session_config.height = fitted_height;
                // The modeset below is driven by `resolution`, so fitting only
                // `session_config` would leave the display armed at a geometry
                // the encoder must then reject.
                resolution = match nvctrl::Resolution::new(fitted_width, fitted_height) {
                    Ok(fitted) => fitted,
                    Err(error) => {
                        warn!(
                            target: DISPLAY,
                            %error,
                            fitted_width,
                            fitted_height,
                            "fitted session geometry is not a valid resolution"
                        );
                        send_critical_control(
                            &mut ws,
                            close_with_reason("capture/encoder initialization failed"),
                            "fitted_resolution_invalid_close",
                        )
                        .await;
                        return;
                    }
                };
                let binary = match session_config.resolve_capenc_binary() {
                    Some(binary) => binary,
                    None => {
                        warn!(target: CAPENC, "no capenc binary — refusing display mutation");
                        send_critical_control(
                            &mut ws,
                            close_with_reason("capture/encoder initialization failed"),
                            "capenc_preflight_missing_close",
                        )
                        .await;
                        return;
                    }
                };
                if let Err(error) = capenc::preflight(session_config.capenc_config(
                    binary,
                    session_lease.as_ref().map(|lease| lease.execution.clone()),
                    session_log_id.clone(),
                    cursor_preference,
                ))
                .await
                {
                    warn!(
                        target: CAPENC,
                        %error,
                        "media limits/backend preflight failed before display mutation"
                    );
                    send_critical_control(
                        &mut ws,
                        close_with_reason("capture/encoder initialization failed"),
                        "capenc_preflight_close",
                    )
                    .await;
                    return;
                }
                let controller = NvControl::new(
                    session_config.display.clone(),
                    session_config.xauthority.clone(),
                );
                let permit = session_permit
                    .take()
                    .expect("authenticated session permit available");
                match MetaModeGuard::apply_with_hold(
                    controller,
                    resolution,
                    permit,
                    emitter.clone(),
                    session_log_id.clone(),
                )
                .await
                {
                    Ok(guard) => {
                        display_guard = Some(guard);
                    }
                    Err(error) => {
                        warn!(target: DISPLAY, %error, "client resolution resize failed");
                        send_critical_control(
                            &mut ws,
                            Message::Close(None),
                            "display_resize_close",
                        )
                        .await;
                        return;
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                warn!(target: DISPLAY, %error, "invalid authenticated client resolution");
                send_critical_control(&mut ws, Message::Close(None), "invalid_resolution_close")
                    .await;
                return;
            }
        }
    }
    let cfg = Arc::new(session_config);
    let mut display_resources = Some(HeldDisplayResources::new(display_guard, session_permit));
    let mut session_lease = session_lease;
    let wants_resume = auth_response
        .as_ref()
        .is_some_and(|response| cfg.reconnect_window_secs > 0 && response.resume_requested);
    let resume_setup = if wants_resume {
        let response = auth_response.as_ref().expect("resume response present");
        let lease = session_lease.as_ref().expect("PAM session lease present");
        let setup = async {
            let bindings = build_linux_resume_bindings(
                response,
                lease,
                current_host_identity,
                cfg.disclaimer.as_deref(),
            )?;
            session_registry
                .validate_native_session(&lease.metadata)
                .await
                .map_err(|_| ())?;
            let policy = ReconnectPolicy::new(cfg.reconnect_window_secs).map_err(|_| ())?;
            let (owner, commands) = mpsc::unbounded_channel();
            let grant = session_registry
                .resume()
                .issue_initial(bindings, policy, owner, &session_log_id)
                .map_err(|_| ())?;
            Ok::<_, ()>((grant, commands))
        }
        .await;
        match setup {
            Ok(value) => Some(value),
            Err(()) => {
                send_resume_rejection(
                    &mut ws,
                    "resume initialization failed",
                    ResumeErrorCode::InternalFailure,
                )
                .await;
                if let Some(resources) = display_resources.as_mut() {
                    resources.restore().await;
                }
                if let Some(lease) = session_lease.take() {
                    lease.terminate().await;
                }
                return;
            }
        }
    } else {
        None
    };

    if let Some(response) = auth_response.as_mut() {
        clear_resume_secrets(response);
        let grant = resume_setup.as_ref().map(|(grant, _)| grant);
        if !send_auth_result_with_resume(
            &mut ws,
            true,
            "Authenticated",
            grant,
            grant.map(|_| cfg.reconnect_window_secs),
            false,
            None,
        )
        .await
        {
            if let Some(lease) = session_lease.take() {
                if resume_setup.is_some() {
                    if let Ok(active) = active_session_id(&lease.metadata) {
                        if let Err(error) = session_registry.resume().begin_drain(&active) {
                            tracing::error!(
                                target: SESSION,
                                ?error,
                                "initial resume slot drain failed"
                            );
                        }
                        if let Some(resources) = display_resources.as_mut() {
                            resources.restore().await;
                        }
                        lease.terminate().await;
                        if let Err(error) = session_registry.resume().complete_drain(&active) {
                            tracing::error!(
                                target: SESSION,
                                ?error,
                                "initial resume slot drain completion failed"
                            );
                        }
                    } else {
                        lease.terminate().await;
                    }
                } else {
                    if let Some(resources) = display_resources.as_mut() {
                        resources.restore().await;
                    }
                    lease.disconnect();
                }
            }
            return;
        }
    }

    if let Some((_, mut commands)) = resume_setup {
        let Some(lease) = session_lease.take() else {
            error!(
                target: SESSION,
                "resume setup exists without an authenticated session lease"
            );
            return;
        };
        run_resumable_session(
            ws,
            cfg,
            emitter,
            session_registry,
            lease,
            display_resources.take().expect("display ownership present"),
            &mut commands,
            session_log_id,
            cursor_preference,
            negotiated_display_mode,
            shutdown,
            &remote_host,
            remote_scope_id,
            Arc::clone(&service_health),
            Arc::clone(&qos_targets),
        )
        .await;
    } else {
        // Do not call `session_lease.take()` while merely deciding whether
        // resume was negotiated. A tuple-pattern version of this branch used
        // to consume the lease even when `resume_setup` was `None`, causing
        // legacy/non-resumable clients to lose the committed multi-monitor
        // topology and start the single-primary capenc path.
        let end = run_attachment(
            ws,
            Arc::clone(&cfg),
            session_lease.as_ref(),
            &emitter,
            session_log_id,
            false,
            None,
            cursor_preference,
            negotiated_display_mode,
            display_resources.as_mut(),
            &mut shutdown,
            &remote_host,
            remote_scope_id,
            Arc::clone(&service_health),
            Arc::clone(&qos_targets),
        )
        .await;
        if let Some(resources) = display_resources.as_mut() {
            resources.restore().await;
        }
        if let Some(lease) = session_lease.take() {
            // Latch durable proof that this desktop's committed plan is
            // usable *before* reading it back below, so this same
            // attachment's own success is immediately reflected — see
            // `SessionLease::mark_multi_monitor_ever_usable` and
            // `multi_monitor_attachment_must_terminate_desktop`'s doc
            // comment for why a pre-usable multi-monitor failure must
            // terminate the whole persistent desktop rather than
            // `disconnect`-and-preserve it (with its now-proven-broken
            // committed plan) for reconnect — but only when this desktop
            // has never proven that plan usable before.
            if end.reached_usable {
                lease.mark_multi_monitor_ever_usable();
            }
            if multi_monitor_attachment_must_terminate_desktop(
                end.reached_usable,
                lease.multi_monitor_ever_usable(),
                lease.metadata.multi_monitor_plan.as_ref(),
            ) {
                warn!(
                    target: SESSION,
                    end_reason = ?end.reason,
                    "multi-monitor attachment failed before becoming usable; terminating the persistent desktop instead of preserving its committed topology for reconnect"
                );
                lease.terminate().await;
            } else {
                lease.disconnect();
            }
        }
        debug!(target: SESSION, end_reason = ?end.reason, "non-resumable attachment ended");
    }
}

fn build_linux_resume_bindings(
    response: &AuthResponse,
    lease: &SessionLease,
    host_identity: HostIdentity,
    disclaimer: Option<&PreparedDisclaimer>,
) -> Result<ResumeBindings, ()> {
    let holder_nonce = response
        .resume_holder_nonce
        .as_deref()
        .and_then(resume::decode_holder_nonce)
        .ok_or(())?;
    let logind_session_id =
        LogindSessionId::new(lease.metadata.session_id.clone()).map_err(|_| ())?;
    let active_session_id = active_session_id(&lease.metadata)?;
    let (disclaimer_digest, disclaimer_version) =
        resume::disclaimer_binding(disclaimer).map_err(|_| ())?;
    Ok(ResumeBindings {
        host_identity,
        active_session_id,
        native_principal: NativePrincipal::Linux {
            uid: lease.metadata.uid,
            logind_session_id,
        },
        holder_nonce,
        disclaimer_digest,
        disclaimer_version,
        topology: TopologyBinding::from_response(response).map_err(|_| ())?,
    })
}

fn active_session_id(
    metadata: &crate::session::lifecycle::SessionMetadata,
) -> Result<ActiveHostSessionId, ()> {
    ActiveHostSessionId::new(format!("linux-logind:{}", metadata.session_id)).map_err(|_| ())
}

fn resumable_transport_loss(reason: SessionEndReason) -> bool {
    matches!(
        reason,
        SessionEndReason::TransportError
            | SessionEndReason::ReadLivenessTimeout
            | SessionEndReason::WriterEnded
    )
}

/// Whether a just-ended non-resumable attachment must terminate/remove the
/// whole persistent desktop instead of the ordinary `lease.disconnect()`
/// (which merely marks the desktop `connected = false` and preserves it,
/// unchanged, for a future reconnect).
///
/// A committed multi-monitor plan is fixed for the desktop's entire
/// lifetime and carried forward *verbatim* into every future reconnect
/// (see `SessionMetadata::multi_monitor_plan` and
/// `SessionRegistry::acquire`'s `Reconnect` arm) — never recomputed. So an
/// attachment that failed before it ever reached a usable state
/// (`!reached_usable` — i.e. before capenc/READY/mux/applied-capability/
/// `ServerHello` all actually succeeded; see `AttachmentEnd::reached_usable`)
/// proves that *this exact committed plan* cannot currently be served at
/// all — but only when this desktop has *never* proven that plan usable in
/// any earlier attachment either (`!ever_usable`; see
/// `SessionLease::multi_monitor_ever_usable`). A later reconnect's own
/// early failure, on a desktop that already reached usable at least once
/// since this plan was committed, is far more likely to be an ordinary
/// transient condition (a race during Xorg/capenc restart, a momentary
/// resource contention, etc.) than proof the committed plan itself is
/// broken — that desktop is preserved via the existing `disconnect`
/// (persist for reconnect) policy exactly like a legacy session, so a
/// healthy, previously-proven desktop is never destroyed by one flaky
/// reconnect attempt. Only the very first attachment against a freshly
/// committed plan (`!ever_usable`) forces termination: persisting *that*
/// via `disconnect` would just repeat the identical failure on every
/// subsequent reconnect attempt forever, with no path to recovery short of
/// an operator manually killing the desktop — so it must terminate the
/// desktop instead, forcing the next connection attempt to perform a fresh
/// `Create` with a freshly (re)admitted topology.
///
/// Legacy single-monitor sessions (`committed_multi_monitor_plan: None`)
/// have no such committed, fixed, verbatim-reused state to poison — their
/// media plan is always recomputed fresh per attempt — so they always keep
/// the existing `disconnect` (persist desktop for reconnect) behavior,
/// regardless of `reached_usable` or `ever_usable`.
///
/// Extracted to exactly the three pieces of state the decision depends on
/// so it is directly unit-testable without a live session/lease.
fn multi_monitor_attachment_must_terminate_desktop(
    reached_usable: bool,
    ever_usable: bool,
    committed_multi_monitor_plan: Option<&LinuxTopologyPlan>,
) -> bool {
    !reached_usable && !ever_usable && committed_multi_monitor_plan.is_some()
}

fn attachment_requires_fresh_idr(resumed: bool) -> bool {
    resumed
}

/// Resolves the fixed raster the shared uinput absolute pointer/pen device
/// must be declared against for one attachment.
///
/// Priority order:
/// 1. A committed multi-monitor plan's own combined `(virtual_width,
///    virtual_height)` — its `DedicatedXorg` never holds a `MetaModeGuard`
///    (multi-monitor sessions are always fixed-topology, never
///    live-resized), so the absolute device must span the whole
///    RandR-verified combined desktop rather than just the primary
///    monitor's own footprint.
/// 2. The held display guard's own `raster_size()`, when one is present
///    (legacy single-shared-X-server sessions with live resize support).
/// 3. The negotiated media plan's `(width, height)` — legacy fallback when
///    neither of the above applies.
///
/// Relative motion is unaffected by any of this: it uses a separate device
/// with no raster dependency.
fn resolve_input_raster(
    multi_monitor_virtual_size: Option<(u32, u32)>,
    display_raster_size: Option<(u32, u32)>,
    media_plan_size: (u32, u32),
) -> (u32, u32) {
    multi_monitor_virtual_size
        .or(display_raster_size)
        .unwrap_or(media_plan_size)
}

/// Whether `run_ws`'s legacy single-viewport `MetaModeGuard` resize step
/// must be skipped entirely for this attachment, in favor of leaving the
/// dedicated Xorg session's own RandR-verified multi-head raster untouched.
///
/// Extracted to exactly the one piece of state the decision actually
/// depends on — the committed plan from the session lease, if any — so it
/// is directly unit-testable against a real [`LinuxTopologyPlan`] without
/// needing a live `NvControl`/X display.
///
/// True for both a fresh attach and a reconnect: `session_lease` is built
/// identically on either path (`SessionRegistry::acquire` always carries
/// forward the original `Create`'s committed plan — see the doc comment on
/// `SessionMetadata::multi_monitor_plan`), so there is no separate
/// reconnect-specific decision to make here.
fn skip_metamode_resize_for_multi_monitor(
    committed_multi_monitor_plan: Option<&LinuxTopologyPlan>,
) -> bool {
    committed_multi_monitor_plan.is_some()
}

/// Whether this attachment just discarded a freshly admitted
/// `multi_monitor_v1` topology because `SessionRegistry::acquire` reattached
/// it to a persistent desktop that was created without one.
///
/// A desktop's committed topology is fixed for its lifetime (see the doc
/// comment on `SessionMetadata::multi_monitor_plan`), so a `Reconnect` inside
/// `reconnect_window_secs` keeps serving the *original* `Create`'s topology —
/// including `None`, which is what every legacy/single-primary connect (and
/// every capability-probe connect that authenticates without offering
/// `multi_monitor_v1`) commits. The dedicated Xorg was started with that
/// desktop's head layout baked in (`session::launcher` hands the plan to
/// `OutputTransaction::acquire` at open time), so this attachment cannot grow
/// a second head onto it, and it has no committed per-monitor raster to route
/// region input into: `build_server_hello` would truthfully report
/// `input_protocol_version = 4` with `input_capabilities.region_input =
/// Unavailable` while streaming the primary client monitor only.
///
/// ADR-0009 freezes multi-monitor admission as *atomic* — "either proves and
/// serves the full requested topology or fails Match My Layout with a
/// reconnect path. It must never silently serve a subset" — so `run_ws`
/// refuses the attachment here (see [`MULTI_MONITOR_REATTACH_REFUSAL`])
/// instead of quietly falling back to the legacy single-primary degradation
/// path. The persistent desktop itself is untouched: dropping the lease only
/// marks it disconnected, so the user's running applications survive and a
/// Primary-Display-Only reconnect still reattaches to them.
fn admitted_multi_monitor_topology_was_discarded(
    admitted_monitor_count: Option<usize>,
    committed_multi_monitor_plan: Option<&LinuxTopologyPlan>,
) -> bool {
    admitted_monitor_count.is_some() && committed_multi_monitor_plan.is_none()
}

/// Close reason sent when [`admitted_multi_monitor_topology_was_discarded`]
/// refuses an attachment.
///
/// The exact shared literal from `arcen_protocol`, so a client can recognise
/// this specific conflict and offer the two real recoveries -- reconnect to
/// the existing desktop as-is, or replace it via
/// `AuthResponse::replace_incompatible_desktop` -- instead of showing the
/// user a dead end.
///
/// Deliberately no longer instructs the user to sign out of the remote
/// desktop by hand: that was the only recovery offered on 2026-08-11, when a
/// desktop created 85 minutes earlier without a committed topology refused a
/// Match My Layout connect and left no in-app path forward.
const MULTI_MONITOR_REATTACH_REFUSAL: &str =
    arcen_protocol::messages::MULTI_MONITOR_TOPOLOGY_CONFLICT_REASON;

fn retain_display_for_final_cleanup(
    display_resources: &mut Option<HeldDisplayResources>,
    resources: HeldDisplayResources,
) {
    debug_assert!(display_resources.is_none());
    *display_resources = Some(resources);
}

#[derive(Clone)]
struct ResumeRefreshContext {
    session_registry: Arc<SessionRegistry>,
    active_session_id: ActiveHostSessionId,
    window_secs: u32,
}

struct ResumableAttachment<'a> {
    owner_commands: &'a mut mpsc::UnboundedReceiver<OwnerCommand>,
    refresh: ResumeRefreshContext,
}

fn resume_refresh_interval(window_secs: u32) -> Duration {
    Duration::from_millis((u64::from(window_secs) * 1_000 / 2).max(1))
}

fn resumable_write_timeout(window_secs: u32) -> Duration {
    WS_SEND_TIMEOUT.min(resume_refresh_interval(window_secs))
}

#[allow(clippy::too_many_arguments)]
async fn run_resumable_session(
    mut ws: DirectSessionSocket,
    cfg: Arc<Config>,
    emitter: LifecycleEmitter,
    session_registry: Arc<SessionRegistry>,
    lease: SessionLease,
    display_resources: HeldDisplayResources,
    commands: &mut mpsc::UnboundedReceiver<OwnerCommand>,
    mut session_log_id: CorrelationId,
    cursor_preference: CursorMode,
    display_mode: auth::SessionDisplayMode,
    mut shutdown: watch::Receiver<bool>,
    remote_host: &str,
    remote_scope_id: Option<u32>,
    service_health: Arc<AtomicU8>,
    qos_targets: Arc<RwLock<QosTargets>>,
) {
    let active_session_id = match active_session_id(&lease.metadata) {
        Ok(value) => value,
        Err(()) => return,
    };
    let policy = ReconnectPolicy::new(cfg.reconnect_window_secs)
        .expect("CLI reconnect policy was validated");
    let mut reconnect = DirectReconnect::new(policy);
    let mut force_fresh_idr = false;
    let mut display_is_held = false;
    let mut display_resources = Some(display_resources);
    'session: loop {
        let end = run_attachment(
            ws,
            Arc::clone(&cfg),
            Some(&lease),
            &emitter,
            session_log_id.clone(),
            force_fresh_idr,
            Some(ResumableAttachment {
                owner_commands: commands,
                refresh: ResumeRefreshContext {
                    session_registry: Arc::clone(&session_registry),
                    active_session_id: active_session_id.clone(),
                    window_secs: cfg.reconnect_window_secs,
                },
            }),
            cursor_preference,
            display_mode,
            display_resources.as_mut(),
            &mut shutdown,
            remote_host,
            remote_scope_id,
            Arc::clone(&service_health),
            Arc::clone(&qos_targets),
        )
        .await;
        if !resumable_transport_loss(end.reason) {
            let event = if end.reason == SessionEndReason::ClientClosed {
                ReconnectEvent::ExplicitDisconnect
            } else {
                ReconnectEvent::NativeSessionEnded
            };
            let _ = reconnect.apply(event, session_registry.resume().monotonic_now());
            break;
        }

        let loss_observed_at = end
            .transport_loss_observed_at
            .expect("resumable transport loss records its observation time");
        let actions = reconnect.apply(ReconnectEvent::UnexpectedLoss, loss_observed_at);
        if !actions.hold_restore_leases {
            break;
        }
        if session_registry
            .hold_display(
                lease.metadata.generation,
                &lease.metadata.username,
                display_resources
                    .take()
                    .expect("attached display ownership present"),
            )
            .is_err()
        {
            break;
        }
        display_is_held = true;
        if let Err(error) = session_registry.resume().mark_detached(&active_session_id) {
            tracing::error!(
                target: SESSION,
                ?error,
                "detached resume slot transition failed closed"
            );
            break;
        }
        tracing::info!(
            target: SESSION,
            reason_class = end.reason.reason_class(),
            "direct transport detached; desktop, display, and timezone leases retained"
        );

        loop {
            let (deadline, timer_generation) = match reconnect.state() {
                ReconnectState::Detached {
                    deadline,
                    timer_generation,
                }
                | ReconnectState::Resuming {
                    deadline,
                    timer_generation,
                } => (deadline, timer_generation),
                _ => break 'session,
            };
            let now = session_registry.resume().monotonic_now();
            if now >= deadline {
                let _ = reconnect.apply(ReconnectEvent::ExplicitDisconnect, now);
                break 'session;
            }
            let remaining_ms = deadline.get().saturating_sub(now.get());
            let wait_ms = u64::try_from(remaining_ms).unwrap_or(u64::MAX);
            let mut monitor = tokio::time::interval(Duration::from_secs(2));
            monitor.tick().await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {
                    let timer_now = session_registry.resume().monotonic_now();
                    let _ = reconnect.apply(ReconnectEvent::DeadlineReached { timer_generation }, timer_now);
                    break 'session;
                }
                command = commands.recv() => match command {
                    Some(OwnerCommand::Resume(handoff)) => {
                        let mut handoff = *handoff;
                        let resume_now = session_registry.resume().monotonic_now();
                        if resume_now >= deadline {
                            send_resume_rejection(
                                &mut handoff.socket,
                                "resume grant expired",
                                ResumeErrorCode::Expired,
                            )
                            .await;
                            let _ = reconnect.apply(ReconnectEvent::ExplicitDisconnect, resume_now);
                            break 'session;
                        }
                        let _ = reconnect.apply(ReconnectEvent::BeginResume, resume_now);
                        tracing::info!(
                            target: SESSION,
                            sid = %handoff.session_log_id,
                            previous_sid = %handoff.previous_session_log_id,
                            "credential-free direct transport resume candidate consumed"
                        );
                        if session_registry.validate_native_session(&lease.metadata).await.is_err() {
                            send_resume_rejection(
                                &mut handoff.socket,
                                "native session identity changed",
                                ResumeErrorCode::NativeIdentityChanged,
                            ).await;
                            break 'session;
                        }
                        let resources = match session_registry.take_held_display(
                            lease.metadata.generation,
                            &lease.metadata.username,
                        ) {
                            Ok(resources) => resources,
                            Err(_) => {
                                send_resume_rejection(
                                    &mut handoff.socket,
                                    "resume topology changed",
                                    ResumeErrorCode::TopologyChanged,
                                ).await;
                                break 'session;
                            }
                        };
                        display_is_held = false;
                        let post_validation_now = session_registry.resume().monotonic_now();
                        if post_validation_now >= deadline {
                            let mut resources = resources;
                            resources.restore().await;
                            send_resume_rejection(
                                &mut handoff.socket,
                                "resume grant expired",
                                ResumeErrorCode::Expired,
                            )
                            .await;
                            let _ = reconnect
                                .apply(ReconnectEvent::ExplicitDisconnect, post_validation_now);
                            break 'session;
                        }
                        if !send_successor_auth_result_or_drain(
                            &mut handoff.socket,
                            "Resumed",
                            &handoff.successor_grant,
                            handoff.window_secs,
                            true,
                            CRITICAL_CONTROL_TIMEOUT,
                            session_registry.resume(),
                            &active_session_id,
                        ).await {
                            retain_display_for_final_cleanup(&mut display_resources, resources);
                            let _ = reconnect.apply(
                                ReconnectEvent::OwnerCrashed,
                                session_registry.resume().monotonic_now(),
                            );
                            break 'session;
                        }
                        let _ = session_registry.replace_session_log_id(
                            &lease.metadata,
                            handoff.session_log_id.clone(),
                        );
                        display_resources = Some(resources);
                        session_log_id = handoff.session_log_id;
                        ws = handoff.socket;
                        if let Err(error) =
                            session_registry.resume().mark_attached(&active_session_id)
                        {
                            tracing::error!(
                                target: SESSION,
                                ?error,
                                "accepted resume could not mark slot attached"
                            );
                            break 'session;
                        }
                        let _ = reconnect.apply(
                            ReconnectEvent::ResumeAccepted,
                            session_registry.resume().monotonic_now(),
                        );
                        force_fresh_idr = true;
                        continue 'session;
                    }
                    Some(OwnerCommand::Terminal) => {
                        let _ = reconnect.apply(
                            ReconnectEvent::OwnerCrashed,
                            session_registry.resume().monotonic_now(),
                        );
                        break 'session;
                    }
                    Some(OwnerCommand::PierShutdown) | None => {
                        let _ = reconnect.apply(
                            ReconnectEvent::ExplicitDisconnect,
                            session_registry.resume().monotonic_now(),
                        );
                        break 'session;
                    }
                },
                _ = monitor.tick() => {
                    if session_registry.validate_native_session(&lease.metadata).await.is_err() {
                        let _ = reconnect.apply(
                            ReconnectEvent::NativeSessionEnded,
                            session_registry.resume().monotonic_now(),
                        );
                        break 'session;
                    }
                }
            }
        }
    }

    if let Err(error) = session_registry.resume().begin_drain(&active_session_id) {
        tracing::error!(
            target: SESSION,
            ?error,
            "terminal resume slot drain failed"
        );
    }
    if !display_is_held {
        if let Some(resources) = display_resources.take() {
            if session_registry
                .hold_display(
                    lease.metadata.generation,
                    &lease.metadata.username,
                    resources,
                )
                .is_err()
            {
                warn!(
                    target: SESSION,
                    "terminal display cleanup transfer failed; Drop fallback remains armed"
                );
            }
        }
    }
    let termination = lease.start_terminate();
    termination.wait().await;
    if let Err(error) = session_registry.resume().complete_drain(&active_session_id) {
        tracing::error!(
            target: SESSION,
            ?error,
            "terminal resume slot drain completion failed"
        );
    }
}

async fn next_microphone_agent_event(
    events: &mut Option<watch::Receiver<crate::microphone_input::MicrophoneAgentEvent>>,
) -> Option<crate::microphone_input::MicrophoneAgentEvent> {
    let Some(events) = events.as_mut() else {
        std::future::pending::<()>().await;
        return None;
    };
    if events.changed().await.is_err() {
        return Some(crate::microphone_input::MicrophoneAgentEvent::Failed);
    }
    Some(*events.borrow_and_update())
}

async fn send_microphone_failure(control: &ControlSender, generation: u32) -> bool {
    let stop = MicrophoneStreamStopMsg::new(
        generation,
        arcen_protocol::messages::MicrophoneStreamReason::CaptureFailure,
    );
    let message = serde_json::to_string(&stop).expect("microphone stop must serialize");
    control
        .send_required(Message::Text(message), MICROPHONE_STREAM_STOP)
        .await
}

async fn shutdown_microphone_agent(agent: crate::microphone_input::MicrophoneAgent) -> bool {
    match agent.shutdown().await {
        crate::microphone_input::MicrophoneCleanupOutcome::Graceful => true,
        crate::microphone_input::MicrophoneCleanupOutcome::Forced => {
            warn!(
                target: AUDIO,
                "microphone helper required forced cleanup; restoration verified"
            );
            true
        }
        crate::microphone_input::MicrophoneCleanupOutcome::Failure(error) => {
            tracing::error!(
                target: AUDIO,
                %error,
                "microphone cleanup failed; recovery journal retained"
            );
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_attachment(
    ws: DirectSessionSocket,
    cfg: Arc<Config>,
    session_lease: Option<&SessionLease>,
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    force_fresh_idr: bool,
    resumable: Option<ResumableAttachment<'_>>,
    cursor_preference: CursorMode,
    display_mode: auth::SessionDisplayMode,
    mut display: Option<&mut HeldDisplayResources>,
    shutdown: &mut watch::Receiver<bool>,
    remote_host: &str,
    remote_scope_id: Option<u32>,
    service_health: Arc<AtomicU8>,
    qos_targets: Arc<RwLock<QosTargets>>,
) -> AttachmentEnd {
    // Authenticated top-level identity for every session-scoped lifecycle
    // event emitted from this attachment (auth already confirmed this
    // username via PAM before a `SessionLease` could exist; `--no-auth`
    // attachments have no lease and stay `None`, matching that mode's
    // existing no-identity behavior).
    let session_user = session_lease.map(|lease| lease.metadata.username.clone());
    let active_transport = ws.transport_capability();
    let (mut owner_commands, refresh) = match resumable {
        Some(resumable) => (Some(resumable.owner_commands), Some(resumable.refresh)),
        None => (None, None),
    };
    // Mid-session stream resize is offered only for Windowed sessions that hold
    // a display guard they can retarget.
    let resize_supported = display
        .as_ref()
        .is_some_and(|resources| resources.can_reassign())
        && display_mode.allows_live_resize();
    let (mut sink, mut stream) = ws.split();
    // Recovery is tied to the authenticated user environment, not to current
    // microphone negotiation or media startup.
    let session_agent_binary = cfg.resolve_session_agent_binary();
    let microphone_recovery_verified =
        if let (Some(binary), Some(lease)) = (session_agent_binary.as_deref(), session_lease) {
            match crate::microphone_input::recover_for_user(
                binary,
                &cfg.pactl_bin,
                &lease.execution,
                &session_log_id,
            )
            .await
            {
                Ok(()) => true,
                Err(_) => {
                    warn!(
                        target: AUDIO,
                        event = "mic_linux_recovery_failure",
                        sid = %session_log_id,
                        reason = "recovery_failed",
                        "session-user microphone recovery failed"
                    );
                    false
                }
            }
        } else {
            false
        };
    let binary = match cfg.resolve_capenc_binary() {
        Some(binary) => binary,
        None => {
            warn!(target: CAPENC, "no capenc binary — closing connection");
            send_critical_control(&mut sink, Message::Close(None), "capenc_missing_close").await;
            return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
        }
    };
    let user_execution = session_lease.map(|lease| lease.execution.clone());
    let authoritative_timezone = session_lease.and_then(SessionLease::timezone).cloned();
    let initial_capenc = match display
        .as_ref()
        .and_then(|resources| resources.resolution())
    {
        Some(resolution) => cfg.capenc_config_for_size(
            binary.clone(),
            user_execution.clone(),
            session_log_id.clone(),
            cursor_preference,
            resolution.width,
            resolution.height,
        ),
        None => cfg.capenc_config(
            binary.clone(),
            user_execution.clone(),
            session_log_id.clone(),
            cursor_preference,
        ),
    };
    // Committed at admission (`session::multi_monitor::admit_requested_topology`)
    // and persisted onto this session's lease (`session::lifecycle`); fixed
    // for this session's lifetime — a reconnect reuses the exact same plan,
    // never a freshly recomputed one (see `SessionRegistry::acquire`).
    let multi_monitor_committed = session_lease.and_then(|lease| {
        lease
            .metadata
            .multi_monitor_plan
            .as_ref()
            .zip(lease.metadata.multi_monitor_carrier)
    });
    // Populated only for `CapencHandle::Multi`: the primary monitor's own
    // idr/frame-stream (mirroring the single-monitor `capenc.idr()` /
    // `capenc.take_frames()` call sites below), every other applied
    // monitor's frame source (muxed in once `queue`/`pump` exist), and the
    // applied capability attached to `ServerHello` before any IDR flows.
    let mut primary_idr: Option<IdrRequester> = None;
    let mut primary_frames_rx: Option<mpsc::Receiver<crate::media::annexb::AccessUnit>> = None;
    let mut multi_monitor_secondary_sources: Vec<multi_capenc::MonitorFrameSource> = Vec::new();
    let mut applied_multi_monitor_capability: Option<ServerMultiMonitorMsg> = None;
    let (capenc, media_plan): (CapencHandle, ResolvedMediaPlan) = match multi_monitor_committed {
        Some((plan, carrier)) => {
            let template = MonitorPipelineTemplate {
                binary: initial_capenc.binary.clone(),
                codec: initial_capenc.codec.clone(),
                encoder: initial_capenc.encoder,
                fps: initial_capenc.fps,
                yuv444: initial_capenc.yuv444,
                bit_depth: initial_capenc.bit_depth,
                color_range: initial_capenc.color_range,
                color_matrix: initial_capenc.color_matrix,
                intent: initial_capenc.intent,
                qp_map: initial_capenc.qp_map,
                video_selection: initial_capenc.video_selection,
                cursor_mode: initial_capenc.cursor_mode,
                display: initial_capenc.display.clone(),
                xauthority: initial_capenc.xauthority.clone(),
                execution: initial_capenc.execution.clone(),
                session_log_id: initial_capenc.session_log_id.clone(),
            };
            let encoder_plan = match encoder_admission::plan_encoder_sets(
                plan,
                &template,
                cfg.multi_monitor.nvenc_session_limit,
                cfg.multi_monitor.allow_software_fallback && cfg.exact_pins_allow_software_h264(),
            ) {
                Ok(plan) => plan,
                Err(error) => {
                    warn!(
                        target: CAPENC,
                        %error,
                        "multi-monitor pipeline configuration is invalid — closing connection"
                    );
                    send_critical_control(
                        &mut sink,
                        close_with_reason("capture/encoder initialization failed"),
                        "multi_monitor_config_close",
                    )
                    .await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            };
            let admission_targets = qos_targets
                .read()
                .map(|targets| *targets)
                .unwrap_or(cfg.logging.qos_targets);
            let thresholds =
                arcen_media::EncoderAdmissionThresholds::from_qos_targets(admission_targets);
            let (encoder_plan, decision) = match tokio::task::spawn_blocking(move || {
                let decision = encoder_plan.admit_runtime(thresholds);
                (encoder_plan, decision)
            })
            .await
            {
                Ok((plan, Ok(decision))) => (plan, decision),
                Ok((_, Err(error))) => {
                    warn!(
                        target: CAPENC,
                        %error,
                        "multi-monitor encoder admission configuration failed — closing connection"
                    );
                    send_critical_control(
                        &mut sink,
                        close_with_reason("encoder admission failed"),
                        "multi_monitor_encoder_admission_close",
                    )
                    .await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
                Err(error) => {
                    warn!(
                        target: CAPENC,
                        %error,
                        "multi-monitor encoder admission worker failed — closing connection"
                    );
                    send_critical_control(
                        &mut sink,
                        close_with_reason("encoder admission failed"),
                        "multi_monitor_encoder_admission_worker_close",
                    )
                    .await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            };
            encoder_admission::emit_admission_telemetry(&decision);
            // The accepted candidate's specs *and* the negotiated media roster
            // it was admitted with: the roster names each region's committed
            // bitrate budget, which the applied capability publishes verbatim
            // rather than re-deriving from the resolved geometry.
            let Some((specs, negotiated_media)) = encoder_plan
                .selected_specs(&decision)
                .map(<[_]>::to_vec)
                .zip(encoder_plan.selected_media_roster(&decision).cloned())
            else {
                warn!(
                    target: CAPENC,
                    "every exact multi-monitor encoder candidate failed measured admission"
                );
                send_critical_control(
                    &mut sink,
                    close_with_reason("encoder capacity admission rejected"),
                    "multi_monitor_encoder_capacity_close",
                )
                .await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            };
            let mut supervisor = match MultiCapencSupervisor::start(specs).await {
                Ok(supervisor) => supervisor,
                Err(error) => {
                    warn!(
                        target: CAPENC,
                        %error,
                        "multi-monitor capenc pipelines failed before READY — closing connection"
                    );
                    send_critical_control(
                        &mut sink,
                        close_with_reason("capture/encoder initialization failed"),
                        "multi_monitor_capenc_startup_close",
                    )
                    .await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            };
            let mut sources = supervisor.take_frame_sources();
            // Fail closed rather than let this committed multi-head Xorg
            // geometry silently diverge from what every monitor's own
            // `capenc` child actually resolved and reported in its READY
            // handshake — see `multi_capenc::verify_uniform_exact_pipeline_geometry`.
            // `multi_capenc::build_pipeline_specs` (above) already makes a
            // *policy*-driven clamp/backend split structurally impossible
            // for this host's two permitted concrete encoder requests, but
            // this still checks what actually happened, catching e.g. a
            // genuine hardware quirk that clamps one monitor's resolved
            // geometry even though no policy asked it to. Never a partial
            // multi-monitor session: any mismatch shuts every already
            // -started pipeline down before returning.
            if let Err(error) = multi_capenc::verify_uniform_exact_pipeline_geometry(plan, &sources)
            {
                warn!(
                    target: CAPENC,
                    %error,
                    "multi-monitor pipelines resolved a non-exact or non-uniform geometry/backend — closing connection"
                );
                supervisor.shutdown().await;
                send_critical_control(
                    &mut sink,
                    close_with_reason("capture/encoder initialization failed"),
                    "multi_monitor_geometry_mismatch_close",
                )
                .await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
            let media_for_capability: Vec<_> = sources
                .iter()
                .map(|source| (source.session_monitor_id, source.plan))
                .collect();
            let gate = multi_monitor_gate(&cfg);
            let capability = match multi_monitor::build_applied_capability(
                &gate,
                plan,
                carrier,
                &media_for_capability,
                &negotiated_media,
            ) {
                Ok(capability) => capability,
                Err(error) => {
                    warn!(
                        target: CAPENC,
                        %error,
                        "applied multi_monitor_v1 capability could not be assembled — closing connection"
                    );
                    supervisor.shutdown().await;
                    send_critical_control(
                        &mut sink,
                        close_with_reason("capture/encoder initialization failed"),
                        "multi_monitor_capability_close",
                    )
                    .await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            };
            let primary_id = plan.primary().session_monitor_id;
            let Some(primary_position) = sources
                .iter()
                .position(|source| source.session_monitor_id == primary_id)
            else {
                warn!(
                    target: CAPENC,
                    "plan's primary monitor has no matching started pipeline — closing connection"
                );
                supervisor.shutdown().await;
                send_critical_control(
                    &mut sink,
                    close_with_reason("capture/encoder initialization failed"),
                    "multi_monitor_primary_missing_close",
                )
                .await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            };
            let primary_source = sources.remove(primary_position);
            let media_plan = primary_source.plan;
            primary_idr = Some(primary_source.idr);
            primary_frames_rx = Some(primary_source.frames);
            multi_monitor_secondary_sources = sources;
            applied_multi_monitor_capability = Some(capability);
            (CapencHandle::Multi(supervisor), media_plan)
        }
        None => match capenc::spawn(initial_capenc.clone()).await {
            Ok((session, plan)) => (CapencHandle::Single(session), plan),
            Err(error) => {
                warn!(
                    target: CAPENC,
                    %error,
                    "capenc failed before READY — closing connection"
                );
                send_critical_control(
                    &mut sink,
                    close_with_reason("capture/encoder initialization failed"),
                    "capenc_startup_close",
                )
                .await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
        },
    };
    let (input_controller, input_stats): (Option<InputController>, Option<Arc<InputStats>>) = if cfg
        .input_mode
        == InputMode::Uinput
    {
        // The ABS range stays aligned to the fixed X11 raster. ViewPortIn
        // changes capture scaling, not the pointer's normalized X space.
        //
        // A committed multi-monitor plan takes priority: its dedicated
        // `DedicatedXorg` never holds a `MetaModeGuard` (multi-monitor
        // sessions are always fixed-topology, never live-resized), so
        // `display.raster_size()` is always `None` there, and
        // `media_plan` alone only reflects the *primary* monitor's own
        // footprint. The global absolute pointer/pen device must instead
        // span the whole RandR-verified combined desktop
        // (`plan.virtual_width`/`virtual_height`). Region-local logical
        // coordinates stay in the shared region domain until
        // `input::region_adapter` maps them into this final combined
        // raster. Relative motion is unaffected: it uses the separate
        // relative device, never this absolute-device sizing.
        let (device_w, device_h) = resolve_input_raster(
            multi_monitor_committed
                .map(|(plan, _carrier)| (plan.virtual_width, plan.virtual_height)),
            display.as_ref().and_then(|r| r.raster_size()),
            (media_plan.width, media_plan.height),
        );
        let region_input = match multi_monitor_committed
            .map(|(plan, _carrier)| RegionInputAdapter::from_plan(plan))
            .transpose()
        {
            Ok(adapter) => adapter,
            Err(error) => {
                warn!(target: INPUT, %error, "shared Linux region input adapter setup failed");
                send_critical_control(&mut sink, Message::Close(None), "region_input_setup_close")
                    .await;
                capenc.shutdown().await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
        };
        match InputController::new(device_w, device_h, region_input) {
            Ok((controller, stats)) => {
                info!(
                    target: INPUT,
                    device_w, device_h,
                    pen_available = controller.pen_available(),
                    "native uinput device created"
                );
                // Give Xorg/libinput time to process the udev hotplug and
                // create the tablet tool virtual subdevice before the client
                // connects and starts sending pen events. Without this wait,
                // early pen events arrive while libinput has not yet opened
                // the evdev fd — those events are silently dropped by the
                // kernel's evdev ring (no fd = no reader). ~1 s is needed
                // in practice for a libinput tablet "Pen (0)" subdevice to
                // appear; 1.5 s gives comfortable margin.
                if controller.pen_available() {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                (Some(controller), Some(stats))
            }
            Err(error) => {
                warn!(target: INPUT, %error, "native uinput setup failed");
                send_critical_control(&mut sink, Message::Close(None), "input_setup_close").await;
                capenc.shutdown().await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
        }
    } else {
        (None, None)
    };
    // Runtime-established truth only, probed/created above before this
    // `ServerHello` is built: `false` whenever the tablet-tool uinput device
    // was not actually created, including when the input backend is off.
    // Pen failure alone must never fail this match arm or disable
    // mouse/keyboard, which is why it is read from the already-constructed
    // controller rather than gating construction above.
    let pen_available = input_controller
        .as_ref()
        .is_some_and(InputController::pen_available);
    let region_input_available = input_controller
        .as_ref()
        .is_some_and(InputController::region_input_available);

    // The child has initialized its real capture+encoder path. Only now can the
    // host advertise the active backend and format truthfully.
    let microphone_backend_available = cfg.microphone_input_enabled
        && microphone_recovery_verified
        && if let Some(lease) = session_lease {
            crate::microphone_input::probe_backend(&cfg.pactl_bin, &lease.execution).await
        } else {
            false
        };
    info!(
        target: AUDIO,
        event = "mic_linux_backend_probe",
        sid = %session_log_id,
        operator_enabled = cfg.microphone_input_enabled,
        backend = "pulseaudio_pipe_source",
        backend_available = microphone_backend_available,
        recovery_verified = microphone_recovery_verified,
        reason = if cfg.microphone_input_enabled {
            if microphone_backend_available { "available" } else { "backend_unavailable" }
        } else {
            "policy_off"
        },
        "Linux microphone backend probe completed"
    );
    let mut hello = build_server_hello(
        &cfg,
        &media_plan,
        session_lease.map(|lease| &lease.metadata),
        resize_supported,
        microphone_backend_available,
        pen_available,
        region_input_available,
    );
    hello.negotiated_transport = Some(active_transport.to_string());
    if let Some(capability) = applied_multi_monitor_capability.as_ref() {
        hello = match hello.with_multi_monitor_v1(capability) {
            Ok(hello) => hello,
            Err(error) => {
                warn!(
                    target: SESSION,
                    %error,
                    "applied multi_monitor_v1 capability rejected by hello encoder — closing connection"
                );
                capenc.shutdown().await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
        };
    }
    match serde_json::to_string(&hello) {
        Ok(json) => {
            if !send_critical_control(&mut sink, Message::Text(json), "server_hello").await {
                capenc.shutdown().await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
        }
        Err(e) => {
            warn!(target: SESSION, error = %e, "failed to serialize server_hello");
            capenc.shutdown().await;
            return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
        }
    }
    debug!(target: SESSION, "server_hello sent");
    let client_hello = match receive_client_hello(&mut stream, HANDSHAKE_RECEIVE_TIMEOUT).await {
        Ok(hello) => hello,
        Err(error) => {
            warn!(target: SESSION, %error, "client hello rejected");
            send_critical_control(
                &mut sink,
                close_with_reason("client capability exchange failed"),
                "client_hello_close",
            )
            .await;
            capenc.shutdown().await;
            return handshake_receive_attachment_end(&error, refresh.as_ref());
        }
    };
    if let Some(initial) = cfg.auth_video_request.as_ref() {
        let echoed =
            arcen_protocol::messages::ClientVideoCapabilitiesMsg::from_client_hello(&client_hello);
        if echoed != initial.capabilities {
            warn!(
                target: SESSION,
                "ClientHello video capabilities differ from the authenticated setup request"
            );
            send_critical_control(
                &mut sink,
                close_with_reason(
                    "client video capabilities differ from authenticated setup request",
                ),
                "client_video_capability_mismatch_close",
            )
            .await;
            capenc.shutdown().await;
            return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
        }
    }
    if applied_multi_monitor_capability.is_some()
        && !match_layout_region_input_negotiated(region_input_available, &client_hello)
    {
        warn!(
            target: SESSION,
            required_input_protocol = REGION_INPUT_PROTOCOL_VERSION,
            host_region_input = region_input_available,
            client_input_protocol = client_hello.input_protocol_version,
            client_region_input = ?client_hello.input_capabilities.region_input,
            "multi-monitor client did not negotiate region input"
        );
        send_critical_control(
            &mut sink,
            close_with_reason("multi-monitor region input capability required"),
            "region_input_negotiation_close",
        )
        .await;
        capenc.shutdown().await;
        return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
    }
    let _negotiated_transport =
        match negotiate_client_transport(&client_hello.transport_capabilities, active_transport) {
            Some(transport) => {
                tracing::info!(
                    target: SESSION,
                    negotiated_transport = %transport,
                    "transport negotiated"
                );
                transport
            }
            None => {
                warn!(
                    target: SESSION,
                    client_capabilities = ?client_hello.transport_capabilities,
                    "transport negotiation failed: no common transport capability"
                );
                send_critical_control(
                    &mut sink,
                    close_with_reason("no common transport capability"),
                    "transport_negotiation_close",
                )
                .await;
                capenc.shutdown().await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            }
        };
    let initial_quality =
        match receive_quality_settings(&mut stream, HANDSHAKE_RECEIVE_TIMEOUT).await {
            Ok(quality) => quality,
            Err(error) => {
                warn!(target: SESSION, %error, "initial quality settings rejected");
                send_critical_control(
                    &mut sink,
                    close_with_reason("initial quality negotiation failed"),
                    "quality_settings_close",
                )
                .await;
                capenc.shutdown().await;
                return handshake_receive_attachment_end(&error, refresh.as_ref());
            }
        };
    if let Some(initial) = cfg.auth_video_request.as_ref() {
        if initial_quality != initial.quality {
            warn!(
                target: SESSION,
                "quality_settings differ from the authenticated setup request"
            );
            send_critical_control(
                &mut sink,
                close_with_reason("quality settings differ from authenticated video request"),
                "quality_video_request_mismatch_close",
            )
            .await;
            capenc.shutdown().await;
            return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
        }
    }
    if client_hello.cursor_preference != cursor_preference {
        warn!(
            target: SESSION,
            "ClientHello cursor preference differs from authenticated decision"
        );
    }
    // Honour the client's codec/chroma request when it differs from the
    // host default and the concrete backend can produce it.
    //
    // Protocol order: capenc must start before server_hello so the hello
    // truthfully reports the active codec. After hello the client sends
    // quality_settings with its preference. If that preference differs,
    // respawn capenc now — before any frame pump or queue is created — so
    // every downstream component sees the client-requested plan.
    //
    let mut active_encode_intent = initial_capenc.intent;
    let (mut capenc, media_plan) = match capenc {
        CapencHandle::Multi(supervisor) => {
            if cfg.auth_video_request.is_none() {
                let adaptive = initial_quality.video_selection
                    == arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance;
                let codec_mismatch = !adaptive
                    && !initial_quality.codec.is_empty()
                    && initial_quality.codec != media_plan.codec_token();
                let chroma_mismatch = !initial_quality.chroma.is_empty()
                    && initial_quality.chroma != media_plan.chroma_token();
                let (bit_depth, color_range, color_matrix) =
                    resolve_client_color_request_with_matrix_caps(
                        cfg.color_policy,
                        ColorCeiling {
                            bit_depth: cfg.bit_depth,
                            color_range: cfg.color_range,
                            color_matrix: cfg.color_matrix,
                        },
                        ClientColorRequest {
                            bit_depth: BitDepth::from_token(&initial_quality.bit_depth),
                            color_range: ColorRange::from_token(&initial_quality.color_range),
                            color_matrix: ColorMatrix::from_token(&initial_quality.color_matrix),
                            supports_main10: client_hello.supports_main10,
                            supports_main12: client_hello.supports_main12,
                            supports_full_range: client_hello.supports_full_range,
                            supports_identity_matrix: client_hello.supports_identity_matrix,
                        },
                        ColorMatrixCapabilities {
                            bt601: client_hello.supports_bt601_matrix,
                            bt2020_ncl: client_hello.supports_bt2020_ncl_matrix,
                        },
                    );
                let color_mismatch = bit_depth != media_plan.video.bit_depth
                    || color_range != media_plan.video.range
                    || color_matrix != media_plan.video.matrix;
                let intent_mismatch = EncodeIntent::from_token(&initial_quality.encode_intent)
                    .is_some_and(|intent| intent != cfg.requested_encode_intent());
                if codec_mismatch || chroma_mismatch || color_mismatch || intent_mismatch {
                    warn!(
                        target: SESSION,
                        "legacy multi-monitor quality change requires a whole-roster reconnect"
                    );
                    send_critical_control(
                        &mut sink,
                        close_with_reason(
                            "legacy multi-monitor quality change is unsupported; reconnect with a current Arcen Deck",
                        ),
                        "legacy_multi_monitor_quality_close",
                    )
                    .await;
                    CapencHandle::Multi(supervisor).shutdown().await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            }
            (CapencHandle::Multi(supervisor), media_plan)
        }
        CapencHandle::Single(session) => {
            let adaptive = initial_quality.video_selection
                == arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance;
            let want_codec = if adaptive {
                media_plan.codec_token()
            } else {
                initial_quality.codec.as_str()
            };
            let want_yuv444 = initial_quality.chroma == "yuv444";
            let codec_mismatch =
                !adaptive && !want_codec.is_empty() && want_codec != media_plan.codec_token();
            let chroma_mismatch = want_codec != "h264" // h264 only supports yuv420
                && !initial_quality.chroma.is_empty()
                && want_yuv444 != matches!(media_plan.video.chroma, ChromaSubsampling::Yuv444);

            // Parse the client's bit_depth/color_range/color_matrix tokens.
            // An unrecognised token is logged distinctly and treated as no
            // client preference for that axis (exactly like an absent field
            // from an old client) rather than silently defaulted — a
            // silently-defaulted colour field would produce a stream that
            // lies about its own colour, which is the exact failure this
            // workstream exists to remove.
            let requested_bit_depth = BitDepth::from_token(&initial_quality.bit_depth);
            if requested_bit_depth.is_none() {
                warn!(
                    target: SESSION,
                    token = initial_quality.bit_depth.as_str(),
                    "quality_settings bit_depth token not recognised — treating as no client preference"
                );
            }
            let requested_color_range = ColorRange::from_token(&initial_quality.color_range);
            if requested_color_range.is_none() {
                warn!(
                    target: SESSION,
                    token = initial_quality.color_range.as_str(),
                    "quality_settings color_range token not recognised — treating as no client preference"
                );
            }
            let requested_color_matrix = ColorMatrix::from_token(&initial_quality.color_matrix);
            if requested_color_matrix.is_none() {
                warn!(
                    target: SESSION,
                    token = initial_quality.color_matrix.as_str(),
                    "quality_settings color_matrix token not recognised — treating as no client preference"
                );
            }
            // Intent gets the same treatment as the colour axes above, for the
            // same reason: an unrecognised token is a client that means
            // something this host does not understand, and guessing at it
            // would spend the session's latency budget on an intent nobody
            // asked for. Unlike them it has no operator-configured ceiling to
            // resolve against, so "no preference" is simply what this session
            // already spawned with.
            let requested_intent = EncodeIntent::from_token(&initial_quality.encode_intent);
            if requested_intent.is_none() {
                warn!(
                    target: SESSION,
                    token = initial_quality.encode_intent.as_str(),
                    "quality_settings encode_intent token not recognised — treating as no client preference"
                );
            }
            let resolved_intent = requested_intent.unwrap_or(initial_capenc.intent);
            // Policy precedence, then the absolute client-capability
            // cross-check: never grant more than `client_hello` claimed this
            // client can decode, regardless of what policy would otherwise
            // serve.
            let (resolved_bit_depth, resolved_color_range, resolved_color_matrix) =
                resolve_client_color_request_with_matrix_caps(
                    cfg.color_policy,
                    ColorCeiling {
                        bit_depth: cfg.bit_depth,
                        color_range: cfg.color_range,
                        color_matrix: cfg.color_matrix,
                    },
                    ClientColorRequest {
                        bit_depth: requested_bit_depth,
                        color_range: requested_color_range,
                        color_matrix: requested_color_matrix,
                        supports_main10: client_hello.supports_main10,
                        supports_main12: client_hello.supports_main12,
                        supports_full_range: client_hello.supports_full_range,
                        supports_identity_matrix: client_hello.supports_identity_matrix,
                    },
                    ColorMatrixCapabilities {
                        bt601: client_hello.supports_bt601_matrix,
                        bt2020_ncl: client_hello.supports_bt2020_ncl_matrix,
                    },
                );
            let color_mismatch = resolved_bit_depth != media_plan.video.bit_depth
                || resolved_color_range != media_plan.video.range
                || resolved_color_matrix != media_plan.video.matrix;
            // Intent is fixed at spawn exactly as codec and chroma are, so
            // honouring a change costs the same respawn. Without this term a
            // client that already agrees with the host on every format axis —
            // the common case for a grading session, which asks for the
            // host's own 10-bit 4:4:4 plan — could never move the encoder off
            // latency-first at all.
            let intent_mismatch = resolved_intent != initial_capenc.intent;

            if codec_mismatch || chroma_mismatch || color_mismatch || intent_mismatch {
                if let Some(concrete_encoder) = concrete_encoder_for(media_plan) {
                    // `codec_mismatch`/`chroma_mismatch` above read an empty
                    // field as "this client states no preference on that
                    // axis". A respawn triggered by one of the other axes has
                    // to honour that same reading, or an intent-only or
                    // colour-only request would hand capenc an empty codec
                    // token it must reject and quietly drop a 4:4:4 session to
                    // 4:2:0 on the way through.
                    let stated_codec = if want_codec.is_empty() {
                        media_plan.codec_token()
                    } else {
                        want_codec
                    };
                    let final_yuv444 = if initial_quality.chroma.is_empty() {
                        matches!(media_plan.video.chroma, ChromaSubsampling::Yuv444)
                    } else {
                        want_yuv444 && stated_codec != "h264"
                    };
                    let want_codec_enum =
                        VideoCodec::from_token(stated_codec).unwrap_or(media_plan.video.codec);
                    let candidate_video = VideoConfiguration {
                        codec: want_codec_enum,
                        chroma: if final_yuv444 {
                            ChromaSubsampling::Yuv444
                        } else {
                            ChromaSubsampling::Yuv420
                        },
                        bit_depth: resolved_bit_depth,
                        range: resolved_color_range,
                        matrix: resolved_color_matrix,
                        primaries: ColorPrimaries::Bt709,
                        transfer: TransferCharacteristics::Bt709,
                    };
                    // Validate coherence and backend capability before ever
                    // forwarding this to capenc: an incoherent request (e.g.
                    // an identity matrix below 4:4:4) or one this concrete
                    // backend cannot serve (e.g. 12-bit on NVENC) falls back
                    // to the host's current plan with a logged reason,
                    // rather than being handed to capenc to reject blindly.
                    if !color_contract_is_servable(candidate_video, &media_plan) {
                        warn!(
                            target: SESSION,
                            want_codec,
                            want_chroma = initial_quality.chroma.as_str(),
                            want_bit_depth = resolved_bit_depth.token(),
                            want_color_range = resolved_color_range.token(),
                            want_color_matrix = resolved_color_matrix.token(),
                            want_encode_intent = resolved_intent.token(),
                            host_codec = media_plan.codec_token(),
                            host_chroma = media_plan.chroma_token(),
                            "resolved colour/codec request is incoherent or unsupported by this backend — keeping host plan"
                        );
                        (CapencHandle::Single(session), media_plan)
                    } else {
                        info!(
                            target: SESSION,
                            want_codec,
                            want_chroma = initial_quality.chroma.as_str(),
                            want_bit_depth = resolved_bit_depth.token(),
                            want_color_range = resolved_color_range.token(),
                            want_color_matrix = resolved_color_matrix.token(),
                            want_encode_intent = resolved_intent.token(),
                            host_codec = media_plan.codec_token(),
                            host_chroma = media_plan.chroma_token(),
                            "client quality_settings differ from initial plan — respawning capenc"
                        );
                        let mut override_config = initial_capenc.clone();
                        override_config.codec = stated_codec.to_string();
                        override_config.yuv444 = final_yuv444;
                        override_config.bit_depth = resolved_bit_depth;
                        override_config.color_range = resolved_color_range;
                        override_config.color_matrix = resolved_color_matrix;
                        override_config.intent = resolved_intent;
                        override_config.encoder = concrete_encoder;
                        active_encode_intent = override_config.intent;
                        session.shutdown().await;
                        match capenc::spawn(override_config.clone()).await {
                            Ok((session, plan)) => {
                                info!(
                                    target: SESSION,
                                    resolved_codec = plan.codec_token(),
                                    resolved_chroma = plan.chroma_token(),
                                    resolved_bit_depth = plan.bit_depth_token(),
                                    resolved_color_range = plan.range_token(),
                                    resolved_color_matrix = plan.matrix_token(),
                                    // Not carried on `ResolvedMediaPlan`:
                                    // intent changes how the encoder spends
                                    // its budget, never what format it
                                    // announces, so the requested value is
                                    // the only truth there is to report.
                                    resolved_encode_intent = override_config.intent.token(),
                                    "capenc respawn for client codec request succeeded"
                                );
                                (CapencHandle::Single(session), plan)
                            }
                            Err(error) => {
                                warn!(
                                    target: CAPENC,
                                    %error,
                                    "capenc respawn for client codec request failed — closing connection"
                                );
                                send_critical_control(
                                    &mut sink,
                                    close_with_reason("codec override respawn failed"),
                                    "codec_respawn_close",
                                )
                                .await;
                                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                            }
                        }
                    }
                } else {
                    warn!(
                        target: SESSION,
                        want_codec,
                        want_chroma = initial_quality.chroma.as_str(),
                        want_bit_depth = resolved_bit_depth.token(),
                        want_color_range = resolved_color_range.token(),
                        want_color_matrix = resolved_color_matrix.token(),
                        want_encode_intent = resolved_intent.token(),
                        host_codec = media_plan.codec_token(),
                        "client requested codec unsupported by this backend — keeping host plan"
                    );
                    (CapencHandle::Single(session), media_plan)
                }
            } else {
                (CapencHandle::Single(session), media_plan)
            }
        }
    };
    let active_encoder = concrete_encoder_for(media_plan).unwrap_or(initial_capenc.encoder);
    let mut active_capenc_config = initial_capenc.clone().pinned_to_active_plan(
        &media_plan,
        active_encoder,
        active_encode_intent,
    );
    if timezone_echo_mismatch(
        authoritative_timezone.as_ref(),
        client_hello.timezone.as_deref(),
    ) {
        warn!(
            target: SESSION,
            "ClientHello timezone differs from authoritative desktop decision"
        );
    }
    let clipboard_eligible = session_lease.is_some_and(|lease| {
        cfg.auth_mode == AuthMode::Pam
            && lease.metadata.session_type == "x11"
            && lease.metadata.display == cfg.session_display
            && lease.metadata.uid != 0
    });
    let clipboard_negotiation =
        ClipboardNegotiation::from_client(cfg.clipboard_policy, clipboard_eligible, &client_hello);
    let (mut clipboard_process, clipboard_agent) =
        if let (Some(negotiation), Some(lease)) = (clipboard_negotiation, session_lease) {
            let Some(binary) = session_agent_binary.clone() else {
                warn!(target: SESSION, "clipboard agent binary unavailable");
                capenc.shutdown().await;
                return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
            };
            match spawn_clipboard_agent(
                &binary,
                &lease.execution,
                negotiation.policy(),
                &session_log_id,
            )
            .await
            {
                Ok((process, websocket)) => (Some(process), Some(websocket)),
                Err(error) => {
                    warn!(target: SESSION, %error, "clipboard agent startup failed");
                    capenc.shutdown().await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            }
        } else {
            (None, None)
        };
    let microphone_policy = MicrophonePolicy {
        operator_enabled: cfg.microphone_input_enabled,
        backend_available: microphone_backend_available,
        codecs: arcen_media::audio::MicrophoneCodecAvailability {
            opus: true,
            pcm: true,
        },
    };
    let microphone_generation_result = next_microphone_generation();
    let microphone_randomness_available = microphone_generation_result.is_ok();
    let microphone_generation = match microphone_generation_result {
        Ok(generation) => generation,
        Err(_) => {
            warn!(
                target: AUDIO,
                event = "mic_frame_rejected",
                sid = %session_log_id,
                reason = "generation_unavailable",
                "microphone generation unavailable; disabling attachment input"
            );
            1
        }
    };
    let microphone_policy = MicrophonePolicy {
        backend_available: microphone_policy.backend_available
            && microphone_randomness_available
            && !*shutdown.borrow(),
        ..microphone_policy
    };
    let mut microphone_stream = microphone_policy.resolve(
        client_hello.microphone_output.as_ref(),
        client_hello.microphone_output.is_some(),
        microphone_generation,
        64,
    );
    let mut microphone_agent = if microphone_stream.is_enabled() {
        match (session_agent_binary.clone(), session_lease) {
            (Some(binary), Some(lease)) => match crate::microphone_input::spawn(
                &binary,
                &cfg.pactl_bin,
                &lease.execution,
                microphone_stream,
                &session_log_id,
            )
            .await
            {
                Ok(agent) => Some(agent),
                Err(_) => {
                    warn!(
                        target: AUDIO,
                        event = "mic_linux_helper_failure",
                        sid = %session_log_id,
                        backend = "pulseaudio_pipe_source",
                        reason = "startup_failed",
                        "session-user microphone agent startup failed"
                    );
                    microphone_stream = arcen_media::audio::ResolvedMicrophoneStream::disabled(
                        microphone_generation,
                        arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
                    );
                    None
                }
            },
            _ => {
                microphone_stream = arcen_media::audio::ResolvedMicrophoneStream::disabled(
                    microphone_generation,
                    arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
                );
                None
            }
        }
    } else {
        None
    };
    info!(
        target: AUDIO,
        event = "mic_negotiation",
        sid = %session_log_id,
        platform = "linux",
        enabled = microphone_stream.is_enabled(),
        operator_enabled = cfg.microphone_input_enabled,
        client_capability = client_hello.microphone_output.is_some(),
        backend_available = microphone_backend_available,
        backend = "pulseaudio_pipe_source",
        codec = ?microphone_stream.codec,
        sample_rate_hz = 48_000u32,
        channels = 1u8,
        frame_duration_ms = 20u16,
        generation = microphone_generation,
        reason = ?microphone_stream.reason,
        "microphone negotiation completed"
    );
    let microphone_result = serde_json::to_string(&microphone_stream.result())
        .expect("MicrophoneStreamResultMsg must serialize");
    if !send_critical_control(
        &mut sink,
        Message::Text(microphone_result),
        arcen_protocol::messages::MICROPHONE_STREAM_RESULT,
    )
    .await
    {
        if let Some(agent) = microphone_agent.take() {
            shutdown_microphone_agent(agent).await;
        }
        capenc.shutdown().await;
        return AttachmentEnd::terminal(SessionEndReason::WriterEnded);
    }
    let cursor_result = CursorModeResultMsg {
        requested: cursor_preference,
        active: media_plan.cursor_mode,
        accepted: cursor_preference == media_plan.cursor_mode,
        reason: CursorModeReason::default(),
        ..CursorModeResultMsg::default()
    };
    let cursor_result =
        serde_json::to_string(&cursor_result).expect("CursorModeResultMsg must serialize");
    if !send_critical_control(
        &mut sink,
        Message::Text(cursor_result),
        "cursor_mode_result",
    )
    .await
    {
        if let Some(agent) = microphone_agent.take() {
            shutdown_microphone_agent(agent).await;
        }
        capenc.shutdown().await;
        return AttachmentEnd::terminal(SessionEndReason::WriterEnded);
    }
    let client_tablet_capabilities = client_hello.effective_tablet_mode_capabilities();
    let usb_hard_speed =
        crate::usb_bridge::authorized_device_speed(client_hello.usb_hard_device.as_ref());
    let usb_hard_available = client_hello.usb_hard_v1
        && usb_hard_speed.is_some()
        && crate::usb_bridge::runtime_available();
    info!(
        target: NET,
        requested = ?client_hello.tablet_mode_requested,
        client_usb_hard_v1 = client_hello.usb_hard_v1,
        client_usb_hard_device = ?client_hello.usb_hard_device,
        runtime_available = crate::usb_bridge::runtime_available(),
        client_bridge_capability = ?client_tablet_capabilities.wacom_usb_bridge,
        usb_hard_available,
        "Hard USB negotiation inputs"
    );
    let host_tablet_capabilities = TabletModeCapabilitiesMsg {
        local_termination: if pen_available {
            InputCapabilityAvailability::Available
        } else {
            InputCapabilityAvailability::Unavailable
        },
        wacom_usb_bridge: if usb_hard_available {
            InputCapabilityAvailability::Available
        } else {
            InputCapabilityAvailability::Unavailable
        },
        disabled_mouse_compat: InputCapabilityAvailability::Available,
    };
    #[allow(unused_mut)]
    let mut tablet_mode_result = resolve_linux_tablet_mode_result(
        client_hello.tablet_mode_requested,
        client_tablet_capabilities,
        host_tablet_capabilities,
    );
    #[cfg(feature = "usb-hard-lab")]
    let mut usb_bridge = if tablet_mode_result.accepted
        && tablet_mode_result.active == TabletModeMsg::WacomUsbBridge
    {
        match (
            crate::current_pier_exe(),
            crate::usb_bridge::fresh_attachment_generation(),
        ) {
            (Some(binary), Ok(generation)) => {
                match crate::usb_bridge::BridgeProcess::spawn(
                    &binary,
                    generation,
                    usb_hard_speed.expect("accepted Hard USB mode has authorized speed"),
                )
                .await
                {
                    Ok(process) => Some(process),
                    Err(error) => {
                        warn!(target: NET, %error, "failed to start Hard USB helper");
                        tablet_mode_result.accepted = false;
                        tablet_mode_result.active = TabletModeMsg::DisabledMouseCompat;
                        tablet_mode_result.reason = TabletModeReason::try_from(
                            "Hard USB helper failed to start; mouse compatibility remains active"
                                .to_owned(),
                        )
                        .unwrap_or_default();
                        None
                    }
                }
            }
            (None, _) => {
                tablet_mode_result.accepted = false;
                tablet_mode_result.active = TabletModeMsg::DisabledMouseCompat;
                tablet_mode_result.reason =
                    TabletModeReason::try_from("Hard USB helper binary is unavailable".to_owned())
                        .unwrap_or_default();
                None
            }
            (_, Err(error)) => {
                warn!(target: NET, %error, "failed to create Hard USB attachment generation");
                tablet_mode_result.accepted = false;
                tablet_mode_result.active = TabletModeMsg::DisabledMouseCompat;
                tablet_mode_result.reason =
                    TabletModeReason::try_from("Hard USB generation is unavailable".to_owned())
                        .unwrap_or_default();
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(feature = "usb-hard-lab"))]
    let mut usb_bridge: Option<crate::usb_bridge::BridgeProcess> = None;
    // Hard USB is now settled, including any downgrade the helper spawn above
    // forced. A Hard USB session must present exactly one tablet to the seat:
    // the operator's real device, arriving on the virtual USB controller with
    // the vendor driver behind it. The typed pen device built before this
    // negotiation would otherwise linger as a second, permanently silent
    // tablet, so it is destroyed here — before the bridge attaches the real
    // one, which keeps the two out of each other's way by construction.
    //
    // Rebound rather than declared `mut` at its own binding: the controller is
    // built long before the requested tablet mode is known, and mutability is
    // wanted only for this one decision.
    let mut input_controller = input_controller;
    if tablet_mode_result.accepted && tablet_mode_result.active == TabletModeMsg::WacomUsbBridge {
        if let Some(controller) = input_controller.as_mut() {
            if controller.release_tablet_device() {
                info!(
                    target: INPUT,
                    "released the typed virtual tablet device for Hard USB; \
                     the bridged device is the only tablet on this seat"
                );
            }
        }
    }
    let tablet_mode_result =
        serde_json::to_string(&tablet_mode_result).expect("TabletModeResultMsg must serialize");
    if !send_critical_control(
        &mut sink,
        Message::Text(tablet_mode_result),
        arcen_protocol::messages::TABLET_MODE_RESULT,
    )
    .await
    {
        if let Some(agent) = microphone_agent.take() {
            shutdown_microphone_agent(agent).await;
        }
        capenc.shutdown().await;
        return AttachmentEnd::terminal(SessionEndReason::WriterEnded);
    }

    // Cursor shape streaming: only active in Local cursor mode. The watcher
    // runs in a dedicated OS thread and sends CursorShapeMsg JSON strings
    // whenever the X11 cursor changes. Old clients that don't understand the
    // message type ignore it.
    #[cfg(target_os = "linux")]
    let cursor_shape_rx = if media_plan.cursor_mode == CursorMode::Local {
        crate::cursor_watcher::spawn(cfg.session_display.clone(), cfg.xauthority.clone())
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let cursor_shape_rx: Option<tokio::sync::mpsc::Receiver<String>> = None;

    let idr = match &capenc {
        CapencHandle::Single(session) => session.idr(),
        CapencHandle::Multi(_) => primary_idr
            .clone()
            .expect("multi-monitor primary idr captured at spawn"),
    };
    if attachment_requires_fresh_idr(force_fresh_idr) {
        idr.request();
        // IDR barrier: every applied monitor's first frame must be a
        // keyframe, not just the primary's — a mid-attachment client sees a
        // fully decodable picture on every monitor from the first delivered
        // frame.
        for source in &multi_monitor_secondary_sources {
            source.idr.request();
        }
    }
    let frames_rx = match &mut capenc {
        CapencHandle::Single(session) => session
            .take_frames()
            .expect("capenc frames receiver available exactly once"),
        CapencHandle::Multi(_) => primary_frames_rx
            .take()
            .expect("multi-monitor primary frames receiver captured at spawn"),
    };
    let audio_queue = Arc::new(AudioQueue::new());
    let audio_binary = if cfg.audio_enabled {
        cfg.resolve_audiocap_binary()
    } else {
        None
    };
    if cfg.audio_enabled && audio_binary.is_none() {
        warn!(target: AUDIO, "native audiocap binary not found; continuing without audio");
    }
    let audio_config = audio_binary.map(|binary| AudioConfig {
        binary,
        execution: if cfg.audio_user_mode == AudioUserMode::Session {
            user_execution.clone()
        } else {
            None
        },
        session_log_id: session_log_id.clone(),
    });
    let audio_policy = AudioPolicy::configured(audio_config.is_some(), cfg.audio_compressed);
    let mut initial_audio_stream = audio_policy.resolve(
        client_hello.audio_output.as_ref(),
        initial_quality.enable_audio,
    );
    let mut audio_fallback_reason = None;
    let audio_encoder = match AudioFrameEncoder::new(initial_audio_stream) {
        Ok(encoder) => encoder,
        Err(_) if initial_audio_stream.codec == Some(AudioCodec::Opus) => {
            audio_fallback_reason = Some("opus_encoder_unavailable");
            let fallback_policy = audio_policy.without_opus();
            initial_audio_stream = fallback_policy.resolve(
                client_hello.audio_output.as_ref(),
                initial_quality.enable_audio,
            );
            AudioFrameEncoder::new(initial_audio_stream).unwrap_or_else(|_| {
                initial_audio_stream = ResolvedAudioStream::disabled(
                    initial_audio_stream.mode,
                    arcen_protocol::messages::AudioStreamReason::CodecUnavailable,
                );
                AudioFrameEncoder::new(initial_audio_stream)
                    .expect("disabled audio encoder is infallible")
            })
        }
        Err(_) => {
            initial_audio_stream = ResolvedAudioStream::disabled(
                initial_audio_stream.mode,
                arcen_protocol::messages::AudioStreamReason::CodecUnavailable,
            );
            AudioFrameEncoder::new(initial_audio_stream)
                .expect("disabled audio encoder is infallible")
        }
    };
    if let Some(reason) = audio_fallback_reason {
        warn!(
            target: AUDIO,
            event = "audio_output_codec_unavailable",
            sid = %session_log_id,
            requested_codec = "opus",
            fallback_codec = ?initial_audio_stream.codec,
            fallback_enabled = initial_audio_stream.is_enabled(),
            configured_compressed = cfg.audio_compressed,
            reason,
            "configured audio output codec is unavailable"
        );
    }
    info!(
        target: AUDIO,
        event = "audio_output_negotiated",
        sid = %session_log_id,
        enabled = initial_audio_stream.is_enabled(),
        codec = ?initial_audio_stream.codec,
        bitrate = ?initial_audio_stream.bitrate,
        configured_compressed = cfg.audio_compressed,
        requested_bitrate_kbps = initial_quality.audio_bitrate_kbps,
        sample_rate_hz = 48_000u32,
        channels = 2u8,
        packet_duration_ms = 20u16,
        capture_backend = "pulseaudio_monitor",
        reason = ?initial_audio_stream.reason,
        "audio output negotiation completed"
    );
    if let Some(result) = initial_audio_stream.result() {
        let result = serde_json::to_string(&result).expect("AudioStreamResultMsg must serialize");
        if !send_critical_control(
            &mut sink,
            Message::Text(result),
            arcen_protocol::messages::AUDIO_STREAM_RESULT,
        )
        .await
        {
            if let Some(agent) = microphone_agent.take() {
                shutdown_microphone_agent(agent).await;
            }
            capenc.shutdown().await;
            return AttachmentEnd::terminal(SessionEndReason::WriterEnded);
        }
    }
    let (audio_failure_tx, mut audio_failure_rx) = mpsc::channel(1);
    let audio_encoder = Arc::new(Mutex::new(audio_encoder));
    let mut audio_session = if initial_audio_stream.is_enabled() {
        match audiocap::spawn(
            audio_config
                .clone()
                .expect("enabled audio stream requires capture configuration"),
            Arc::clone(&audio_queue),
            Arc::clone(&audio_encoder),
            audio_failure_tx.clone(),
        ) {
            Ok(session) => {
                info!(
                    target: AUDIO,
                    event = "audio_output_capture_started",
                    sid = %session_log_id,
                    capture_backend = "pulseaudio_monitor",
                    codec = ?initial_audio_stream.codec,
                    fallback_reason = audio_fallback_reason.unwrap_or("none"),
                    "audio output capture started"
                );
                Some(session)
            }
            Err(error) => {
                warn!(target: AUDIO, %error, "native audiocap spawn failed; continuing without audio");
                initial_audio_stream = ResolvedAudioStream::disabled(
                    initial_audio_stream.mode,
                    arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
                );
                audio_encoder
                    .lock()
                    .expect("audio encoder poisoned")
                    .reconfigure(initial_audio_stream)
                    .expect("disabling audio is infallible");
                if let Some(result) = initial_audio_stream.result() {
                    let result = serde_json::to_string(&result)
                        .expect("AudioStreamResultMsg must serialize");
                    let _ = send_critical_control(
                        &mut sink,
                        Message::Text(result),
                        arcen_protocol::messages::AUDIO_STREAM_RESULT,
                    )
                    .await;
                }
                None
            }
        }
    } else {
        None
    };
    let audio_stats = Arc::new(RwLock::new(
        audio_session.as_ref().map(|session| session.stats()),
    ));
    let audio_control = AudioControl::new(
        cfg.audio_enabled,
        cfg.audio_compressed,
        audio_config.is_some(),
        client_hello.audio_output.clone(),
        initial_audio_stream,
    );
    let audio_runtime = Arc::new(tokio::sync::Mutex::new(audio_session.take()));

    // Media, display, input, and audio setup have all completed and
    // `server_hello` was delivered: the stream is now truly active.
    emit_session_stream_start(
        emitter,
        session_log_id.clone(),
        &media_plan,
        session_user.as_deref(),
        Some(remote_host),
    );
    info!(
        target: MEDIA,
        event = "media_plan_resolved",
        sid = %session_log_id,
        requested_codec = %initial_quality.codec,
        requested_fps = initial_quality.max_fps,
        requested_bandwidth_mbps = initial_quality.max_bandwidth_mbps,
        resolved_codec = media_plan.codec_token(),
        resolved_encoder_backend = media_plan.backend.ready_token(),
        resolved_width = media_plan.width,
        resolved_height = media_plan.height,
        resolved_fps = media_plan.fps,
        ready = true,
        idr_requested = attachment_requires_fresh_idr(force_fresh_idr),
        "media plan active"
    );
    let session_started_at = Instant::now();

    // Host-side health assessment for this session: hysteresis over shared,
    // pure `arcen_telemetry::{assess_health, HealthTracker}`, fed by the
    // dispatcher's `HEALTH_PING.client_telemetry` and the health beat's host
    // counters below. Seeded from the shared, SIGHUP-reloadable cell rather
    // than `cfg`'s startup snapshot, so a session started before a reload
    // still begins with the latest validated thresholds.
    let session_health = Arc::new(Mutex::new(crate::observability::SessionHealth::new(
        qos_targets
            .read()
            .map(|targets| *targets)
            .unwrap_or(cfg.logging.qos_targets),
    )));

    // One-time bounded network-path probe (sysfs/procfs; no packet capture,
    // no SSID/RSSI disclosure). Absent entirely when no usable interface is
    // found, rather than reporting fabricated facts. The result seeds the
    // health tick's own off-hot-path re-probe below (f5) so a later
    // change/loss/restoration is diffed against the path actually active at
    // session start, not a stale default.
    let initial_network = crate::netinfo::snapshot(remote_host, remote_scope_id);
    if let Some(network) = &initial_network {
        let context = emitter.session_context(
            session_log_id.clone(),
            session_user.clone(),
            Some(remote_host.to_string()),
            None,
        );
        crate::emit_lifecycle_event_with_context(
            emitter,
            LifecycleEventKind::NetworkPathActive,
            context,
            crate::netinfo::lifecycle_fields(network),
        );
    }

    // 4. Backpressure queue + control channel (health_pong / Pong).
    let queue = Arc::new(FrameQueue::new(idr.clone()));
    // Every applied monitor's own `IdrRequester`: just the primary's for a
    // legacy single-monitor session (unchanged behavior), or primary +
    // every secondary's for a committed multi-monitor plan (populated
    // below, alongside `mux_queues`, before `multi_monitor_secondary_sources`
    // is consumed). A client-triggered `request_full_frame` — or any other
    // whole-session recovery/discontinuity trigger — requests a fresh IDR
    // from every one of these exactly once, not only the primary's.
    let mut all_monitor_idrs: Vec<IdrRequester> = vec![idr.clone()];
    let clipboard_queue = ClipboardWriterQueue::new();
    if clipboard_agent.is_none() {
        clipboard_queue.close();
    }
    let (ctrl_tx, ctrl_rx) = ControlSender::channel(CONTROL_CHANNEL_CAPACITY);

    // Carrier A tags every wire frame with its owning session monitor id;
    // the plan's primary monitor id (or the legacy `0` for a single-monitor
    // session) — never the plan-roster position — is what
    // `media::build_video_frame` stamps into `VideoHeader.monitor_id`.
    let primary_monitor_id = multi_monitor_committed
        .map(|(plan, _carrier)| plan.primary().session_monitor_id.get())
        .unwrap_or(0);
    let region_topology_generation = multi_monitor_committed
        .map(|(plan, _carrier)| plan.generation.get())
        .unwrap_or(0);

    // Mid-session resize requests flow dispatcher → supervisor through a
    // watch channel: latest-wins coalescing, no queue growth during storms.
    let (resize_tx, mut resize_rx) = tokio::sync::watch::channel::<Option<ResizeRequest>>(None);

    // 3b. Sender: owns the sink; drains the queue + forwards control messages.
    //
    // Carrier A (multi-monitor): every other applied monitor gets its own
    // queue here, muxed together with the primary's queue — the mux is
    // fully built, with every monitor's queue in it, before *any* pump
    // (primary's or any secondary's) is spawned. Every pump, once spawned,
    // gets the resulting `Arc<MonitorMux>` passed in directly as its own
    // `on_ended` hook (see `spawn_frame_pump`): the instant that pump's
    // frame channel ends (pipeline crash or normal end), it calls
    // `MonitorMux::close_and_clear_all` from inside its own task body,
    // before its `JoinHandle` can resolve for any consumer — atomically
    // closing *and clearing any buffered frames from* every monitor's
    // queue, not only the one that ended, no matter which monitor's pump
    // is the one that ends first. That is what actually guarantees no
    // stale, already-buffered frame from a still-nominally-open sibling
    // queue can leak out of `MonitorMux::dequeue` afterward; see that
    // method's doc comment for the ordering hazard a plain single-queue
    // `FrameQueue::close` cannot close on its own. There is deliberately no
    // detached "watcher" task for any monitor any more — every pump's own
    // task body is the sole place that ever calls `close_and_clear_all` for
    // it, so the guarantee is identical and inline for all of them. Note
    // this is strictly a superset of the single-monitor legacy guarantee:
    // that path's only pump also gets `on_ended: None` unchanged (no mux
    // exists to tear down). `mux_handle` below keeps the same mux reachable
    // from the post-`select!`-loop teardown code too, so a
    // sender/dispatcher/owner-command/shutdown-triggered end (i.e. nothing
    // to do with any one pump) still tears every monitor down the same way.
    let mut mux_handle: Option<Arc<MonitorMux>> = None;
    // Only used for a committed multi-monitor plan: every secondary's
    // pump-spawn inputs, held here until the mux exists so no secondary
    // pump is ever spawned before it.
    struct PendingSecondaryPump {
        plan: ResolvedMediaPlan,
        frames: mpsc::Receiver<crate::media::annexb::AccessUnit>,
        queue: Arc<FrameQueue>,
        monitor_id: u16,
        topology_generation: u64,
        stream_epoch: u64,
    }
    let video_source = match multi_monitor_committed {
        None => VideoSource::Single(queue.clone()),
        Some((plan, _carrier)) => {
            let mut mux_queues: Vec<(SessionMonitorId, Arc<FrameQueue>)> =
                vec![(plan.primary().session_monitor_id, queue.clone())];
            let mut pending_secondaries: Vec<PendingSecondaryPump> = Vec::new();
            for source in multi_monitor_secondary_sources {
                all_monitor_idrs.push(source.idr.clone());
                let secondary_queue = Arc::new(FrameQueue::new(source.idr));
                mux_queues.push((source.session_monitor_id, secondary_queue.clone()));
                pending_secondaries.push(PendingSecondaryPump {
                    plan: source.plan,
                    frames: source.frames,
                    queue: secondary_queue,
                    monitor_id: source.session_monitor_id.get(),
                    topology_generation: plan.generation.get(),
                    stream_epoch: plan.generation.get(),
                });
            }
            match MonitorMux::new(mux_queues) {
                Ok(mux) => {
                    let mux = Arc::new(mux);
                    for pending in pending_secondaries {
                        // Fire-and-forget: this pump's own task body (via
                        // `on_ended`) is now the only thing responsible for
                        // tearing the mux down when it ends, so nothing
                        // outside needs to hold or await its `JoinHandle` —
                        // dropping it here does not cancel or detach the
                        // spawned task; it keeps running to completion
                        // exactly as it always did.
                        spawn_frame_pump(
                            pending.plan,
                            pending.frames,
                            pending.queue,
                            None,
                            pending.monitor_id,
                            pending.topology_generation,
                            pending.stream_epoch,
                            Some(Arc::clone(&mux)),
                        );
                    }
                    mux_handle = Some(Arc::clone(&mux));
                    VideoSource::Muxed(mux)
                }
                Err(error) => {
                    warn!(
                        target: MEDIA,
                        %error,
                        "multi-monitor mux construction failed — closing connection"
                    );
                    capenc.shutdown().await;
                    return AttachmentEnd::terminal(SessionEndReason::MediaEnded);
                }
            }
        }
    };

    // 3a. Frame pump: capenc AUs → wire frames → queue. Respawned together
    // with capenc on a mid-session stream resize (single-monitor only;
    // structurally inert for multi-monitor — see `resize_supported` above).
    //
    // Spawned only now, after `mux_handle` above is known: a committed
    // multi-monitor plan's primary pump gets `mux_handle.clone()` as its
    // `on_ended` hook, so it gets *exactly the same* immediate,
    // inline-in-its-own-task-body atomic-teardown guarantee every secondary
    // pump above now gets too — the instant *this* (root) monitor's
    // pipeline ends, not only once the outer `select!` loop below happens
    // to notice `pump` completed and falls through to the shared post-loop
    // teardown code. That hook runs from inside this pump's own completing
    // task body (see `spawn_frame_pump`): the `JoinHandle` this function
    // returns cannot resolve `Ready` for any consumer (`sender_loop`
    // included) until that call has already finished running, since it is
    // sequenced before this same task's return.
    let mut pump = spawn_frame_pump(
        media_plan,
        frames_rx,
        queue.clone(),
        None,
        primary_monitor_id,
        region_topology_generation,
        region_topology_generation,
        mux_handle.clone(),
    );
    let sender_audio = audio_queue.clone();
    let sender_clipboard = Arc::clone(&clipboard_queue);
    let sender_audio_control = audio_control.clone();
    let audio_metrics = audio_control.clone();
    let loss_clock = refresh
        .as_ref()
        .map(|refresh| Arc::clone(&refresh.session_registry));
    let write_timeout = refresh
        .as_ref()
        .map(|refresh| resumable_write_timeout(refresh.window_secs))
        .unwrap_or(WS_SEND_TIMEOUT);
    let mut sender = tokio::spawn(
        sender_loop(
            sink,
            video_source,
            sender_audio,
            sender_clipboard,
            ctrl_rx,
            sender_audio_control,
            refresh,
            write_timeout,
        )
        .instrument(tracing::Span::current()),
    );
    let mut microphone_frames = microphone_agent
        .as_ref()
        .map(crate::microphone_input::MicrophoneAgent::frame_sender);
    let mut microphone_events = microphone_agent
        .as_ref()
        .map(crate::microphone_input::MicrophoneAgent::events);
    let mut microphone_ingress = microphone_stream
        .codec
        .map(|codec| MicrophoneIngressValidator::new(codec, microphone_generation));
    let microphone_transport_started_at = Instant::now();
    let microphone_transport_stats = Arc::new(Mutex::new(MicrophoneStatsTracker::default()));
    let microphone_transport_summary_reported = Arc::new(AtomicBool::new(false));
    let microphone_transport_telemetry =
        spawn_linux_transport_telemetry(session_log_id.clone(), microphone_generation);

    // 3c. Dispatcher: inbound control messages.
    //
    // Carrier A: `all_monitor_idrs` holds every applied monitor's own
    // `IdrRequester` (primary + every secondary); a legacy single-monitor
    // session's copy holds just the one, so `request_full_frame` behaves
    // exactly as before for it. See `handle_control_json`'s
    // `REQUEST_FULL_FRAME` arm for the fanout.
    let disp_idrs = all_monitor_idrs;
    let disp_ctrl = ctrl_tx.clone();
    let disp_plan = media_plan;
    let disp_session_health = Arc::clone(&session_health);
    let disp_session_log_id = session_log_id.clone();
    let disp_timezone = authoritative_timezone;
    let disp_started_at = session_started_at;
    let disp_clipboard = clipboard_agent;
    let disp_clipboard_queue = Arc::clone(&clipboard_queue);
    let disp_audio_control = audio_control;
    let disp_audio_encoder = Arc::clone(&audio_encoder);
    let disp_audio_queue = Arc::clone(&audio_queue);
    let disp_audio_config = audio_config;
    let disp_audio_runtime = Arc::clone(&audio_runtime);
    let disp_audio_stats = Arc::clone(&audio_stats);
    let disp_audio_failure_tx = audio_failure_tx;
    let input_controller = Arc::new(Mutex::new(input_controller));
    let disp_input = Arc::clone(&input_controller);
    let disp_resize_tx = resize_tx;
    let disp_microphone_stats = Arc::clone(&microphone_transport_stats);
    let disp_microphone_summary_reported = Arc::clone(&microphone_transport_summary_reported);
    let disp_microphone_telemetry = microphone_transport_telemetry.clone();
    let disp_usb_bridge = usb_bridge.take();
    // SEC-raw-hid. Mutual, explicit negotiation for the quarantined
    // experimental raw-HID passthrough: this host must have its own runtime
    // opt-in AND the connecting client must have explicitly asserted the
    // capability in ClientHello. An old/unknown client (field absent, so
    // `false`) or a host without its own opt-in never activates this,
    // regardless of what the other side claims.
    #[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
    let disp_raw_hid_permitted =
        client_hello.experimental_raw_hid && crate::input::experimental_raw_hid_runtime_enabled();
    let disp_cursor_shape_rx = cursor_shape_rx;
    let dispatcher_span = tracing::Span::current();
    let mut dispatcher = tokio::spawn(
        async move {
            let mut last_display_update_seq: u64 = 0;
            let mut last_full_frame_idr = Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now);
            let mut input_sequence = InputSequenceTracker::default();
            let mut clipboard_agent = disp_clipboard;
            let mut deck_reassembler = clipboard_negotiation
                .and_then(|negotiation| {
                    arcen_protocol::clipboard::ClipboardReassembler::new(
                        negotiation.policy().max_bytes,
                    )
                    .ok()
                });
            let mut agent_reassembler = clipboard_negotiation
                .and_then(|negotiation| {
                    arcen_protocol::clipboard::ClipboardReassembler::new(
                        negotiation.policy().max_bytes,
                    )
                    .ok()
                });
            let mut clipboard_expiry = tokio::time::interval(Duration::from_secs(1));
            let mut microphone_stats_tick = tokio::time::interval(MICROPHONE_STATS_INTERVAL);
            clipboard_expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            microphone_stats_tick
                .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            microphone_stats_tick.tick().await;
            let mut microphone_order_warning =
                crate::microphone_input::RateLimitedMicrophoneWarning::default();
            let mut microphone_protocol_warning =
                crate::microphone_input::RateLimitedMicrophoneWarning::default();
            let mut microphone_codec_warning =
                crate::microphone_input::RateLimitedMicrophoneWarning::default();
            let mut microphone_authorization_warning =
                crate::microphone_input::RateLimitedMicrophoneWarning::default();
            // Experimental raw-HID passthrough (quarantined): virtual devices
            // keyed by the client-assigned device_id. Only compiled in with
            // `experimental-raw-hid`; still requires `disp_raw_hid_permitted`
            // at runtime before anything is admitted. Not USB bridging.
            #[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
            let mut hid_devices: std::collections::HashMap<u8, crate::input::UhidDevice> =
                std::collections::HashMap::new();
            let mut usb_bridge = disp_usb_bridge;
            let mut cursor_shape_rx = disp_cursor_shape_rx;
            let reason = loop {
                tokio::select! {
                    _ = clipboard_expiry.tick() => {
                        if let Some(reassembler) = deck_reassembler.as_mut() {
                            let _ = reassembler.expire(Instant::now());
                        }
                        if let Some(reassembler) = agent_reassembler.as_mut() {
                            let _ = reassembler.expire(Instant::now());
                        }
                    }
                    cursor_json = async { cursor_shape_rx.as_mut()?.recv().await },
                        if cursor_shape_rx.is_some() =>
                    {
                        match cursor_json {
                            Some(json) => {
                                disp_ctrl.send_best_effort(Message::Text(json), "cursor_shape");
                            }
                            None => {
                                // Watcher thread exited (X11 connection dropped or
                                // session ended) — stop polling.
                                cursor_shape_rx = None;
                            }
                        }
                    }
                    _ = microphone_stats_tick.tick(), if microphone_ingress.is_some() => {
                        let stats = lock_recover(&disp_microphone_stats).take_interval();
                        disp_microphone_telemetry.try_snapshot(
                            stats,
                            false,
                            microphone_transport_started_at.elapsed(),
                            "running",
                        );
                    }
                    agent_message = next_clipboard_agent_message(&mut clipboard_agent) => {
                        match agent_message {
                            Some(Ok(message)) => handle_agent_clipboard_message(
                                message,
                                clipboard_negotiation,
                                agent_reassembler.as_mut(),
                                &disp_clipboard_queue,
                            ),
                            Some(Err(error)) => {
                                warn!(target: SESSION, %error, "clipboard agent IPC failed");
                                break SessionEndReason::TransportError;
                            }
                            None if clipboard_agent.is_some() => {
                                break SessionEndReason::TransportError;
                            }
                            None => {}
                        }
                    }
                    codec_failure = audio_failure_rx.recv() => {
                        if let Some(reason) = codec_failure {
                            if !disable_audio_after_codec_failure(
                                reason,
                                &disp_ctrl,
                                &disp_audio_control,
                                &disp_audio_encoder,
                                &disp_audio_queue,
                                &disp_audio_runtime,
                            ).await {
                                break SessionEndReason::WriterEnded;
                            }
                        }
                    }
                    microphone_event = next_microphone_agent_event(&mut microphone_events) => {
                        match microphone_event {
                            Some(crate::microphone_input::MicrophoneAgentEvent::Failed) => {
                                microphone_events = None;
                                microphone_ingress = None;
                                if let Some(frames) = microphone_frames.take() {
                                    frames.stop();
                                }
                                warn!(target: AUDIO, "microphone helper exited unexpectedly");
                                if !send_microphone_failure(&disp_ctrl, microphone_generation).await {
                                    break SessionEndReason::WriterEnded;
                                }
                            }
                            Some(crate::microphone_input::MicrophoneAgentEvent::Stopped) => {
                                microphone_events = None;
                                microphone_ingress = None;
                                microphone_frames = None;
                            }
                            _ => {}
                        }
                    }
                    usb_frame = async {
                        match usb_bridge.as_mut() {
                            Some(bridge) => Some(bridge.next_frame().await),
                            None => None,
                        }
                    }, if usb_bridge.is_some() => {
                        match usb_frame {
                            Some(Ok(frame)) => {
                                if !disp_ctrl
                                    .send_required(Message::Binary(frame), "usb_bridge_urb")
                                    .await
                                {
                                    break SessionEndReason::WriterEnded;
                                }
                            }
                            Some(Err(error)) => {
                                let diagnostic = usb_bridge
                                    .as_mut()
                                    .map(|bridge| bridge.diagnostic());
                                let diagnostic = match diagnostic {
                                    Some(diagnostic) => diagnostic.await,
                                    None => String::new(),
                                };
                                warn!(
                                    target: NET,
                                    %error,
                                    helper_diagnostic = %diagnostic,
                                    "Hard USB helper IPC failed"
                                );
                                break SessionEndReason::TransportError;
                            }
                            None => {}
                        }
                    }
                    deck_message = next_with_liveness(&mut stream, READ_LIVENESS_TIMEOUT) => {
                        let msg = match deck_message {
                            Ok(Some(message)) => message,
                            Ok(None) => break SessionEndReason::TransportError,
                            Err(_) => {
                                warn!(target: NET, "client read-liveness timeout");
                                break SessionEndReason::ReadLivenessTimeout;
                            }
                        };
                        match msg {
                    Ok(Message::Text(text)) => {
                        let microphone_stop = serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .filter(|value| msg_type(value) == Some(MICROPHONE_STREAM_STOP));
                        if let Some(value) = microphone_stop {
                            let stop =
                                serde_json::from_value::<MicrophoneStreamStopMsg>(value);
                            match stop {
                                Ok(stop)
                                    if stop.is_valid()
                                        && stop.generation == microphone_generation =>
                                {
                                    if let Some(frames) = microphone_frames.take() {
                                        frames.stop();
                                    }
                                    microphone_ingress = None;
                                    info!(
                                        target: AUDIO,
                                        reason = ?stop.reason,
                                        "client stopped microphone publication"
                                    );
                                }
                                _ => break SessionEndReason::ProtocolError,
                            }
                        } else if is_clipboard_data_message(&text) {
                            handle_deck_clipboard_offer(
                                &text,
                                clipboard_negotiation,
                                deck_reassembler.as_mut(),
                            );
                        } else if let Some(update) = parse_display_update(&text) {
                            if !resize_supported {
                                send_display_update_result(
                                    &disp_ctrl,
                                    update.sequence,
                                    false,
                                    disp_plan.width,
                                    disp_plan.height,
                                    display_update_rejection_message(display_mode),
                                );
                            } else if update.sequence <= last_display_update_seq {
                                debug!(
                                    target: DISPLAY,
                                    sequence = update.sequence,
                                    "stale display_update ignored"
                                );
                            } else {
                                last_display_update_seq = update.sequence;
                                let _ = disp_resize_tx.send(Some(ResizeRequest {
                                    sequence: update.sequence,
                                    width: update.width,
                                    height: update.height,
                                    reason: update.reason,
                                }));
                            }
                        } else {
                            if let Ok(quality) = serde_json::from_str::<QualitySettings>(&text) {
                                if quality.msg_type == "quality_settings" {
                                    if !apply_audio_quality(
                                        quality,
                                        &disp_ctrl,
                                        &disp_audio_control,
                                        &disp_audio_encoder,
                                        &disp_audio_queue,
                                        disp_audio_config.as_ref(),
                                        &disp_audio_runtime,
                                        &disp_audio_stats,
                                        &disp_audio_failure_tx,
                                    ).await {
                                        break SessionEndReason::WriterEnded;
                                    }
                                }
                            }
                            let mut input = lock_recover(&disp_input);
                            handle_control_json(
                                &text,
                                &disp_idrs,
                                &disp_ctrl,
                                &disp_plan,
                                &disp_session_log_id,
                                disp_timezone.as_ref(),
                                input.as_mut(),
                                &mut last_full_frame_idr,
                                &mut input_sequence,
                                disp_plan.cursor_mode,
                                &disp_session_health,
                                disp_started_at,
                            );
                        }
                    }
                    Ok(Message::Binary(bytes)) => {
                        match bytes.first().copied().and_then(|b| FrameType::try_from(b).ok()) {
                            Some(FrameType::AudioUpstream) => {
                                lock_recover(&disp_microphone_stats).record_received(bytes.len());
                                let wire_bytes = bytes.len();
                                if let (Some(frames), Some(ingress)) =
                                    (microphone_frames.as_ref(), microphone_ingress.as_mut())
                                {
                                    match ingress.validate(&bytes) {
                                        Ok(()) => match frames.try_send(bytes) {
                                            Ok(()) => {
                                                lock_recover(&disp_microphone_stats).record_ingest(
                                                    MicrophoneIngestOutcome::Accepted,
                                                    wire_bytes,
                                                );
                                            }
                                            Err(crate::microphone_input::MicrophoneSendError::Full) => {
                                                lock_recover(&disp_microphone_stats)
                                                    .record_transport_backpressure_drop();
                                            }
                                            Err(crate::microphone_input::MicrophoneSendError::Closed) => {
                                                lock_recover(&disp_microphone_stats)
                                                    .record_backend_underrun();
                                                microphone_ingress = None;
                                                if let Some(frames) = microphone_frames.take() {
                                                    frames.stop();
                                                }
                                                warn!(
                                                    target: AUDIO,
                                                    event = "mic_linux_helper_failure",
                                                    sid = %disp_session_log_id,
                                                    backend = "pulseaudio_pipe_source",
                                                    reason = "ipc_closed",
                                                    "microphone helper IPC closed"
                                                );
                                                if !send_microphone_failure(
                                                    &disp_ctrl,
                                                    microphone_generation,
                                                )
                                                .await
                                                {
                                                    break SessionEndReason::WriterEnded;
                                                }
                                            }
                                        },
                                        Err(rejection) => {
                                            let terminate_stream = rejection.terminates_stream();
                                            match rejection {
                                                MicrophoneIngressRejection::Sequence(
                                                    MicrophoneFrameDecision::Duplicate,
                                                ) => lock_recover(&disp_microphone_stats)
                                                    .record_ingest(
                                                    MicrophoneIngestOutcome::DroppedDuplicate,
                                                    0,
                                                ),
                                                MicrophoneIngressRejection::Sequence(
                                                    MicrophoneFrameDecision::Late,
                                                ) => lock_recover(&disp_microphone_stats)
                                                    .record_ingest(
                                                    MicrophoneIngestOutcome::DroppedLate,
                                                    0,
                                                ),
                                                MicrophoneIngressRejection::Sequence(
                                                    MicrophoneFrameDecision::WrongGeneration,
                                                ) => lock_recover(&disp_microphone_stats)
                                                    .record_ingest(
                                                    MicrophoneIngestOutcome::DroppedWrongGeneration,
                                                    0,
                                                ),
                                                MicrophoneIngressRejection::Sequence(
                                                    MicrophoneFrameDecision::Discontinuity,
                                                ) => lock_recover(&disp_microphone_stats)
                                                    .record_ingest(
                                                    MicrophoneIngestOutcome::RejectedDiscontinuity,
                                                    0,
                                                ),
                                                MicrophoneIngressRejection::Malformed
                                                | MicrophoneIngressRejection::Codec
                                                | MicrophoneIngressRejection::Sequence(
                                                    MicrophoneFrameDecision::First
                                                    | MicrophoneFrameDecision::OnTime
                                                    | MicrophoneFrameDecision::Gap { .. },
                                                ) => lock_recover(&disp_microphone_stats)
                                                    .record_decoder_error(),
                                            }
                                            let warning = match rejection {
                                                MicrophoneIngressRejection::Malformed => {
                                                    &mut microphone_protocol_warning
                                                }
                                                MicrophoneIngressRejection::Codec => {
                                                    &mut microphone_codec_warning
                                                }
                                                MicrophoneIngressRejection::Sequence(_) => {
                                                    &mut microphone_order_warning
                                                }
                                            };
                                            if let Some(suppressed) = warning.observe() {
                                                warn!(
                                                    target: AUDIO,
                                                    event = "mic_frame_rejected",
                                                    sid = %disp_session_log_id,
                                                    generation = microphone_generation,
                                                    reason = ?rejection,
                                                    suppressed_since_last = suppressed,
                                                    "Linux microphone frame rejected"
                                                );
                                            }
                                            if terminate_stream {
                                                microphone_ingress = None;
                                                microphone_events = None;
                                                if let Some(frames) = microphone_frames.take() {
                                                    frames.stop();
                                                }
                                                warn!(
                                                    target: AUDIO,
                                                    "microphone ordering discontinuity terminated the negotiated stream"
                                                );
                                                if !send_microphone_failure(
                                                    &disp_ctrl,
                                                    microphone_generation,
                                                )
                                                .await
                                                {
                                                    break SessionEndReason::WriterEnded;
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    if let Some(suppressed) =
                                        microphone_authorization_warning.observe()
                                    {
                                        warn!(
                                            target: AUDIO,
                                            event = "mic_frame_rejected",
                                            sid = %disp_session_log_id,
                                            generation = microphone_generation,
                                            reason = "not_negotiated",
                                            suppressed_since_last = suppressed,
                                            "unauthorized microphone frame dropped"
                                        );
                                    }
                                }
                            }
                            Some(FrameType::Clipboard) => {
                                if let Some(item) = handle_deck_clipboard_binary(
                                    &bytes,
                                    clipboard_negotiation,
                                    deck_reassembler.as_mut(),
                                ) {
                                    if let Some(agent) = clipboard_agent.as_mut() {
                                        if let Err(error) = send_clipboard_item(agent, item).await {
                                            warn!(target: SESSION, %error, "clipboard agent injection IPC failed");
                                            break SessionEndReason::TransportError;
                                        }
                                    }
                                }
                            }
                            #[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
                            Some(FrameType::HidDeviceAdded) => {
                                use arcen_protocol::decode_hid_device_added;
                                if !disp_raw_hid_permitted {
                                    warn!(
                                        target: NET,
                                        event = "raw_hid_rejected",
                                        reason = "not_negotiated",
                                        "experimental raw-HID frame rejected: capability not negotiated"
                                    );
                                } else if let Ok((hdr, descriptor)) = decode_hid_device_added(&bytes) {
                                    if !crate::input::is_experimental_raw_hid_vendor(hdr.vendor_id) {
                                        warn!(
                                            target: NET,
                                            event = "raw_hid_rejected",
                                            reason = "vendor_not_allowed",
                                            vid = hdr.vendor_id,
                                            "experimental raw-HID device rejected: vendor not on the allow-list"
                                        );
                                    } else if hid_devices.len() >= MAX_EXPERIMENTAL_RAW_HID_DEVICES {
                                        warn!(
                                            target: NET,
                                            event = "raw_hid_rejected",
                                            reason = "device_limit",
                                            "experimental raw-HID device rejected: per-session device limit reached"
                                        );
                                    } else {
                                        let name = format!("Arcen HID {:04x}:{:04x}", hdr.vendor_id, hdr.product_id);
                                        match crate::input::UhidDevice::create(
                                            &name,
                                            hdr.vendor_id,
                                            hdr.product_id,
                                            descriptor,
                                            hdr.device_id,
                                        ) {
                                            Ok(dev) => {
                                                debug!(target: NET, device_id = hdr.device_id, vid = hdr.vendor_id, pid = hdr.product_id, "uhid device created");
                                                hid_devices.insert(hdr.device_id, dev);
                                            }
                                            Err(e) => warn!(target: NET, %e, "uhid device creation failed"),
                                        }
                                    }
                                }
                            }
                            #[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
                            Some(FrameType::HidReport) => {
                                use arcen_protocol::decode_hid_report;
                                if disp_raw_hid_permitted {
                                    if let Ok((device_id, report)) = decode_hid_report(&bytes) {
                                        if let Some(dev) = hid_devices.get(&device_id) {
                                            if let Err(e) = dev.write_report(report) {
                                                warn!(target: NET, %e, device_id, "uhid report write failed");
                                            }
                                        }
                                    }
                                }
                            }
                            #[cfg(all(target_os = "linux", feature = "experimental-raw-hid"))]
                            Some(FrameType::HidDeviceRemoved) => {
                                use arcen_protocol::decode_hid_device_removed;
                                if let Ok(device_id) = decode_hid_device_removed(&bytes) {
                                    hid_devices.remove(&device_id);
                                    debug!(target: NET, device_id, "uhid device removed");
                                }
                            }
                            Some(FrameType::UsbBridgeUrbComplete) => {
                                let Some(bridge) = usb_bridge.as_mut() else {
                                    break SessionEndReason::ProtocolError;
                                };
                                if let Err(error) = bridge.send_frame(&bytes).await {
                                    warn!(target: NET, %error, "Hard USB completion IPC failed");
                                    break SessionEndReason::TransportError;
                                }
                            }
                            _ => {
                                trace!(target: NET, "ignoring unexpected binary frame from client")
                            }
                        }
                    }
                    Ok(Message::Ping(p)) => {
                        disp_ctrl.send_best_effort(Message::Pong(p), "ws_pong");
                    }
                    Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                    Ok(Message::Close(_)) => {
                        debug!(target: NET, "client sent close");
                        break SessionEndReason::ClientClosed;
                    }
                    Err(e) => {
                        warn!(target: NET, error = %e, "ws read error");
                        break SessionEndReason::TransportError;
                    }
                }
                    }
                }
            };
            if let Some(bridge) = usb_bridge.take() {
                bridge.shutdown().await;
            }
            if !disp_microphone_summary_reported.swap(true, Ordering::AcqRel) {
                disp_microphone_telemetry.try_snapshot(
                    lock_recover(&disp_microphone_stats).total(),
                    true,
                    microphone_transport_started_at.elapsed(),
                    microphone_stop_reason(reason),
                );
            }
            debug!(target: NET, ?reason, "dispatcher task ended");
            reason
        }
        .instrument(dispatcher_span),
    );

    // 3d. Health beat: periodic health_pong (server_state), like health_loop.py.
    let health_ctrl = ctrl_tx.clone();
    let health_span = tracing::Span::current();
    let health_queue = queue.clone();
    let health_input_stats = input_stats.clone();
    let health_session_health = Arc::clone(&session_health);
    let health_emitter = emitter.clone();
    let health_session_log_id = session_log_id.clone();
    let health_media_plan = media_plan;
    let health_service_health = service_health;
    let health_qos_targets = Arc::clone(&qos_targets);
    let health_started_at = session_started_at;
    let health_user = session_user.clone();
    let health_remote_host = remote_host.to_string();
    let health_remote_scope_id = remote_scope_id;
    let health_initial_network = initial_network;
    let health = tokio::spawn(
        async move {
            let mut seq: u64 = 0;
            let mut ticker = tokio::time::interval(HEALTH_BEAT);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // consume the immediate first tick
                                 // Five-sample (10s) aggregation window for a Level2 (Info)
                                 // summary; the existing per-tick Level3 (Debug) line below keeps
                                 // its 2s detail unchanged.
            let mut window_frames_sent: u64 = 0;
            let mut window_frames_dropped: u64 = 0;
            let mut window_input_events: u64 = 0;
            let mut window_worst_overall: Option<arcen_telemetry::HealthState> = None;
            let mut window_ticks: u32 = 0;
            // Off-hot-path network-path monitor (f5): re-probed only at the
            // same bounded 10s cadence as the Level2 aggregation window
            // below, never per-tick/per-frame. Tracks the last known path so
            // a change/loss/restoration is only ever emitted on an actual
            // transition, and the lost-at timestamp so `NETWORK_PATH_RESTORED`
            // can report a real outage duration.
            let mut network_last = health_initial_network;
            let mut network_lost_at_ms: Option<u64> = None;
            loop {
                ticker.tick().await;
                seq += 1;
                let pong = HealthPongMsg {
                    msg_type: HEALTH_PONG.to_string(),
                    ping_timestamp_ms: 0,
                    sequence: seq,
                    server_timestamp_ms: now_ms_u64(),
                    server_state: "streaming".to_string(),
                };
                match serde_json::to_string(&pong) {
                    Ok(j) => {
                        health_ctrl.send_best_effort(Message::Text(j), "health_beat");
                    }
                    Err(_) => break,
                }
                if seq % 5 == 0 {
                    health_ctrl.send_best_effort(
                        Message::Ping(seq.to_be_bytes().to_vec()),
                        "liveness_ping",
                    );
                }

                // Aggregate atomics only — no per-frame/per-input logging;
                // this is the existing 2s cadence, not a hot loop.
                let input_events = health_input_stats.as_ref().map_or(0, |stats| {
                    stats
                        .key_events()
                        .saturating_add(stats.mouse_moves())
                        .saturating_add(stats.mouse_buttons())
                        .saturating_add(stats.scroll_events())
                        .saturating_add(stats.pen_events())
                });
                let counters = crate::observability::HostCounters {
                    frames_sent: health_queue.frames_sent(),
                    frames_dropped: health_queue.frames_dropped(),
                    bytes_sent: health_queue.bytes_sent(),
                    input_events,
                    // Not currently tracked by `InputController`/`InputStats`;
                    // a documented scoped gap rather than a fabricated value.
                    last_input_sequence: 0,
                    last_input_type: "",
                };
                // SIGHUP may have replaced the validated thresholds since the
                // last tick; apply the shared cell's current value before
                // assessing so an active session never keeps stale limits.
                if let Ok(targets) = health_qos_targets.read() {
                    lock_recover(&health_session_health).set_targets(*targets);
                } else {
                    warn!(
                        target: HEALTH,
                        sid = %health_session_log_id,
                        "qos_targets shared cell is poisoned; keeping session's prior thresholds"
                    );
                }
                // The hysteresis tracker requires a monotonic clock (see
                // `arcen_telemetry::HealthTracker::update`); wall-clock time
                // is reserved for each lifecycle event's own record
                // timestamp (`eventlog::canonical_now`), set independently.
                let monotonic_now_ms =
                    u64::try_from(health_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                let observation = lock_recover(&health_session_health).observe(
                    monotonic_now_ms,
                    health_media_plan.fps,
                    counters,
                );
                if let Some(error) = &observation.clock_error {
                    warn!(
                        target: HEALTH,
                        sid = %health_session_log_id,
                        %error,
                        "health tracker rejected a non-monotonic tick; assessment skipped, \
                         not silently swallowed"
                    );
                }
                let stats = crate::observability::SessionHealth::health_stats(
                    &observation,
                    counters,
                    health_media_plan.fps,
                    health_media_plan.codec_token(),
                    health_media_plan.chroma_token(),
                    format!("{}x{}", health_media_plan.width, health_media_plan.height),
                );
                if let Ok(j) = serde_json::to_string(&stats) {
                    health_ctrl.send_best_effort(Message::Text(j), "health_stats");
                }
                if let Some(state) = observation.assessment.overall {
                    health_service_health.fetch_max(health_state_rank(state), Ordering::Relaxed);
                }
                // Built before any emit below so both the state transition
                // and the 60s snapshot carry the same explicit top-level
                // session identity (`user`/`peer_addr`) — never a nested
                // `sid` field, and never the identity-less default.
                let context = health_emitter.session_context(
                    health_session_log_id.clone(),
                    health_user.clone(),
                    Some(health_remote_host.clone()),
                    observation.assessment.overall,
                );
                if let Some((kind, fields)) = observation.transition {
                    crate::emit_lifecycle_event_with_context(
                        &health_emitter,
                        kind,
                        context.clone(),
                        fields,
                    );
                }
                if observation.snapshot_due {
                    let client_network = lock_recover(&health_session_health)
                        .client()
                        .and_then(|telemetry| telemetry.network.as_ref())
                        .cloned();
                    crate::emit_lifecycle_event_with_context(
                        &health_emitter,
                        LifecycleEventKind::HealthSnapshot,
                        context,
                        crate::observability::snapshot_fields(
                            &observation.sample,
                            &observation.assessment,
                            client_network.as_ref(),
                        ),
                    );
                    // Sink-loss notices are drained exclusively at the
                    // service heartbeat (`emit_service_loss_notices`, called
                    // from `serve`'s own independent timer), not here: loss
                    // is a process-level fact, not a per-session one, and
                    // draining only on a session's own tick could lose a
                    // short session's contribution if it ends before its
                    // first snapshot.
                }
                // Level3 (Debug) only: shows client-experience facts on the
                // host side without altering the client's own profile.
                debug!(
                    target: HEALTH,
                    sid = %health_session_log_id,
                    host_state = ?observation.assessment.host_delivery.state,
                    client_state = ?observation.assessment.client_experience.state,
                    overall_state = ?observation.assessment.overall,
                    fps_actual = ?observation.sample.fps_actual,
                    bandwidth_mbps = observation.bandwidth_mbps,
                    "session health tick"
                );

                // Level2 (Info): a coarser 10s summary over the last five 2s
                // samples, distinct from — and never a substitute for —
                // Level3's per-tick detail above.
                accumulate_health_window(
                    &mut window_frames_sent,
                    &mut window_frames_dropped,
                    &mut window_input_events,
                    &mut window_worst_overall,
                    &observation.sample,
                    observation.assessment.overall,
                );
                window_ticks += 1;
                if window_ticks >= 5 {
                    // Explicit CanonicalRecord/session-context path (finding
                    // #2): top-level sid/user/host/peer_addr, Level2 minimum
                    // profile, and Info severity — never a `tracing`
                    // diagnostic macro, whose fields carry no top-level
                    // identity promotion.
                    let summary_context = health_emitter.session_context(
                        health_session_log_id.clone(),
                        health_user.clone(),
                        Some(health_remote_host.clone()),
                        observation.assessment.overall,
                    );
                    health_emitter.emit_summary(
                        summary_context,
                        TelemetryTarget::new(arcen_telemetry::names::target::HEALTH)
                            .expect("HEALTH is a canonical arcen:: target"),
                        "session health 10s summary",
                        OperationalProfile::Info,
                        crate::observability::window_summary_fields(
                            window_ticks,
                            window_frames_sent,
                            window_frames_dropped,
                            window_input_events,
                            window_worst_overall,
                        ),
                    );
                    window_frames_sent = 0;
                    window_frames_dropped = 0;
                    window_input_events = 0;
                    window_worst_overall = None;
                    window_ticks = 0;

                    // Off-hot-path network-path re-probe (f5): bounded
                    // sysfs/procfs read at the same 10s cadence as the
                    // summary above, diffed against the last known path.
                    let network_context = health_emitter.session_context(
                        health_session_log_id.clone(),
                        health_user.clone(),
                        Some(health_remote_host.clone()),
                        observation.assessment.overall,
                    );
                    let probed =
                        crate::netinfo::snapshot(&health_remote_host, health_remote_scope_id);
                    match (&network_last, &probed) {
                        (Some(old), Some(new)) if old != new => {
                            crate::emit_lifecycle_event_with_context(
                                &health_emitter,
                                LifecycleEventKind::NetworkPathChanged,
                                network_context,
                                crate::netinfo::changed_fields(old, new),
                            );
                        }
                        (Some(old), None) => {
                            crate::emit_lifecycle_event_with_context(
                                &health_emitter,
                                LifecycleEventKind::NetworkPathLost,
                                network_context,
                                crate::netinfo::lost_fields(old),
                            );
                            network_lost_at_ms = Some(monotonic_now_ms);
                        }
                        (None, Some(new)) => {
                            let gap_ms = network_lost_at_ms
                                .map_or(0, |lost_at| monotonic_now_ms.saturating_sub(lost_at));
                            crate::emit_lifecycle_event_with_context(
                                &health_emitter,
                                LifecycleEventKind::NetworkPathRestored,
                                network_context,
                                crate::netinfo::restored_fields(new, gap_ms),
                            );
                            network_lost_at_ms = None;
                        }
                        _ => {}
                    }
                    network_last = probed;
                }
            }
            debug!(target: HEALTH, "health beat ended");
        }
        .instrument(health_span),
    );

    // 4. Run until any half ends — or swap the media pipeline on a resize —
    // then tear down cleanly. A resize is always a `continue`; only the four
    // original completion branches (plus a failed respawn) reach teardown.
    let mut capenc = Some(capenc);
    let mut current_plan = media_plan.clone();
    let mut last_resize_apply: Option<Instant> = None;
    let mut resize_open = true;
    let end_reason = loop {
        tokio::select! {
            r = &mut pump => {
                debug!(target: MEDIA, "pump completed → teardown");
                break r.unwrap_or(SessionEndReason::MediaEnded);
            }
            r = &mut sender => {
                debug!(target: NET, "sender completed → teardown");
                break r.unwrap_or(SessionEndReason::WriterEnded);
            }
            r = &mut dispatcher => {
                debug!(target: NET, "dispatcher completed → teardown");
                break r.unwrap_or(SessionEndReason::TransportError);
            }
            _ = receive_owner_command(owner_commands.as_deref_mut()) => {
                debug!(target: SESSION, "resume owner command ended active attachment");
                break SessionEndReason::HostShutdown;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    debug!(target: SESSION, "service shutdown ended active attachment");
                    break SessionEndReason::HostShutdown;
                }
            }
            changed = resize_rx.changed(), if resize_open => {
                if changed.is_err() {
                    // Dispatcher ended; its completion branch reports why.
                    resize_open = false;
                    continue;
                }
                // Rate limit: at most one applied resize per second. The
                // watch holds only the latest request, so waiting here
                // naturally coalesces resize storms.
                if let Some(last) = last_resize_apply {
                    let since = last.elapsed();
                    if since < RESIZE_MIN_INTERVAL {
                        tokio::time::sleep(RESIZE_MIN_INTERVAL - since).await;
                    }
                }
                let request = resize_rx.borrow_and_update().clone();
                let Some(request) = request else { continue };
                let resolution = match nvctrl::requested_resolution(request.width, request.height) {
                    Ok(Some(resolution))
                        if resolution.width % 4 == 0
                            && resolution.height % 2 == 0
                            && (current_plan.backend != EncoderBackend::OpenH264
                                || (resolution.width <= 1920 && resolution.height <= 1080)) =>
                    {
                        resolution
                    }
                    _ => {
                        warn!(
                            target: DISPLAY,
                            width = request.width,
                            height = request.height,
                            "rejected invalid display_update resolution"
                        );
                        send_display_update_result(
                            &ctrl_tx,
                            request.sequence,
                            false,
                            current_plan.width,
                            current_plan.height,
                            "invalid resolution",
                        );
                        continue;
                    }
                };
                if resolution.width == current_plan.width
                    && resolution.height == current_plan.height
                {
                    if !send_authoritative_display_update_result(
                        &ctrl_tx,
                        request.sequence,
                        current_plan.width,
                        current_plan.height,
                    )
                    .await
                    {
                        break SessionEndReason::WriterEnded;
                    }
                    continue;
                }
                let Some(display_resources) = display.as_mut() else {
                    send_display_update_result(
                        &ctrl_tx,
                        request.sequence,
                        false,
                        current_plan.width,
                        current_plan.height,
                        "no display control",
                    );
                    continue;
                };
                let Some(concrete_encoder) = concrete_encoder_for(current_plan) else {
                    send_display_update_result(
                        &ctrl_tx,
                        request.sequence,
                        false,
                        current_plan.width,
                        current_plan.height,
                        "active backend cannot be recreated on Linux",
                    );
                    continue;
                };
                info!(
                    target: DISPLAY,
                    sequence = request.sequence,
                    width = resolution.width,
                    height = resolution.height,
                    reason = %request.reason,
                    "mid-session stream resize begin"
                );
                // Freeze and clear the old generation before waiting for the
                // writer. Once the barrier completes, no old-size AU is queued
                // or in flight.
                queue.begin_generation();
                pump.abort();
                let _ = (&mut pump).await;
                if !ctrl_tx.barrier().await {
                    break SessionEndReason::WriterEnded;
                }
                if let Some(session) = capenc.take() {
                    session.shutdown().await;
                }

                // Borrowed capture/mapping state is gone before the X screen is
                // retargeted. Failure is terminal because the old media
                // generation has already been retired.
                match display_resources.reassign(resolution).await {
                    Ok(()) => {}
                    Err(error) => {
                        warn!(
                            target: DISPLAY,
                            %error,
                            "mid-session display retarget failed"
                        );
                        send_display_update_result(
                            &ctrl_tx,
                            request.sequence,
                            false,
                            current_plan.width,
                            current_plan.height,
                            &format!("display retarget failed: {error}"),
                        );
                        break SessionEndReason::MediaEnded;
                    }
                }

                // Pin the concrete backend resolved for this attachment. Auto
                // fallback is legal only before hello, never during resize.
                let mut resize_config = active_capenc_config.clone();
                resize_config.width = resolution.width;
                resize_config.height = resolution.height;
                resize_config.encoder = concrete_encoder;
                let (mut session, plan) = match capenc::spawn(resize_config.clone()).await {
                    Ok(spawned) => spawned,
                    Err(error) => {
                        warn!(
                            target: CAPENC,
                            %error,
                            "capenc respawn after resize failed; retrying pinned media contract"
                        );
                        tokio::time::sleep(Duration::from_millis(600)).await;
                        match capenc::spawn(resize_config.clone()).await {
                            Ok(spawned) => {
                                info!(
                                    target: CAPENC,
                                    "capenc retry after resize failure succeeded"
                                );
                                spawned
                            }
                            Err(retry_error) => {
                                warn!(
                                    target: CAPENC,
                                    error = %retry_error,
                                    "capenc retry after resize failed — ending session media"
                                );
                                break SessionEndReason::MediaEnded;
                            }
                        }
                    }
                };
                if plan.width != resolution.width
                    || plan.height != resolution.height
                    || !resize_contract_matches(current_plan, plan)
                {
                    session.shutdown().await;
                    warn!(
                        target: CAPENC,
                        "replacement READY changed geometry or active media contract"
                    );
                    break SessionEndReason::MediaEnded;
                }

                // Every IdrRequester clone (FrameQueue backpressure,
                // dispatcher full-frame requests) follows the new child.
                idr.retarget(&session);
                let frames = session
                    .take_frames()
                    .expect("fresh capenc frames receiver available");
                let (keyframe_ready, keyframe_wait) = oneshot::channel();
                pump = spawn_frame_pump(
                    plan,
                    frames,
                    queue.clone(),
                    Some(keyframe_ready),
                    0,
                    0,
                    0,
                    mux_handle.clone(),
                );
                idr.request();
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(10), keyframe_wait).await,
                    Ok(Ok(()))
                ) {
                    session.shutdown().await;
                    warn!(target: MEDIA, "replacement media generation did not produce an IDR");
                    break SessionEndReason::MediaEnded;
                }
                capenc = Some(CapencHandle::Single(session));

                // Keep the replacement IDR paused until the accepted result is
                // physically written. Deck updates display/input geometry from
                // this control before it can observe a new-size AU.
                if !send_authoritative_display_update_result(
                    &ctrl_tx,
                    request.sequence,
                    plan.width,
                    plan.height,
                )
                .await
                {
                    break SessionEndReason::WriterEnded;
                }
                current_plan = plan;
                active_capenc_config = resize_config;
                if !queue.activate_generation() {
                    tracing::error!(
                        target: MEDIA,
                        "replacement generation lost its pinned recovery AU"
                    );
                    break SessionEndReason::MediaEnded;
                }
                last_resize_apply = Some(Instant::now());
                info!(
                    target: DISPLAY,
                    width = current_plan.width,
                    height = current_plan.height,
                    "mid-session stream resize generation activated"
                );
            }
        }
    };
    let transport_loss_observed_at = if resumable_transport_loss(end_reason) {
        loss_clock
            .as_ref()
            .map(|registry| registry.resume().monotonic_now())
    } else {
        None
    };

    queue.close(); // wake sender to drain & exit
    if let Some(mux) = &mux_handle {
        // Multi-monitor atomic teardown: no matter which half of the
        // `select!` loop above ended it (primary pump, sender, dispatcher,
        // owner command, shutdown, or a failed resize), no further frame
        // may be emitted for *any* monitor once we reach here. Discard any
        // frames already buffered in any monitor's queue — including the
        // primary's — rather than letting the legacy single-queue
        // drain-then-close behavior (just applied to `queue` above) leak
        // one more frame out for a monitor whose pipeline already ended.
        mux.close_and_clear_all();
    }
    audio_queue.close();
    clipboard_queue.close();
    drop(ctrl_tx);
    if let Some(agent) = microphone_agent.take() {
        shutdown_microphone_agent(agent).await;
    }
    if let Some(session) = capenc.take() {
        session.shutdown().await; // stop child → frames_rx closes → pump exits
    }
    if let Some(audio) = audio_runtime.lock().await.take() {
        audio.shutdown().await;
    }
    pump.abort();
    sender.abort();
    abort_and_reap_if_pending(&mut dispatcher).await;
    if !microphone_transport_summary_reported.swap(true, Ordering::AcqRel) {
        microphone_transport_telemetry.try_snapshot(
            lock_recover(&microphone_transport_stats).total(),
            true,
            microphone_transport_started_at.elapsed(),
            microphone_stop_reason(end_reason),
        );
    }

    // Abort AND await: a bare `abort()` only requests cancellation, and does
    // not guarantee the task has actually stopped before this function
    // continues to the terminal SESSION_END/final-telemetry emission below.
    // Awaiting the (now-aborted) handle guarantees no health-tick emission
    // can race with or follow SESSION_END.
    health.abort();
    let _ = health.await;
    if let Some(process) = clipboard_process.take() {
        process.shutdown().await;
    }

    let session_end_client_network = lock_recover(&session_health)
        .client()
        .and_then(|telemetry| telemetry.network.as_ref())
        .cloned();
    emit_session_end_or_interrupted(
        emitter,
        session_log_id,
        end_reason,
        session_started_at.elapsed(),
        queue.frames_sent(),
        queue.frames_dropped(),
        session_user.as_deref(),
        Some(remote_host),
        session_end_client_network.as_ref(),
    );
    let audio_stats = audio_stats
        .read()
        .expect("audio stats lock poisoned")
        .clone();

    info!(
        target: SESSION,
        ?end_reason,
        frames_sent = queue.frames_sent(),
        frames_dropped = queue.frames_dropped(),
        audio_frames_dequeued = audio_queue.sent(),
        audio_frames_dropped = audio_queue.dropped(),
        audio_frames_sent = audio_metrics.wire_frames_sent(),
        audio_bytes_sent = audio_metrics.wire_bytes_sent(),
        audio_encode_failures = audio_metrics.encode_failures(),
        audio_frames_captured = audio_stats
            .as_ref()
            .map_or(0, |stats| stats.captured_frames()),
        audio_restarts = audio_stats.as_ref().map_or(0, |stats| stats.restarts()),
        audio_restart_failures = audio_stats
            .as_ref()
            .map_or(0, |stats| stats.restart_failures()),
        audio_capture_gaps = audio_stats
            .as_ref()
            .map_or(0, |stats| stats.capture_gaps()),
        audio_idle_periods = audio_stats.as_ref().map_or(0, |stats| stats.idle_periods()),
        audio_termination_failures = audio_stats
            .as_ref()
            .map_or(0, |stats| stats.termination_failures()),
        key_events = input_stats.as_ref().map_or(0, |stats| stats.key_events()),
        mouse_moves = input_stats.as_ref().map_or(0, |stats| stats.mouse_moves()),
        mouse_buttons = input_stats.as_ref().map_or(0, |stats| stats.mouse_buttons()),
        scroll_events = input_stats.as_ref().map_or(0, |stats| stats.scroll_events()),
        pen_events = input_stats.as_ref().map_or(0, |stats| stats.pen_events()),
        input_resets = input_stats.as_ref().map_or(0, |stats| stats.resets()),
        unmapped_keys = input_stats.as_ref().map_or(0, |stats| stats.unmapped_keys()),
        "session ended"
    );
    AttachmentEnd {
        reason: end_reason,
        transport_loss_observed_at,
        reached_usable: true,
    }
}

async fn abort_and_reap_if_pending<T>(handle: &mut tokio::task::JoinHandle<T>) {
    if handle.is_finished() {
        return;
    }
    handle.abort();
    let _ = handle.await;
}

async fn receive_owner_command(
    commands: Option<&mut mpsc::UnboundedReceiver<OwnerCommand>>,
) -> Option<OwnerCommand> {
    match commands {
        Some(commands) => commands.recv().await,
        None => std::future::pending().await,
    }
}

/// One validated mid-session resize request, dispatcher → supervisor.
#[derive(Clone, Debug)]
struct ResizeRequest {
    sequence: u64,
    width: u32,
    height: u32,
    reason: String,
}

/// Parse a `display_update` control message; `None` for any other type.
fn parse_display_update(text: &str) -> Option<DisplayUpdateMsg> {
    let update: DisplayUpdateMsg = serde_json::from_str(text).ok()?;
    (update.msg_type == DISPLAY_UPDATE).then_some(update)
}

fn display_update_rejection_message(mode: auth::SessionDisplayMode) -> &'static str {
    match mode {
        auth::SessionDisplayMode::Windowed => "resize not supported for this session",
        auth::SessionDisplayMode::SinglePrimary | auth::SessionDisplayMode::MatchLayout => {
            "display mode is pinned to the negotiated client monitor size; choose Windowed in Deck Settings → Displays to resize the stream"
        }
    }
}

/// Answer a `display_update` with the size actually streaming. Best-effort:
/// a full control channel drops the ack and the client's fit tracker
/// self-resyncs against the received frame size.
fn send_display_update_result(
    ctrl: &ControlSender,
    sequence: u64,
    accepted: bool,
    width: u32,
    height: u32,
    message: &str,
) {
    let result = DisplayUpdateResultMsg {
        sequence,
        accepted,
        width,
        height,
        message: message.to_string(),
        ..DisplayUpdateResultMsg::default()
    };
    if let Ok(json) = serde_json::to_string(&result) {
        ctrl.send_best_effort(Message::Text(json), "display_update_result");
    }
}

async fn send_authoritative_display_update_result(
    ctrl: &ControlSender,
    sequence: u64,
    width: u32,
    height: u32,
) -> bool {
    let Some(json) = authoritative_display_update_result_json(sequence, width, height) else {
        return false;
    };
    ctrl.send_required_with_timeout(
        Message::Text(json),
        "display_update_result",
        CRITICAL_CONTROL_TIMEOUT,
    )
    .await
        && ctrl.barrier().await
}

fn authoritative_display_update_result_json(
    sequence: u64,
    width: u32,
    height: u32,
) -> Option<String> {
    let result = DisplayUpdateResultMsg {
        sequence,
        accepted: true,
        width,
        height,
        ..DisplayUpdateResultMsg::default()
    };
    serde_json::to_string(&result).ok()
}

fn resolve_linux_tablet_mode_result(
    requested: TabletModeMsg,
    client_capabilities: TabletModeCapabilitiesMsg,
    host_capabilities: TabletModeCapabilitiesMsg,
) -> TabletModeResultMsg {
    let negotiation = negotiate_tablet_mode(
        tablet_mode_to_input(requested),
        mutual_capability(
            capability_to_input(client_capabilities.local_termination),
            capability_to_input(host_capabilities.local_termination),
        ),
        mutual_capability(
            capability_to_input(client_capabilities.wacom_usb_bridge),
            capability_to_input(host_capabilities.wacom_usb_bridge),
        ),
    );
    let reason = if negotiation.accepted {
        TabletModeReason::default()
    } else {
        let message = match requested {
            TabletModeMsg::LocalTermination
                if client_capabilities.local_termination
                    != InputCapabilityAvailability::Available =>
            {
                "local termination unavailable: client did not advertise detected tablet/input-v3 pen support"
            }
            TabletModeMsg::LocalTermination => {
                "local termination unavailable: host pen backend did not initialize"
            }
            TabletModeMsg::WacomUsbBridge => {
                "Native tablet (USB bridged) is unavailable on this Linux host: it needs the USB virtualization backend and a host Wacom driver. Use Tablet support instead: it needs no host driver and works over any network."
            }
            TabletModeMsg::DisabledMouseCompat => {
                "mouse compatibility mode negotiation failed unexpectedly"
            }
        };
        TabletModeReason::try_from(message.to_string()).unwrap_or_default()
    };
    TabletModeResultMsg {
        requested,
        active: tablet_mode_from_input(negotiation.active),
        accepted: negotiation.accepted,
        reason,
        reconnect_required: requested == TabletModeMsg::WacomUsbBridge && !negotiation.accepted,
        ..TabletModeResultMsg::default()
    }
}

const fn tablet_mode_to_input(mode: TabletModeMsg) -> InputTabletMode {
    match mode {
        TabletModeMsg::LocalTermination => InputTabletMode::LocalTermination,
        TabletModeMsg::WacomUsbBridge => InputTabletMode::WacomUsbBridge,
        TabletModeMsg::DisabledMouseCompat => InputTabletMode::DisabledMouseCompat,
    }
}

const fn tablet_mode_from_input(mode: InputTabletMode) -> TabletModeMsg {
    match mode {
        InputTabletMode::LocalTermination => TabletModeMsg::LocalTermination,
        InputTabletMode::WacomUsbBridge => TabletModeMsg::WacomUsbBridge,
        InputTabletMode::DisabledMouseCompat => TabletModeMsg::DisabledMouseCompat,
    }
}

const fn capability_to_input(availability: InputCapabilityAvailability) -> InputCapabilityTruth {
    match availability {
        InputCapabilityAvailability::Available => InputCapabilityTruth::Available,
        InputCapabilityAvailability::Unavailable => InputCapabilityTruth::Unavailable,
        InputCapabilityAvailability::Unknown => InputCapabilityTruth::Unknown,
    }
}

fn concrete_encoder_for(plan: ResolvedMediaPlan) -> Option<EncoderRequest> {
    match plan.backend {
        EncoderBackend::NativeNvenc => Some(EncoderRequest::NativeNvenc),
        EncoderBackend::OpenH264 => Some(EncoderRequest::SoftwareH264),
        EncoderBackend::WindowsMediaFoundation => None,
        EncoderBackend::Rav1e => Some(EncoderRequest::SoftwareAv1),
    }
}

fn resize_contract_matches(current: ResolvedMediaPlan, candidate: ResolvedMediaPlan) -> bool {
    current.backend == candidate.backend
        && current.video == candidate.video
        && current.fps == candidate.fps
        && current.cursor_mode == candidate.cursor_mode
        && current.cursor_in_video == candidate.cursor_in_video
        // Compare the capability sets directly: a resize must not quietly
        // change what the backend can do. Adding a codec needs no edit here.
        && current.codecs == candidate.codecs
        && current.chroma == candidate.chroma
}

/// Pump capenc access units into the backpressure queue as wire frames.
/// Ends (with `MediaEnded`) when the capenc frame channel closes — either a
/// dying child or a deliberate shutdown during a mid-session resize.
///
/// `monitor_id` is the `VideoHeader.monitor_id` tag stamped onto every wire
/// frame this pump produces. The legacy single-monitor path always passes
/// `0` (unchanged behavior); a Carrier A multi-monitor session passes one
/// pump per applied monitor, each with that monitor's own
/// `SessionMonitorId`.
///
/// `on_ended`, when `Some`, atomically tears down every monitor's queue
/// (`MonitorMux::close_and_clear_all`) from *inside this pump's own task
/// body*, right before it returns — guaranteeing that call has already run
/// to completion before this function's `JoinHandle` can resolve for any
/// consumer. Used only for the *primary* monitor's pump in a committed
/// multi-monitor session, so its completion gets the exact same
/// atomic-teardown guarantee a secondary pump's own detached watcher task
/// gives for its monitor — actually a strictly stronger one, since nothing
/// else can run in between "this pump ended" and "every queue is torn
/// down". `None` for the legacy single-monitor path and for every
/// secondary pump (which are torn down by their own watcher tasks instead;
/// see the call site in `run_attachment`).
#[allow(clippy::too_many_arguments)]
fn spawn_frame_pump(
    plan: ResolvedMediaPlan,
    mut frames_rx: mpsc::Receiver<crate::media::annexb::AccessUnit>,
    queue: Arc<FrameQueue>,
    mut generation_keyframe: Option<oneshot::Sender<()>>,
    monitor_id: u16,
    topology_generation: u64,
    stream_epoch: u64,
    on_ended: Option<Arc<MonitorMux>>,
) -> tokio::task::JoinHandle<SessionEndReason> {
    let span = tracing::Span::current();
    tokio::spawn(
        async move {
            while let Some(au) = frames_rx.recv().await {
                let classify_recovery =
                    generation_keyframe.is_some() || queue.requires_generation_recovery();
                let generation_recovery = classify_recovery
                    && NalCodec::from_codec_token(plan.codec_token()).is_some_and(|codec| {
                        crate::media::annexb::access_unit_is_recovery_point(&au.data, codec)
                    });
                let frame = media::build_video_frame(
                    &plan,
                    &au,
                    now_ms_u32(),
                    monitor_id,
                    topology_generation,
                    stream_epoch,
                );
                if generation_recovery && generation_keyframe.is_some() {
                    if queue.pin_generation_recovery(frame) {
                        if let Some(ready) = generation_keyframe.take() {
                            let _ = ready.send(());
                        }
                    }
                } else {
                    queue.enqueue_classified(frame, au.is_keyframe, generation_recovery);
                }
            }
            debug!(target: MEDIA, "frame pump ended (capenc frames closed)");
            if let Some(mux) = on_ended {
                mux.close_and_clear_all();
            }
            SessionEndReason::MediaEnded
        }
        .instrument(span),
    )
}

/// Typed classification of why a resume-eligible handshake receive
/// (`receive_client_hello`/`receive_quality_settings`) failed, so
/// `run_attachment` can map each cause to the correct `SessionEndReason`
/// instead of collapsing every failure into a single non-resumable
/// "protocol failure" outcome as before. A transient read-liveness
/// timeout, a clean/erroring transport close, and a genuinely malformed or
/// protocol-violating message are meaningfully different: only the last
/// one proves the client itself sent something this host can never accept,
/// so only it must always stay terminal.
#[derive(Debug)]
enum HandshakeReceiveError {
    /// The read deadline elapsed before a complete message arrived — a
    /// transient condition (slow client, brief network stall), not proof
    /// of anything wrong with the session itself.
    Timeout,
    /// The stream ended (client closed the socket / EOF) before a
    /// complete message arrived — classified exactly like any other
    /// unexpected mid-session transport loss.
    Closed,
    /// The underlying transport reported a read error — classified
    /// exactly like any other unexpected mid-session transport loss.
    Transport(tokio_tungstenite::tungstenite::Error),
    /// The message was received intact at the transport layer but is
    /// malformed or violates the handshake protocol (undecodable JSON,
    /// wrong `msg_type`, non-text frame). Always terminal.
    Protocol(String),
}

impl std::fmt::Display for HandshakeReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timed out waiting for message"),
            Self::Closed => write!(f, "connection closed before message"),
            Self::Transport(error) => write!(f, "transport read error: {error}"),
            Self::Protocol(message) => f.write_str(message),
        }
    }
}

impl HandshakeReceiveError {
    /// The `SessionEndReason` this failure must be classified as. A
    /// transient timeout maps to the same
    /// `SessionEndReason::ReadLivenessTimeout` a mid-session read-liveness
    /// deadline would, and a closed/erroring transport maps to the same
    /// `SessionEndReason::TransportError` an unexpected mid-session
    /// transport loss would — both already eligible for the existing
    /// `resumable_transport_loss` disconnect/reconnect-within-window
    /// policy, applied identically whether this handshake belongs to the
    /// very first attachment or a resumed one. A malformed/protocol
    /// violation maps to `SessionEndReason::ProtocolError`, which
    /// `resumable_transport_loss` never treats as resumable — it always
    /// stays terminal.
    const fn session_end_reason(&self) -> SessionEndReason {
        match self {
            Self::Timeout => SessionEndReason::ReadLivenessTimeout,
            Self::Closed | Self::Transport(_) => SessionEndReason::TransportError,
            Self::Protocol(_) => SessionEndReason::ProtocolError,
        }
    }
}

/// Builds the `AttachmentEnd` a ClientHello/`quality_settings` handshake
/// receive failure must return. Applies exactly the same
/// `transport_loss_observed_at` policy the main session select loop
/// applies for its own transport-loss exits: populated only when the
/// classified reason is itself eligible for `resumable_transport_loss`, so
/// a resumed attachment's own transient timeout/close during the handshake
/// gets exactly the same disconnect-and-hold-for-reconnect-within-window
/// treatment a mid-session transport loss would, while a malformed/
/// protocol-violating message stays terminal (`transport_loss_observed_at:
/// None`) regardless of whether this is the first or a resumed attachment.
fn handshake_receive_attachment_end(
    error: &HandshakeReceiveError,
    refresh: Option<&ResumeRefreshContext>,
) -> AttachmentEnd {
    let reason = error.session_end_reason();
    let transport_loss_observed_at = if resumable_transport_loss(reason) {
        refresh.map(|refresh| refresh.session_registry.resume().monotonic_now())
    } else {
        None
    };
    AttachmentEnd {
        reason,
        transport_loss_observed_at,
        reached_usable: false,
    }
}

async fn receive_client_hello<S>(
    stream: &mut S,
    timeout: Duration,
) -> Result<ClientHelloMsg, HandshakeReceiveError>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| HandshakeReceiveError::Timeout)?
            .ok_or(HandshakeReceiveError::Closed)?
            .map_err(HandshakeReceiveError::Transport)?;
        match message {
            Message::Text(text) => {
                let hello: ClientHelloMsg = serde_json::from_str(&text).map_err(|error| {
                    HandshakeReceiveError::Protocol(format!("decode client hello: {error}"))
                })?;
                if hello.msg_type != CLIENT_HELLO {
                    return Err(HandshakeReceiveError::Protocol(
                        "first streaming control was not client_hello".to_string(),
                    ));
                }
                return Ok(hello);
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            _ => {
                return Err(HandshakeReceiveError::Protocol(
                    "client hello must be a JSON text frame".to_string(),
                ));
            }
        }
    }
}

async fn receive_quality_settings<S>(
    stream: &mut S,
    timeout: Duration,
) -> Result<QualitySettings, HandshakeReceiveError>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| HandshakeReceiveError::Timeout)?
            .ok_or(HandshakeReceiveError::Closed)?
            .map_err(HandshakeReceiveError::Transport)?;
        match message {
            Message::Text(text) => {
                let quality: QualitySettings = serde_json::from_str(&text).map_err(|error| {
                    HandshakeReceiveError::Protocol(format!("decode quality settings: {error}"))
                })?;
                if quality.msg_type != "quality_settings" {
                    return Err(HandshakeReceiveError::Protocol(
                        "second streaming control was not quality_settings".to_string(),
                    ));
                }
                return Ok(quality);
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            _ => {
                return Err(HandshakeReceiveError::Protocol(
                    "quality settings must be a JSON text frame".to_string(),
                ));
            }
        }
    }
}

async fn next_clipboard_agent_message(
    agent: &mut Option<WebSocketStream<ClipboardAgentIo>>,
) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    match agent {
        Some(agent) => agent.next().await,
        None => std::future::pending().await,
    }
}

fn is_clipboard_data_message(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    arcen_protocol::messages::msg_type(&value) == Some(CLIPBOARD_DATA)
}

fn handle_deck_clipboard_offer(
    text: &str,
    negotiation: Option<ClipboardNegotiation>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
) {
    let (Some(negotiation), Some(reassembler)) = (negotiation, reassembler) else {
        return;
    };
    let Ok(offer) = serde_json::from_str::<ClipboardDataMsg>(text) else {
        warn!(target: SESSION, reason = "invalid_metadata", "clipboard offer rejected");
        return;
    };
    if !negotiation.allows(ClipboardFlow::ClientToHost, offer.kind)
        || negotiation
            .policy()
            .check_size(
                ClipboardFlow::ClientToHost,
                clipboard_kind(offer.kind),
                usize::try_from(offer.size_bytes).unwrap_or(usize::MAX),
            )
            .is_err()
    {
        warn!(
            target: SESSION,
            sequence = offer.sequence,
            kind = ?offer.kind,
            size = offer.size_bytes,
            reason = "policy",
            "clipboard offer rejected"
        );
        return;
    }
    if let Err(error) = reassembler.begin(offer.clone()) {
        warn!(
            target: SESSION,
            sequence = offer.sequence,
            kind = ?offer.kind,
            size = offer.size_bytes,
            reason = %error,
            "clipboard offer rejected"
        );
    }
}

fn handle_deck_clipboard_binary(
    bytes: &[u8],
    negotiation: Option<ClipboardNegotiation>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
) -> Option<ClipboardItem> {
    let (negotiation, reassembler) = (negotiation?, reassembler?);
    let (header, payload) = decode_clipboard_chunk(bytes).ok()?;
    if !negotiation.allows(ClipboardFlow::ClientToHost, header.kind)
        || negotiation
            .policy()
            .check_size(
                ClipboardFlow::ClientToHost,
                clipboard_kind(header.kind),
                usize::try_from(header.total_size).unwrap_or(usize::MAX),
            )
            .is_err()
    {
        reassembler.abort();
        return None;
    }
    let mut completed = match reassembler.push(header, payload) {
        Ok(Some(completed)) => completed,
        Ok(None) => return None,
        Err(_) => {
            reassembler.abort();
            return None;
        }
    };
    if !validate_clipboard_completed(
        negotiation.policy(),
        ClipboardFlow::ClientToHost,
        completed.kind,
        &completed.bytes,
    ) {
        return None;
    }
    ClipboardItem::new(
        completed.sequence,
        completed.kind,
        completed.take_bytes(),
        completed.truncated,
    )
}

async fn send_clipboard_item(
    agent: &mut WebSocketStream<ClipboardAgentIo>,
    item: ClipboardItem,
) -> Result<(), String> {
    let size = u32::try_from(item.bytes.len())
        .map_err(|_| "clipboard IPC item size exceeds u32".to_string())?;
    let offer = ClipboardDataMsg::new(item.sequence, item.kind, size, item.truncated);
    agent
        .send(Message::Text(
            serde_json::to_string(&offer)
                .map_err(|error| format!("serialize clipboard IPC offer: {error}"))?
                .into(),
        ))
        .await
        .map_err(|error| format!("send clipboard IPC offer: {error}"))?;
    for (index, payload) in item.bytes.chunks(arcen_protocol::CHUNK_BYTES).enumerate() {
        let offset = index
            .checked_mul(arcen_protocol::CHUNK_BYTES)
            .and_then(|offset| u32::try_from(offset).ok())
            .ok_or_else(|| "clipboard IPC offset overflow".to_string())?;
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: item.kind,
                sequence: item.sequence,
                total_size: size,
                offset,
            },
            payload,
        )
        .map_err(|error| format!("encode clipboard IPC chunk: {error:?}"))?;
        agent
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|error| format!("send clipboard IPC chunk: {error}"))?;
        tokio::task::yield_now().await;
    }
    Ok(())
}

fn handle_agent_clipboard_message(
    message: Message,
    negotiation: Option<ClipboardNegotiation>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
    outbound: &ClipboardWriterQueue,
) {
    let (Some(negotiation), Some(reassembler)) = (negotiation, reassembler) else {
        return;
    };
    match message {
        Message::Text(text) => {
            if !is_clipboard_data_message(&text) {
                return;
            }
            let Ok(offer) = serde_json::from_str::<ClipboardDataMsg>(&text) else {
                return;
            };
            if negotiation.allows(ClipboardFlow::HostToClient, offer.kind)
                && negotiation
                    .policy()
                    .check_size(
                        ClipboardFlow::HostToClient,
                        clipboard_kind(offer.kind),
                        usize::try_from(offer.size_bytes).unwrap_or(usize::MAX),
                    )
                    .is_ok()
            {
                let _ = reassembler.begin(offer);
            }
        }
        Message::Binary(bytes) => {
            let Ok((header, payload)) = decode_clipboard_chunk(&bytes) else {
                reassembler.abort();
                return;
            };
            if !negotiation.allows(ClipboardFlow::HostToClient, header.kind) {
                reassembler.abort();
                return;
            }
            let mut completed = match reassembler.push(header, payload) {
                Ok(Some(completed)) => completed,
                Ok(None) => return,
                Err(_) => {
                    reassembler.abort();
                    return;
                }
            };
            if validate_clipboard_completed(
                negotiation.policy(),
                ClipboardFlow::HostToClient,
                completed.kind,
                &completed.bytes,
            ) {
                if let Some(item) = ClipboardItem::new(
                    completed.sequence,
                    completed.kind,
                    completed.take_bytes(),
                    completed.truncated,
                ) {
                    let _ = outbound.enqueue(item);
                }
            }
        }
        _ => {}
    }
}

fn validate_clipboard_completed(
    policy: arcen_media::clipboard::ClipboardPolicy,
    flow: ClipboardFlow,
    kind: arcen_protocol::messages::ClipboardContentKind,
    bytes: &[u8],
) -> bool {
    if policy
        .check_size(flow, clipboard_kind(kind), bytes.len())
        .is_err()
    {
        return false;
    }
    match kind {
        arcen_protocol::messages::ClipboardContentKind::TextUtf8 => {
            std::str::from_utf8(bytes).is_ok()
        }
        arcen_protocol::messages::ClipboardContentKind::ImagePng => {
            arcen_media::clipboard::validate_png(
                bytes,
                arcen_media::clipboard::ImageLimits {
                    max_encoded_bytes: policy.max_bytes,
                    ..arcen_media::clipboard::ImageLimits::default()
                },
            )
            .is_ok()
        }
    }
}

fn clipboard_kind(kind: arcen_protocol::messages::ClipboardContentKind) -> ClipboardKind {
    match kind {
        arcen_protocol::messages::ClipboardContentKind::TextUtf8 => ClipboardKind::TextUtf8,
        arcen_protocol::messages::ClipboardContentKind::ImagePng => ClipboardKind::ImagePng,
    }
}

#[derive(Clone)]
struct AudioControl {
    policy: ConfiguredAudioPolicy,
    capture_available: bool,
    peer: Option<arcen_protocol::messages::AudioOutputCapabilitiesMsg>,
    stream: Arc<RwLock<ResolvedAudioStream>>,
    wire_frames_sent: Arc<AtomicU64>,
    wire_bytes_sent: Arc<AtomicU64>,
    encode_failures: Arc<AtomicU64>,
}

impl AudioControl {
    fn new(
        host_enabled: bool,
        compressed: bool,
        capture_available: bool,
        peer: Option<arcen_protocol::messages::AudioOutputCapabilitiesMsg>,
        stream: ResolvedAudioStream,
    ) -> Self {
        let policy = AudioPolicy::configured(host_enabled, compressed);
        Self {
            policy,
            capture_available,
            peer,
            stream: Arc::new(RwLock::new(stream)),
            wire_frames_sent: Arc::new(AtomicU64::new(0)),
            wire_bytes_sent: Arc::new(AtomicU64::new(0)),
            encode_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    fn stream(&self) -> ResolvedAudioStream {
        *self.stream.read().expect("audio stream lock poisoned")
    }

    fn resolve(&self, enabled: bool, _bitrate_kbps: u32) -> ResolvedAudioStream {
        if enabled && !self.capture_available {
            let mode = if self.peer.is_some() {
                arcen_media::audio::AudioProtocolMode::V1
            } else {
                arcen_media::audio::AudioProtocolMode::Legacy
            };
            return ResolvedAudioStream::disabled(
                mode,
                arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
            );
        }
        self.policy.resolve(self.peer.as_ref(), enabled)
    }

    fn resolve_with_codec_preflight(
        &self,
        enabled: bool,
        _bitrate_kbps: u32,
    ) -> ResolvedAudioStream {
        let stream = self.resolve(enabled, 0);
        if AudioFrameEncoder::new(stream).is_ok() || stream.codec != Some(AudioCodec::Opus) {
            return stream;
        }
        self.policy
            .without_opus()
            .resolve(self.peer.as_ref(), enabled)
    }

    fn set_stream(&self, stream: ResolvedAudioStream) {
        *self.stream.write().expect("audio stream lock poisoned") = stream;
    }

    fn record_sent(&self, payload_bytes: usize) {
        self.wire_frames_sent.fetch_add(1, Ordering::Relaxed);
        self.wire_bytes_sent
            .fetch_add(payload_bytes as u64, Ordering::Relaxed);
    }

    fn record_encode_failure(&self) {
        self.encode_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn wire_frames_sent(&self) -> u64 {
        self.wire_frames_sent.load(Ordering::Relaxed)
    }

    fn wire_bytes_sent(&self) -> u64 {
        self.wire_bytes_sent.load(Ordering::Relaxed)
    }

    fn encode_failures(&self) -> u64 {
        self.encode_failures.load(Ordering::Relaxed)
    }
}

async fn send_audio_result_required(control: &ControlSender, stream: ResolvedAudioStream) -> bool {
    let Some(result) = stream.result() else {
        return true;
    };
    let Ok(json) = serde_json::to_string(&result) else {
        return false;
    };
    control
        .send_required(
            Message::Text(json),
            arcen_protocol::messages::AUDIO_STREAM_RESULT,
        )
        .await
}

async fn apply_audio_quality(
    quality: QualitySettings,
    control: &ControlSender,
    audio: &AudioControl,
    encoder: &Arc<Mutex<AudioFrameEncoder>>,
    queue: &Arc<AudioQueue>,
    capture_config: Option<&AudioConfig>,
    capture: &Arc<tokio::sync::Mutex<Option<audiocap::AudioSession>>>,
    stats: &Arc<RwLock<Option<Arc<audiocap::AudioStats>>>>,
    failure_tx: &mpsc::Sender<arcen_protocol::messages::AudioStreamReason>,
) -> bool {
    let mut stream =
        audio.resolve_with_codec_preflight(quality.enable_audio, quality.audio_bitrate_kbps);
    if stream.is_enabled() {
        if !send_audio_result_required(control, stream).await {
            return false;
        }
        queue.clear();
        if encoder
            .lock()
            .expect("audio encoder poisoned")
            .reconfigure(stream)
            .is_err()
        {
            stream = ResolvedAudioStream::disabled(
                stream.mode,
                arcen_protocol::messages::AudioStreamReason::CodecUnavailable,
            );
            audio.set_stream(stream);
            encoder
                .lock()
                .expect("audio encoder poisoned")
                .reconfigure(stream)
                .expect("disabling audio is infallible");
            queue.clear();
            let session = capture.lock().await.take();
            if let Some(session) = session {
                session.shutdown_preserving_queue().await;
            }
            return send_audio_result_required(control, stream).await;
        }

        let mut capture = capture.lock().await;
        if capture.is_none() {
            let Some(config) = capture_config.cloned() else {
                stream = ResolvedAudioStream::disabled(
                    stream.mode,
                    arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
                );
                audio.set_stream(stream);
                encoder
                    .lock()
                    .expect("audio encoder poisoned")
                    .reconfigure(stream)
                    .expect("disabling audio is infallible");
                return send_audio_result_required(control, stream).await;
            };
            match audiocap::spawn(
                config,
                Arc::clone(queue),
                Arc::clone(encoder),
                failure_tx.clone(),
            ) {
                Ok(session) => {
                    *stats.write().expect("audio stats lock poisoned") = Some(session.stats());
                    *capture = Some(session);
                }
                Err(error) => {
                    warn!(target: AUDIO, %error, "native audiocap spawn failed after runtime enable");
                    stream = ResolvedAudioStream::disabled(
                        stream.mode,
                        arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
                    );
                    audio.set_stream(stream);
                    encoder
                        .lock()
                        .expect("audio encoder poisoned")
                        .reconfigure(stream)
                        .expect("disabling audio is infallible");
                    return send_audio_result_required(control, stream).await;
                }
            }
        }
        audio.set_stream(stream);
        return true;
    }

    audio.set_stream(stream);
    encoder
        .lock()
        .expect("audio encoder poisoned")
        .reconfigure(stream)
        .expect("disabling audio is infallible");
    queue.clear();
    let session = capture.lock().await.take();
    if let Some(session) = session {
        session.shutdown_preserving_queue().await;
    }
    send_audio_result_required(control, stream).await
}

async fn disable_audio_after_codec_failure(
    reason: arcen_protocol::messages::AudioStreamReason,
    control: &ControlSender,
    audio: &AudioControl,
    encoder: &Arc<Mutex<AudioFrameEncoder>>,
    queue: &Arc<AudioQueue>,
    capture: &Arc<tokio::sync::Mutex<Option<audiocap::AudioSession>>>,
) -> bool {
    audio.record_encode_failure();
    let stream = ResolvedAudioStream::disabled(audio.stream().mode, reason);
    audio.set_stream(stream);
    encoder
        .lock()
        .expect("audio encoder poisoned")
        .reconfigure(stream)
        .expect("disabling audio is infallible");
    queue.clear();
    let session = capture.lock().await.take();
    if let Some(session) = session {
        session.shutdown_preserving_queue().await;
    }
    send_audio_result_required(control, stream).await
}

#[allow(clippy::too_many_arguments)] // Per-connection sender context: mux source plus audio/clipboard/control/resume wiring.
async fn sender_loop<S>(
    mut sink: S,
    video: crate::session::monitor_mux::VideoSource,
    audio: Arc<AudioQueue>,
    clipboard: Arc<ClipboardWriterQueue>,
    mut control: mpsc::Receiver<WriterControl>,
    audio_control: AudioControl,
    refresh: Option<ResumeRefreshContext>,
    write_timeout: Duration,
) -> SessionEndReason
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let mut refresh_interval = refresh.as_ref().map(|refresh| {
        let mut interval = tokio::time::interval(resume_refresh_interval(refresh.window_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval
    });
    if let Some(interval) = refresh_interval.as_mut() {
        interval.tick().await;
    }
    let mut clipboard_open = true;
    let mut clipboard_allowed = true;
    let mut audio_open = true;
    loop {
        tokio::select! {
            biased;
            _ = async {
                match refresh_interval.as_mut() {
                    Some(interval) => {
                        interval.tick().await;
                    }
                    None => std::future::pending().await,
                }
            } => {
                let refresh = refresh.as_ref().expect("refresh interval requires refresh context");
                let registry = refresh.session_registry.resume();
                let grant = match registry.refresh_grant(&refresh.active_session_id) {
                    Ok(grant) => grant,
                    Err(_) => {
                        if let Err(error) = registry.begin_drain(&refresh.active_session_id) {
                            tracing::error!(
                                target: SESSION,
                                ?error,
                                "resume refresh failure could not drain authority"
                            );
                        }
                        return SessionEndReason::ResumeAuthorityFailure;
                    }
                };
                if !send_successor_auth_result_or_drain(
                    &mut sink,
                    "Resume grant refreshed",
                    &grant.grant,
                    grant.window_secs,
                    false,
                    write_timeout,
                    registry,
                    &refresh.active_session_id,
                ).await {
                    return SessionEndReason::ResumeAuthorityFailure;
                }
            },
            ctrl = control.recv() => match ctrl {
                Some(WriterControl::Message(msg)) => if !send_ws_with_timeout(
                    &mut sink,
                    msg,
                    "control",
                    write_timeout,
                ).await { break; },
                Some(WriterControl::Barrier(complete)) => {
                    let _ = complete.send(());
                }
                None => break,
            },
            message = clipboard.pop(), if clipboard_open && clipboard_allowed => match message {
                Ok(Some(message)) => {
                    clipboard_allowed = false;
                    if !send_ws_with_timeout(
                        &mut sink,
                        message,
                        "clipboard",
                        write_timeout,
                    ).await {
                        break;
                    }
                }
                Ok(None) => clipboard_open = false,
                Err(error) => {
                    warn!(target: SESSION, %error, "clipboard writer failed");
                    break;
                }
            },
            packet = audio.dequeue(), if audio_open => match packet {
                Some(packet) => {
                    clipboard_allowed = true;
                    let stream = audio_control.stream();
                    if !stream.is_enabled() || stream.codec != Some(packet.codec) {
                        continue;
                    }
                    let payload_bytes = packet.payload.len();
                    let bytes = media::build_audio_frame(
                        packet.codec,
                        &packet.payload,
                        packet.timestamp_ms,
                    );
                    if !send_ws_with_timeout(
                        &mut sink,
                        Message::Binary(bytes),
                        "audio",
                        write_timeout,
                    ).await {
                        break;
                    }
                    audio_control.record_sent(payload_bytes);
                },
                None => audio_open = false,
            },
            frame = video.dequeue() => match frame {
                Some(bytes) => {
                    clipboard_allowed = true;
                    if !send_ws_with_timeout(
                        &mut sink,
                        Message::Binary(bytes),
                        "video",
                        write_timeout,
                    ).await {
                        break;
                    }
                },
                None => break,
            },
            () = clipboard_cooldown(), if clipboard_open && !clipboard_allowed => {
                clipboard_allowed = true;
            },
        }
    }

    async fn clipboard_cooldown() {
        tokio::task::yield_now().await;
    }
    let _ = tokio::time::timeout(write_timeout, sink.close()).await;
    debug!(target: NET, "sender task ended");
    SessionEndReason::WriterEnded
}

/// Dispatch one inbound JSON control message.
///
/// `idr` is every applied monitor's own `IdrRequester` — one entry for a
/// legacy single-monitor session, primary + every secondary's for a
/// committed Carrier A multi-monitor plan (see `all_monitor_idrs` at the
/// call site). Any whole-session recovery trigger below (currently just
/// `REQUEST_FULL_FRAME`) requests a fresh IDR from every one of them
/// exactly once, so a client recovering from a video discontinuity gets a
/// fully decodable picture on every monitor, not only the primary's.
#[allow(clippy::too_many_arguments)] // Existing dispatcher inputs plus read-only TZ consistency state.
fn handle_control_json(
    text: &str,
    idr: &[IdrRequester],
    ctrl: &ControlSender,
    media_plan: &capenc::ResolvedMediaPlan,
    session_log_id: &CorrelationId,
    authoritative_timezone: Option<&IanaTimeZone>,
    input: Option<&mut InputController>,
    last_full_frame_idr: &mut Instant,
    input_sequence: &mut InputSequenceTracker,
    active_cursor_mode: CursorMode,
    session_health: &Mutex<crate::observability::SessionHealth>,
    session_started_at: Instant,
) {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            debug!(target: NET, error = %e, "ignoring malformed control JSON");
            return;
        }
    };
    let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if is_legacy_region_input_message(msg_type)
        && input
            .as_ref()
            .is_some_and(|controller| controller.region_input_available())
    {
        warn!(
            target: INPUT,
            message_type = msg_type,
            "legacy input rejected in a region-authoritative session"
        );
        return;
    }

    match msg_type {
        REQUEST_FULL_FRAME => {
            if let Err(error) = serde_json::from_str::<RequestFullFrameMsg>(text) {
                debug!(target: NET, %error, "invalid request_full_frame message");
                return;
            }
            let now = Instant::now();
            if now.duration_since(*last_full_frame_idr) >= FULL_FRAME_IDR_GUARD {
                *last_full_frame_idr = now;
                for monitor_idr in idr {
                    monitor_idr.request();
                }
                debug!(
                    target: MEDIA,
                    monitor_count = idr.len(),
                    "request_full_frame → IDR (all applied monitors)"
                );
            } else {
                trace!(target: MEDIA, "request_full_frame throttled (<500ms)");
            }
        }
        CLIENT_HELLO => {
            let parsed_hello = serde_json::from_str::<ClientHelloMsg>(text);
            let echoed_timezone = match &parsed_hello {
                Ok(hello) => hello.timezone.clone(),
                Err(error) => {
                    debug!(
                        target: NET,
                        %error,
                        "client_hello could not be fully typed; using bounded timezone field extraction"
                    );
                    value
                        .get("timezone")
                        .and_then(|timezone| timezone.as_str())
                        .map(str::to_string)
                }
            };
            if let Ok(hello) = parsed_hello {
                if hello.cursor_preference != active_cursor_mode {
                    let reason = CursorModeReason::try_from(
                        "cursor preference was unavailable before capture startup".to_string(),
                    )
                    .expect("static cursor reason is bounded");
                    let result = CursorModeResultMsg {
                        requested: hello.cursor_preference,
                        active: active_cursor_mode,
                        accepted: false,
                        reason,
                        ..CursorModeResultMsg::default()
                    };
                    if let Ok(json) = serde_json::to_string(&result) {
                        ctrl.send_best_effort(Message::Text(json), "cursor_mode_result");
                    }
                }
            }
            if timezone_echo_mismatch(authoritative_timezone, echoed_timezone.as_deref()) {
                warn!(
                    target: SESSION,
                    active_timezone = authoritative_timezone
                        .map_or("<none>", IanaTimeZone::as_str),
                    client_timezone_present = echoed_timezone.is_some(),
                    "client_hello timezone differs from authoritative desktop decision; retaining active timezone"
                );
            }
            match value.get("session_log_id").and_then(|value| value.as_str()) {
                Some(value) => match CorrelationId::parse_uuid(value) {
                    Ok(client_id) if client_id != *session_log_id => warn!(
                        target: SESSION,
                        client_sid = %client_id,
                        "late client session log id differs from the established host id"
                    ),
                    Ok(_) => {}
                    Err(_) => warn!(
                        target: SESSION,
                        "late client session log id is invalid and was ignored"
                    ),
                },
                None => {}
            }

            let w = value
                .get("screen_width")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let h = value
                .get("screen_height")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            info!(
                target: SESSION,
                client_w = w,
                client_h = h,
                "client_hello received (resolution ingest + resize land in Stage 2/3)"
            );
        }
        "quality_settings" => {
            let want_codec = value.get("codec").and_then(|v| v.as_str()).unwrap_or("");
            let want_chroma = value.get("chroma").and_then(|v| v.as_str()).unwrap_or("");
            let want_bit_depth = value
                .get("bit_depth")
                .and_then(|v| v.as_str())
                .unwrap_or("8");
            let want_color_range = value
                .get("color_range")
                .and_then(|v| v.as_str())
                .unwrap_or("limited");
            let want_color_matrix = value
                .get("color_matrix")
                .and_then(|v| v.as_str())
                .unwrap_or("bt709");
            // Logged, not compared: `ResolvedMediaPlan` carries the format the
            // encoder announces, not what it was told to optimise for, so this
            // handler has no active intent to diff against. A mid-session
            // change lands in the record and stops there, like every other
            // mid-session video change below.
            let want_encode_intent = value
                .get("encode_intent")
                .and_then(|v| v.as_str())
                .unwrap_or("interactive");
            let codec_mismatch = !want_codec.is_empty() && want_codec != media_plan.codec_token();
            let chroma_mismatch =
                !want_chroma.is_empty() && want_chroma != media_plan.chroma_token();
            let color_mismatch = want_bit_depth != media_plan.bit_depth_token()
                || want_color_range != media_plan.range_token()
                || want_color_matrix != media_plan.matrix_token();
            if codec_mismatch || chroma_mismatch || color_mismatch {
                // Mid-session quality_settings codec/colour changes are not
                // yet implemented: NVENC cannot reconfigure depth or chroma
                // without a session recreate, and this also requires capenc
                // respawn coordination with the active frame pump, which is
                // out of scope here. Log the mismatch for diagnostics; the
                // initial quality_settings at session setup already
                // honoured the client request if the backend could serve it.
                info!(
                    target: SESSION,
                    want_codec, want_chroma, want_bit_depth, want_color_range, want_color_matrix,
                    want_encode_intent,
                    host_codec = media_plan.codec_token(),
                    host_chroma = media_plan.chroma_token(),
                    host_bit_depth = media_plan.bit_depth_token(),
                    host_color_range = media_plan.range_token(),
                    host_color_matrix = media_plan.matrix_token(),
                    "mid-session quality_settings codec/depth/range/matrix change not yet implemented; keeping active plan"
                );
            } else {
                info!(
                    target: SESSION,
                    want_codec, want_chroma, want_bit_depth, want_color_range, want_color_matrix,
                    want_encode_intent,
                    "quality_settings acknowledged"
                );
            }
        }
        HEALTH_PING => {
            let ping = match serde_json::from_str::<HealthPingMsg>(text) {
                Ok(ping) => ping,
                Err(error) => {
                    debug!(target: NET, %error, "invalid health_ping message");
                    return;
                }
            };
            // Monotonic, not wall-clock: `SessionHealth::observe`'s
            // staleness check compares `client_received_ms` against its own
            // monotonic `timestamp_ms` (session-start-anchored elapsed
            // milliseconds), so recording receipt on the same basis avoids
            // a false-stale/false-fresh result across an NTP/DST/leap-second
            // wall-clock jump.
            let client_received_ms =
                u64::try_from(session_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            lock_recover(session_health)
                .record_client_at(client_received_ms, ping.client_telemetry);
            let pong = HealthPongMsg {
                msg_type: HEALTH_PONG.to_string(),
                ping_timestamp_ms: ping.timestamp_ms,
                sequence: ping.sequence,
                server_timestamp_ms: now_ms_u64(),
                server_state: "streaming".to_string(),
            };
            if let Ok(j) = serde_json::to_string(&pong) {
                ctrl.send_best_effort(Message::Text(j), "health_reply");
            }
        }
        "key_event" => {
            let Some(input) = input else {
                trace!(target: INPUT, "key_event ignored: input backend disabled");
                return;
            };
            match parse_sequenced_input::<KeyEventMsg>(text, input_sequence) {
                Ok(message) => {
                    if let Err(error) = input.key_event(&message) {
                        warn!(target: INPUT, %error, "key injection failed");
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid key_event"),
            }
        }
        KEY_RESET_MODIFIERS => {
            let Some(input) = input else {
                return;
            };
            match serde_json::from_str::<KeyResetModifiersMsg>(text) {
                Ok(message) => {
                    debug!(target: INPUT, reason = %message.reason, "releasing held input state");
                    if let Err(error) = input.reset_keyboard_held() {
                        warn!(target: INPUT, %error, "keyboard reset failed");
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid key_reset_modifiers"),
            }
        }
        REGION_POINTER_ENTER => {
            let Some(input) = input else {
                return;
            };
            if let Err(error) =
                dispatch_region_input::<RegionPointerEnterMsg, _>(text, input_sequence, |message| {
                    input.region_pointer_enter(message)
                })
            {
                debug!(target: INPUT, %error, "invalid region_pointer_enter");
            }
        }
        REGION_POINTER_LEAVE => {
            let Some(input) = input else {
                return;
            };
            if let Err(error) =
                dispatch_region_input::<RegionPointerLeaveMsg, _>(text, input_sequence, |message| {
                    input.region_pointer_leave(message)
                })
            {
                debug!(target: INPUT, %error, "invalid region_pointer_leave");
            }
        }
        REGION_POINTER_MOTION => {
            let Some(input) = input else {
                return;
            };
            if let Err(error) = dispatch_region_input::<RegionPointerMotionMsg, _>(
                text,
                input_sequence,
                |message| input.region_pointer_motion(message),
            ) {
                debug!(target: INPUT, %error, "invalid region_pointer_motion");
            }
        }
        REGION_POINTER_BUTTON => {
            let Some(input) = input else {
                return;
            };
            if let Err(error) = dispatch_region_input::<RegionPointerButtonMsg, _>(
                text,
                input_sequence,
                |message| input.region_pointer_button(message),
            ) {
                debug!(target: INPUT, %error, "invalid region_pointer_button");
            }
        }
        REGION_POINTER_SCROLL => {
            let Some(input) = input else {
                return;
            };
            if let Err(error) = dispatch_region_input::<RegionPointerScrollMsg, _>(
                text,
                input_sequence,
                |message| input.region_pointer_scroll(message),
            ) {
                debug!(target: INPUT, %error, "invalid region_pointer_scroll");
            }
        }
        REGION_PEN_EVENT => {
            let Some(input) = input else {
                trace!(target: INPUT, "region_pen_event ignored: input backend disabled");
                return;
            };
            if let Err(error) =
                dispatch_region_input::<RegionPenEventMsg, _>(text, input_sequence, |message| {
                    input.region_pen_event(message)
                })
            {
                debug!(target: INPUT, %error, "invalid region_pen_event");
            }
        }
        "mouse_move" => {
            let Some(input) = input else {
                return;
            };
            match parse_sequenced_input::<MouseMoveMsg>(text, input_sequence) {
                Ok(message) => {
                    if let Err(error) = input.mouse_move(&message) {
                        warn!(target: INPUT, %error, "mouse move injection failed");
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid mouse_move"),
            }
        }
        MOUSE_MOVE_RELATIVE => {
            let Some(input) = input else {
                return;
            };
            match parse_sequenced_input::<MouseMoveRelativeMsg>(text, input_sequence) {
                Ok(message) => {
                    if let Err(error) = input.mouse_move_relative(&message) {
                        warn!(target: INPUT, %error, "relative mouse move injection failed");
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid mouse_move_relative"),
            }
        }
        "mouse_button" => {
            let Some(input) = input else {
                return;
            };
            match parse_sequenced_input::<MouseButtonMsg>(text, input_sequence) {
                Ok(message) => {
                    if let Err(error) = input.mouse_button(&message) {
                        warn!(target: INPUT, %error, "mouse button injection failed");
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid mouse_button"),
            }
        }
        MOUSE_SCROLL => {
            let Some(input) = input else {
                return;
            };
            match parse_sequenced_input::<MouseScrollMsg>(text, input_sequence) {
                Ok(message) => {
                    if let Err(error) = input.mouse_scroll(&message) {
                        warn!(target: INPUT, %error, "mouse scroll injection failed");
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid mouse_scroll"),
            }
        }
        PEN_EVENT => {
            let Some(input) = input else {
                trace!(target: INPUT, "pen_event ignored: input backend disabled");
                return;
            };
            match parse_sequenced_pen_event(text, input_sequence) {
                Ok(message) => {
                    trace!(
                        target: INPUT,
                        x_norm = message.x,
                        y_norm = message.y,
                        pressure = message.pressure,
                        in_proximity = message.in_proximity,
                        touching = message.touching,
                        tool = ?message.tool,
                        seq = message.sequence,
                        "pen_event received"
                    );
                    match input.pen_event(&message) {
                        Ok(()) => {}
                        Err(InputError::PenUnavailable) => {
                            trace!(
                                target: INPUT,
                                "pen_event ignored: tablet backend unavailable on this session"
                            );
                        }
                        Err(error) => warn!(target: INPUT, %error, "pen injection failed"),
                    }
                }
                Err(error) => debug!(target: INPUT, %error, "invalid pen_event"),
            }
        }

        other => debug!(target: NET, msg_type = other, "unhandled control message (stage 1)"),
    }
}

fn match_layout_region_input_negotiated(
    host_region_input_available: bool,
    client_hello: &ClientHelloMsg,
) -> bool {
    host_region_input_available
        && supports_region_input_v1(
            client_hello.input_protocol_version,
            client_hello.input_capabilities,
        )
}

fn is_legacy_region_input_message(message_type: &str) -> bool {
    matches!(
        message_type,
        "mouse_move" | MOUSE_MOVE_RELATIVE | "mouse_button" | MOUSE_SCROLL | PEN_EVENT
    )
}

trait SequencedInput {
    fn sequence(&self) -> u64;
}

impl SequencedInput for KeyEventMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl SequencedInput for MouseMoveMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl SequencedInput for MouseMoveRelativeMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl SequencedInput for MouseButtonMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl SequencedInput for MouseScrollMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

trait RegionWireInput: DeserializeOwned {
    fn sequence(&self) -> u64;
    fn validate_region(&self) -> Result<(), RegionInputValidationError>;
}

macro_rules! impl_region_wire_input {
    ($($message:ty),+ $(,)?) => {
        $(
            impl RegionWireInput for $message {
                fn sequence(&self) -> u64 {
                    self.metadata.sequence
                }

                fn validate_region(&self) -> Result<(), RegionInputValidationError> {
                    self.validate()
                }
            }
        )+
    };
}

impl_region_wire_input!(
    RegionPointerEnterMsg,
    RegionPointerLeaveMsg,
    RegionPointerMotionMsg,
    RegionPointerButtonMsg,
    RegionPointerScrollMsg,
    RegionPenEventMsg,
);

fn dispatch_region_input<T, F>(
    text: &str,
    input_sequence: &mut InputSequenceTracker,
    inject: F,
) -> Result<(), String>
where
    T: RegionWireInput,
    F: FnOnce(&T) -> Result<(), InputError>,
{
    let message: T = serde_json::from_str(text).map_err(|error| error.to_string())?;
    message
        .validate_region()
        .map_err(|error| error.to_string())?;
    let sequence = message.sequence();
    if sequence <= input_sequence.last_nonzero() {
        return Err(format!(
            "duplicate or out-of-order input sequence {sequence}; previous {}",
            input_sequence.last_nonzero()
        ));
    }
    inject(&message).map_err(|error| error.to_string())?;
    if !input_sequence.accept(sequence) {
        return Err(format!(
            "input sequence {sequence} became stale during region injection"
        ));
    }
    Ok(())
}

fn parse_sequenced_input<T>(
    text: &str,
    input_sequence: &mut InputSequenceTracker,
) -> Result<T, String>
where
    T: DeserializeOwned + SequencedInput,
{
    let message: T = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let sequence = message.sequence();
    if !input_sequence.accept(sequence) {
        return Err(format!(
            "duplicate or out-of-order input sequence {sequence}; previous {}",
            input_sequence.last_nonzero()
        ));
    }
    Ok(message)
}

/// Parses and validates one `PenEventMsg` before ever advancing the shared
/// [`InputSequenceTracker`], unlike `parse_sequenced_input` (no other
/// dispatched message type carries `PenEventMsg::validate`'s finite/
/// range-checked domain rules). A malformed or out-of-range payload is
/// rejected here and never reaches sequence acceptance or native injection.
fn parse_sequenced_pen_event(
    text: &str,
    input_sequence: &mut InputSequenceTracker,
) -> Result<PenEventMsg, String> {
    let message: PenEventMsg = serde_json::from_str(text).map_err(|error| error.to_string())?;
    message.validate().map_err(|error| error.to_string())?;
    let sequence = message.sequence;
    if !input_sequence.accept(sequence) {
        return Err(format!(
            "duplicate or out-of-order input sequence {sequence}; previous {}",
            input_sequence.last_nonzero()
        ));
    }
    Ok(message)
}

fn timezone_echo_mismatch(authoritative: Option<&IanaTimeZone>, echoed: Option<&str>) -> bool {
    authoritative.map(IanaTimeZone::as_str) != echoed
}

/// Wall-clock milliseconds since the Unix epoch (u64).
fn now_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Wire timestamp: `int(time.time()*1000) & 0xFFFFFFFF` (matches Python).
fn now_ms_u32() -> u32 {
    (now_ms_u64() & 0xFFFF_FFFF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capenc::{test_support::fake_idr, ResolvedMediaPlan, StdinCmd};
    use arcen_protocol::messages::{
        ClientQosSampleMsg, ClientTelemetrySnapshotMsg, SampleWindowSecs,
    };
    use arcen_protocol::FrameType;
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::task::{Context, Poll};

    #[test]
    fn auth_time_grading_request_resolves_before_multi_monitor_quality_admission() {
        let config = Config {
            bit_depth: BitDepth::Ten,
            color_range: ColorRange::Full,
            color_matrix: ColorMatrix::Bt709,
            color_policy: crate::cli::ColorPolicy::DefaultOff,
            ..Config::default()
        };
        assert_eq!(config.chroma, "yuv420");

        let mut response = AuthResponse::pam("artist", "credential");
        response.initial_video = Some(arcen_protocol::messages::InitialVideoRequestMsg {
            quality: QualitySettings {
                max_fps: 30,
                codec: "h265".to_string(),
                chroma: "yuv444".to_string(),
                video_selection: arcen_protocol::messages::VideoSelectionIntent::ColorFidelity,
                bit_depth: "10".to_string(),
                color_range: "full".to_string(),
                color_matrix: "bt709".to_string(),
                encode_intent: "quality".to_string(),
                ..QualitySettings::default()
            },
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                yuv444: true,
                main10: true,
                full_range: true,
                ..arcen_protocol::messages::ClientVideoCapabilitiesMsg::default()
            },
        });

        let resolved =
            config_with_initial_video_request(&config, &response).expect("grading request");
        assert_eq!(resolved.codec, "h265");
        assert_eq!(resolved.chroma, "yuv444");
        assert_eq!(resolved.bit_depth, BitDepth::Ten);
        assert_eq!(resolved.color_range, ColorRange::Full);
        assert_eq!(resolved.requested_encode_intent(), EncodeIntent::Quality);
    }

    #[tokio::test]
    async fn completed_join_handle_is_not_polled_twice_during_teardown() {
        let mut completed = tokio::spawn(async { 7_u8 });
        assert_eq!((&mut completed).await.unwrap(), 7);
        abort_and_reap_if_pending(&mut completed).await;

        let mut pending = tokio::spawn(std::future::pending::<()>());
        abort_and_reap_if_pending(&mut pending).await;
        assert!(pending.is_finished());
    }

    /// A rejection reason is shown to the operator verbatim by the Deck, so it
    /// has to actually arrive and actually help.
    ///
    /// The empty case is the one worth pinning. `TabletModeReason::try_from`
    /// refuses anything over `MAX_TABLET_MODE_REASON_BYTES`, and the call site
    /// ends in `unwrap_or_default()` — so a reason edited past the limit does
    /// not fail loudly, it silently becomes the empty string and the operator
    /// is told nothing at all.
    fn assert_reason_reaches_the_operator(reason: &str) {
        assert!(
            !reason.is_empty(),
            "reason was dropped; it is almost certainly over \
             MAX_TABLET_MODE_REASON_BYTES and was replaced by the default"
        );
        assert!(
            reason.len() <= arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES,
            "reason is {} bytes, over the {} the wire allows",
            reason.len(),
            arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES
        );
        assert!(
            reason.contains("Tablet support"),
            "a rejection must name the mode the operator should use instead: {reason}"
        );
    }

    #[test]
    fn linux_tablet_mode_resolution_keeps_native_bridge_explicit() {
        let client = TabletModeCapabilitiesMsg {
            local_termination: InputCapabilityAvailability::Available,
            wacom_usb_bridge: InputCapabilityAvailability::Available,
            disabled_mouse_compat: InputCapabilityAvailability::Available,
        };
        let host = TabletModeCapabilitiesMsg {
            local_termination: InputCapabilityAvailability::Available,
            wacom_usb_bridge: InputCapabilityAvailability::Unavailable,
            disabled_mouse_compat: InputCapabilityAvailability::Available,
        };
        let local = resolve_linux_tablet_mode_result(TabletModeMsg::LocalTermination, client, host);
        assert!(local.accepted);
        assert_eq!(local.active, TabletModeMsg::LocalTermination);

        let bridge = resolve_linux_tablet_mode_result(TabletModeMsg::WacomUsbBridge, client, host);
        assert!(!bridge.accepted);
        assert_eq!(bridge.active, TabletModeMsg::DisabledMouseCompat);
        assert_reason_reaches_the_operator(bridge.reason.as_str());
        assert!(bridge.reconnect_required);

        let disabled =
            resolve_linux_tablet_mode_result(TabletModeMsg::DisabledMouseCompat, client, host);
        assert!(disabled.accepted);
        assert_eq!(disabled.active, TabletModeMsg::DisabledMouseCompat);

        let unavailable_client = TabletModeCapabilitiesMsg {
            local_termination: InputCapabilityAvailability::Unavailable,
            ..client
        };
        let rejected_local = resolve_linux_tablet_mode_result(
            TabletModeMsg::LocalTermination,
            unavailable_client,
            host,
        );
        assert!(!rejected_local.accepted);
        assert_eq!(rejected_local.active, TabletModeMsg::DisabledMouseCompat);
        assert!(rejected_local.reason.as_str().contains("client did not"));
    }

    /// Re-review finding #2/#1: reloaded QoS targets must publish whenever
    /// the profile/QoS state itself was committed (`SighupOutcome::
    /// state_committed`), independent of TLS or any other combined SIGHUP
    /// maintenance outcome — including a managed-log reopen or archive
    /// cleanup failure reported in the very same `handle_sighup` call.
    /// Active and future sessions must never miss a corrected threshold
    /// just because an unrelated step failed in the same signal.
    #[test]
    fn qos_targets_publish_is_gated_on_state_committed_alone() {
        let options = crate::logging::LoggingOptions::from_args(&[]).expect("logging options");
        let log_controller = crate::logging::init(&options).expect("log controller");
        let qos_targets = RwLock::new(QosTargets::default());
        let (emitter, _recorded) = LifecycleEmitter::recording();

        assert!(
            publish_qos_targets_if_logging_reloaded(
                &crate::logging::SighupOutcome::for_test(true, Vec::new()),
                &emitter,
                &log_controller,
                &qos_targets,
            ),
            "a committed state must publish qos_targets even if this SIGHUP had no other errors"
        );
        assert!(
            publish_qos_targets_if_logging_reloaded(
                &crate::logging::SighupOutcome::for_test(
                    true,
                    vec!["reopen managed log: ENOTDIR".to_string()],
                ),
                &emitter,
                &log_controller,
                &qos_targets,
            ),
            "a committed state must publish qos_targets even if an unrelated reopen/cleanup/TLS \
             step in the same SIGHUP failed"
        );
        assert!(
            !publish_qos_targets_if_logging_reloaded(
                &crate::logging::SighupOutcome::for_test(
                    false,
                    vec!["reload observability profile: parse error".to_string()],
                ),
                &emitter,
                &log_controller,
                &qos_targets,
            ),
            "an uncommitted state must never publish a stale/partial qos_targets value"
        );
    }

    /// Re-review finding #6: an IPv6 peer's zone/scope id must survive
    /// extraction from the real connection `SocketAddr`, since it is the
    /// only signal that can disambiguate a link-local peer's owning
    /// interface later at the network-probe call sites. An IPv4 peer, or
    /// an IPv6 peer with an unspecified (`0`) scope id, must normalize to
    /// `None` rather than a misleading `Some(0)`.
    #[test]
    fn peer_scope_id_extracts_ipv6_zone_and_normalizes_ipv4_and_unspecified_to_none() {
        let v6_scoped = SocketAddr::V6(std::net::SocketAddrV6::new(
            "fe80::1".parse().unwrap(),
            9000,
            0,
            7,
        ));
        assert_eq!(peer_scope_id(v6_scoped), Some(7));

        let v6_unscoped = SocketAddr::V6(std::net::SocketAddrV6::new(
            "2001:db8::1".parse().unwrap(),
            9000,
            0,
            0,
        ));
        assert_eq!(
            peer_scope_id(v6_unscoped),
            None,
            "an unspecified (0) scope id must normalize to None, not Some(0)"
        );

        let v4: SocketAddr = "203.0.113.9:9000".parse().unwrap();
        assert_eq!(peer_scope_id(v4), None);
    }

    /// Re-review finding #5: the Level2 (Info) 10s aggregation window must
    /// sum each tick's own delta sample, never a raw cumulative
    /// `HostCounters` total — otherwise the reported `input_events` would
    /// grow unbounded with session lifetime instead of reflecting only the
    /// last five 2s ticks.
    #[test]
    fn health_window_sums_deltas_not_cumulative_totals() {
        let mut window_frames_sent = 0;
        let mut window_frames_dropped = 0;
        let mut window_input_events = 0;
        let mut window_worst_overall = None;

        // Three ticks, each carrying only its own delta (as
        // `SessionHealth::observe` computes via `saturating_sub`), never the
        // ever-growing cumulative counter a regression might pass instead.
        for delta in [100_u64, 40, 60] {
            let sample = QosSample {
                input_events: Some(delta),
                ..QosSample::default()
            };
            accumulate_health_window(
                &mut window_frames_sent,
                &mut window_frames_dropped,
                &mut window_input_events,
                &mut window_worst_overall,
                &sample,
                None,
            );
        }
        assert_eq!(
            window_input_events, 200,
            "the window must sum the three per-tick deltas (100+40+60), not a cumulative total \
             (which would read >= the largest single cumulative counter value observed, e.g. \
             far above 200 for a long-running session)"
        );
    }

    /// Re-review finding #4: health-state transitions must carry the same
    /// explicit top-level session identity (`user`/`peer_addr`) as the 60s
    /// `HEALTH_SNAPSHOT`, routed through `emit_lifecycle_event_with_context`
    /// rather than the identity-less `emit_lifecycle_event` default.
    #[test]
    fn health_transition_uses_top_level_identity_not_the_identity_less_default() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitter, runtime) = crate::eventlog::test_support::emitter_with_writer(
            SharedBufferWriter(Arc::clone(&buffer)),
        );
        let context = emitter.session_context(
            test_session_log_id(),
            Some("alice".to_string()),
            Some("203.0.113.5:5900".to_string()),
            Some(arcen_telemetry::HealthState::Degraded),
        );

        crate::emit_lifecycle_event_with_context(
            &emitter,
            LifecycleEventKind::HealthDegraded,
            context,
            {
                let mut fields = StructuredFields::default();
                let _ = fields.insert("dominant_cause", FieldValue::String("fps".to_string()));
                let _ = fields.insert("value", FieldValue::Integer(42));
                fields
            },
        );
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush canonical sink");

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text.lines().next().expect("one canonical JSON line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(value["user"], serde_json::json!("alice"));
        assert_eq!(value["peer_addr"], serde_json::json!("203.0.113.5:5900"));
        assert!(
            !value["fields"]
                .as_object()
                .map(|fields| fields.contains_key("sid")
                    || fields.contains_key("user")
                    || fields.contains_key("peer_addr"))
                .unwrap_or(false),
            "identity must stay top-level, never nested inside the event's own field set"
        );
    }

    /// Finding #2 (this round): the Level2 (Info) 10s QoS window summary
    /// must carry the same explicit top-level `sid`/`user`/`host`/
    /// `peer_addr` identity context as `HEALTH_DEGRADED`/`HEALTH_SNAPSHOT`
    /// — via `LifecycleEmitter::emit_summary`'s ad-hoc `CanonicalRecord`
    /// path, never a `tracing` diagnostic macro whose fields carry no
    /// top-level identity promotion.
    #[test]
    fn qos_summary_uses_top_level_identity_via_canonical_record() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitter, runtime) = crate::eventlog::test_support::emitter_with_writer(
            SharedBufferWriter(Arc::clone(&buffer)),
        );
        let context = emitter.session_context(
            test_session_log_id(),
            Some("alice".to_string()),
            Some("203.0.113.5:5900".to_string()),
            Some(arcen_telemetry::HealthState::Ok),
        );

        emitter.emit_summary(
            context,
            TelemetryTarget::new(arcen_telemetry::names::target::HEALTH)
                .expect("HEALTH is a canonical arcen:: target"),
            "session health 10s summary",
            OperationalProfile::Info,
            crate::observability::window_summary_fields(
                5,
                500,
                12,
                80,
                Some(arcen_telemetry::HealthState::Ok),
            ),
        );
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush canonical sink");

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text.lines().next().expect("one canonical JSON line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(value["user"], serde_json::json!("alice"));
        assert_eq!(value["peer_addr"], serde_json::json!("203.0.113.5:5900"));
        assert_eq!(value["profile_level"], serde_json::json!(2));
        assert_eq!(value["severity"], serde_json::json!("info"));
        assert_eq!(value["fields"]["frames_sent"], serde_json::json!(500));
        assert_eq!(value["fields"]["input_events"], serde_json::json!(80));
        assert!(
            !value["fields"]
                .as_object()
                .map(|fields| fields.contains_key("sid")
                    || fields.contains_key("user")
                    || fields.contains_key("peer_addr"))
                .unwrap_or(false),
            "identity must stay top-level, never nested inside the summary's own field set"
        );
    }

    /// Fix (this round): `emit_summary` must route through
    /// `ObservabilityHandle::emit_ad_hoc`, which assigns the *same*
    /// process-wide canonical `sequence` counter used by
    /// `emit_lifecycle`/`emit_context` — a 10s QoS summary interleaved
    /// with `ServiceStart`/`ServiceStop` lifecycle events must produce
    /// three unique, strictly increasing `sequence` values in one
    /// canonical stream, never a separate self-consistent-but-disjoint
    /// counter.
    #[test]
    fn qos_summary_and_lifecycle_events_share_one_increasing_sequence() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitter, runtime) = crate::eventlog::test_support::emitter_with_writer(
            SharedBufferWriter(Arc::clone(&buffer)),
        );

        crate::emit_lifecycle_event(
            &emitter,
            LifecycleEventKind::ServiceStart,
            eventlog::random_correlation_id(),
            {
                let mut fields = StructuredFields::default();
                let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
                fields
            },
        );

        let summary_context = emitter.session_context(
            test_session_log_id(),
            Some("alice".to_string()),
            Some("203.0.113.5:5900".to_string()),
            Some(arcen_telemetry::HealthState::Ok),
        );
        emitter.emit_summary(
            summary_context,
            TelemetryTarget::new(arcen_telemetry::names::target::HEALTH)
                .expect("HEALTH is a canonical arcen:: target"),
            "session health 10s summary",
            OperationalProfile::Info,
            crate::observability::window_summary_fields(
                5,
                500,
                12,
                80,
                Some(arcen_telemetry::HealthState::Ok),
            ),
        );

        crate::emit_lifecycle_event(
            &emitter,
            LifecycleEventKind::ServiceStop,
            eventlog::random_correlation_id(),
            {
                let mut fields = StructuredFields::default();
                let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
                let _ = fields.insert(
                    "reason_class",
                    FieldValue::String("signal_shutdown".to_string()),
                );
                let _ = fields.insert("uptime_ms", FieldValue::Integer(0));
                fields
            },
        );

        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush canonical sink");

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let sequences: Vec<u64> = text
            .lines()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
                value["sequence"]
                    .as_u64()
                    .expect("every canonical record carries a numeric sequence")
            })
            .collect();
        assert_eq!(
            sequences.len(),
            3,
            "ServiceStart, the QoS summary, and ServiceStop must each produce one record"
        );
        let mut sorted = sequences.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            sequences.len(),
            "every sequence value must be unique: {sequences:?}"
        );
        assert!(
            sequences.windows(2).all(|pair| pair[0] < pair[1]),
            "sequence values must be strictly increasing in emission order: {sequences:?}"
        );
    }

    fn microphone_frame(
        codec: AudioCodec,
        sequence: u32,
        timestamp_ms: u32,
        generation: u32,
    ) -> Vec<u8> {
        let header = arcen_protocol::encode_microphone_header(arcen_protocol::MicrophoneHeader {
            codec,
            sequence,
            timestamp_ms,
            generation,
        })
        .unwrap();
        let payload_len = match codec {
            AudioCodec::Pcm => arcen_protocol::MICROPHONE_PCM_BYTES,
            AudioCodec::Opus => 32,
        };
        let mut frame = header.to_vec();
        frame.resize(frame.len() + payload_len, 0x5a);
        frame
    }

    #[test]
    fn microphone_ingress_rejects_large_malformed_and_wrong_codec_frames() {
        let mut ingress = MicrophoneIngressValidator::new(AudioCodec::Pcm, 41);
        assert_eq!(
            ingress.validate(&vec![0; MAX_INBOUND_WS_MESSAGE]),
            Err(MicrophoneIngressRejection::Malformed)
        );
        assert_eq!(
            ingress.validate(&[FrameType::AudioUpstream as u8]),
            Err(MicrophoneIngressRejection::Malformed)
        );
        assert_eq!(
            ingress.validate(&microphone_frame(AudioCodec::Opus, 1, 20, 41)),
            Err(MicrophoneIngressRejection::Codec)
        );
        assert!(ingress
            .validate(&microphone_frame(AudioCodec::Pcm, 1, 20, 41))
            .is_ok());
    }

    #[test]
    fn microphone_generation_is_restart_unique_and_rejects_stale_replay() {
        let before_restart = microphone_generation_from_entropy([1, 2, 3, 4]).unwrap();
        let after_restart = microphone_generation_from_entropy([4, 3, 2, 1]).unwrap();
        assert_ne!(before_restart, after_restart);
        assert!(microphone_generation_from_entropy([0; 4]).is_none());

        let mut ingress = MicrophoneIngressValidator::new(AudioCodec::Pcm, after_restart);
        assert_eq!(
            ingress.validate(&microphone_frame(AudioCodec::Pcm, 1, 20, before_restart)),
            Err(MicrophoneIngressRejection::Sequence(
                MicrophoneFrameDecision::WrongGeneration
            ))
        );
        assert!(ingress
            .validate(&microphone_frame(AudioCodec::Pcm, 1, 20, after_restart))
            .is_ok());
        assert_eq!(
            ingress.validate(&microphone_frame(AudioCodec::Pcm, 1, 20, after_restart)),
            Err(MicrophoneIngressRejection::Sequence(
                MicrophoneFrameDecision::Duplicate
            ))
        );
    }

    #[test]
    fn server_shutdown_budget_covers_complete_microphone_cleanup() {
        assert!(SHUTDOWN_DISPLAY_DRAIN_TIMEOUT > crate::microphone_input::MICROPHONE_CLEANUP_BOUND);
    }

    #[test]
    fn microphone_telemetry_queue_accumulates_and_resets_drop_count() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let sink = LinuxTransportTelemetrySink {
            sender,
            dropped: Arc::clone(&dropped),
        };
        sink.try_snapshot(MicrophoneStats::default(), false, Duration::ZERO, "running");
        sink.try_snapshot(MicrophoneStats::default(), false, Duration::ZERO, "running");
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        let first = receiver.recv().unwrap();
        assert_eq!(first.stats.telemetry_drops, 0);
        sink.try_snapshot(MicrophoneStats::default(), true, Duration::ZERO, "test");
        let final_snapshot = receiver.recv().unwrap();
        assert_eq!(final_snapshot.stats.telemetry_drops, 1);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn rejected_discontinuity_does_not_move_microphone_ordering_anchor() {
        let mut ingress = MicrophoneIngressValidator::new(AudioCodec::Pcm, 41);
        assert!(ingress
            .validate(&microphone_frame(AudioCodec::Pcm, 1, 20, 41))
            .is_ok());
        assert_eq!(
            ingress.validate(&microphone_frame(AudioCodec::Pcm, 100, 2_000, 41)),
            Err(MicrophoneIngressRejection::Sequence(
                MicrophoneFrameDecision::Discontinuity
            ))
        );
        assert!(
            MicrophoneIngressRejection::Sequence(MicrophoneFrameDecision::Discontinuity)
                .terminates_stream()
        );
        assert_eq!(
            ingress.validate(&microphone_frame(AudioCodec::Pcm, 101, 2_020, 41)),
            Err(MicrophoneIngressRejection::Sequence(
                MicrophoneFrameDecision::Discontinuity
            ))
        );
        assert!(
            !MicrophoneIngressRejection::Sequence(MicrophoneFrameDecision::WrongGeneration)
                .terminates_stream()
        );
    }

    #[test]
    fn parse_display_update_accepts_only_its_own_type() {
        let update = parse_display_update(
            r#"{"type":"display_update","sequence":3,"width":1512,"height":944,"scale":1.0,"reason":"fullscreen"}"#,
        )
        .expect("well-formed display_update parses");
        assert_eq!(update.sequence, 3);
        assert_eq!(update.width, 1512);
        assert_eq!(update.height, 944);
        // Every other control type — even ones that parse structurally thanks
        // to serde defaults — must not be treated as a resize.
        assert!(parse_display_update(r#"{"type":"quality_settings"}"#).is_none());
        assert!(parse_display_update(r#"{"type":"health_ping"}"#).is_none());
        assert!(parse_display_update("not json").is_none());
    }

    #[tokio::test]
    async fn display_update_result_reports_actual_streaming_size() {
        let (sender, mut receiver) = ControlSender::channel(3);
        send_display_update_result(&sender, 9, false, 1512, 982, "invalid resolution");
        let message = receiver.recv().await.expect("ack queued");
        let WriterControl::Message(Message::Text(text)) = message else {
            panic!("ack must be a text frame");
        };
        let ack: DisplayUpdateResultMsg = serde_json::from_str(&text).unwrap();
        assert_eq!(ack.sequence, 9);
        assert!(!ack.accepted);
        assert_eq!((ack.width, ack.height), (1512, 982));
        assert_eq!(ack.message, "invalid resolution");
    }

    #[test]
    fn accepted_display_update_result_has_exact_v3_json() {
        assert_eq!(
            authoritative_display_update_result_json(9, 1512, 982).as_deref(),
            Some(
                r#"{"type":"display_update_result","sequence":9,"accepted":true,"width":1512,"height":982,"message":""}"#
            )
        );
    }

    #[test]
    fn resize_contract_allows_only_geometry_to_change() {
        let current = resolved_media_plan();
        let mut resized = current;
        resized.width = 1280;
        resized.height = 720;
        assert!(resize_contract_matches(current, resized));
        assert_eq!(
            concrete_encoder_for(current),
            Some(EncoderRequest::NativeNvenc)
        );

        resized.backend = EncoderBackend::OpenH264;
        assert!(!resize_contract_matches(current, resized));
        assert_eq!(
            concrete_encoder_for(resized),
            Some(EncoderRequest::SoftwareH264)
        );
        resized.backend = EncoderBackend::WindowsMediaFoundation;
        assert_eq!(concrete_encoder_for(resized), None);
    }

    #[tokio::test]
    async fn writer_barrier_follows_earlier_control() {
        let (sender, mut receiver) = ControlSender::channel(2);
        assert!(
            sender
                .send_required(Message::Text("before".into()), "test")
                .await
        );
        let barrier = tokio::spawn({
            let sender = sender.clone();
            async move { sender.barrier().await }
        });

        assert!(matches!(
            receiver.recv().await,
            Some(WriterControl::Message(Message::Text(text))) if text == "before"
        ));
        let Some(WriterControl::Barrier(complete)) = receiver.recv().await else {
            panic!("barrier must follow earlier control");
        };
        complete.send(()).expect("barrier waiter remains live");
        assert!(barrier.await.expect("barrier task completes"));
    }

    #[test]
    fn refresh_interval_is_exact_half_window_in_milliseconds() {
        assert_eq!(resume_refresh_interval(1), Duration::from_millis(500));
        assert_eq!(resume_refresh_interval(3), Duration::from_millis(1_500));
        assert_eq!(resume_refresh_interval(7_200), Duration::from_secs(3_600));
        assert_eq!(resume_refresh_interval(0), Duration::from_millis(1));
    }

    #[test]
    fn resumable_write_timeout_is_half_window_capped_by_existing_timeout() {
        assert_eq!(resumable_write_timeout(0), Duration::from_millis(1));
        assert_eq!(resumable_write_timeout(1), Duration::from_millis(500));
        assert_eq!(resumable_write_timeout(3), Duration::from_millis(1_500));
        assert_eq!(resumable_write_timeout(60), WS_SEND_TIMEOUT);
    }

    #[test]
    fn cleanup_time_consumes_loss_anchored_window_and_exact_deadline_drains() {
        let loss_observed_at = MonotonicMillis::new(10_000);
        let end = AttachmentEnd {
            reason: SessionEndReason::TransportError,
            transport_loss_observed_at: Some(loss_observed_at),
            reached_usable: true,
        };
        let mut reconnect = DirectReconnect::new(ReconnectPolicy::new(1).unwrap());
        let _ = reconnect.apply(
            ReconnectEvent::UnexpectedLoss,
            end.transport_loss_observed_at.unwrap(),
        );
        let (deadline, timer_generation) = match reconnect.state() {
            ReconnectState::Detached {
                deadline,
                timer_generation,
            } => (deadline, timer_generation),
            state => panic!("unexpected reconnect state: {state:?}"),
        };
        assert_eq!(deadline, MonotonicMillis::new(11_000));

        let actions = reconnect.apply(
            ReconnectEvent::DeadlineReached { timer_generation },
            deadline,
        );
        assert!(actions.restore_leases);
        assert!(actions.stop_media);
        assert!(actions.revoke_grant);
        assert!(!actions.hold_restore_leases);
    }

    #[test]
    fn resume_route_never_falls_through_to_pam() {
        for mut response in [
            AuthResponse::resume("malformed", "malformed"),
            AuthResponse::resume("00".repeat(32), "tampered"),
        ] {
            response.credential = "must-not-reach-pam".to_string();
            assert_eq!(
                authentication_route(&response),
                AuthenticationRoute::ResumeRegistry
            );
            assert!(auth::validate_pam_response(&response).is_err());
        }
        assert_eq!(
            authentication_route(&AuthResponse::pam("artist", "password")),
            AuthenticationRoute::Pam
        );
    }

    #[test]
    fn resume_is_advertised_only_when_wired_and_enabled() {
        assert!(!build_auth_request(None, false, false, None).supports_resume());
        let initial = build_auth_request(None, true, false, None);
        assert!(initial.supports_resume());
        assert!(initial.multi_monitor_v1_offer().is_none());
        assert!(!serde_json::to_value(&initial)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("multi_monitor_v1"));
        let detached = build_auth_request(None, true, true, None);
        assert!(detached.supports_resume());
        assert!(detached.disclaimer.is_none());
    }

    #[test]
    fn build_auth_request_serializes_a_present_multi_monitor_offer() {
        use arcen_protocol::messages::{
            AuthMultiMonitorOfferMsg, MultiMonitorCarrierMsg, RotationMsg,
        };
        let offer = AuthMultiMonitorOfferMsg::new(
            2,
            vec![RotationMsg::Degrees0],
            vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
        )
        .expect("valid offer");
        let request = build_auth_request(None, false, false, Some(offer));
        assert!(request.multi_monitor_v1_offer().is_some());
        assert!(serde_json::to_value(&request)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("multi_monitor_v1"));
    }

    #[test]
    fn multi_monitor_gate_defaults_to_disabled_from_default_config() {
        let gate = multi_monitor_gate(&Config::default());
        assert!(!gate.advertise_enabled);
        assert!(gate.inventory.is_none());
    }

    #[test]
    fn multi_monitor_gate_degrades_to_disabled_when_config_is_invalid() {
        // CLI/config-load-time validation already rejects unknown head
        // tokens (see `cli::validate_gpu_head`), so this exercises the
        // helper's own defensive fallback in case that earlier validation is
        // ever bypassed (e.g. a future hot-reload path).
        let config = Config {
            multi_monitor: crate::config::LinuxMultiMonitorConfig {
                advertise_enabled: true,
                heads: vec!["not-a-real-head".to_owned()],
                ..crate::config::LinuxMultiMonitorConfig::default()
            },
            ..Config::default()
        };
        let gate = multi_monitor_gate(&config);
        assert!(!gate.advertise_enabled);
        assert!(gate.inventory.is_none());
    }

    #[test]
    fn multi_monitor_offer_is_produced_through_the_server_wiring_once_fully_opted_in() {
        // Full operator opt-in now genuinely produces an offer: the
        // hardcoded carrier gate in `media::multi_capenc` is `true` now that
        // Carrier A is fully wired end to end, and an explicit encoder pin
        // (required by the fail-closed encoder-policy gate: `Auto`/`Mf`
        // withhold the offer) is the only remaining thing standing between
        // legacy behavior and an offer.
        let config = Config {
            multi_monitor: crate::config::LinuxMultiMonitorConfig {
                advertise_enabled: true,
                heads: vec!["DFP-0".to_owned(), "DFP-1".to_owned()],
                ..crate::config::LinuxMultiMonitorConfig::default()
            },
            encoder: EncoderRequest::NativeNvenc,
            ..Config::default()
        };
        let gate = multi_monitor_gate(&config);
        assert!(gate.advertise_enabled);
        assert!(multi_monitor::build_offer(&gate).is_some());
    }

    #[test]
    fn resumed_attachment_forces_fresh_idr_and_only_transport_loss_detaches() {
        assert!(attachment_requires_fresh_idr(true));
        assert!(!attachment_requires_fresh_idr(false));
        assert!(resumable_transport_loss(SessionEndReason::TransportError));
        assert!(resumable_transport_loss(
            SessionEndReason::ReadLivenessTimeout
        ));
        assert!(resumable_transport_loss(SessionEndReason::WriterEnded));
        assert!(!resumable_transport_loss(SessionEndReason::ClientClosed));
        assert!(!resumable_transport_loss(SessionEndReason::MediaEnded));
        assert!(!resumable_transport_loss(SessionEndReason::HostShutdown));
    }

    #[test]
    fn resolve_input_raster_prefers_a_committed_multi_monitor_plans_combined_virtual_size() {
        // Even when a stale display-guard raster or a narrower primary-only
        // media plan size is also available, the combined multi-monitor
        // virtual desktop always wins: the absolute pointer/pen device must
        // span every applied monitor, not just the primary one.
        assert_eq!(
            resolve_input_raster(Some((7680, 1080)), Some((1920, 1080)), (2560, 1440)),
            (7680, 1080)
        );
        assert_eq!(
            resolve_input_raster(Some((3200, 1080)), None, (1920, 1080)),
            (3200, 1080)
        );
    }

    #[test]
    fn resolve_input_raster_falls_back_to_the_display_guards_raster_without_a_multi_monitor_plan() {
        assert_eq!(
            resolve_input_raster(None, Some((1920, 1080)), (1280, 720)),
            (1920, 1080)
        );
    }

    #[test]
    fn resolve_input_raster_falls_back_to_the_media_plan_size_legacy_behavior_unaffected() {
        assert_eq!(resolve_input_raster(None, None, (1920, 1080)), (1920, 1080));
    }

    fn three_head_topology_plan() -> LinuxTopologyPlan {
        use crate::display::topology::LinuxMonitorPlan;
        use arcen_media::{Rotation, TopologyGeneration};
        let head = |session_monitor_id: u16, name: &str, x: i32, primary: bool| LinuxMonitorPlan {
            session_monitor_id: SessionMonitorId::new(session_monitor_id)
                .expect("nonzero session monitor id"),
            client_display_id: name.to_owned(),
            head: format!("DFP-{}", session_monitor_id - 1),
            x,
            y: 0,
            width: 1920,
            height: 1080,
            logical_rect: arcen_media::LogicalRect::new(
                arcen_media::LogicalPoint::from_pixels(i64::from(x), 0).expect("logical origin"),
                arcen_media::LogicalSize::from_pixels(1920, 1080).expect("logical size"),
            )
            .expect("logical rect"),
            physical_size: arcen_media::PhysicalSize::new(1920, 1080).expect("physical size"),
            scale: arcen_media::Scale120::new(120).expect("unit scale"),
            rotation: Rotation::Degrees0,
            primary,
            quality_intent: arcen_protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized,
            mode_token: "1920x1080".to_owned(),
        };
        LinuxTopologyPlan {
            generation: TopologyGeneration::new(1).expect("nonzero generation"),
            virtual_width: 5760,
            virtual_height: 1080,
            monitors: vec![
                head(1, "primary", 0, true),
                head(2, "second", 1920, false),
                head(3, "third", 3840, false),
            ],
        }
    }

    #[test]
    fn skip_metamode_resize_for_multi_monitor_is_true_for_a_committed_three_head_plan() {
        // The exact gate `run_ws` checks before ever constructing an
        // `NvControl`/calling `MetaModeGuard::apply_with_hold` — proving a
        // committed 3-head plan (the same plan a fresh attach *and* a
        // reconnect both carry, per `SessionRegistry::acquire` reusing the
        // original `Create`'s plan unchanged) always takes the skip path,
        // so the legacy single-viewport MetaMode resize — and therefore
        // `build_viewport_metamode` — is never reached for either attach or
        // reconnect.
        let plan = three_head_topology_plan();
        assert!(skip_metamode_resize_for_multi_monitor(Some(&plan)));
    }

    #[test]
    fn skip_metamode_resize_for_multi_monitor_is_false_without_a_committed_plan() {
        // Legacy single-monitor sessions (no committed multi-monitor plan
        // in the lease, or no lease at all) are unaffected: they still take
        // the resize branch exactly as before this fix.
        assert!(!skip_metamode_resize_for_multi_monitor(None));
    }

    #[test]
    fn an_admitted_topology_reattached_to_a_plan_less_desktop_is_refused() {
        // The exact pier-linux.example.internal combination behind the reported
        // "host reported v4 / Unavailable": a Match-My-Layout connect whose
        // topology *was* admitted, reattaching inside the reconnect window to
        // a desktop an earlier plan-less connect created. ADR-0009 makes
        // admission atomic, so this must refuse rather than silently serve
        // the single-primary subset.
        assert!(admitted_multi_monitor_topology_was_discarded(Some(2), None));
    }

    #[test]
    fn an_admitted_topology_that_was_committed_is_not_refused() {
        let plan = three_head_topology_plan();
        assert!(!admitted_multi_monitor_topology_was_discarded(
            Some(3),
            Some(&plan)
        ));
    }

    #[test]
    fn a_session_that_never_offered_multi_monitor_is_not_refused() {
        // Every legacy/single-primary connect: nothing was admitted, so there
        // is nothing to have been discarded and the legacy path is untouched.
        assert!(!admitted_multi_monitor_topology_was_discarded(None, None));
    }

    #[test]
    fn the_multi_monitor_reattach_refusal_survives_the_close_frame_budget() {
        // The refusal names the user's recovery step, so it is only useful if
        // it reaches the client whole: `close_with_reason` silently truncates
        // at 120 bytes.
        let Message::Close(Some(frame)) = close_with_reason(MULTI_MONITOR_REATTACH_REFUSAL) else {
            panic!("close_with_reason must produce a close frame with a reason");
        };
        assert_eq!(frame.reason.as_ref(), MULTI_MONITOR_REATTACH_REFUSAL);
    }

    #[test]
    fn multi_monitor_attachment_must_terminate_desktop_when_it_never_became_usable() {
        // The very first attachment against a freshly committed plan
        // (never yet proven usable) that failed to reach a usable state
        // (capenc/READY/mux/applied-capability/ServerHello never all
        // succeeded) must never be `disconnect`-preserved for reconnect:
        // the exact same broken plan would just be retried and fail
        // identically forever. A subsequent retry (fresh `Create`) starts
        // from a fresh, unproven plan/session — see
        // `multi_monitor_ever_usable_defaults_false_for_a_fresh_desktop` in
        // `session::lifecycle`.
        let plan = three_head_topology_plan();
        assert!(multi_monitor_attachment_must_terminate_desktop(
            false,
            false,
            Some(&plan)
        ));
    }

    #[test]
    fn multi_monitor_attachment_must_terminate_desktop_is_false_once_usable() {
        // Once the attachment genuinely reached a usable state, an
        // ordinary end (client closed, transport loss, media ended mid
        // session, etc.) is just a normal disconnect — the desktop and its
        // proven-working committed plan are preserved for reconnect exactly
        // as before this fix. `ever_usable` is irrelevant here — the
        // function must never terminate once `reached_usable` is true.
        let plan = three_head_topology_plan();
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            true,
            false,
            Some(&plan)
        ));
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            true,
            true,
            Some(&plan)
        ));
    }

    #[test]
    fn multi_monitor_attachment_must_terminate_desktop_preserves_a_desktop_already_proven_usable() {
        // A *later* reconnect's own early pre-usable failure, on a desktop
        // that already reached usable at least once earlier in its
        // lifetime (`ever_usable`), must not nuke a known-healthy desktop
        // over what is far more likely an ordinary transient hiccup on
        // this one reconnect attempt — it is preserved via the existing
        // `disconnect` (persist for reconnect) policy instead, exactly
        // like a legacy session.
        let plan = three_head_topology_plan();
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            false,
            true,
            Some(&plan)
        ));
    }

    #[test]
    fn multi_monitor_attachment_must_terminate_desktop_is_false_for_legacy_single_monitor() {
        // Legacy single-monitor sessions have no committed, fixed,
        // verbatim-reused topology to poison — their media plan is always
        // recomputed fresh per attempt — so a pre-usable failure there
        // keeps the existing `disconnect` (persist for reconnect) behavior
        // completely unchanged, regardless of `reached_usable` or
        // `ever_usable`.
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            false, false, None
        ));
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            false, true, None
        ));
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            true, false, None
        ));
        assert!(!multi_monitor_attachment_must_terminate_desktop(
            true, true, None
        ));
    }

    #[test]
    fn attachment_end_terminal_never_reports_usable() {
        // Every early return inside `run_attachment` is constructed via
        // `AttachmentEnd::terminal`, which must always report
        // `reached_usable: false` — the single final `AttachmentEnd { .. }`
        // literal built after the live session-serving select loop runs is
        // the only place that ever asserts `true` (see
        // `multi_monitor_attachment_must_terminate_desktop`'s reliance on
        // this distinction).
        for reason in [
            SessionEndReason::ClientClosed,
            SessionEndReason::ReadLivenessTimeout,
            SessionEndReason::TransportError,
            SessionEndReason::ProtocolError,
            SessionEndReason::MediaEnded,
            SessionEndReason::WriterEnded,
            SessionEndReason::HostShutdown,
            SessionEndReason::ResumeAuthorityFailure,
        ] {
            assert!(!AttachmentEnd::terminal(reason).reached_usable);
        }
    }

    struct NonReadingSink;

    fn resolved_media_plan() -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend: EncoderBackend::NativeNvenc,
            video: VideoConfiguration {
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                bit_depth: BitDepth::Eight,
                range: ColorRange::Limited,
                matrix: ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            },
            width: 1920,
            height: 1080,
            fps: 60,
            codecs: arcen_media::CodecSet::from_slice(&[VideoCodec::H264, VideoCodec::H265]),
            chroma: arcen_media::ChromaSet::from_slice(&[
                ChromaSubsampling::Yuv420,
                ChromaSubsampling::Yuv444,
            ]),
            bit_depths: EncoderBackend::NativeNvenc.contract().bit_depths,
            ranges: EncoderBackend::NativeNvenc.contract().ranges,
            cursor_mode: CursorMode::Local,
            cursor_in_video: false,
        }
    }

    mod color_negotiation {
        use super::*;
        use crate::cli::ColorPolicy;
        use arcen_media::video::{cap_bit_depth_to_client, resolve_client_color_request};

        /// An elevated ceiling distinct on every axis from the conservative
        /// baseline, so a test asserting "the ceiling was served" cannot be
        /// confused with "the baseline was served".
        fn elevated_ceiling() -> ColorCeiling {
            ColorCeiling {
                bit_depth: BitDepth::Ten,
                color_range: ColorRange::Full,
                color_matrix: ColorMatrix::Bt2020Ncl,
            }
        }

        /// A client that claims every decode capability, so tests about
        /// policy precedence are not incidentally also testing the
        /// capability cross-check.
        fn permissive_client() -> ClientColorRequest {
            ClientColorRequest {
                bit_depth: None,
                color_range: None,
                color_matrix: None,
                supports_main10: true,
                supports_main12: true,
                supports_full_range: true,
                supports_identity_matrix: true,
            }
        }

        #[test]
        fn always_on_forces_the_ceiling_regardless_of_a_lesser_client_request() {
            let request = ClientColorRequest {
                bit_depth: Some(BitDepth::Eight),
                color_range: Some(ColorRange::Limited),
                color_matrix: Some(ColorMatrix::Bt709),
                ..permissive_client()
            };
            assert_eq!(
                resolve_client_color_request(ColorPolicy::AlwaysOn, elevated_ceiling(), request),
                (BitDepth::Ten, ColorRange::Full, ColorMatrix::Bt2020Ncl)
            );
        }

        #[test]
        fn always_off_forces_the_conservative_baseline_regardless_of_a_greater_client_request() {
            let request = ClientColorRequest {
                bit_depth: Some(BitDepth::Twelve),
                color_range: Some(ColorRange::Full),
                color_matrix: Some(ColorMatrix::Bt2020Ncl),
                ..permissive_client()
            };
            assert_eq!(
                resolve_client_color_request(ColorPolicy::AlwaysOff, elevated_ceiling(), request),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt709)
            );
        }

        #[test]
        fn default_on_defaults_to_the_ceiling_but_honours_an_explicit_lesser_request() {
            let no_request = permissive_client();
            assert_eq!(
                resolve_client_color_request(
                    ColorPolicy::DefaultOn,
                    elevated_ceiling(),
                    no_request
                ),
                (BitDepth::Ten, ColorRange::Full, ColorMatrix::Bt2020Ncl)
            );
            let explicit_request = ClientColorRequest {
                bit_depth: Some(BitDepth::Eight),
                color_range: Some(ColorRange::Limited),
                color_matrix: Some(ColorMatrix::Bt601),
                ..permissive_client()
            };
            assert_eq!(
                resolve_client_color_request(
                    ColorPolicy::DefaultOn,
                    elevated_ceiling(),
                    explicit_request
                ),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt601)
            );
        }

        #[test]
        fn default_off_defaults_conservative_but_lets_a_client_negotiate_up_to_the_ceiling() {
            let no_request = permissive_client();
            assert_eq!(
                resolve_client_color_request(
                    ColorPolicy::DefaultOff,
                    elevated_ceiling(),
                    no_request
                ),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt709)
            );
            let explicit_request = ClientColorRequest {
                bit_depth: Some(BitDepth::Ten),
                color_range: Some(ColorRange::Full),
                color_matrix: Some(ColorMatrix::Bt2020Ncl),
                ..permissive_client()
            };
            assert_eq!(
                resolve_client_color_request(
                    ColorPolicy::DefaultOff,
                    elevated_ceiling(),
                    explicit_request
                ),
                (BitDepth::Ten, ColorRange::Full, ColorMatrix::Bt2020Ncl)
            );
        }

        #[test]
        fn client_capability_cross_check_overrides_every_policy_including_always_on() {
            // `AlwaysOn` would otherwise force the ten-bit/full-range
            // ceiling, but a client that never claimed either capability in
            // `client_hello` must not receive them regardless of policy: the
            // cross-check is an absolute ceiling, not one more policy input.
            let request = ClientColorRequest {
                bit_depth: None,
                color_range: None,
                color_matrix: None,
                supports_main10: false,
                supports_main12: false,
                supports_full_range: false,
                supports_identity_matrix: false,
            };
            assert_eq!(
                resolve_client_color_request(ColorPolicy::AlwaysOn, elevated_ceiling(), request),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt2020Ncl)
            );
        }

        #[test]
        fn bit_depth_cap_steps_down_one_level_at_a_time_rather_than_collapsing_to_eight() {
            assert_eq!(
                cap_bit_depth_to_client(BitDepth::Twelve, true, false),
                BitDepth::Ten,
                "no twelve-bit support but ten-bit support must land on ten, not eight"
            );
            assert_eq!(
                cap_bit_depth_to_client(BitDepth::Twelve, false, false),
                BitDepth::Eight
            );
            assert_eq!(
                cap_bit_depth_to_client(BitDepth::Twelve, true, true),
                BitDepth::Twelve
            );
            assert_eq!(
                cap_bit_depth_to_client(BitDepth::Ten, false, true),
                BitDepth::Eight,
                "supports_main12 alone cannot license ten-bit"
            );
        }

        #[test]
        fn client_capability_cross_check_caps_full_range_and_identity_matrix() {
            let ceiling = ColorCeiling {
                bit_depth: BitDepth::Eight,
                color_range: ColorRange::Full,
                color_matrix: ColorMatrix::Identity,
            };
            let request = ClientColorRequest {
                bit_depth: None,
                color_range: None,
                color_matrix: None,
                supports_main10: true,
                supports_main12: true,
                supports_full_range: false,
                supports_identity_matrix: false,
            };
            assert_eq!(
                resolve_client_color_request(ColorPolicy::AlwaysOn, ceiling, request),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt709)
            );
        }

        #[test]
        fn unrecognised_token_is_modelled_as_no_preference_and_still_obeys_policy() {
            // The caller maps an unparsable `quality_settings` token to
            // `None` (after logging it distinctly) rather than a silent
            // hardcoded default -- `None` must still flow through the same
            // policy precedence as a genuinely absent field, not bypass it.
            let unparsed = ClientColorRequest {
                bit_depth: None, // stands in for `BitDepth::from_token("garbled")`
                color_range: None,
                color_matrix: None,
                ..permissive_client()
            };
            assert_eq!(
                resolve_client_color_request(ColorPolicy::AlwaysOn, elevated_ceiling(), unparsed),
                (BitDepth::Ten, ColorRange::Full, ColorMatrix::Bt2020Ncl),
                "always-on must still force its ceiling for an unrecognised token"
            );
            assert_eq!(
                resolve_client_color_request(ColorPolicy::DefaultOff, elevated_ceiling(), unparsed),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt709),
                "default-off must still fall back to the conservative baseline"
            );
        }

        #[test]
        fn client_with_only_wire_default_colour_fields_reproduces_todays_behaviour() {
            // A client sending exactly the wire defaults (`"8"`/`"limited"`/
            // `"bt709"` -- what every client that predates this feature
            // sends) against an untouched host configuration (`DefaultOff`,
            // and a ceiling that is itself already the conservative
            // baseline) must resolve to exactly the pre-existing behaviour.
            let untouched_ceiling = ColorCeiling {
                bit_depth: BitDepth::Eight,
                color_range: ColorRange::Limited,
                color_matrix: ColorMatrix::Bt709,
            };
            let wire_default_request = ClientColorRequest {
                bit_depth: Some(BitDepth::Eight),
                color_range: Some(ColorRange::Limited),
                color_matrix: Some(ColorMatrix::Bt709),
                supports_main10: false,
                supports_main12: false,
                supports_full_range: false,
                supports_identity_matrix: false,
            };
            assert_eq!(
                resolve_client_color_request(
                    ColorPolicy::DefaultOff,
                    untouched_ceiling,
                    wire_default_request
                ),
                (BitDepth::Eight, ColorRange::Limited, ColorMatrix::Bt709)
            );
        }

        #[test]
        fn identity_matrix_below_444_is_rejected_as_incoherent() {
            let plan = resolved_media_plan();
            let video = VideoConfiguration {
                codec: VideoCodec::H265,
                chroma: ChromaSubsampling::Yuv420,
                bit_depth: BitDepth::Eight,
                range: ColorRange::Limited,
                matrix: ColorMatrix::Identity,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            };
            assert!(!color_contract_is_servable(video, &plan));
        }

        #[test]
        fn twelve_bit_is_rejected_on_a_backend_without_a_twelve_bit_path() {
            // `resolved_media_plan`'s NVENC backend contract has no 12-bit
            // entry at all -- no NVIDIA GPU encodes 12-bit at any
            // subsampling.
            let plan = resolved_media_plan();
            let video = VideoConfiguration {
                codec: VideoCodec::H265,
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Twelve,
                range: ColorRange::Full,
                matrix: ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            };
            assert!(!color_contract_is_servable(video, &plan));
        }

        #[test]
        fn identity_matrix_is_rejected_when_the_backend_cannot_encode_it() {
            let mut plan = resolved_media_plan();
            plan.backend = EncoderBackend::OpenH264; // identity_matrix: false
            plan.chroma = arcen_media::ChromaSet::from_slice(&[ChromaSubsampling::Yuv444]);
            let video = VideoConfiguration {
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Eight,
                range: ColorRange::Limited,
                matrix: ColorMatrix::Identity,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            };
            assert!(!color_contract_is_servable(video, &plan));
        }

        #[test]
        fn coherent_and_backend_supported_contract_is_servable() {
            let plan = resolved_media_plan();
            let video = VideoConfiguration {
                codec: VideoCodec::H265,
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Ten,
                range: ColorRange::Full,
                matrix: ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            };
            assert!(color_contract_is_servable(video, &plan));
        }
    }

    #[tokio::test]
    async fn root_pump_ending_tears_down_the_mux_before_a_sibling_frame_can_leak() {
        // The exact scenario the primary/root pump's `on_ended` hook exists
        // for: a sibling monitor has a frame already buffered and ready to
        // go the whole time, right when the root monitor's own pipeline
        // ends (capenc's frame channel closes, ending this pump normally).
        let (root_idr, _root_idr_rx) = fake_idr();
        let root_queue = Arc::new(FrameQueue::new(root_idr));
        let (sibling_idr, _sibling_idr_rx) = fake_idr();
        let sibling_queue = Arc::new(FrameQueue::new(sibling_idr));
        sibling_queue.enqueue(vec![0xAB], true);

        let mux = Arc::new(
            MonitorMux::new(vec![
                (
                    SessionMonitorId::new(1).expect("nonzero"),
                    Arc::clone(&root_queue),
                ),
                (
                    SessionMonitorId::new(2).expect("nonzero"),
                    Arc::clone(&sibling_queue),
                ),
            ])
            .expect("two distinct monitor ids"),
        );

        let (frames_tx, frames_rx) = mpsc::channel::<crate::media::annexb::AccessUnit>(4);
        let pump = spawn_frame_pump(
            resolved_media_plan(),
            frames_rx,
            Arc::clone(&root_queue),
            None,
            1,
            1,
            1,
            Some(Arc::clone(&mux)),
        );

        // The root pipeline ends: capenc's stdout pipe closes, so
        // `frames_rx.recv()` returns `None` and the pump's loop exits
        // normally — exactly like a dying/crashed capenc child would.
        drop(frames_tx);
        assert_eq!(pump.await.unwrap(), SessionEndReason::MediaEnded);

        // By the time this pump's `JoinHandle` resolves, its `on_ended`
        // hook has already run (it is sequenced inside the same task,
        // before the return) — so the sibling's already-buffered frame
        // must already be gone, not merely "about to be gone" on some
        // other, independently scheduled task.
        assert_eq!(
            mux.dequeue().await,
            None,
            "a sibling's already-buffered frame must never leak after the root pump ends"
        );
    }

    #[tokio::test]
    async fn secondary_pump_ending_tears_down_the_mux_before_a_sibling_frame_can_leak() {
        // Mirrors `root_pump_ending_tears_down_the_mux_before_a_sibling_frame_
        // can_leak`, but for a *secondary* monitor (id 2 of 3) ending
        // instead of the primary/root (id 1): every applied monitor's own
        // pump now gets the exact same inline `on_ended` hook — there is no
        // longer a special "only the root gets the strong guarantee, every
        // other monitor gets a looser detached watcher" split.
        let (idr_1, _idr_1_rx) = fake_idr();
        let queue_1 = Arc::new(FrameQueue::new(idr_1));
        // Sibling 1 (the primary; never ends in this test) already has a
        // frame buffered and ready to go the entire time.
        queue_1.enqueue(vec![0xAB], true);

        let (idr_2, _idr_2_rx) = fake_idr();
        let queue_2 = Arc::new(FrameQueue::new(idr_2));

        let (idr_3, _idr_3_rx) = fake_idr();
        let queue_3 = Arc::new(FrameQueue::new(idr_3));
        // Sibling 3 (another secondary; never ends either) also has a
        // frame buffered and ready to go the entire time.
        queue_3.enqueue(vec![0xCD], true);

        let mux = Arc::new(
            MonitorMux::new(vec![
                (
                    SessionMonitorId::new(1).expect("nonzero"),
                    Arc::clone(&queue_1),
                ),
                (
                    SessionMonitorId::new(2).expect("nonzero"),
                    Arc::clone(&queue_2),
                ),
                (
                    SessionMonitorId::new(3).expect("nonzero"),
                    Arc::clone(&queue_3),
                ),
            ])
            .expect("three distinct monitor ids"),
        );

        let (frames_tx, frames_rx) = mpsc::channel::<crate::media::annexb::AccessUnit>(4);
        let secondary_pump = spawn_frame_pump(
            resolved_media_plan(),
            frames_rx,
            Arc::clone(&queue_2),
            None,
            2,
            1,
            1,
            Some(Arc::clone(&mux)),
        );

        // Monitor 2's pipeline ends (capenc's stdout pipe closes) while
        // both of its siblings already have buffered, ready-to-send
        // frames.
        drop(frames_tx);
        // Awaiting the `JoinHandle` this function returns directly (rather
        // than via any wrapper/watcher task) proves the spawned pump task
        // actually completes on its own — nothing is left running forever
        // waiting on it, and there is no separate task to leak.
        assert_eq!(secondary_pump.await.unwrap(), SessionEndReason::MediaEnded);

        // By the time that `.await` resolves, `on_ended` has already run
        // (sequenced inside the same task, before the return) — neither
        // sibling's already-buffered frame may leak out afterward.
        assert_eq!(
            mux.dequeue().await,
            None,
            "a sibling's already-buffered frame must never leak after a secondary pump ends"
        );
    }

    impl futures_util::Sink<Message> for NonReadingSink {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            unreachable!("non-reading sink never becomes ready")
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    struct RecordingSink(Arc<StdMutex<Vec<Message>>>);

    impl futures_util::Sink<Message> for RecordingSink {
        type Error = Infallible;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            self.0.lock().unwrap().push(item);
            Ok(())
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn closed_clipboard_queue() -> Arc<ClipboardWriterQueue> {
        let queue = ClipboardWriterQueue::new();
        queue.close();
        queue
    }

    fn test_audio_control() -> AudioControl {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let stream = policy.resolve(None, true, 128);
        AudioControl::new(true, false, true, None, stream)
    }

    fn registered_resume() -> (Arc<SessionRegistry>, ResumeBindings, ActiveHostSessionId) {
        let session_registry = SessionRegistry::new(None).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        response.screen_width = 1_920;
        response.screen_height = 1_080;
        let (disclaimer_digest, disclaimer_version) = resume::no_disclaimer_binding().unwrap();
        let bindings = ResumeBindings {
            host_identity: HostIdentity::new("spki-sha256:test-host").unwrap(),
            active_session_id: ActiveHostSessionId::new("linux-logind:c7").unwrap(),
            native_principal: NativePrincipal::Linux {
                uid: 1_001,
                logind_session_id: LogindSessionId::new("c7").unwrap(),
            },
            holder_nonce: arcen_identity::DeckHolderNonce::new([3; 32]),
            disclaimer_digest,
            disclaimer_version,
            topology: TopologyBinding::from_response(&response).unwrap(),
        };
        let active_session_id = bindings.active_session_id.clone();
        let (owner, _commands) = mpsc::unbounded_channel();
        session_registry
            .resume()
            .issue_initial(
                bindings.clone(),
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        (session_registry, bindings, active_session_id)
    }

    #[tokio::test]
    async fn tls_configuration_is_complete_before_bind() {
        let order = Arc::new(StdMutex::new(Vec::new()));
        let loaded = Arc::clone(&order);
        let ready = Arc::clone(&order);
        let bound = Arc::clone(&order);
        let result = initialize_tls_before_bind(
            || {
                loaded.lock().unwrap().push("load");
                Ok("tls")
            },
            |value| {
                assert_eq!(*value, "tls");
                ready.lock().unwrap().push("ready");
            },
            || async move {
                bound.lock().unwrap().push("bind");
                Ok("listener")
            },
        )
        .await
        .expect("prepared");
        assert_eq!(result, ("tls", "listener"));
        assert_eq!(*order.lock().unwrap(), ["load", "ready", "bind"]);

        let bind_called = Cell::new(false);
        let failed = initialize_tls_before_bind(
            || Err::<(), _>(std::io::Error::other("TLS failed")),
            |_| {},
            || async {
                bind_called.set(true);
                Ok(())
            },
        )
        .await;
        assert!(failed.is_err());
        assert!(!bind_called.get());
    }

    #[test]
    fn non_reading_client_control_flood_stays_bounded() {
        let (sender, receiver) = ControlSender::channel(3);
        for sequence in 0..10_000u64 {
            let message =
                Message::Text(format!(r#"{{"type":"health_pong","sequence":{sequence}}}"#));
            sender.send_best_effort(message, "health_test");
        }

        assert_eq!(
            receiver.len(),
            3,
            "mailbox cannot exceed configured capacity"
        );
        assert_eq!(
            sender.dropped(),
            9_997,
            "all excess health controls are dropped"
        );
    }

    #[test]
    fn inbound_websocket_messages_are_bounded_for_preauth_clients() {
        let config = ws_config();
        assert_eq!(config.max_message_size, Some(MAX_INBOUND_WS_MESSAGE));
        assert_eq!(config.max_frame_size, Some(MAX_INBOUND_WS_MESSAGE));
        assert_eq!(
            PREAUTH_CAPACITY * config.max_message_size.unwrap(),
            16 * (arcen_protocol::CHUNK_BYTES + arcen_protocol::CLIPBOARD_HEADER_SIZE)
        );
    }

    #[tokio::test]
    async fn critical_control_fails_with_bounded_timeout_for_non_reader() {
        let mut sink = NonReadingSink;
        let sent = send_critical_control_with_timeout(
            &mut sink,
            Message::Text("critical".into()),
            "critical_test",
            Duration::from_millis(5),
        )
        .await;
        assert!(!sent);
    }

    #[tokio::test]
    async fn refresh_successor_send_failure_returns_authority_failure_and_reopens_after_cleanup() {
        let (session_registry, bindings, active_session_id) = registered_resume();
        let (idr, _idr_rx) = fake_idr();
        let video = Arc::new(FrameQueue::new(idr));
        let audio = Arc::new(AudioQueue::new());
        let (_control_tx, control_rx) = mpsc::channel(1);
        let reason = tokio::time::timeout(
            Duration::from_millis(100),
            sender_loop(
                NonReadingSink,
                crate::session::monitor_mux::VideoSource::Single(video),
                audio,
                closed_clipboard_queue(),
                control_rx,
                test_audio_control(),
                Some(ResumeRefreshContext {
                    session_registry: Arc::clone(&session_registry),
                    active_session_id: active_session_id.clone(),
                    window_secs: 0,
                }),
                Duration::from_millis(5),
            ),
        )
        .await
        .expect("refresh send failure must be bounded");
        assert_eq!(reason, SessionEndReason::ResumeAuthorityFailure);
        assert!(!session_registry
            .resume()
            .resume_handshake_available()
            .unwrap());

        session_registry
            .resume()
            .complete_drain(&active_session_id)
            .unwrap();
        let (owner, _commands) = mpsc::unbounded_channel();
        assert!(session_registry
            .resume()
            .issue_initial(
                bindings,
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .is_ok());
    }

    #[tokio::test]
    async fn resumed_successor_send_failure_drains_without_redetaching() {
        let (session_registry, bindings, active_session_id) = registered_resume();
        let successor = session_registry
            .resume()
            .refresh_grant(&active_session_id)
            .unwrap();
        assert!(
            !send_successor_auth_result_or_drain(
                &mut NonReadingSink,
                "Resumed",
                &successor.grant,
                successor.window_secs,
                true,
                Duration::from_millis(5),
                session_registry.resume(),
                &active_session_id,
            )
            .await
        );
        assert!(!session_registry
            .resume()
            .resume_handshake_available()
            .unwrap());
        assert_eq!(
            session_registry
                .resume()
                .mark_detached(&active_session_id)
                .unwrap_err(),
            resume::ResumeRegistryError::SlotUnavailable
        );

        session_registry
            .resume()
            .complete_drain(&active_session_id)
            .unwrap();
        let (owner, _commands) = mpsc::unbounded_channel();
        assert!(session_registry
            .resume()
            .issue_initial(
                bindings,
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .is_ok());
    }

    #[tokio::test]
    async fn failed_resume_retains_display_until_idempotent_final_restore() {
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
        let resources = HeldDisplayResources::new(None, Some(permit));
        let mut retained = None;

        retain_display_for_final_cleanup(&mut retained, resources);
        assert!(Arc::clone(&permits).try_acquire_owned().is_err());
        retained.as_mut().unwrap().restore().await;
        let reacquired = Arc::clone(&permits).try_acquire_owned().unwrap();
        drop(reacquired);
        retained.as_mut().unwrap().restore().await;
        assert!(Arc::clone(&permits).try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn awaiting_idr_does_not_block_control_audio_or_clean_shutdown() {
        let (idr, _idr_rx) = fake_idr();
        let video = Arc::new(FrameQueue::new(idr));
        for value in 0..=crate::session::client::CAPACITY {
            video.enqueue(vec![value as u8], false);
        }
        assert!(video.awaiting_keyframe());

        let audio = Arc::new(AudioQueue::new());
        audio.enqueue(crate::session::audio::EncodedAudioPacket {
            codec: AudioCodec::Pcm,
            payload: vec![7],
            timestamp_ms: 7,
        });
        let (control_tx, control_rx) = ControlSender::channel(1);
        assert!(
            control_tx
                .send_required(Message::Text("health".into()), "test")
                .await
        );
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let writer = tokio::spawn(sender_loop(
            RecordingSink(Arc::clone(&sent)),
            crate::session::monitor_mux::VideoSource::Single(Arc::clone(&video)),
            Arc::clone(&audio),
            closed_clipboard_queue(),
            control_rx,
            test_audio_control(),
            None,
            Duration::from_millis(20),
        ));

        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if sent.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("control and audio must remain responsive");

        {
            let sent = sent.lock().unwrap();
            assert!(sent
                .iter()
                .any(|message| matches!(message, Message::Text(_))));
            assert!(sent.iter().any(
                |message| matches!(message, Message::Binary(bytes) if bytes[0] == FrameType::Audio as u8)
            ));
            assert!(
                !sent.iter().any(
                    |message| matches!(message, Message::Binary(bytes) if bytes[0] != FrameType::Audio as u8)
                ),
                "no unsafe video may reach the writer before IDR"
            );
        }

        video.close();
        audio.close();
        drop(control_tx);
        tokio::time::timeout(Duration::from_millis(100), writer)
            .await
            .expect("closed queues must stop the writer")
            .unwrap();
    }

    #[tokio::test]
    async fn slow_writer_times_out_while_recovery_shutdown_remains_bounded() {
        let (idr, _idr_rx) = fake_idr();
        let video = Arc::new(FrameQueue::new(idr));
        assert!(video.enqueue(vec![FrameType::VideoH264 as u8], true));
        let audio = Arc::new(AudioQueue::new());
        let (_control_tx, control_rx) = mpsc::channel(1);

        tokio::time::timeout(
            Duration::from_millis(50),
            sender_loop(
                NonReadingSink,
                crate::session::monitor_mux::VideoSource::Single(Arc::clone(&video)),
                Arc::clone(&audio),
                closed_clipboard_queue(),
                control_rx,
                test_audio_control(),
                None,
                Duration::from_millis(5),
            ),
        )
        .await
        .expect("slow writer and close paths must both be bounded");
        video.close();
        audio.close();
    }

    #[tokio::test]
    async fn stalled_pre_auth_clients_do_not_consume_display_singleton() {
        let preauth = Arc::new(Semaphore::new(2));
        let display = Arc::new(Semaphore::new(1));
        let _attacker_one = preauth.clone().try_acquire_owned().unwrap();
        let _attacker_two = preauth.clone().try_acquire_owned().unwrap();
        assert!(
            preauth.clone().try_acquire_owned().is_err(),
            "pre-auth capacity must be bounded"
        );
        assert!(
            display.clone().try_acquire_owned().is_ok(),
            "pre-auth stalls must not consume the shared display permit"
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_deferred_display_restore_hold() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DeferredRestore {
            permit: Option<OwnedSemaphorePermit>,
            released: Arc<AtomicBool>,
        }

        impl Drop for DeferredRestore {
            fn drop(&mut self) {
                let permit = self.permit.take();
                let released = self.released.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(50));
                    released.store(true, Ordering::SeqCst);
                    drop(permit);
                });
            }
        }

        let session_slot = Arc::new(Semaphore::new(1));
        let released = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let mut connections = JoinSet::new();
        let task_slot = session_slot.clone();
        let task_released = released.clone();
        connections.spawn(async move {
            let permit = task_slot.acquire_owned().await.unwrap();
            let _restore = DeferredRestore {
                permit: Some(permit),
                released: task_released,
            };
            let _ = ready_tx.send(());
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();

        let started = Instant::now();
        drain_connections_and_display_with_timeout(
            &mut connections,
            session_slot,
            Duration::from_millis(100),
        )
        .await;
        assert!(released.load(Ordering::SeqCst));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[tokio::test]
    async fn missing_client_read_fails_liveness_deadline() {
        let mut stream = futures_util::stream::pending::<
            Result<Message, tokio_tungstenite::tungstenite::Error>,
        >();
        assert!(next_with_liveness(&mut stream, Duration::from_millis(5))
            .await
            .is_err());
    }

    #[test]
    fn handshake_receive_error_session_end_reason_classifies_each_variant() {
        assert_eq!(
            HandshakeReceiveError::Timeout.session_end_reason(),
            SessionEndReason::ReadLivenessTimeout,
            "a transient read deadline must map to the same reason a mid-session \
             read-liveness timeout would, so it is eligible for reconnect"
        );
        assert_eq!(
            HandshakeReceiveError::Closed.session_end_reason(),
            SessionEndReason::TransportError,
            "a clean close before the handshake completes is an ordinary transport loss"
        );
        assert_eq!(
            HandshakeReceiveError::Transport(tokio_tungstenite::tungstenite::Error::AlreadyClosed)
                .session_end_reason(),
            SessionEndReason::TransportError,
            "a transport read error before the handshake completes is an ordinary transport loss"
        );
        assert_eq!(
            HandshakeReceiveError::Protocol("malformed".to_string()).session_end_reason(),
            SessionEndReason::ProtocolError,
            "a malformed/protocol-violating message is never treated as a transient condition"
        );
    }

    #[test]
    fn handshake_receive_attachment_end_never_populates_transport_loss_clock_without_a_refresh_context(
    ) {
        for error in [
            HandshakeReceiveError::Timeout,
            HandshakeReceiveError::Closed,
            HandshakeReceiveError::Transport(tokio_tungstenite::tungstenite::Error::AlreadyClosed),
            HandshakeReceiveError::Protocol("bad".to_string()),
        ] {
            let end = handshake_receive_attachment_end(&error, None);
            assert_eq!(end.reason, error.session_end_reason());
            assert!(
                !end.reached_usable,
                "a handshake failure is never a usable attachment"
            );
            assert!(
                end.transport_loss_observed_at.is_none(),
                "there is no clock to sample from without a resume refresh context"
            );
        }
    }

    #[tokio::test]
    async fn handshake_receive_attachment_end_populates_transport_loss_clock_only_for_resumable_reasons(
    ) {
        let (session_registry, _bindings, active_session_id) = registered_resume();
        let refresh = ResumeRefreshContext {
            session_registry: Arc::clone(&session_registry),
            active_session_id,
            window_secs: 30,
        };

        for error in [
            HandshakeReceiveError::Timeout,
            HandshakeReceiveError::Closed,
            HandshakeReceiveError::Transport(tokio_tungstenite::tungstenite::Error::AlreadyClosed),
        ] {
            let end = handshake_receive_attachment_end(&error, Some(&refresh));
            assert_eq!(end.reason, error.session_end_reason());
            assert!(
                resumable_transport_loss(end.reason),
                "{error} must remain eligible for the existing \
                 disconnect/reconnect-within-window policy"
            );
            assert!(
                end.transport_loss_observed_at.is_some(),
                "a resumable reason with a live refresh context must record the loss clock"
            );
        }

        let protocol_end = handshake_receive_attachment_end(
            &HandshakeReceiveError::Protocol("malformed".to_string()),
            Some(&refresh),
        );
        assert_eq!(protocol_end.reason, SessionEndReason::ProtocolError);
        assert!(
            !resumable_transport_loss(protocol_end.reason),
            "a malformed/protocol handshake failure must never be treated as a resumable \
             transport loss, even mid-resume with a live refresh context available"
        );
        assert!(
            protocol_end.transport_loss_observed_at.is_none(),
            "a malformed/protocol handshake failure must never record a transport loss clock"
        );
    }

    #[tokio::test]
    async fn receive_client_hello_times_out_on_read_deadline() {
        let mut stream = futures_util::stream::pending::<
            Result<Message, tokio_tungstenite::tungstenite::Error>,
        >();
        let error = receive_client_hello(&mut stream, Duration::from_millis(5))
            .await
            .expect_err("no client hello arrives before the deadline");
        assert!(matches!(error, HandshakeReceiveError::Timeout));
    }

    #[tokio::test]
    async fn receive_client_hello_classifies_clean_close_as_closed() {
        let mut stream =
            futures_util::stream::empty::<Result<Message, tokio_tungstenite::tungstenite::Error>>();
        let error = receive_client_hello(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("an exhausted stream never yields a client hello");
        assert!(matches!(error, HandshakeReceiveError::Closed));
    }

    #[tokio::test]
    async fn receive_client_hello_classifies_transport_read_error() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Err(tokio_tungstenite::tungstenite::Error::AlreadyClosed)
        }));
        let error = receive_client_hello(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("a transport error must not be treated as a valid client hello");
        assert!(matches!(error, HandshakeReceiveError::Transport(_)));
    }

    #[tokio::test]
    async fn receive_client_hello_rejects_malformed_json_as_protocol_error() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Ok(Message::Text("not json".to_string()))
        }));
        let error = receive_client_hello(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("malformed JSON is not a valid client hello");
        assert!(matches!(error, HandshakeReceiveError::Protocol(_)));
    }

    #[tokio::test]
    async fn receive_client_hello_rejects_wrong_msg_type_as_protocol_error() {
        let wrong_type = ClientHelloMsg {
            msg_type: "quality_settings".to_string(),
            ..ClientHelloMsg::default()
        };
        let json = serde_json::to_string(&wrong_type).unwrap();
        let mut stream = Box::pin(futures_util::stream::once(async move {
            Ok(Message::Text(json))
        }));
        let error = receive_client_hello(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("wrong msg_type must never be accepted as client_hello");
        assert!(matches!(error, HandshakeReceiveError::Protocol(_)));
    }

    #[tokio::test]
    async fn receive_client_hello_rejects_non_text_frame_as_protocol_error() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Ok(Message::Binary(vec![1, 2, 3]))
        }));
        let error = receive_client_hello(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("client hello must be a text frame");
        assert!(matches!(error, HandshakeReceiveError::Protocol(_)));
    }

    #[tokio::test]
    async fn receive_client_hello_accepts_a_well_formed_hello() {
        let hello = ClientHelloMsg::default();
        let json = serde_json::to_string(&hello).unwrap();
        let mut stream = Box::pin(futures_util::stream::once(async move {
            Ok(Message::Text(json))
        }));
        let received = receive_client_hello(&mut stream, Duration::from_secs(5))
            .await
            .expect("a well-formed client_hello must be accepted");
        assert_eq!(received.msg_type, CLIENT_HELLO);
    }

    #[tokio::test]
    async fn receive_quality_settings_times_out_on_read_deadline() {
        let mut stream = futures_util::stream::pending::<
            Result<Message, tokio_tungstenite::tungstenite::Error>,
        >();
        let error = receive_quality_settings(&mut stream, Duration::from_millis(5))
            .await
            .expect_err("no quality settings arrive before the deadline");
        assert!(matches!(error, HandshakeReceiveError::Timeout));
    }

    #[tokio::test]
    async fn receive_quality_settings_classifies_clean_close_as_closed() {
        let mut stream =
            futures_util::stream::empty::<Result<Message, tokio_tungstenite::tungstenite::Error>>();
        let error = receive_quality_settings(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("an exhausted stream never yields quality settings");
        assert!(matches!(error, HandshakeReceiveError::Closed));
    }

    #[tokio::test]
    async fn receive_quality_settings_classifies_transport_read_error() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Err(tokio_tungstenite::tungstenite::Error::AlreadyClosed)
        }));
        let error = receive_quality_settings(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("a transport error must not be treated as valid quality settings");
        assert!(matches!(error, HandshakeReceiveError::Transport(_)));
    }

    #[tokio::test]
    async fn receive_quality_settings_rejects_malformed_json_as_protocol_error() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Ok(Message::Text("not json".to_string()))
        }));
        let error = receive_quality_settings(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("malformed JSON is not valid quality settings");
        assert!(matches!(error, HandshakeReceiveError::Protocol(_)));
    }

    #[tokio::test]
    async fn receive_quality_settings_rejects_non_text_frame_as_protocol_error() {
        let mut stream = Box::pin(futures_util::stream::once(async {
            Ok(Message::Binary(vec![4, 5, 6]))
        }));
        let error = receive_quality_settings(&mut stream, Duration::from_secs(5))
            .await
            .expect_err("quality settings must be a text frame");
        assert!(matches!(error, HandshakeReceiveError::Protocol(_)));
    }

    #[tokio::test]
    async fn receive_quality_settings_accepts_well_formed_settings() {
        let quality = QualitySettings::default();
        let json = serde_json::to_string(&quality).unwrap();
        let mut stream = Box::pin(futures_util::stream::once(async move {
            Ok(Message::Text(json))
        }));
        let received = receive_quality_settings(&mut stream, Duration::from_secs(5))
            .await
            .expect("well-formed quality settings must be accepted");
        assert_eq!(received.msg_type, "quality_settings");
    }

    /// Composition test proving the actual acceptance criterion end to
    /// end: a resumed attachment whose ClientHello/quality_settings
    /// handshake times out or the socket drops cleanly must classify to a
    /// `resumable_transport_loss`-eligible `SessionEndReason` with a
    /// recorded transport-loss clock (so `run_resumable_session`'s
    /// existing reconnect-window logic preserves the desktop), while a
    /// malformed/protocol-violating message on the very same resumed
    /// attachment must still be fully terminal.
    #[tokio::test]
    async fn resumed_handshake_timeout_and_close_stay_within_the_existing_reconnect_window_policy()
    {
        let (session_registry, _bindings, active_session_id) = registered_resume();
        let refresh = ResumeRefreshContext {
            session_registry: Arc::clone(&session_registry),
            active_session_id,
            window_secs: 30,
        };

        let mut timeout_stream = futures_util::stream::pending::<
            Result<Message, tokio_tungstenite::tungstenite::Error>,
        >();
        let timeout_error = receive_client_hello(&mut timeout_stream, Duration::from_millis(5))
            .await
            .expect_err("resumed handshake timeout");
        let timeout_end = handshake_receive_attachment_end(&timeout_error, Some(&refresh));
        assert!(resumable_transport_loss(timeout_end.reason));
        assert!(timeout_end.transport_loss_observed_at.is_some());

        let mut closed_stream =
            futures_util::stream::empty::<Result<Message, tokio_tungstenite::tungstenite::Error>>();
        let closed_error = receive_quality_settings(&mut closed_stream, Duration::from_secs(5))
            .await
            .expect_err("resumed handshake socket drop");
        let closed_end = handshake_receive_attachment_end(&closed_error, Some(&refresh));
        assert!(resumable_transport_loss(closed_end.reason));
        assert!(closed_end.transport_loss_observed_at.is_some());

        let mut malformed_stream = Box::pin(futures_util::stream::once(async {
            Ok(Message::Text("not json".to_string()))
        }));
        let malformed_error = receive_client_hello(&mut malformed_stream, Duration::from_secs(5))
            .await
            .expect_err("malformed resumed client hello");
        let malformed_end = handshake_receive_attachment_end(&malformed_error, Some(&refresh));
        assert!(
            !resumable_transport_loss(malformed_end.reason),
            "malformed handshake payloads stay terminal even mid-resume"
        );
        assert!(malformed_end.transport_loss_observed_at.is_none());
    }

    /// The initial/non-resumable attachment's multi-monitor fail-closed
    /// decision (`multi_monitor_attachment_must_terminate_desktop`) never
    /// consults `AttachmentEnd::reason` — only `reached_usable`,
    /// `ever_usable`, and the committed plan — so reclassifying handshake
    /// failures from `MediaEnded` to `ReadLivenessTimeout`/`TransportError`/
    /// `ProtocolError` cannot change that decision for an initial
    /// attachment. Every handshake failure still has `reached_usable:
    /// false`, so a fresh multi-monitor desktop's first attachment still
    /// fails closed regardless of which of the new reasons applies.
    #[test]
    fn initial_multi_monitor_fail_closed_decision_is_unaffected_by_handshake_reason_reclassification(
    ) {
        let plan = three_head_topology_plan();
        for error in [
            HandshakeReceiveError::Timeout,
            HandshakeReceiveError::Closed,
            HandshakeReceiveError::Transport(tokio_tungstenite::tungstenite::Error::AlreadyClosed),
            HandshakeReceiveError::Protocol("bad".to_string()),
        ] {
            let end = handshake_receive_attachment_end(&error, None);
            assert!(
                multi_monitor_attachment_must_terminate_desktop(
                    end.reached_usable,
                    false,
                    Some(&plan)
                ),
                "a fresh multi-monitor desktop's first attachment must still fail closed for {error}"
            );
        }
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn websocket_upgrade_rejects_browser_origin() {
        let request = Request::builder()
            .header("Origin", "https://attacker.example")
            .body(())
            .unwrap();
        let response = Response::new(());
        let rejected = reject_browser_origin(&request, response).unwrap_err();
        assert_eq!(rejected.status(), 403);
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn websocket_upgrade_accepts_native_client_without_origin() {
        let request = Request::new(());
        let response = Response::new(());
        assert!(reject_browser_origin(&request, response).is_ok());
    }

    #[test]
    fn session_log_id_accepts_deck_uuid_and_falls_back_for_legacy_clients() {
        let value = "01234567-89ab-4def-8123-456789abcdef";
        let (accepted, replaced) = resolve_session_log_id(Some(value)).unwrap();
        assert_eq!(accepted.as_str(), value);
        assert!(!replaced);

        let (fallback, replaced) = resolve_session_log_id(None).unwrap();
        assert!(replaced);
        CorrelationId::parse_uuid(fallback.to_string()).unwrap();
    }

    #[test]
    fn legacy_single_monitor_typed_request_full_frame_dispatches_existing_idr_path() {
        let (idr, mut idr_rx) = fake_idr();
        let (control, _control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        let mut input_sequence = InputSequenceTracker::default();
        let json = serde_json::to_string(&RequestFullFrameMsg::default()).unwrap();
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));

        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        handle_control_json(
            &json,
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );

        assert!(matches!(idr_rx.try_recv(), Ok(StdinCmd::Idr)));

        handle_control_json(
            &json,
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );
        assert!(
            idr_rx.try_recv().is_err(),
            "the existing 500ms guard must suppress repeated typed requests"
        );
    }

    #[test]
    fn typed_request_full_frame_fans_out_to_every_applied_monitors_idr_exactly_once() {
        // A committed Carrier A multi-monitor session's dispatcher holds one
        // `IdrRequester` per applied monitor (primary + every secondary),
        // not just the primary's — a `request_full_frame` from the client
        // must reach every one of them exactly once so every monitor
        // recovers a fully decodable picture, not only the primary's.
        let (primary_idr, mut primary_rx) = fake_idr();
        let (secondary_a_idr, mut secondary_a_rx) = fake_idr();
        let (secondary_b_idr, mut secondary_b_rx) = fake_idr();
        let all_monitor_idrs = [primary_idr, secondary_a_idr, secondary_b_idr];
        let (control, _control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        let mut input_sequence = InputSequenceTracker::default();
        let json = serde_json::to_string(&RequestFullFrameMsg::default()).unwrap();
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));
        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);

        handle_control_json(
            &json,
            &all_monitor_idrs,
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );

        for rx in [&mut primary_rx, &mut secondary_a_rx, &mut secondary_b_rx] {
            assert!(matches!(rx.try_recv(), Ok(StdinCmd::Idr)));
            assert!(
                rx.try_recv().is_err(),
                "each monitor's idr must be requested exactly once per request_full_frame"
            );
        }
    }

    #[test]
    fn typed_health_ping_dispatches_matching_pong() {
        let (idr, _idr_rx) = fake_idr();
        let (control, mut control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now();
        let mut input_sequence = InputSequenceTracker::default();
        let ping = HealthPingMsg {
            timestamp_ms: 1_700_000_000_000,
            sequence: 42,
            client_state: "streaming".to_string(),
            client_telemetry: Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_presented: Some(120),
                    sample_window_secs: SampleWindowSecs::try_from(2).ok(),
                    ..ClientQosSampleMsg::default()
                }),
                network: None,
            }),
            ..HealthPingMsg::default()
        };
        let json = serde_json::to_string(&ping).unwrap();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));

        handle_control_json(
            &json,
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );

        let WriterControl::Message(Message::Text(text)) = control_rx.try_recv().unwrap() else {
            panic!("health_ping must enqueue a text health_pong");
        };
        let pong: HealthPongMsg = serde_json::from_str(&text).unwrap();
        assert_eq!(pong.ping_timestamp_ms, ping.timestamp_ms);
        assert_eq!(pong.sequence, ping.sequence);
        assert_eq!(pong.server_state, "streaming");

        // The PR3 `client_telemetry` payload must reach `SessionHealth`
        // rather than being discarded alongside the pong reply.
        let recorded = lock_recover(&session_health).client().cloned();
        assert_eq!(recorded, ping.client_telemetry);
    }

    /// Re-review finding #1: client telemetry receipt must be recorded on
    /// the same monotonic (session-start-anchored elapsed) clock that
    /// `SessionHealth::observe`'s staleness check uses, never a wall-clock
    /// (`SystemTime`/epoch) reading — otherwise an NTP/DST/leap-second jump
    /// could make fresh telemetry look stale (or stale telemetry look
    /// fresh) purely from a clock-basis mismatch.
    #[test]
    fn health_ping_records_client_telemetry_on_the_session_monotonic_clock() {
        let (idr, _idr_rx) = fake_idr();
        let (control, _control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now();
        let mut input_sequence = InputSequenceTracker::default();
        let ping = HealthPingMsg {
            client_telemetry: Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_presented: Some(120),
                    sample_window_secs: SampleWindowSecs::try_from(2).ok(),
                    ..ClientQosSampleMsg::default()
                }),
                network: None,
            }),
            ..HealthPingMsg::default()
        };
        let json = serde_json::to_string(&ping).unwrap();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));
        let session_started_at = Instant::now();

        handle_control_json(
            &json,
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            session_started_at,
        );

        let received_ms = lock_recover(&session_health)
            .client_received_ms()
            .expect("client telemetry was recorded");
        // A wall-clock (`SystemTime`) reading would be on the order of
        // 1.7e12ms (current Unix epoch milliseconds); a monotonic
        // session-start-anchored elapsed reading taken microseconds after
        // `session_started_at` is on the order of single-digit
        // milliseconds. This bound distinguishes the two clock bases
        // without depending on wall-clock/test timing precision.
        assert!(
            received_ms < 60_000,
            "client_received_ms ({received_ms}) must be a small session-monotonic elapsed \
             value, not a ~1.7e12ms wall-clock epoch reading"
        );
    }

    #[test]
    fn one_sequence_tracker_orders_all_linux_input_types() {
        let mut tracker = InputSequenceTracker::default();
        let absolute = MouseMoveMsg {
            sequence: 20,
            ..MouseMoveMsg::default()
        };
        assert!(parse_sequenced_input::<MouseMoveMsg>(
            &serde_json::to_string(&absolute).unwrap(),
            &mut tracker
        )
        .is_ok());
        let relative = MouseMoveRelativeMsg {
            sequence: 21,
            ..MouseMoveRelativeMsg::default()
        };
        assert!(parse_sequenced_input::<MouseMoveRelativeMsg>(
            &serde_json::to_string(&relative).unwrap(),
            &mut tracker
        )
        .is_ok());
        let stale_key = KeyEventMsg {
            sequence: 20,
            ..KeyEventMsg::default()
        };
        assert!(parse_sequenced_input::<KeyEventMsg>(
            &serde_json::to_string(&stale_key).unwrap(),
            &mut tracker
        )
        .is_err());
        assert_eq!(tracker.last_nonzero(), 21);
        let button = MouseButtonMsg {
            sequence: 22,
            ..MouseButtonMsg::default()
        };
        assert!(parse_sequenced_input::<MouseButtonMsg>(
            &serde_json::to_string(&button).unwrap(),
            &mut tracker
        )
        .is_ok());
        let wheel = MouseScrollMsg {
            sequence: 23,
            ..MouseScrollMsg::default()
        };
        assert!(parse_sequenced_input::<MouseScrollMsg>(
            &serde_json::to_string(&wheel).unwrap(),
            &mut tracker
        )
        .is_ok());
        assert!(parse_sequenced_input::<MouseMoveMsg>(
            &serde_json::to_string(&MouseMoveMsg::default()).unwrap(),
            &mut tracker
        )
        .is_ok());
        assert_eq!(tracker.last_nonzero(), 23);
        let pen = PenEventMsg {
            sequence: 24,
            ..PenEventMsg::default()
        };
        assert!(
            parse_sequenced_pen_event(&serde_json::to_string(&pen).unwrap(), &mut tracker).is_ok()
        );
        assert_eq!(tracker.last_nonzero(), 24);
    }

    #[test]
    fn match_layout_requires_mutual_input_v4_region_capability() {
        let mut hello = ClientHelloMsg::default();
        hello.input_capabilities.region_input = InputCapabilityAvailability::Available;

        hello.input_protocol_version = REGION_INPUT_PROTOCOL_VERSION - 1;
        assert!(!match_layout_region_input_negotiated(true, &hello));

        hello.input_protocol_version = REGION_INPUT_PROTOCOL_VERSION;
        hello.input_capabilities.region_input = InputCapabilityAvailability::Unknown;
        assert!(!match_layout_region_input_negotiated(true, &hello));

        hello.input_capabilities.region_input = InputCapabilityAvailability::Available;
        assert!(match_layout_region_input_negotiated(true, &hello));
        assert!(!match_layout_region_input_negotiated(false, &hello));
    }

    #[test]
    fn committed_region_sessions_identify_every_legacy_pointer_and_pen_message() {
        for message_type in [
            "mouse_move",
            MOUSE_MOVE_RELATIVE,
            "mouse_button",
            MOUSE_SCROLL,
            PEN_EVENT,
        ] {
            assert!(is_legacy_region_input_message(message_type));
        }
        for message_type in [
            "key_event",
            KEY_RESET_MODIFIERS,
            REGION_POINTER_MOTION,
            REGION_PEN_EVENT,
        ] {
            assert!(!is_legacy_region_input_message(message_type));
        }
    }

    #[test]
    fn parse_sequenced_pen_event_rejects_out_of_range_before_advancing_tracker() {
        let mut tracker = InputSequenceTracker::default();
        // Out of range (pressure > 1.0) at a sequence number ahead of the
        // tracker: validation must reject this before it ever reaches
        // `InputSequenceTracker::accept`, so the tracker's position stays
        // unmoved and a later, in-range, lower-or-equal sequence can still
        // be accepted (the malformed sample never "burns" the sequence).
        let invalid = PenEventMsg {
            sequence: 100,
            pressure: 1.5,
            ..PenEventMsg::default()
        };
        assert!(
            parse_sequenced_pen_event(&serde_json::to_string(&invalid).unwrap(), &mut tracker)
                .is_err()
        );
        assert_eq!(
            tracker.last_nonzero(),
            0,
            "an out-of-range pen sample must not advance the shared sequence tracker"
        );

        let valid = PenEventMsg {
            sequence: 5,
            ..PenEventMsg::default()
        };
        assert!(
            parse_sequenced_pen_event(&serde_json::to_string(&valid).unwrap(), &mut tracker)
                .is_ok()
        );
        assert_eq!(tracker.last_nonzero(), 5);
    }

    #[test]
    fn parse_sequenced_pen_event_rejects_non_finite_fields() {
        let mut tracker = InputSequenceTracker::default();
        let invalid = PenEventMsg {
            sequence: 1,
            tilt_x_degrees: f32::NAN,
            ..PenEventMsg::default()
        };
        assert!(
            parse_sequenced_pen_event(&serde_json::to_string(&invalid).unwrap(), &mut tracker)
                .is_err()
        );
        assert_eq!(tracker.last_nonzero(), 0);
    }

    #[test]
    fn dispatch_pen_event_with_no_input_backend_is_a_no_op() {
        let (idr, _idr_rx) = fake_idr();
        let (control, _control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now();
        let mut input_sequence = InputSequenceTracker::default();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));
        let pen = PenEventMsg {
            sequence: 1,
            ..PenEventMsg::default()
        };

        // No InputController is wired (`None`): dispatching a well-formed
        // pen_event must not panic and must leave mouse/keyboard dispatch
        // paths untouched, matching the "pen failure never disables
        // mouse/keyboard" requirement even in the degenerate no-backend case.
        handle_control_json(
            &serde_json::to_string(&pen).unwrap(),
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );
    }

    #[test]
    fn typed_health_ping_uses_shared_defaults_for_missing_optional_fields() {
        let (idr, _idr_rx) = fake_idr();
        let (control, mut control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now();
        let mut input_sequence = InputSequenceTracker::default();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));

        handle_control_json(
            r#"{"type":"health_ping"}"#,
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );

        let WriterControl::Message(Message::Text(text)) = control_rx.try_recv().unwrap() else {
            panic!("minimal typed health_ping must enqueue a health_pong");
        };
        let pong: HealthPongMsg = serde_json::from_str(&text).unwrap();
        assert_eq!(pong.ping_timestamp_ms, 0);
        assert_eq!(pong.sequence, 0);
        // No `client_telemetry` on a minimal ping — an old/minimal client
        // must read as "unavailable", never a stand-in healthy zero.
        assert_eq!(lock_recover(&session_health).client(), None);
    }

    #[test]
    fn malformed_typed_health_ping_is_rejected() {
        let (idr, _idr_rx) = fake_idr();
        let (control, mut control_rx) = ControlSender::channel(1);
        let plan = resolved_media_plan();
        let mut last_request = Instant::now();
        let mut input_sequence = InputSequenceTracker::default();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([0; 16]);
        let session_health = Mutex::new(crate::observability::SessionHealth::new(
            arcen_telemetry::QosTargets::default(),
        ));

        handle_control_json(
            r#"{"type":"health_ping","timestamp_ms":"not-a-number"}"#,
            std::slice::from_ref(&idr),
            &control,
            &plan,
            &session_log_id,
            None,
            None,
            &mut last_request,
            &mut input_sequence,
            plan.cursor_mode,
            &session_health,
            Instant::now(),
        );

        assert!(
            control_rx.try_recv().is_err(),
            "malformed shared typed payload must not produce a pong"
        );
    }

    #[test]
    fn no_auth_timezone_request_is_inert_even_when_feature_is_enabled() {
        let config = Config {
            timezone_redirection: true,
            zoneinfo_root: std::path::PathBuf::from("deliberately-missing-zoneinfo"),
            ..Config::default()
        };
        let response = AuthResponse::none().with_timezone("Europe/Oslo");
        assert_eq!(validated_requested_timezone(&config, &response), None);
    }

    #[test]
    fn client_hello_timezone_compares_against_authoritative_desktop() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        assert!(!timezone_echo_mismatch(
            Some(&timezone),
            Some("Europe/Oslo")
        ));
        assert!(timezone_echo_mismatch(
            Some(&timezone),
            Some("Europe/London")
        ));
        assert!(timezone_echo_mismatch(Some(&timezone), None));
        assert!(timezone_echo_mismatch(None, Some("Europe/Oslo")));
    }

    fn test_session_log_id() -> CorrelationId {
        CorrelationId::from_uuid_v4_bytes([7; 16])
    }

    #[test]
    fn session_auth_ok_and_fail_emit_the_stable_lifecycle_kinds() {
        let (emitter, recorded) = LifecycleEmitter::recording();

        emit_session_auth_ok(
            &emitter,
            test_session_log_id(),
            "alice",
            Some("198.51.100.7"),
            Some(42),
        );
        emit_session_auth_fail(
            &emitter,
            test_session_log_id(),
            Some("198.51.100.7"),
            "pam_authenticate",
            "rejected",
        );

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].kind(), LifecycleEventKind::SessionAuthOk);
        assert_eq!(recorded[1].kind(), LifecycleEventKind::SessionAuthFail);
        // Never carries username/uid in nested fields: identity flows only
        // through the top-level LifecycleContext (asserted separately in
        // `session_auth_ok_carries_authenticated_user_and_peer_at_top_level`).
        assert!(!recorded[0].fields().as_map().contains_key("username"));
        assert!(!recorded[0].fields().as_map().contains_key("uid"));
        assert!(!recorded[1].fields().as_map().contains_key("username"));
    }

    #[test]
    fn session_auth_ok_carries_authenticated_user_and_peer_at_top_level() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitter, runtime) = crate::eventlog::test_support::emitter_with_writer(
            SharedBufferWriter(Arc::clone(&buffer)),
        );

        emit_session_auth_ok(
            &emitter,
            test_session_log_id(),
            "alice",
            Some("198.51.100.7"),
            Some(42),
        );
        runtime
            .handle()
            .flush(std::time::Duration::from_secs(1))
            .expect("flush canonical sink");

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text.lines().next().expect("one canonical JSON line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(value["user"], serde_json::json!("alice"));
        assert_eq!(value["peer_addr"], serde_json::json!("198.51.100.7"));
        // Identity never leaks into nested structured fields either.
        assert!(value["fields"].get("username").is_none());

        // Native sink policy is unaffected: journald's closed field
        // allowlist never reads `user`/`peer_addr`/`host` from the same
        // canonical JSON value, so delivery there continues to omit
        // identity even though the canonical file now carries it.
        let journal_fields = crate::eventlog::build_journal_fields_from_canonical(&value)
            .expect("journal field mapping succeeds")
            .expect("event_id present");
        assert!(!journal_fields
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("user")
                || key.eq_ignore_ascii_case("peer_addr")
                || key.eq_ignore_ascii_case("host")));
    }

    #[test]
    fn session_auth_fail_never_carries_user_but_keeps_peer_addr() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (emitter, runtime) = crate::eventlog::test_support::emitter_with_writer(
            SharedBufferWriter(Arc::clone(&buffer)),
        );

        emit_session_auth_fail(
            &emitter,
            test_session_log_id(),
            Some("198.51.100.7"),
            "pam_authenticate",
            "rejected",
        );
        runtime
            .handle()
            .flush(std::time::Duration::from_secs(1))
            .expect("flush canonical sink");

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text.lines().next().expect("one canonical JSON line");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert!(value["user"].is_null());
        assert_eq!(value["peer_addr"], serde_json::json!("198.51.100.7"));
    }

    /// Thread-safe in-memory canonical writer used only to assert on the
    /// exact JSON Lines bytes a real [`arcen_observability::ObservabilityRuntime`]
    /// produces (mirrors similar fixtures already used by `logging::tests`).
    #[derive(Clone)]
    struct SharedBufferWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn session_stream_start_reflects_the_resolved_media_plan() {
        let (emitter, recorded) = LifecycleEmitter::recording();
        let plan = resolved_media_plan();

        emit_session_stream_start(
            &emitter,
            test_session_log_id(),
            &plan,
            Some("alice"),
            Some("198.51.100.7"),
        );

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].kind(), LifecycleEventKind::SessionStreamStart);
        let fields = recorded[0].fields().as_map();
        assert_eq!(
            fields.get("width"),
            Some(&FieldValue::Integer(i64::from(plan.width)))
        );
        assert_eq!(
            fields.get("codec"),
            Some(&FieldValue::String(plan.codec_token().to_string()))
        );
    }

    #[test]
    fn clean_client_close_emits_session_end_not_interrupted() {
        let (emitter, recorded) = LifecycleEmitter::recording();

        emit_session_end_or_interrupted(
            &emitter,
            test_session_log_id(),
            SessionEndReason::ClientClosed,
            Duration::from_millis(1200),
            10,
            0,
            Some("alice"),
            Some("198.51.100.7"),
            None,
        );

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].kind(), LifecycleEventKind::SessionEnd);
        assert_eq!(
            recorded[0].fields().as_map().get("reason_class"),
            Some(&FieldValue::String("client_closed".to_string()))
        );
    }

    #[test]
    fn transport_and_media_failures_emit_session_interrupted_with_a_stage() {
        let (emitter, recorded) = LifecycleEmitter::recording();

        for reason in [
            SessionEndReason::ReadLivenessTimeout,
            SessionEndReason::TransportError,
            SessionEndReason::MediaEnded,
            SessionEndReason::WriterEnded,
        ] {
            emit_session_end_or_interrupted(
                &emitter,
                test_session_log_id(),
                reason,
                Duration::from_millis(500),
                1,
                1,
                Some("alice"),
                Some("198.51.100.7"),
                None,
            );
        }

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 4);
        for event in &recorded {
            assert_eq!(event.kind(), LifecycleEventKind::SessionInterrupted);
            assert!(event.fields().as_map().contains_key("stage"));
        }
    }

    /// Native lifecycle delivery must never change what the caller returns,
    /// even when every native sink is failing.
    #[test]
    fn failure_injected_emitter_never_changes_the_auth_outcome() {
        let (emitter, _runtime) = crate::eventlog::test_support::emitter_with_writer(
            crate::eventlog::test_support::AlwaysFailWriter,
        );

        emit_session_auth_ok(&emitter, test_session_log_id(), "alice", None, None);
        emit_session_auth_fail(
            &emitter,
            test_session_log_id(),
            None,
            "pam_authenticate",
            "timeout",
        );
        emit_session_stream_start(
            &emitter,
            test_session_log_id(),
            &resolved_media_plan(),
            Some("alice"),
            Some("198.51.100.7"),
        );
        emit_session_end_or_interrupted(
            &emitter,
            test_session_log_id(),
            SessionEndReason::TransportError,
            Duration::from_secs(1),
            0,
            0,
            Some("alice"),
            Some("198.51.100.7"),
            None,
        );
        // No panics, no blocking, and nothing above returned early: the
        // caller-visible control flow is entirely unaffected by sink failure.
    }

    #[test]
    fn launcher_error_reason_classes_are_a_closed_safe_set() {
        // Every variant maps to a fixed lowercase snake_case token — never
        // the error's `Display` text (which may carry PAM/child detail).
        let samples = [
            LauncherError::BinaryUnavailable,
            LauncherError::Rejected,
            LauncherError::Protocol,
            LauncherError::Timeout,
            LauncherError::PamInitialization,
            LauncherError::PamSession,
            LauncherError::LogindSession,
            LauncherError::RootRequired,
            LauncherError::XorgConfig,
            LauncherError::XorgStart,
            LauncherError::XorgVerify,
            LauncherError::XorgRelease,
            LauncherError::LogindActivation,
            LauncherError::LogindUnlock,
        ];
        for error in samples {
            let reason = launcher_error_reason_class(&error);
            assert!(reason
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_'));
        }
    }

    fn prepared_disclaimer() -> Arc<PreparedDisclaimer> {
        Arc::new(
            PreparedDisclaimer::from_bytes(
                arcen_identity::DisclaimerLocale::new("en_US").unwrap(),
                b"Authorized use only.",
            )
            .unwrap(),
        )
    }

    #[test]
    fn disclaimer_gate_blocks_pam_and_launcher_entry_on_negative_acknowledgments() {
        let disclaimer = prepared_disclaimer();
        let mut response = AuthResponse::pam("user", "password");
        assert_eq!(
            validate_disclaimer_acknowledgment(Some(disclaimer.clone()), &response).unwrap_err(),
            "acknowledgment_missing"
        );
        response.disclaimer_acceptance_sha256 = Some("0".repeat(64));
        assert_eq!(
            validate_disclaimer_acknowledgment(Some(disclaimer.clone()), &response).unwrap_err(),
            "acknowledgment_mismatch"
        );
        response.disclaimer_acceptance_sha256 = Some("A".repeat(64));
        assert_eq!(
            validate_disclaimer_acknowledgment(Some(disclaimer), &response).unwrap_err(),
            "acknowledgment_invalid"
        );
    }

    #[test]
    fn disclaimer_gate_accepts_only_the_exact_digest() {
        let disclaimer = prepared_disclaimer();
        let mut response = AuthResponse::pam("user", "password");
        response.disclaimer_acceptance_sha256 = Some(disclaimer.digest().to_lower_hex());
        assert!(
            validate_disclaimer_acknowledgment(Some(disclaimer), &response)
                .unwrap()
                .is_some()
        );
        assert!(
            validate_disclaimer_acknowledgment(None, &AuthResponse::pam("user", "password"))
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn linux_audio_encoder_preserves_legacy_pcm_and_emits_bounded_v1_opus() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let pcm: Vec<u8> = (0..3_840).map(|index| (index % 251) as u8).collect();
        let queue = AudioQueue::new();
        let mut encoder =
            AudioFrameEncoder::new(policy.resolve(None, true, 128)).expect("legacy encoder");
        assert_eq!(
            encoder.encode(&pcm, 0x1122_3344, &queue),
            crate::session::audio::EncodeOutcome::Packet
        );
        let legacy = queue.dequeue().await.expect("legacy packet");
        assert_eq!(legacy.codec, AudioCodec::Pcm);
        assert_eq!(legacy.payload, pcm);

        let peer = policy.capabilities();
        encoder
            .reconfigure(policy.resolve(Some(&peer), true, 128))
            .expect("Opus configure");
        assert_eq!(
            encoder.encode(&pcm, 0x1122_3344, &queue),
            crate::session::audio::EncodeOutcome::Packet
        );
        let opus = queue.dequeue().await.expect("Opus packet");
        assert_eq!(opus.codec, AudioCodec::Opus);
        assert!((1..=arcen_media::audio::MAX_OPUS_PACKET_BYTES).contains(&opus.payload.len()));

        let mut decoder = arcen_media::audio::OpusDecoder::new().expect("decoder");
        let mut decoded = [0i16; 1_920];
        decoder
            .decode(&opus.payload, &mut decoded)
            .expect("decode host packet");

        encoder
            .reconfigure(policy.resolve(Some(&peer), true, 64))
            .expect("bitrate update");
        assert_eq!(
            encoder.encode(&pcm, 2, &queue),
            crate::session::audio::EncodeOutcome::Packet
        );
        encoder
            .reconfigure(policy.resolve(Some(&peer), false, 64))
            .expect("disable");
        assert_eq!(
            encoder.encode(&pcm, 3, &queue),
            crate::session::audio::EncodeOutcome::Skipped
        );
    }

    #[tokio::test]
    async fn linux_audio_waits_for_v1_result_mailbox_before_activation() {
        let peer = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        }
        .capabilities();
        let disabled = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        }
        .resolve(Some(&peer), false, 0);
        let audio = AudioControl::new(true, true, true, Some(peer), disabled);
        let (control, mut receiver) = ControlSender::channel(1);
        assert!(control.send_best_effort(Message::Text("occupied".into()), "test"));

        let stream = audio.resolve_with_codec_preflight(true, 128);
        let send = send_audio_result_required(&control, stream);
        tokio::pin!(send);
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut send)
            .await
            .is_err());
        assert!(!audio.stream().is_enabled());

        assert!(receiver.try_recv().is_ok());
        if send.await {
            audio.set_stream(stream);
        }
        assert_eq!(audio.stream().codec, Some(AudioCodec::Opus));
        let result = receiver.try_recv().expect("queued audio result");
        assert!(matches!(result, WriterControl::Message(Message::Text(_))));
    }

    #[test]
    fn linux_audio_reports_capture_unavailable_without_disabling_other_media() {
        let peer = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        }
        .capabilities();
        let initial = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        }
        .resolve(Some(&peer), false, 0);
        let audio = AudioControl::new(true, true, false, Some(peer), initial);
        let stream = audio.resolve(true, 128);
        assert!(!stream.is_enabled());
        assert_eq!(
            stream.reason,
            arcen_protocol::messages::AudioStreamReason::CaptureUnavailable
        );
        audio.record_sent(42);
        audio.record_encode_failure();
        assert_eq!(audio.wire_frames_sent(), 1);
        assert_eq!(audio.wire_bytes_sent(), 42);
        assert_eq!(audio.encode_failures(), 1);
    }

    #[tokio::test]
    async fn closed_audio_capture_queue_does_not_end_video_or_control_writer() {
        let (idr, _idr_rx) = fake_idr();
        let video = Arc::new(FrameQueue::new(idr));
        let audio = Arc::new(AudioQueue::new());
        audio.close();
        let clipboard = closed_clipboard_queue();
        let (control_tx, control_rx) = ControlSender::channel(1);
        assert!(
            control_tx
                .send_required(Message::Text("still-streaming".into()), "test")
                .await
        );
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let mut sender = tokio::spawn(sender_loop(
            RecordingSink(Arc::clone(&sent)),
            crate::session::monitor_mux::VideoSource::Single(Arc::clone(&video)),
            audio,
            clipboard,
            control_rx,
            test_audio_control(),
            None,
            Duration::from_millis(100),
        ));

        tokio::time::timeout(Duration::from_millis(100), async {
            while sent.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("control must remain writable after audio ends");
        assert!(!sender.is_finished());

        video.close();
        drop(control_tx);
        tokio::time::timeout(Duration::from_millis(100), &mut sender)
            .await
            .expect("writer must stop after remaining streams close")
            .unwrap();
    }

    #[test]
    fn health_state_rank_orders_by_severity() {
        assert!(
            health_state_rank(arcen_telemetry::HealthState::Ok)
                < health_state_rank(arcen_telemetry::HealthState::Degraded)
        );
        assert!(
            health_state_rank(arcen_telemetry::HealthState::Degraded)
                < health_state_rank(arcen_telemetry::HealthState::Critical)
        );
    }

    #[test]
    fn effective_profile_fields_report_level_name_and_source() {
        let fields = effective_profile_fields(OperationalProfile::Debug, "cli_override");
        assert_eq!(
            fields.as_map().get("profile_level"),
            Some(&FieldValue::Integer(3))
        );
        assert_eq!(
            fields.as_map().get("profile_name"),
            Some(&FieldValue::String("debug".to_string()))
        );
        assert_eq!(
            fields.as_map().get("profile_source"),
            Some(&FieldValue::String("cli_override".to_string()))
        );
        // Production default (Level0/Critical) must round-trip too — this is
        // the packaged default, never baked-in Debug/pier-linux.example.internal.
        let production =
            effective_profile_fields(OperationalProfile::Critical, "production_default");
        assert_eq!(
            production.as_map().get("profile_level"),
            Some(&FieldValue::Integer(0))
        );
    }

    #[test]
    fn service_health_snapshot_reports_worst_state_since_last_tick_and_resets() {
        let (emitter, recorded) = LifecycleEmitter::recording();
        let service_health = AtomicU8::new(0);

        // Nothing observed yet: never a stand-in healthy value.
        emit_service_health_snapshot(&emitter, &service_health);
        assert_eq!(
            recorded
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .fields()
                .as_map()
                .get("overall_state"),
            Some(&FieldValue::String("unavailable".to_string()))
        );

        service_health.fetch_max(
            health_state_rank(arcen_telemetry::HealthState::Ok),
            Ordering::Relaxed,
        );
        service_health.fetch_max(
            health_state_rank(arcen_telemetry::HealthState::Critical),
            Ordering::Relaxed,
        );
        emit_service_health_snapshot(&emitter, &service_health);
        let events = recorded.lock().unwrap();
        assert_eq!(
            events
                .last()
                .unwrap()
                .fields()
                .as_map()
                .get("overall_state"),
            Some(&FieldValue::String("critical".to_string())),
            "the worst of the two sessions observed in the window must win"
        );
        assert_eq!(
            service_health.load(Ordering::Relaxed),
            0,
            "read-and-reset must not leave a stale severity for the next 60s window"
        );
    }

    /// A journald/syslog fake whose journal and syslog sends both always
    /// fail, mirroring `eventlog::tests::FakeDatagramApi` but scoped to this
    /// module since that fixture is private to `eventlog`.
    struct AlwaysFailJournalApi;

    impl crate::eventlog::JournalSyslogApi for AlwaysFailJournalApi {
        fn send_journal(&self, _bytes: &[u8]) -> Result<(), crate::eventlog::SocketSendError> {
            Err(crate::eventlog::SocketSendError::Unavailable)
        }

        fn send_syslog(&self, _bytes: &[u8]) -> Result<(), crate::eventlog::SocketSendError> {
            Err(crate::eventlog::SocketSendError::Unavailable)
        }
    }

    /// Re-review finding #3: sink loss must be drained and reported at the
    /// **service** heartbeat (`emit_service_loss_notices`, wired into
    /// `serve`'s own independent timer), not only from a per-session health
    /// tick — this is the exact call a short-lived session (which may end
    /// before its own first snapshot) can never provide on its own.
    #[test]
    fn service_loss_notices_drains_and_reports_without_a_session() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let runtime = arcen_observability::ObservabilityBuilder::new(
            arcen_telemetry::TelemetryRole::Host,
            arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
            arcen_telemetry::TelemetryPlatform::Linux,
            arcen_telemetry::OperationalProfile::Debug,
        )
        .canonical_writer("test", SharedBufferWriter(Arc::clone(&buffer)))
        .arcen_log(None::<String>)
        .register_sink(
            "journald",
            crate::eventlog::CanonicalJournalSink::new(AlwaysFailJournalApi),
        )
        .build()
        .expect("test observability runtime");
        let runtime = Arc::new(runtime);
        let emitter = LifecycleEmitter::new(runtime.handle(), Some("test-host".to_string()));

        // Drive one lifecycle event through the bridge with no session in
        // scope at all: both journal and syslog fail, so the shared runtime
        // counts one `delivery_failure` loss for the "journald" sink.
        crate::emit_lifecycle_event(
            &emitter,
            LifecycleEventKind::ServiceStart,
            eventlog::random_correlation_id(),
            {
                let mut fields = StructuredFields::default();
                let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
                fields
            },
        );
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");

        buffer.lock().expect("buffer lock").clear();
        emit_service_loss_notices(&emitter);
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");
        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text
            .lines()
            .next()
            .expect("the service heartbeat must drain and report the delta with no session");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(
            value["fields"]["sink"],
            serde_json::json!("journald:delivery_failure")
        );
        assert_eq!(value["fields"]["dropped_count"], serde_json::json!(1));

        // A second service-heartbeat drain with nothing new must be silent.
        buffer.lock().expect("buffer lock").clear();
        emit_service_loss_notices(&emitter);
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");
        assert!(
            buffer.lock().expect("buffer lock").is_empty(),
            "draining twice in a row without new loss must not re-emit a stale delta"
        );
    }

    /// Finding #3 (this round): `unreported_loss_error`'s pure
    /// zero/nonzero decision — the last step of the orderly shutdown
    /// drain — must return `Ok` for a fully-reported drain and an
    /// explicit, count-carrying `Err` (never a silent discard) for any
    /// residual.
    #[test]
    fn unreported_loss_error_is_ok_when_empty_and_an_explicit_err_otherwise() {
        assert_eq!(unreported_loss_error(0), Ok(()));
        let error = unreported_loss_error(3).expect_err("a nonzero residual must be an error");
        assert!(
            error.contains('3'),
            "the error must name the exact unreported residual count: {error}"
        );
    }

    /// Finding #3 (this round): a sink-loss delta that occurred after the
    /// **last** periodic service heartbeat (so no `emit_service_loss_notices`
    /// call has drained it yet) must still be drained, emitted, and flushed
    /// by the orderly shutdown sequence itself — a service that stops
    /// between two heartbeats must never silently lose that delta just
    /// because the process is exiting.
    #[test]
    fn shutdown_drain_reports_loss_that_occurred_after_the_last_heartbeat() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let runtime = arcen_observability::ObservabilityBuilder::new(
            arcen_telemetry::TelemetryRole::Host,
            arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
            arcen_telemetry::TelemetryPlatform::Linux,
            arcen_telemetry::OperationalProfile::Debug,
        )
        .canonical_writer("test", SharedBufferWriter(Arc::clone(&buffer)))
        .arcen_log(None::<String>)
        .register_sink(
            "journald",
            crate::eventlog::CanonicalJournalSink::new(AlwaysFailJournalApi),
        )
        .build()
        .expect("test observability runtime");
        let runtime = Arc::new(runtime);
        let emitter = LifecycleEmitter::new(runtime.handle(), Some("test-host".to_string()));

        // One lifecycle event whose journald delivery fails — a loss delta
        // now exists that no heartbeat has drained yet (this test never
        // calls `emit_service_loss_notices` before the shutdown drain).
        crate::emit_lifecycle_event(
            &emitter,
            LifecycleEventKind::ServiceStart,
            eventlog::random_correlation_id(),
            {
                let mut fields = StructuredFields::default();
                let _ = fields.insert("component", FieldValue::String("arcen-pier".to_string()));
                fields
            },
        );
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");
        buffer.lock().expect("buffer lock").clear();

        let outcome =
            drain_final_loss_before_stop(&emitter, &runtime.handle(), Duration::from_secs(1));
        assert_eq!(
            outcome,
            Ok(()),
            "a fully-reported drain must not surface as an error"
        );

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text
            .lines()
            .next()
            .expect("the shutdown drain itself must emit the never-yet-heartbeat-drained delta");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(
            value["fields"]["sink"],
            serde_json::json!("journald:delivery_failure")
        );
        assert_eq!(value["fields"]["dropped_count"], serde_json::json!(1));
    }

    /// Finding #3 (this round): a bounded-flush failure during the
    /// shutdown drain (the process's very last chance to deliver
    /// `SERVICE_STOP` and any pending loss notices) must surface as an
    /// explicit error, never be silently swallowed as a clean shutdown.
    #[test]
    fn shutdown_drain_reports_a_flush_failure_instead_of_swallowing_it() {
        let (emitter, runtime) = crate::eventlog::test_support::emitter_with_writer(
            crate::eventlog::test_support::AlwaysFailWriter,
        );
        let outcome =
            drain_final_loss_before_stop(&emitter, &runtime.handle(), Duration::from_secs(1));
        assert!(
            outcome.is_err(),
            "a canonical-writer flush failure must be reported, not swallowed"
        );
        assert!(
            outcome
                .unwrap_err()
                .contains("flush before shutdown loss drain"),
            "the error must name which bounded step failed"
        );
    }

    /// Fix (this round): every bounded phase of the shutdown drain must
    /// run unconditionally — a flush failure on one unhealthy sink must
    /// never skip emitting/delivering the loss notice it just produced to
    /// a separate, healthy, non-origin sink. Two canonical writers are
    /// registered: `"broken"`, whose every flush fails (producing exactly
    /// one `FlushFailure` delta the moment the drain's own first flush
    /// runs), and `"healthy"`, a normal in-memory buffer that must still
    /// receive the resulting origin-excluding `TELEMETRY_DROPPED` notice.
    #[test]
    fn shutdown_drain_delivers_a_flush_failure_notice_to_the_healthy_sink_despite_the_broken_one() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let runtime = arcen_observability::ObservabilityBuilder::new(
            arcen_telemetry::TelemetryRole::Host,
            arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
            arcen_telemetry::TelemetryPlatform::Linux,
            arcen_telemetry::OperationalProfile::Debug,
        )
        .canonical_writer("broken", crate::eventlog::test_support::AlwaysFailWriter)
        .canonical_writer("healthy", SharedBufferWriter(Arc::clone(&buffer)))
        .arcen_log(None::<String>)
        .build()
        .expect("test observability runtime");
        let runtime = Arc::new(runtime);
        let emitter = LifecycleEmitter::new(runtime.handle(), Some("test-host".to_string()));

        // No lifecycle event is emitted first: the only loss this test
        // exercises is the `FlushFailure` the drain's own first bounded
        // flush produces against the "broken" sink.
        let outcome =
            drain_final_loss_before_stop(&emitter, &runtime.handle(), Duration::from_secs(1));
        assert!(
            outcome.is_err(),
            "the broken sink's repeated flush failures must still be reported, not swallowed"
        );

        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text.lines().next().expect(
            "the healthy sink must receive the origin-excluding TELEMETRY_DROPPED notice for \
             the broken sink's flush failure even though the drain overall still reports an \
             error",
        );
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(
            value["fields"]["sink"],
            serde_json::json!("broken:flush_failure")
        );
        assert_eq!(value["fields"]["dropped_count"], serde_json::json!(1));
    }

    /// Re-review finding #7 (shutdown race): `abort()` alone only requests
    /// cancellation and does not guarantee a spawned task has actually
    /// stopped before the caller continues. Awaiting the handle afterward
    /// (the pattern now used at the real `health.abort(); let _ =
    /// health.await;` call site before SESSION_END) must guarantee no
    /// further task activity is observable once the await returns.
    #[tokio::test]
    async fn abort_then_await_guarantees_no_activity_after_return() {
        let ticks = Arc::new(AtomicU64::new(0));
        let task_ticks = Arc::clone(&ticks);
        let handle = tokio::spawn(async move {
            loop {
                task_ticks.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
        });

        // Let the task run for a bit so it is almost certainly mid-loop
        // when `abort` is requested below.
        tokio::time::sleep(Duration::from_millis(5)).await;
        handle.abort();
        let _ = handle.await;
        let ticks_at_join = ticks.load(Ordering::SeqCst);

        // If `abort()` alone (without awaiting) were relied on, a
        // still-scheduled poll could still run after this point; give any
        // such lingering activity ample time to occur.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            ticks_at_join,
            "no further task activity may occur once abort+await has returned, matching the \
             SESSION_END-adjacent `health.abort(); let _ = health.await;` shutdown ordering"
        );
    }
}
