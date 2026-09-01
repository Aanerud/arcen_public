use arcen_telemetry::{
    CorrelationId, FieldValue, HealthCause, HealthState, HealthTracker, LifecycleEventKind,
    StructuredFields,
};
use arcen_transport::quic::{
    connect_direct, recommended_transport_config_arc, DirectQuicDialParams, DirectQuicStream,
    QuicTransportError, DIRECT_QUIC_ALPN_PROTOCOL,
};
use arcen_transport::BoundedTransportPolicy;
use futures_util::{stream::FuturesUnordered, Sink, SinkExt, Stream, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(feature = "experimental-raw-hid")]
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;
#[cfg(feature = "wss-compat")]
use tokio_tungstenite::{connect_async_tls_with_config, MaybeTlsStream};
use tokio_tungstenite::{
    tungstenite::{
        protocol::{Role, WebSocketConfig},
        Error as WebSocketError, Message,
    },
    WebSocketStream,
};
use tracing::Instrument;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::clipboard::{media_policy, ClipboardItem, ClipboardSession};
use crate::observability::ClientTelemetry;
use crate::pipeline::frame_queue::{
    incoming_media_inbox, IncomingMediaReceiver, IncomingMediaSender, VIDEO_BYTE_LIMIT,
};
use crate::pipeline::video_decoder::{probe_decode_capabilities, DecodeCapabilities};
use crate::protocol::auth::hash_password;
use crate::protocol::fsm::{ClientEvent, ClientFsm};
use crate::protocol::messages::{
    msg_type, supports_region_input_v1, AudioBitrateTierMsg, AuthRequest, AuthResponse, AuthResult,
    ClientHelloMsg, ClientMonitor, ClientNetworkSnapshotMsg, ClientVideoCapabilitiesMsg,
    ClipboardContentKind, ClipboardDataMsg, ClipboardPolicyMsg, CursorMode, HealthPingMsg,
    HealthPongMsg, InitialVideoRequestMsg, InputCapabilitiesMsg, InputCapabilityAvailability,
    MicrophoneStreamConfigMsg, MicrophoneStreamReason, MicrophoneStreamResultMsg,
    MicrophoneStreamStopMsg, NetworkInterfaceKind, NetworkScopeMsg, QualitySettings,
    ResumeErrorCode, ServerHelloMsg, TabletModeCapabilitiesMsg, TabletModeMsg,
    VideoSelectionIntent, AUTH_REQUEST, AUTH_RESULT, BROKER_HELLO, CLIPBOARD_DATA,
    CLIPBOARD_PROTOCOL_VERSION, HEALTH_PONG, MICROPHONE_STREAM_RESULT,
    REGION_INPUT_PROTOCOL_VERSION, SERVER_HELLO,
};
use crate::protocol::wire::{
    decode_clipboard_chunk, encode_clipboard_chunk, encode_microphone_header, ClipboardChunkHeader,
    FrameType, MicrophoneHeader, CHUNK_BYTES,
};
use crate::reconnect::ResumeAttempt;
use crate::transport::connector::{resolve_transport, DirectTransportKind};
use crate::transport::tls::{
    is_tofu_capture_reject_message, is_tofu_pin_mismatch_message, CertInfo, TlsTrustConfig,
    TlsTrustError, TOFU_CAPTURE_REJECT_ERROR, TOFU_PIN_MISMATCH_ERROR,
};

const MAX_INCOMING_MESSAGE_SIZE: usize =
    VIDEO_BYTE_LIMIT + crate::protocol::REGION_VIDEO_HEADER_SIZE;
const MAX_INCOMING_CONTROL_SIZE: usize = 1024 * 1024;
const MAX_RESUME_GRANT_BYTES: usize = 8_192;
const MAX_DISCLAIMER_CONTENT_BYTES: usize = 16 * 1024;
const AUTH_INTERACTION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
// Stay clear of hosts that reject requests received just inside their 500 ms guard.
const FULL_FRAME_RETRY_INTERVAL: Duration = Duration::from_millis(750);
// Streaming keepalive: send a WebSocket Ping every N seconds so a dead write
// path surfaces quickly, and declare a stall if nothing arrives for STALL_TIMEOUT.
// 6 s is short enough to trigger reconnect before the user sees a frozen frame
// and manually disconnects; Pings every 2 s keep the RTT window small.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const STALL_TIMEOUT: Duration = Duration::from_secs(6);
const OUTBOUND_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const CLIENT_QOS_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_PROOF_INTERVALS: u64 = 12;

#[cfg(feature = "wss-compat")]
type DirectWssSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type DirectQuicSocket = WebSocketStream<DirectQuicStream>;

enum DirectSessionSocket {
    #[cfg(feature = "wss-compat")]
    Wss(DirectWssSocket),
    Quic(DirectQuicSocket),
}

impl DirectSessionSocket {
    async fn write_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => {
                socket.get_mut().write_all(bytes).await?;
                socket.get_mut().flush().await
            }
            Self::Quic(socket) => {
                socket.get_mut().write_all(bytes).await?;
                socket.get_mut().flush().await
            }
        }
    }

    fn quic_feedback(&self) -> Option<arcen_transport::quic::FeedbackSnapshot> {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss(_) => None,
            Self::Quic(socket) => Some(socket.get_ref().feedback_snapshot()),
        }
    }
}

impl Sink<Message> for DirectSessionSocket {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_ready(context),
            Self::Quic(socket) => Pin::new(socket).poll_ready(context),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).start_send(item),
            Self::Quic(socket) => Pin::new(socket).start_send(item),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_flush(context),
            Self::Quic(socket) => Pin::new(socket).poll_flush(context),
        }
    }

    fn poll_close(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_close(context),
            Self::Quic(socket) => Pin::new(socket).poll_close(context),
        }
    }
}

impl Stream for DirectSessionSocket {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_next(context),
            Self::Quic(socket) => Pin::new(socket).poll_next(context),
        }
    }
}

#[cfg(feature = "experimental-raw-hid")]
struct PermissionEventDeduper {
    granted: AtomicBool,
    denied: AtomicBool,
}

#[cfg(feature = "experimental-raw-hid")]
impl PermissionEventDeduper {
    const fn new() -> Self {
        Self {
            granted: AtomicBool::new(false),
            denied: AtomicBool::new(false),
        }
    }

    fn claim(&self, kind: LifecycleEventKind) -> bool {
        let flag = match kind {
            LifecycleEventKind::PermissionGranted => &self.granted,
            LifecycleEventKind::PermissionDenied => &self.denied,
            _ => return true,
        };
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[cfg(feature = "experimental-raw-hid")]
static PROCESS_PERMISSION_EVENTS: PermissionEventDeduper = PermissionEventDeduper::new();

/// Stand-in for the raw-HID event receiver used when the crate is built
/// without the `experimental-raw-hid` feature. `recv()` never resolves, so
/// this branch of the main `tokio::select!` loop is structurally present
/// (the macro has no per-branch `#[cfg]` support) but never actually fires,
/// and no raw-HID vendor code is compiled into default builds at all.
#[cfg(not(feature = "experimental-raw-hid"))]
struct DisabledHidEventReceiver;

#[cfg(not(feature = "experimental-raw-hid"))]
impl DisabledHidEventReceiver {
    async fn recv(&mut self) -> Option<std::convert::Infallible> {
        std::future::pending().await
    }
}

#[cfg(not(feature = "usb-hard-lab"))]
struct DisabledUsbHardResponder;

#[cfg(feature = "usb-hard-lab")]
async fn next_usb_hard_completion(
    responder: &mut Option<crate::usb_bridge::UsbHardResponder>,
) -> Result<Vec<u8>, String> {
    match responder {
        Some(responder) if responder.has_async_completions() => responder.next_completion().await,
        Some(_) | None => std::future::pending().await,
    }
}

#[cfg(not(feature = "usb-hard-lab"))]
async fn next_usb_hard_completion(
    _responder: &mut Option<DisabledUsbHardResponder>,
) -> Result<Vec<u8>, String> {
    std::future::pending().await
}

fn abort_on_microphone_cleanup_timeout() -> ! {
    tracing::error!(
        target: crate::logging::target::TRANSPORT,
        "microphone cleanup exceeded the session close deadline; terminating fail closed",
    );
    std::process::abort();
}

struct CaptureCancelState {
    session_closed: Arc<AtomicBool>,
    generation: Option<Arc<AtomicBool>>,
}

type CaptureCancelSlot = Arc<Mutex<CaptureCancelState>>;

struct SessionCancellation {
    closed: Arc<AtomicBool>,
    close_signal: watch::Sender<bool>,
    capture: CaptureCancelSlot,
    microphone_lifecycle: Arc<AtomicBool>,
    deadline: Mutex<Option<tokio::time::Instant>>,
}

impl SessionCancellation {
    fn new() -> (Arc<Self>, watch::Receiver<bool>) {
        let (close_signal, receiver) = watch::channel(false);
        let closed = Arc::new(AtomicBool::new(false));
        (
            Arc::new(Self {
                closed: Arc::clone(&closed),
                close_signal,
                capture: Arc::new(Mutex::new(CaptureCancelState {
                    session_closed: closed,
                    generation: None,
                })),
                microphone_lifecycle: Arc::new(AtomicBool::new(false)),
                deadline: Mutex::new(None),
            }),
            receiver,
        )
    }

    fn close(&self) {
        let _ = self.close_deadline();
        let capture = self
            .capture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.closed.store(true, Ordering::Release);
        if let Some(cancel) = capture.generation.as_ref() {
            cancel.store(true, Ordering::Release);
        }
        drop(capture);
        self.close_signal.send_replace(true);
    }

    fn close_deadline(&self) -> tokio::time::Instant {
        let mut deadline = self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *deadline.get_or_insert_with(|| tokio::time::Instant::now() + CLOSE_TIMEOUT)
    }

    fn cleanup_deadline(&self) -> tokio::time::Instant {
        if self.is_closed() {
            self.close_deadline()
        } else {
            tokio::time::Instant::now() + CLOSE_TIMEOUT
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    async fn cancelled(&self, receiver: &mut watch::Receiver<bool>) {
        if self.is_closed() {
            return;
        }
        while receiver.changed().await.is_ok() {
            if *receiver.borrow_and_update() {
                return;
            }
        }
    }
}

async fn cancellable_setup<T>(
    future: impl std::future::Future<Output = Result<T, ConnectSmokeError>>,
    cancellation: &SessionCancellation,
    close_receiver: &mut watch::Receiver<bool>,
) -> Result<T, ConnectSmokeError> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled(close_receiver) => Err(ConnectSmokeError::SessionClosed),
        result = future => result,
    }
}

/// What the client asks the host to stream. The default stays the
/// conservative WebSocket MVP profile (H.264 / 4:2:0 / 8-bit / limited range
/// / BT.709 / 15 fps) that the CPU-upload viewer path is known to sustain at
/// 4K; the CLI can request the native 4:4:4 path (h265 / yuv444) for
/// latency/quality testing.
///
/// `codec`/`chroma`/`bit_depth`/`color_range`/`color_matrix`/`encode_intent`
/// are wire tokens (`arcen_media::VideoCodec::token`/
/// `ChromaSubsampling::token`/`BitDepth::token`/`ColorRange::token`/
/// `ColorMatrix::token`/`EncodeIntent::token`), not typed
/// values: this profile is exactly the payload `rust_viewer_quality_settings`
/// forwards into `QualitySettings`, and keeping it as the wire's own strings
/// avoids a parallel typed copy of the same axes. `video_selection`
/// states whether the codec is exact or host-ranked; colour axes remain
/// explicit either way. The Deck's GUI path
/// populates every field here from
/// `ArcenApp::connect_options_with_stream_sizing_policy`, which resolves
/// `ColorFidelitySettings` (see `effective_color_fidelity_variant` in
/// `ui::app`) for the five colour axes, `PerformanceMode` for `max_fps`, and
/// `ClientSettings::encode_intent` for `encode_intent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamProfile {
    pub codec: String,
    pub chroma: String,
    /// Whether the codec is a diagnostic pin, a host-ranked ordinary-session
    /// preference, or a colour-fidelity target.
    pub video_selection: VideoSelectionIntent,
    pub max_fps: u32,
    /// Requested coded component depth, as a `BitDepth` wire token
    /// (`8`/`10`/`12`).
    pub bit_depth: String,
    /// Requested coded sample range (`limited`/`full`).
    pub color_range: String,
    /// Requested matrix coefficients (`bt709`/`identity`/`bt601`/`bt2020ncl`).
    pub color_matrix: String,
    /// Requested transfer characteristics (`bt709`/`srgb`/`pq`/`hlg`).
    ///
    /// **The axis that asks for HDR.** `pq` is what makes a host apply an HDR
    /// EDID, enable Advanced Color and take a wide capture; depth alone does
    /// not, because 10-bit BT.709 is an ordinary SDR request.
    pub transfer: String,
    /// Requested colour primaries (`bt709`/`bt2020`/`display_p3`).
    pub color_primaries: String,
    /// What the host's encoder should optimise for (`interactive`/`quality`).
    ///
    /// Not a colour axis: it never changes which pixels are requested, only
    /// how much the encoder is allowed to spend reaching them, so it takes
    /// no part in the negotiated-truth comparison the five axes above feed.
    pub encode_intent: String,
}

impl Default for StreamProfile {
    fn default() -> Self {
        Self {
            codec: "h264".to_string(),
            chroma: "yuv420".to_string(),
            video_selection: VideoSelectionIntent::Exact,
            max_fps: 15,
            bit_depth: "8".to_string(),
            color_range: "limited".to_string(),
            color_matrix: "bt709".to_string(),
            transfer: "bt709".to_string(),
            color_primaries: "bt709".to_string(),
            encode_intent: "interactive".to_string(),
        }
    }
}

#[derive(Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub username: String,
    pub password: String,
    pub timeout: Duration,
    pub tls: TlsTrustConfig,
    pub profile: StreamProfile,
    /// Client display layout reported to the host on the auth response so it can
    /// size the session's X server to the client's real displays. Empty ⇒ the
    /// host uses its configured default size.
    pub monitors: Vec<ClientMonitor>,
    /// How the client wants displays handled ("match_layout" | "single_primary"
    /// | "windowed" | "pick").
    pub displays_mode: String,
    /// Validated local multi-monitor-v1 requested topology, present only when
    /// `displays_mode` is `match_layout` with more than one active local
    /// display. `send_auth` requires a host `AuthRequest.multi_monitor_v1`
    /// offer before attaching it; a legacy host (no offer) makes `send_auth`
    /// fail the connection with an explicit unsupported-host error rather
    /// than silently sending only the primary display.
    pub multi_monitor_topology:
        Option<crate::transport::multi_monitor::RequestedMultiMonitorSelection>,
    /// IANA time-zone identifier captured once for this connection attempt.
    pub timezone: Option<String>,
    /// Cursor authority requested for the lifetime of this connection.
    pub cursor_preference: CursorMode,
    /// Explicit user instruction to replace a persistent remote desktop
    /// whose committed multi-monitor topology cannot serve this request.
    ///
    /// Set only after the user has chosen "start fresh" in response to the
    /// host reporting the conflict. Replacing the desktop closes the user's
    /// running remote applications, so it is never inferred.
    pub replace_incompatible_desktop: bool,
    /// Local persistent clipboard opt-in. Capability advertisement is false when off.
    pub clipboard_enabled: bool,
    /// Explicit per-launch microphone consent. Capture is never started when false.
    pub microphone_enabled: bool,
    /// Local persisted opt-in for typed tablet/pen input (AppKit
    /// `NSEventTypeTabletPoint`/`NSEventTypeTabletProximity` local
    /// termination). Advertised pen capability in `ClientHello` is always
    /// `Unavailable` when this is false, regardless of actual Wacom
    /// presence, so the host never expects — and the typed path never
    /// activates — pen input the user has turned off.
    pub tablet_input_enabled: bool,
    /// Per-connection tablet mode request.
    pub tablet_mode_requested: TabletModeMsg,
    /// Caller-owned lock-free client experience counters.
    pub telemetry: Arc<ClientTelemetry>,
    /// Legacy compatibility selector. Product builds ignore `false` and use
    /// QUIC; only explicit `wss-compat` builds can select WSS.
    pub quic_enabled: bool,
}

impl fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("use_tls", &self.use_tls)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("timeout", &self.timeout)
            .field("tls", &self.tls)
            .field("profile", &self.profile)
            .field("monitors", &self.monitors)
            .field("displays_mode", &self.displays_mode)
            .field("multi_monitor_topology", &self.multi_monitor_topology)
            .field("timezone", &self.timezone)
            .field("cursor_preference", &self.cursor_preference)
            .field("clipboard_enabled", &self.clipboard_enabled)
            .field("microphone_enabled", &self.microphone_enabled)
            .field("tablet_input_enabled", &self.tablet_input_enabled)
            .field("tablet_mode_requested", &self.tablet_mode_requested)
            .field("telemetry", &"<atomic counters>")
            .field("quic_enabled", &self.quic_enabled)
            .finish()
    }
}

impl Drop for ConnectOptions {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl ConnectOptions {
    #[cfg(feature = "wss-compat")]
    pub fn uri(&self) -> Result<Url, url::ParseError> {
        let scheme = if self.use_tls { "wss" } else { "ws" };
        Url::parse(&format!("{scheme}://{}:{}", self.host, self.port))
    }

    pub fn credential_free_clone(&self) -> Self {
        Self {
            host: self.host.clone(),
            port: self.port,
            use_tls: self.use_tls,
            username: String::new(),
            password: String::new(),
            timeout: self.timeout,
            tls: self.tls.clone(),
            profile: self.profile.clone(),
            monitors: self.monitors.clone(),
            displays_mode: self.displays_mode.clone(),
            multi_monitor_topology: self.multi_monitor_topology.clone(),
            replace_incompatible_desktop: self.replace_incompatible_desktop,
            timezone: self.timezone.clone(),
            cursor_preference: self.cursor_preference,
            clipboard_enabled: self.clipboard_enabled,
            microphone_enabled: self.microphone_enabled,
            tablet_input_enabled: self.tablet_input_enabled,
            tablet_mode_requested: self.tablet_mode_requested,
            telemetry: Arc::clone(&self.telemetry),
            quic_enabled: self.quic_enabled,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectSmokeResult {
    pub uri: String,
    pub server_hello: Option<ServerHelloMsg>,
    pub broker_hello: Option<Value>,
    pub fsm_state: &'static str,
}

#[derive(Debug)]
pub enum SessionEvent {
    CertificateUntrusted(CertInfo),
    AuthRequired(AuthRequest),
    Authenticated(SessionAuthentication),
    ServerHello(ServerHelloMsg),
    BrokerHello(Value),
    Json(Value),
    MicrophoneActive(bool),
    MediaReady,
    Ended(SessionEnd),
}

struct UpstreamMicrophone {
    capture: crate::microphone::MicrophoneCapture,
    receiver: crate::microphone::CapturedMicrophoneReceiver,
    cancel: Arc<AtomicBool>,
    cancel_slot: CaptureCancelSlot,
    lifecycle: Arc<AtomicBool>,
    encoder: Option<arcen_media::audio::OpusEncoder>,
    codec: crate::protocol::AudioCodec,
    generation: u32,
    opus: [u8; arcen_media::audio::MAX_OPUS_PACKET_BYTES],
    pcm: [u8; arcen_protocol::MICROPHONE_PCM_BYTES],
    session_log_id: CorrelationId,
    stats: arcen_media::audio::MicrophoneStatsTracker,
    started_at: std::time::Instant,
    armed: bool,
}

enum MicrophoneStartFailure {
    Channel(MicrophoneStreamReason, &'static str),
    Protocol(&'static str),
}

impl UpstreamMicrophone {
    fn start(
        config: MicrophoneStreamConfigMsg,
        cancel: Arc<AtomicBool>,
        cancel_slot: CaptureCancelSlot,
        lifecycle: Arc<AtomicBool>,
        session_log_id: CorrelationId,
    ) -> Result<Self, MicrophoneStartFailure> {
        if !config.is_valid_v1() {
            return Err(MicrophoneStartFailure::Protocol(
                "host selected an invalid microphone configuration",
            ));
        }
        let encoder = match config.codec {
            crate::protocol::AudioCodec::Opus => {
                let bitrate = match config.bitrate {
                    AudioBitrateTierMsg::Kbps32 => arcen_media::audio::AudioBitrateTier::Kbps32,
                    AudioBitrateTierMsg::Kbps64 => arcen_media::audio::AudioBitrateTier::Kbps64,
                    AudioBitrateTierMsg::Kbps128 => arcen_media::audio::AudioBitrateTier::Kbps128,
                    AudioBitrateTierMsg::Kbps256 => arcen_media::audio::AudioBitrateTier::Kbps256,
                    AudioBitrateTierMsg::Kbps510 => arcen_media::audio::AudioBitrateTier::Kbps510,
                    AudioBitrateTierMsg::Off => {
                        return Err(MicrophoneStartFailure::Protocol(
                            "host disabled microphone bitrate",
                        ));
                    }
                };
                Some(
                    arcen_media::audio::OpusEncoder::new_for_spec(
                        arcen_media::audio::AudioFrameSpec::MICROPHONE_V1,
                        bitrate,
                    )
                    .map_err(|_| {
                        MicrophoneStartFailure::Channel(
                            MicrophoneStreamReason::CaptureFailure,
                            "initialize microphone Opus encoder",
                        )
                    })?,
                )
            }
            crate::protocol::AudioCodec::Pcm => None,
        };
        let session_closed = Arc::clone(
            &cancel_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .session_closed,
        );
        let (capture, receiver) = crate::microphone::MicrophoneCapture::start_with_cancel(
            Arc::clone(&cancel),
            session_closed,
        )
        .map_err(|error| {
            let reason = if error == crate::microphone::MicrophoneCaptureError::PermissionDenied {
                MicrophoneStreamReason::PermissionDenied
            } else {
                MicrophoneStreamReason::CaptureFailure
            };
            MicrophoneStartFailure::Channel(reason, "start AVAudioEngine")
        })?;
        Ok(Self {
            capture,
            receiver,
            cancel,
            cancel_slot,
            lifecycle,
            encoder,
            codec: config.codec,
            generation: config.generation,
            opus: [0; arcen_media::audio::MAX_OPUS_PACKET_BYTES],
            pcm: [0; arcen_protocol::MICROPHONE_PCM_BYTES],
            session_log_id,
            stats: arcen_media::audio::MicrophoneStatsTracker::default(),
            started_at: std::time::Instant::now(),
            armed: true,
        })
    }

    fn encode(
        &mut self,
        captured: &crate::microphone::CapturedMicrophoneFrame,
    ) -> Result<Zeroizing<Vec<u8>>, &'static str> {
        let result = (|| {
            let payload = match self.codec {
                crate::protocol::AudioCodec::Opus => {
                    let length = self
                        .encoder
                        .as_mut()
                        .ok_or("microphone encoder missing")?
                        .encode(&captured.samples, &mut self.opus)
                        .map_err(|_| "microphone Opus encode failed")?;
                    &self.opus[..length]
                }
                crate::protocol::AudioCodec::Pcm => {
                    for (target, sample) in self.pcm.chunks_exact_mut(2).zip(&captured.samples) {
                        target.copy_from_slice(&sample.to_le_bytes());
                    }
                    &self.pcm[..]
                }
            };
            let header = encode_microphone_header(MicrophoneHeader {
                codec: self.codec,
                sequence: captured.sequence,
                timestamp_ms: captured.timestamp_ms,
                generation: self.generation,
            })
            .map_err(|_| "microphone header encode failed")?;
            let mut frame = Zeroizing::new(Vec::with_capacity(header.len() + payload.len()));
            frame.extend_from_slice(&header);
            frame.extend_from_slice(payload);
            Ok(frame)
        })();
        self.opus.zeroize();
        self.pcm.zeroize();
        if let Ok(frame) = &result {
            self.stats.record_encoded(frame.len());
        } else {
            self.stats.record_decoder_error();
        }
        result
    }

    fn collect_capture_stats(&mut self) {
        let (captured, dropped) = self.receiver.take_capture_counters();
        self.stats
            .record_captured_frames(captured, arcen_protocol::MICROPHONE_PCM_BYTES);
        self.stats.record_capture_queue_drop(dropped);
    }

    fn record_sent(&mut self, bytes: usize) {
        self.stats.record_sent(bytes);
    }

    fn record_transport_timeout(&mut self) {
        self.stats.record_transport_timeout();
    }

    fn log_stats(&mut self, final_snapshot: bool, stop_reason: &'static str) {
        self.collect_capture_stats();
        let stats = if final_snapshot {
            self.stats.total()
        } else {
            self.stats.take_interval()
        };
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = if final_snapshot { "mic_deck_teardown_summary" } else { "mic_deck_stats" },
            sid = %self.session_log_id,
            codec = ?self.codec,
            generation = self.generation,
            sample_rate_hz = 48_000u32,
            channels = 1u8,
            frame_duration_ms = 20u16,
            final_snapshot,
            duration_ms = self.started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            stop_reason,
            captured_frames = stats.captured_frames,
            captured_bytes = stats.captured_bytes,
            encoded_frames = stats.encoded_frames,
            encoded_bytes = stats.encoded_bytes,
            sent_frames = stats.sent_frames,
            sent_bytes = stats.sent_bytes,
            capture_queue_drop_oldest = stats.capture_queue_drops,
            transport_backpressure_drops = stats.transport_backpressure_drops,
            transport_timeouts = stats.transport_timeouts,
            "Deck microphone statistics"
        );
    }

    fn stop(mut self, stop_reason: &'static str) {
        self.capture.stop();
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = "mic_deck_capture_stopped",
            sid = %self.session_log_id,
            generation = self.generation,
            stop_reason,
            "Deck microphone capture stopped"
        );
        self.log_stats(true, stop_reason);
        self.lifecycle.store(false, Ordering::Release);
        self.armed = false;
    }
}

impl Drop for UpstreamMicrophone {
    fn drop(&mut self) {
        clear_capture_cancel(&self.cancel_slot, &self.cancel);
        self.lifecycle.store(false, Ordering::Release);
        self.opus.zeroize();
        self.pcm.zeroize();
        if self.armed {
            self.cancel.store(true, Ordering::Release);
            abort_on_microphone_cleanup_timeout();
        }
    }
}

struct PendingMicrophoneStart {
    cancel: Arc<AtomicBool>,
    cancel_slot: CaptureCancelSlot,
    lifecycle: Arc<AtomicBool>,
    codec: crate::protocol::AudioCodec,
    generation: u32,
    session_log_id: CorrelationId,
    started_at: std::time::Instant,
    task: Option<tokio::task::JoinHandle<Result<UpstreamMicrophone, MicrophoneStartFailure>>>,
    armed: bool,
}

impl PendingMicrophoneStart {
    fn spawn(
        config: MicrophoneStreamConfigMsg,
        cancel_slot: CaptureCancelSlot,
        lifecycle: Arc<AtomicBool>,
        session_log_id: CorrelationId,
    ) -> Option<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut state = cancel_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.session_closed.load(Ordering::Acquire) {
            cancel.store(true, Ordering::Release);
            return None;
        }
        state.generation = Some(Arc::clone(&cancel));
        lifecycle.store(true, Ordering::Release);
        drop(state);
        let task_cancel = Arc::clone(&cancel);
        let task_cancel_slot = Arc::clone(&cancel_slot);
        let task_lifecycle = Arc::clone(&lifecycle);
        let task_session_log_id = session_log_id.clone();
        let codec = config.codec;
        let generation = config.generation;
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = "mic_deck_capture_start",
            sid = %session_log_id,
            codec = ?config.codec,
            generation,
            sample_rate_hz = 48_000u32,
            channels = 1u8,
            frame_duration_ms = 20u16,
            "Deck microphone capture starting"
        );
        let task = tokio::task::spawn_blocking(move || {
            UpstreamMicrophone::start(
                config,
                task_cancel,
                task_cancel_slot,
                task_lifecycle,
                task_session_log_id,
            )
        });
        Some(Self {
            cancel,
            cancel_slot,
            lifecycle,
            codec,
            generation,
            session_log_id,
            started_at: std::time::Instant::now(),
            task: Some(task),
            armed: true,
        })
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    fn log_terminal_summary(&self, stop_reason: &'static str) {
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = "mic_deck_capture_stopped",
            sid = %self.session_log_id,
            generation = self.generation,
            stop_reason,
            "Deck microphone capture startup stopped"
        );
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = "mic_deck_teardown_summary",
            sid = %self.session_log_id,
            codec = ?self.codec,
            generation = self.generation,
            sample_rate_hz = 48_000u32,
            channels = 1u8,
            frame_duration_ms = 20u16,
            final_snapshot = true,
            duration_ms = self
                .started_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            stop_reason,
            captured_frames = 0u64,
            captured_bytes = 0u64,
            encoded_frames = 0u64,
            encoded_bytes = 0u64,
            sent_frames = 0u64,
            sent_bytes = 0u64,
            capture_queue_drop_oldest = 0u64,
            transport_backpressure_drops = 0u64,
            transport_timeouts = 0u64,
            "Deck microphone startup statistics"
        );
    }

    async fn cancel_and_join(
        mut self,
        deadline: Option<tokio::time::Instant>,
        stop_reason: &'static str,
    ) {
        self.cancel();
        if let Some(mut task) = self.task.take() {
            let result = if let Some(deadline) = deadline {
                match tokio::time::timeout_at(deadline, &mut task).await {
                    Ok(result) => result,
                    Err(_) => abort_on_microphone_cleanup_timeout(),
                }
            } else {
                task.await
            };
            match result {
                Ok(Ok(runtime)) => {
                    stop_microphone_runtime(runtime, deadline, stop_reason).await;
                }
                Ok(Err(_)) | Err(_) => self.log_terminal_summary(stop_reason),
            }
        } else {
            self.log_terminal_summary(stop_reason);
        }
        self.armed = false;
        clear_capture_cancel(&self.cancel_slot, &self.cancel);
        self.lifecycle.store(false, Ordering::Release);
    }

    async fn join(&mut self) -> Result<UpstreamMicrophone, MicrophoneStartFailure> {
        let result = match self.task.as_mut() {
            Some(task) => task.await.unwrap_or(Err(MicrophoneStartFailure::Channel(
                MicrophoneStreamReason::CaptureFailure,
                "microphone startup task failed",
            ))),
            None => Err(MicrophoneStartFailure::Channel(
                MicrophoneStreamReason::CaptureFailure,
                "microphone startup task ownership was lost",
            )),
        };
        self.task.take();
        self.armed = false;
        if result.is_err() {
            clear_capture_cancel(&self.cancel_slot, &self.cancel);
            self.lifecycle.store(false, Ordering::Release);
        }
        result
    }
}

impl Drop for PendingMicrophoneStart {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancel.store(true, Ordering::Release);
        clear_capture_cancel(&self.cancel_slot, &self.cancel);
        self.lifecycle.store(false, Ordering::Release);
        abort_on_microphone_cleanup_timeout();
    }
}

fn clear_capture_cancel(slot: &CaptureCancelSlot, cancel: &Arc<AtomicBool>) {
    let mut current = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current
        .generation
        .as_ref()
        .is_some_and(|registered| Arc::ptr_eq(registered, cancel))
    {
        current.generation = None;
    }
}

fn activate_microphone_if_open<T>(
    capture: &CaptureCancelSlot,
    active: &mut Option<T>,
    runtime: T,
    on_active: impl FnOnce(),
) -> Result<(), T> {
    let state = capture
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.session_closed.load(Ordering::Acquire) {
        return Err(runtime);
    }
    *active = Some(runtime);
    on_active();
    drop(state);
    Ok(())
}

fn microphone_stop_message(
    generation: u32,
    reason: MicrophoneStreamReason,
) -> Result<Message, serde_json::Error> {
    Ok(Message::Text(serde_json::to_string(
        &MicrophoneStreamStopMsg::new(generation, reason),
    )?))
}

async fn next_microphone_frame(
    microphone: &mut Option<UpstreamMicrophone>,
) -> Result<
    Option<crate::microphone::CapturedMicrophoneFrame>,
    crate::microphone::MicrophoneCaptureError,
> {
    match microphone {
        Some(microphone) => microphone.receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_microphone_start(
    pending: &mut Option<PendingMicrophoneStart>,
) -> Option<Result<UpstreamMicrophone, MicrophoneStartFailure>> {
    match pending {
        Some(pending) => Some(pending.join().await),
        None => std::future::pending().await,
    }
}

async fn wait_for_session_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        }
        None => std::future::pending().await,
    }
}

async fn stop_microphone_capture(
    microphone: &mut Option<UpstreamMicrophone>,
    pending: &mut Option<PendingMicrophoneStart>,
    deadline: Option<tokio::time::Instant>,
    stop_reason: &'static str,
) {
    if let Some(runtime) = microphone.take() {
        stop_microphone_runtime(runtime, deadline, stop_reason).await;
    }
    if let Some(startup) = pending.take() {
        startup.cancel_and_join(deadline, stop_reason).await;
    }
}

async fn stop_microphone_runtime(
    runtime: UpstreamMicrophone,
    deadline: Option<tokio::time::Instant>,
    stop_reason: &'static str,
) {
    let mut task = tokio::task::spawn_blocking(move || runtime.stop(stop_reason));
    if let Some(deadline) = deadline {
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            abort_on_microphone_cleanup_timeout();
        }
    } else {
        let _ = task.await;
    }
}

async fn send_bounded<S>(
    ws: &mut S,
    message: Message,
    cancellation: &SessionCancellation,
    close_receiver: &mut watch::Receiver<bool>,
) -> Result<(), ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    if cancellation.is_closed() {
        return Err(ConnectSmokeError::SessionClosed);
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled(close_receiver) => Err(ConnectSmokeError::SessionClosed),
        result = timeout(OUTBOUND_WRITE_TIMEOUT, ws.send(message)) => {
            result
                .map_err(|_| ConnectSmokeError::Timeout)?
                .map_err(|error| ConnectSmokeError::WebSocket(error.into()))
        }
    }
}

fn masked_client_binary_frame(
    payload: &mut [u8],
    mask: [u8; 4],
) -> Result<Zeroizing<Vec<u8>>, ConnectSmokeError> {
    let payload_len = u16::try_from(payload.len())
        .map_err(|_| ConnectSmokeError::Microphone("microphone wire frame is too large"))?;
    let mut wire = Zeroizing::new(Vec::with_capacity(payload.len() + 8));
    wire.push(0x82);
    if payload_len <= 125 {
        wire.push(0x80 | payload_len as u8);
    } else {
        wire.push(0x80 | 126);
        wire.extend_from_slice(&payload_len.to_be_bytes());
    }
    wire.extend_from_slice(&mask);
    wire.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| *byte ^ mask[index % mask.len()]),
    );
    payload.zeroize();
    Ok(wire)
}

async fn send_microphone_bounded(
    ws: &mut DirectSessionSocket,
    mut payload: Zeroizing<Vec<u8>>,
    cancellation: &SessionCancellation,
    close_receiver: &mut watch::Receiver<bool>,
) -> Result<(), ConnectSmokeError> {
    if cancellation.is_closed() {
        return Err(ConnectSmokeError::SessionClosed);
    }
    arcen_protocol::decode_microphone_frame(&payload)
        .map_err(|_| ConnectSmokeError::Microphone("invalid microphone wire frame"))?;
    let mut mask = [0u8; 4];
    getrandom::getrandom(&mut mask)
        .map_err(|error| ConnectSmokeError::Randomness(error.to_string()))?;
    let wire = masked_client_binary_frame(payload.as_mut_slice(), mask)?;
    tokio::select! {
        biased;
        () = cancellation.cancelled(close_receiver) => Err(ConnectSmokeError::SessionClosed),
        result = timeout(OUTBOUND_WRITE_TIMEOUT, async {
            SinkExt::flush(ws)
                .await
                .map_err(ConnectSmokeError::WebSocket)?;
            ws.write_raw(&wire)
                .await
                .map_err(|error| ConnectSmokeError::WebSocket(
                    tokio_tungstenite::tungstenite::Error::Io(error)
                ))
        }) => {
            result
                .map_err(|_| ConnectSmokeError::Timeout)?
        }
    }
}

async fn send_setup_message<S>(
    ws: &mut S,
    message: Message,
    cancellation: Option<&SessionCancellation>,
    close_receiver: Option<&mut watch::Receiver<bool>>,
) -> Result<(), ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    match (cancellation, close_receiver) {
        (Some(cancellation), Some(close_receiver)) => {
            send_bounded(ws, message, cancellation, close_receiver).await
        }
        (None, None) => ws
            .send(message)
            .await
            .map_err(|error| ConnectSmokeError::WebSocket(error.into())),
        _ => Err(ConnectSmokeError::SessionClosed),
    }
}

async fn close_bounded<S>(ws: &mut S, deadline: tokio::time::Instant)
where
    S: Sink<Message> + Unpin,
{
    if tokio::time::Instant::now() < deadline {
        let _ = tokio::time::timeout_at(deadline, SinkExt::close(ws)).await;
    }
}

#[derive(Debug, Clone)]
pub enum SessionCommand {
    SubmitAuth(AuthSubmission),
    AcceptAuthentication,
    DeclineDisclaimer,
    Json(Value),
    Binary(Vec<u8>),
    HardUsbPen(arcen_input::PenEvent),
    Close,
}

#[derive(Clone)]
pub struct SessionCommandSender {
    sender: mpsc::UnboundedSender<SessionCommand>,
    cancellation: Arc<SessionCancellation>,
}

impl SessionCommandSender {
    pub fn send(
        &self,
        command: SessionCommand,
    ) -> Result<(), mpsc::error::SendError<SessionCommand>> {
        if matches!(command, SessionCommand::Close) {
            self.cancellation.close();
        }
        self.sender.send(command)
    }

    #[cfg(test)]
    pub(crate) fn for_test(sender: mpsc::UnboundedSender<SessionCommand>) -> Self {
        let (cancellation, _) = SessionCancellation::new();
        Self {
            sender,
            cancellation,
        }
    }

    #[cfg(test)]
    fn session_closed(&self) -> bool {
        self.cancellation.is_closed()
    }

    pub fn microphone_lifecycle_active(&self) -> bool {
        self.cancellation
            .microphone_lifecycle
            .load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn set_microphone_lifecycle_for_test(&self, active: bool) {
        self.cancellation
            .microphone_lifecycle
            .store(active, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthenticationKind {
    InitialOptIn,
    Resume,
    Refresh,
}

pub struct SessionAuthentication {
    pub kind: AuthenticationKind,
    pub resume_grant: Option<String>,
    pub resume_window: Option<Duration>,
    pub resumed: bool,
    pub session_log_id: String,
}

impl fmt::Debug for SessionAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionAuthentication")
            .field("kind", &self.kind)
            .field(
                "resume_grant",
                &self.resume_grant.as_ref().map(|_| "<redacted>"),
            )
            .field("resume_window", &self.resume_window)
            .field("resumed", &self.resumed)
            .field("session_log_id", &self.session_log_id)
            .finish()
    }
}

impl Drop for SessionAuthentication {
    fn drop(&mut self) {
        if let Some(grant) = &mut self.resume_grant {
            grant.zeroize();
        }
        self.session_log_id.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientTransportError {
    UnexpectedEof,
    ConnectionReset,
    TimedOut,
    ConnectionRefused,
    NetworkUnreachable,
    HostUnreachable,
    ConnectionAborted,
    BrokenPipe,
    NotConnected,
    TransientIo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalDisconnect {
    Manual,
    GracefulHostClose,
    Authentication,
    Protocol,
    TlsIdentity,
    Resume(Option<ResumeErrorCode>),
    Configuration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    Transient(TransientTransportError),
    Terminal(TerminalDisconnect),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnd {
    pub reason: DisconnectReason,
    pub message: String,
    pub observed_at: Instant,
}

impl SessionEnd {
    pub fn transient(&self) -> bool {
        matches!(self.reason, DisconnectReason::Transient(_))
    }

    fn manual() -> Self {
        Self {
            reason: DisconnectReason::Terminal(TerminalDisconnect::Manual),
            message: "Disconnected".to_string(),
            observed_at: Instant::now(),
        }
    }

    fn graceful_host() -> Self {
        Self {
            reason: DisconnectReason::Terminal(TerminalDisconnect::GracefulHostClose),
            message: "Host closed the session".to_string(),
            observed_at: Instant::now(),
        }
    }
}

enum SessionAuth {
    Legacy,
    InitialOptIn { holder_nonce: String },
    Resume(ResumeAttempt),
}

impl fmt::Debug for SessionAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy => formatter.write_str("Legacy"),
            Self::InitialOptIn { .. } => formatter.write_str("InitialOptIn(<redacted>)"),
            Self::Resume(attempt) => formatter.debug_tuple("Resume").field(attempt).finish(),
        }
    }
}

impl Drop for SessionAuth {
    fn drop(&mut self) {
        if let Self::InitialOptIn { holder_nonce } = self {
            holder_nonce.zeroize();
        }
    }
}

#[derive(Clone)]
pub struct AuthSubmission {
    pub username: String,
    pub password: String,
}

impl fmt::Debug for AuthSubmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthSubmission")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Drop for AuthSubmission {
    fn drop(&mut self) {
        self.clear_sensitive();
    }
}

impl AuthSubmission {
    fn clear_sensitive(&mut self) {
        self.password.zeroize();
    }
}

pub struct FullFrameRequestGate {
    min_interval: Duration,
    last_sent: Option<std::time::Instant>,
    pending: bool,
}

impl Default for FullFrameRequestGate {
    fn default() -> Self {
        Self {
            min_interval: FULL_FRAME_RETRY_INTERVAL,
            last_sent: None,
            pending: false,
        }
    }
}

impl FullFrameRequestGate {
    pub fn request(&mut self) {
        self.pending = true;
    }

    pub fn cancel_pending(&mut self) {
        self.pending = false;
    }

    pub fn is_pending(&self) -> bool {
        self.pending
    }

    pub fn retry_after(&self) -> Option<Duration> {
        self.pending.then(|| {
            self.last_sent
                .map(|last| self.min_interval.saturating_sub(last.elapsed()))
                .unwrap_or_default()
        })
    }

    pub fn send_due(&mut self, commands: &SessionCommandSender) -> bool {
        if self.retry_after().is_none_or(|delay| !delay.is_zero()) {
            return false;
        }
        if commands
            .send(SessionCommand::Json(serde_json::json!({
                "type": "request_full_frame"
            })))
            .is_err()
        {
            self.pending = false;
            return false;
        }
        self.last_sent = Some(std::time::Instant::now());
        true
    }

    #[cfg(test)]
    fn with_interval(min_interval: Duration) -> Self {
        Self {
            min_interval,
            ..Self::default()
        }
    }
}

pub struct SessionHandle {
    pub events: mpsc::UnboundedReceiver<SessionEvent>,
    pub media: IncomingMediaReceiver,
    pub commands: SessionCommandSender,
    pub clipboard: ClipboardSession,
}

/// Stable leading text of [`ConnectSmokeError::MultiMonitorUnsupported`]'s
/// rendered message.
///
/// The negotiation failure reaches the UI as an already-formatted string in
/// `SessionEnd::message`, so recognising it there means matching on text.
/// Pinning the prefix to one constant (asserted against the real rendered
/// message by `multi_monitor_unsupported_message_keeps_its_recognisable_prefix`)
/// keeps the UI's recovery offer from silently disappearing if the wording is
/// ever edited.
pub const MULTI_MONITOR_UNSUPPORTED_PREFIX: &str =
    "Match My Layout could not be negotiated with this host";

#[derive(Debug, Error)]
pub enum ConnectSmokeError {
    #[error("invalid WebSocket URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("TLS trust configuration error: {0}")]
    Tls(#[from] TlsTrustError),
    #[error("TLS identity error: {0}")]
    TlsIdentity(String),
    #[error("untrusted server certificate")]
    CertificateUntrusted(CertInfo),
    #[error("connection timed out")]
    Timeout,
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("resume rejected: {message}")]
    ResumeRejected {
        message: String,
        error_code: Option<ResumeErrorCode>,
    },
    #[error("resume protocol error: {0}")]
    ResumeProtocol(&'static str),
    #[error("microphone stream error: {0}")]
    Microphone(&'static str),
    #[error(
        "Match My Layout could not be negotiated with this host: {0}. \
         Switch Displays to Primary Display Only in Settings and reconnect to use this host."
    )]
    MultiMonitorUnsupported(String),
    #[error("host requires interactive disclaimer acceptance")]
    DisclaimerRequired,
    #[error("host sent an invalid disclaimer: {0}")]
    InvalidDisclaimer(&'static str),
    #[error("authentication was declined")]
    AuthDeclined,
    #[error("unexpected first message type: {0}")]
    UnexpectedFirstMessage(String),
    #[error("expected text message, got binary/control frame")]
    NonTextMessage,
    #[error("host closed the connection during setup{0}")]
    ClosedByHost(String),
    #[error("incoming control message is {size} bytes; limit is {limit} bytes")]
    ControlMessageTooLarge { size: usize, limit: usize },
    #[error("clipboard protocol error: {0}")]
    Clipboard(&'static str),
    #[error("operating-system randomness is unavailable: {0}")]
    Randomness(String),
    #[error("connection ended without a WebSocket close frame")]
    UnexpectedEof,
    #[error("session was closed locally")]
    SessionClosed,
    #[error("{0}")]
    TransportUnavailable(String),
}

pub async fn connect_smoke(
    options: ConnectOptions,
) -> Result<ConnectSmokeResult, ConnectSmokeError> {
    let session_log_id = fresh_session_log_id()?;
    let identity = event_identity(&options);
    let telemetry = Arc::clone(&options.telemetry);
    let transport = crate::transport::connector::resolve_transport(&options)
        .map_err(|message| ConnectSmokeError::TransportUnavailable(message.to_string()))?;
    emit_connect_attempt(&session_log_id, identity.clone(), transport.label());
    let span = tracing::info_span!(
        target: crate::logging::target::SESSION,
        "deck_connection",
        sid = %session_log_id
    );
    let result = connect_smoke_correlated(options, session_log_id.clone())
        .instrument(span)
        .await;
    match &result {
        Ok(_) => {
            emit_auth_ok(&session_log_id, identity.clone(), "interactive");
            emit_connect_ok(&session_log_id, identity.clone(), None);
            emit_client_session_end(&session_log_id, identity, &telemetry, "smoke_complete");
        }
        Err(error) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "reason_class",
                FieldValue::String(connect_error_class(error).to_owned()),
            );
            let _ = fields.insert("stage", FieldValue::String("smoke".to_owned()));
            crate::logging::emit(
                LifecycleEventKind::ClientConnectFail,
                session_log_id,
                identity,
                fields,
                crate::logging::target::TRANSPORT,
                "Deck smoke connection failed",
                None,
            );
        }
    }
    result
}

async fn connect_smoke_correlated(
    mut options: ConnectOptions,
    session_log_id: CorrelationId,
) -> Result<ConnectSmokeResult, ConnectSmokeError> {
    let (mut ws, uri, mut fsm) = open_websocket(&options).await?;

    let first = recv_json(&mut ws, options.timeout).await?;
    match msg_type(&first).unwrap_or("<missing>") {
        AUTH_REQUEST => {
            let auth_request: AuthRequest = serde_json::from_value(first)?;
            validate_disclaimer(&auth_request)?;
            if auth_request.disclaimer.is_some() {
                return Err(ConnectSmokeError::DisclaimerRequired);
            }
            let submission = AuthSubmission {
                username: options.username.clone(),
                password: options.password.clone(),
            };
            send_auth(
                &mut ws,
                &options,
                &auth_request,
                submission,
                &session_log_id,
                &SessionAuth::Legacy,
                None,
                None,
            )
            .await?;
            options.password.zeroize();
            let auth_result: AuthResult =
                serde_json::from_value(recv_json(&mut ws, options.timeout).await?)?;
            if !auth_result.success {
                return Err(ConnectSmokeError::AuthFailed(auth_result.message));
            }
            let _ = fsm.send(ClientEvent::AuthOk);
            let hello = recv_json(&mut ws, options.timeout).await?;
            handle_hello(
                uri.as_str(),
                &mut ws,
                &mut fsm,
                &options,
                &session_log_id,
                hello,
            )
            .await
        }
        SERVER_HELLO | BROKER_HELLO => {
            options.password.zeroize();
            handle_hello(
                uri.as_str(),
                &mut ws,
                &mut fsm,
                &options,
                &session_log_id,
                first,
            )
            .await
        }
        other => Err(ConnectSmokeError::UnexpectedFirstMessage(other.to_string())),
    }
}

pub fn spawn_session(options: ConnectOptions) -> SessionHandle {
    spawn_session_with_auth(options, SessionAuth::Legacy, None)
}

pub fn spawn_session_opt_in(options: ConnectOptions, holder_nonce: String) -> SessionHandle {
    spawn_session_with_auth(options, SessionAuth::InitialOptIn { holder_nonce }, None)
}

pub fn spawn_resume_session(
    options: ConnectOptions,
    attempt: ResumeAttempt,
    deadline: Instant,
) -> SessionHandle {
    spawn_session_with_auth(options, SessionAuth::Resume(attempt), Some(deadline))
}

fn reconnect_lifecycle_fields(attempt: &ResumeAttempt) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("attempt", FieldValue::Integer(i64::from(attempt.attempt)));
    let _ = fields.insert(
        "gap_ms",
        FieldValue::Integer(i64::try_from(attempt.gap.as_millis()).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert("reason_class", FieldValue::String("transport".to_owned()));
    fields
}

fn spawn_session_with_auth(
    options: ConnectOptions,
    auth: SessionAuth,
    resume_deadline: Option<Instant>,
) -> SessionHandle {
    let (event_tx, events) = mpsc::unbounded_channel();
    let (command_tx, commands) = mpsc::unbounded_channel();
    let (cancellation, close_receiver) = SessionCancellation::new();
    let (media_tx, media) = incoming_media_inbox();
    let clipboard = ClipboardSession::new();
    let task_clipboard = clipboard.clone();
    let task_cancellation = Arc::clone(&cancellation);
    tokio::spawn(async move {
        run_session(
            options,
            auth,
            resume_deadline,
            event_tx,
            media_tx,
            commands,
            task_clipboard,
            task_cancellation,
            close_receiver,
        )
        .await;
    });
    SessionHandle {
        events,
        media,
        commands: SessionCommandSender {
            sender: command_tx,
            cancellation,
        },
        clipboard,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    options: ConnectOptions,
    auth: SessionAuth,
    resume_deadline: Option<Instant>,
    tx: mpsc::UnboundedSender<SessionEvent>,
    media: IncomingMediaSender,
    commands: mpsc::UnboundedReceiver<SessionCommand>,
    clipboard: ClipboardSession,
    cancellation: Arc<SessionCancellation>,
    close_receiver: watch::Receiver<bool>,
) {
    let session_log_id = match fresh_session_log_id() {
        Ok(id) => id,
        Err(error) => {
            let _ = tx.send(SessionEvent::Ended(classify_disconnect(&error)));
            return;
        }
    };
    let identity = event_identity(&options);
    let telemetry = Arc::clone(&options.telemetry);
    let auth_method = session_auth_method(&auth);
    let transport = match crate::transport::connector::resolve_transport(&options) {
        Ok(transport) => transport,
        Err(message) => {
            let error = ConnectSmokeError::TransportUnavailable(message.to_string());
            let _ = tx.send(SessionEvent::Ended(classify_disconnect(&error)));
            return;
        }
    };
    emit_connect_attempt(&session_log_id, identity.clone(), transport.label());
    if let SessionAuth::Resume(attempt) = &auth {
        telemetry.record_reconnect();
        crate::logging::emit(
            LifecycleEventKind::ClientReconnect,
            session_log_id.clone(),
            identity.clone(),
            reconnect_lifecycle_fields(attempt),
            crate::logging::target::SESSION,
            "Deck reconnect attempt started",
            None,
        );
    }

    let span = tracing::info_span!(
        target: crate::logging::target::SESSION,
        "deck_connection",
        sid = %session_log_id,
        previous_sid = tracing::field::Empty
    );
    if let SessionAuth::Resume(attempt) = &auth {
        span.record("previous_sid", attempt.previous_sid.as_str());
    }
    async move {
        let result = run_session_correlated(
            options,
            auth,
            tx.clone(),
            media,
            commands,
            clipboard,
            cancellation,
            close_receiver,
            resume_deadline,
            session_log_id.clone(),
        )
        .await;
        emit_session_result(&session_log_id, identity, &telemetry, auth_method, &result);
        publish_session_result(result, &tx);
    }
    .instrument(span)
    .await;
}

fn publish_session_result(
    result: Result<SessionEnd, ConnectSmokeError>,
    tx: &mpsc::UnboundedSender<SessionEvent>,
) {
    match result {
        Ok(end) => {
            let _ = tx.send(SessionEvent::Ended(end));
        }
        Err(ConnectSmokeError::CertificateUntrusted(info)) => {
            let _ = tx.send(SessionEvent::CertificateUntrusted(info));
        }
        Err(error) => {
            tracing::warn!(
                target: crate::logging::target::TRANSPORT,
                %error,
                "session ended with transport error",
            );
            let _ = tx.send(SessionEvent::Ended(classify_disconnect(&error)));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session_correlated(
    mut options: ConnectOptions,
    auth: SessionAuth,
    tx: mpsc::UnboundedSender<SessionEvent>,
    media: IncomingMediaSender,
    mut commands: mpsc::UnboundedReceiver<SessionCommand>,
    clipboard: ClipboardSession,
    cancellation: Arc<SessionCancellation>,
    mut close_receiver: watch::Receiver<bool>,
    resume_deadline: Option<Instant>,
    session_log_id: CorrelationId,
) -> Result<SessionEnd, ConnectSmokeError> {
    let (mut ws, _uri, mut fsm) =
        cancellable_setup(open_websocket(&options), &cancellation, &mut close_receiver).await?;

    let first = cancellable_setup(
        recv_json(&mut ws, options.timeout),
        &cancellation,
        &mut close_receiver,
    )
    .await?;
    let hello = match msg_type(&first).unwrap_or("<missing>") {
        AUTH_REQUEST => {
            tracing::info!(
                target: crate::logging::target::TRANSPORT,
                "server requires authentication",
            );
            let auth_request: AuthRequest = serde_json::from_value(first)?;
            validate_disclaimer(&auth_request)?;
            if matches!(auth, SessionAuth::Resume(_)) {
                validate_resume_request(&auth_request)?;
                send_resume_auth(
                    &mut ws,
                    &options,
                    &auth,
                    &auth_request,
                    &session_log_id,
                    Some(&cancellation),
                    Some(&mut close_receiver),
                )
                .await?;
            } else if auth_request.auth_mode.as_deref() == Some("none")
                && auth_request.disclaimer.is_none()
            {
                send_auth(
                    &mut ws,
                    &options,
                    &auth_request,
                    AuthSubmission {
                        username: String::new(),
                        password: String::new(),
                    },
                    &session_log_id,
                    &auth,
                    Some(&cancellation),
                    Some(&mut close_receiver),
                )
                .await?;
            } else {
                let _ = tx.send(SessionEvent::AuthRequired(auth_request.clone()));
                let command = timeout(AUTH_INTERACTION_TIMEOUT, commands.recv())
                    .await
                    .map_err(|_| ConnectSmokeError::Timeout)?
                    .ok_or(ConnectSmokeError::AuthDeclined)?;
                match command {
                    SessionCommand::SubmitAuth(submission) => {
                        send_auth(
                            &mut ws,
                            &options,
                            &auth_request,
                            submission,
                            &session_log_id,
                            &auth,
                            Some(&cancellation),
                            Some(&mut close_receiver),
                        )
                        .await?;
                    }
                    SessionCommand::DeclineDisclaimer => {
                        close_bounded(&mut ws, cancellation.cleanup_deadline()).await;
                        return Err(ConnectSmokeError::AuthDeclined);
                    }
                    SessionCommand::Close => {
                        close_bounded(&mut ws, cancellation.close_deadline()).await;
                        return Err(ConnectSmokeError::SessionClosed);
                    }
                    SessionCommand::AcceptAuthentication
                    | SessionCommand::Json(_)
                    | SessionCommand::Binary(_)
                    | SessionCommand::HardUsbPen(_) => {
                        close_bounded(&mut ws, cancellation.cleanup_deadline()).await;
                        return Err(ConnectSmokeError::AuthDeclined);
                    }
                }
            }
            let auth_result: AuthResult = serde_json::from_value(
                cancellable_setup(
                    recv_json(&mut ws, options.timeout),
                    &cancellation,
                    &mut close_receiver,
                )
                .await?,
            )?;
            if !auth_result.success {
                tracing::warn!(
                    target: crate::logging::target::TRANSPORT,
                    message = %auth_result.message,
                    "authentication rejected",
                );
                return match &auth {
                    SessionAuth::Resume(_) => Err(ConnectSmokeError::ResumeRejected {
                        message: bounded_message(&auth_result.message),
                        error_code: auth_result.error_code,
                    }),
                    SessionAuth::Legacy | SessionAuth::InitialOptIn { .. } => Err(
                        ConnectSmokeError::AuthFailed(bounded_message(&auth_result.message)),
                    ),
                };
            }
            if let Some(authentication) =
                validate_authentication_result(&auth, auth_result, &session_log_id)?
            {
                let _ = tx.send(SessionEvent::Authenticated(authentication));
                await_authentication_acceptance(&mut ws, &mut commands, &cancellation).await?;
            }
            tracing::info!(
                target: crate::logging::target::TRANSPORT,
                "authentication accepted",
            );
            let _ = fsm.send(ClientEvent::AuthOk);
            cancellable_setup(
                recv_json(&mut ws, options.timeout),
                &cancellation,
                &mut close_receiver,
            )
            .await?
        }
        SERVER_HELLO | BROKER_HELLO => {
            if matches!(auth, SessionAuth::Resume(_)) {
                return Err(ConnectSmokeError::ResumeProtocol(
                    "resume attempt bypassed the resume authentication exchange",
                ));
            }
            options.password.zeroize();
            first
        }
        other => return Err(ConnectSmokeError::UnexpectedFirstMessage(other.to_string())),
    };
    emit_auth_ok(
        &session_log_id,
        event_identity(&options),
        session_auth_method(&auth),
    );

    let mut clipboard_policy = None;
    let mut clipboard_reassembler = None;
    #[cfg(feature = "usb-hard-lab")]
    let mut usb_hard_responder: Option<crate::usb_bridge::UsbHardResponder> = None;
    #[cfg(not(feature = "usb-hard-lab"))]
    let mut usb_hard_responder: Option<DisabledUsbHardResponder> = None;
    // SEC-raw-hid: captured from the host's ServerHelloMsg before it is
    // moved into `tx.send(SessionEvent::ServerHello(..))` below. This is
    // half of the mutual negotiation required to ever start the
    // experimental raw-HID capture session further down — see its use at
    // the `HidSession::start` call site.
    #[cfg(feature = "experimental-raw-hid")]
    let mut host_experimental_raw_hid = false;
    match msg_type(&hello).unwrap_or("<missing>") {
        SERVER_HELLO => {
            let server_hello: ServerHelloMsg = serde_json::from_value(hello)?;
            validate_server_transport(&options, &server_hello)?;
            validate_server_region_input(&options, &server_hello)?;
            #[cfg(feature = "usb-hard-lab")]
            if options.tablet_mode_requested == TabletModeMsg::WacomUsbBridge {
                // Requesting the native bridge is a preference, not a
                // requirement. The tablet is a peripheral the user carries
                // between locations, and the host may not offer the bridge at
                // all; neither is a reason to refuse a desktop session. Every
                // failure here degrades to local termination, which is the
                // ordinary typed-pen path and always available.
                //
                // Degrading is also the safe direction for the failures that
                // are not merely "absent" -- a device denied by profile, or a
                // helper without privilege, simply goes un-bridged.
                let downgrade = if !server_hello.usb_hard_v1 {
                    Some("host does not advertise Hard USB v1".to_owned())
                } else {
                    match crate::usb_bridge::UsbHardResponder::start().await {
                        Ok(responder) => {
                            usb_hard_responder = Some(responder);
                            None
                        }
                        Err(error) => Some(error.to_string()),
                    }
                };
                if let Some(reason) = downgrade {
                    tracing::warn!(
                        target: crate::logging::target::TRANSPORT,
                        reason = %reason,
                        "native tablet bridge unavailable; continuing with local termination",
                    );
                    options.tablet_mode_requested = TabletModeMsg::LocalTermination;
                }
            }
            if fsm.state_id() == "authenticating" {
                let _ = fsm.send(ClientEvent::AuthOk);
            }
            let _ = fsm.send(ClientEvent::HelloReceived);
            #[cfg(feature = "usb-hard-lab")]
            let usb_hard_device = usb_hard_responder
                .as_ref()
                .map(crate::usb_bridge::UsbHardResponder::device);
            #[cfg(not(feature = "usb-hard-lab"))]
            let usb_hard_device = None;
            send_client_ready(
                &mut ws,
                &options,
                &session_log_id,
                usb_hard_device,
                Some(&cancellation),
                Some(&mut close_receiver),
            )
            .await?;
            emit_connect_ok(&session_log_id, event_identity(&options), None);
            clipboard_policy = negotiate_clipboard(&server_hello, options.clipboard_enabled);
            clipboard.set_policy(clipboard_policy);
            clipboard_reassembler = clipboard_policy.and_then(|policy| {
                usize::try_from(policy.max_bytes).ok().and_then(|maximum| {
                    arcen_protocol::clipboard::ClipboardReassembler::new(maximum).ok()
                })
            });
            tracing::info!(
                target: crate::logging::target::VIDEO,
                event = "media_plan_received",
                sid = %session_log_id,
                encoder_backend = %server_hello.encoder_backend,
                codec = %server_hello.codec,
                width = server_hello.screen_width,
                height = server_hello.screen_height,
                ready = true,
                "Deck received active media plan"
            );
            #[cfg(feature = "experimental-raw-hid")]
            {
                host_experimental_raw_hid = server_hello.experimental_raw_hid;
            }
            let _ = tx.send(SessionEvent::ServerHello(server_hello));
        }
        BROKER_HELLO => {
            let _ = tx.send(SessionEvent::BrokerHello(hello));
        }
        other => return Err(ConnectSmokeError::UnexpectedFirstMessage(other.to_string())),
    }

    let mut outbound = None;
    let mut microphone = None;
    let mut microphone_start = None;
    let mut latest_outbound_sequence = 0;
    let mut clipboard_expiry = tokio::time::interval(Duration::from_secs(1));
    clipboard_expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut client_qos = tokio::time::interval(CLIENT_QOS_INTERVAL);
    client_qos.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    client_qos.tick().await;
    let mut health_sequence = 0_u64;
    let mut last_pong_sequence = 0_u64;
    let mut missed_health = 0_u32;
    let mut sent_health = VecDeque::<(u64, Instant)>::with_capacity(8);
    let mut health_tracker = HealthTracker::default();
    let mut degraded_since_ms = None;
    let mut telemetry_window = options.telemetry.window(epoch_ms());
    let mut previous_network = crate::netinfo::probe(&options.host, options.port).snapshot;
    emit_network_active(
        &session_log_id,
        event_identity(&options),
        previous_network.as_ref(),
    );
    let mut microphone_stats_tick =
        tokio::time::interval(arcen_media::audio::MICROPHONE_STATS_INTERVAL);
    microphone_stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    microphone_stats_tick.tick().await;
    let mut last_received = tokio::time::Instant::now();
    let mut first_binary_observed = false;

    // SEC-raw-hid: the experimental raw-HID tablet capture path is compiled
    // in only when this crate is built with the `experimental-raw-hid`
    // feature, and even then a session is only ever started when BOTH this
    // client has a local runtime opt-in (`experimental_raw_hid_client_opt_in`)
    // AND the connected host mutually negotiated the capability in its
    // ServerHelloMsg (`host_experimental_raw_hid`, captured above). Default
    // builds and peers that never negotiated the capability never start
    // this session, so no raw HID data is ever sent or accepted. `hid_tx`
    // itself is kept alive for the life of this loop even when a session is
    // started, so `hid_rx.recv()` below correctly awaits (Pending) instead
    // of resolving immediately once the `HidSession`'s clone is dropped.
    //
    // `hid_rx` always has *some* value (a real bounded receiver when the
    // feature is enabled, or `DisabledHidEventReceiver` — a stub that never
    // resolves — otherwise) so the `tokio::select!` branch below is always
    // present syntactically (the macro has no support for per-branch
    // `#[cfg]`) while the raw-HID vendor code paths themselves remain fully
    // compiled out of default builds.
    #[cfg(feature = "experimental-raw-hid")]
    let (hid_tx, mut hid_rx) =
        tokio::sync::mpsc::channel::<crate::hid::HidEvent>(crate::hid::HID_EVENT_CHANNEL_CAPACITY);
    #[cfg(not(feature = "experimental-raw-hid"))]
    let mut hid_rx = DisabledHidEventReceiver;
    #[cfg(feature = "experimental-raw-hid")]
    let _hid_session = should_start_experimental_raw_hid_capture(
        crate::hid::experimental_raw_hid_client_opt_in(),
        host_experimental_raw_hid,
    )
    .then(|| crate::hid::HidSession::start(hid_tx.clone()));
    #[cfg(feature = "experimental-raw-hid")]
    let mut hid_devices = HashMap::<u8, (u16, u16, u64, u64)>::new();
    let outcome = async {
      loop {
        if clipboard.policy().is_none() {
            clipboard_policy = None;
            if let Some(reassembler) = clipboard_reassembler.as_mut() {
                reassembler.abort();
            }
            clipboard_reassembler = None;
            outbound = None;
        }
        if let Some(item) = clipboard.take_outbound() {
            if item.sequence > latest_outbound_sequence {
                if let Some(policy) = clipboard_policy {
                    if let Some(transfer) = OutboundClipboardTransfer::new(item, policy) {
                        latest_outbound_sequence = transfer.sequence();
                        outbound = Some(transfer);
                    }
                }
            }
        }
        tokio::select! {
            biased;
            command = commands.recv() => {
                let Some(command) = command else {
                    cancellation.close();
                    return Ok(SessionEnd::manual());
                };
                match command {
                    SessionCommand::SubmitAuth(_)
                    | SessionCommand::AcceptAuthentication
                    | SessionCommand::DeclineDisclaimer => {
                        tracing::warn!(
                            target: crate::logging::target::TRANSPORT,
                            "ignored authentication command after handshake",
                        );
                    }
                    SessionCommand::Json(value) => {
                        send_bounded(&mut ws, Message::Text(value.to_string()), &cancellation, &mut close_receiver).await?;
                    }
                    SessionCommand::Binary(bytes) => {
                        send_bounded(&mut ws, Message::Binary(bytes), &cancellation, &mut close_receiver).await?;
                    }
                    SessionCommand::HardUsbPen(event) => {
                        #[cfg(feature = "usb-hard-lab")]
                        if let Some(responder) = usb_hard_responder.as_mut() {
                            responder.update_pen(event);
                        }
                        #[cfg(not(feature = "usb-hard-lab"))]
                        {
                            let _ = event;
                            tracing::warn!(
                                target: crate::logging::target::USB,
                                "Hard USB pen state ignored because this Deck lacks usb-hard-lab",
                            );
                        }
                    }
                    SessionCommand::Close => {
                        return Ok(SessionEnd::manual());
                    }
                }
            }
            () = cancellation.cancelled(&mut close_receiver) => {
                return Ok(SessionEnd::manual());
            }
            _ = wait_for_session_deadline(resume_deadline) => {
                return Err(ConnectSmokeError::Timeout);
            }
            _ = tokio::time::sleep_until(last_received + STALL_TIMEOUT) => {
                return Err(ConnectSmokeError::Timeout);
            }
            _ = keepalive.tick() => {
                send_bounded(&mut ws, Message::Ping(vec![]), &cancellation, &mut close_receiver).await?;
            }
            _ = client_qos.tick() => {
                health_sequence = health_sequence.saturating_add(1);
                if last_pong_sequence < health_sequence.saturating_sub(1) {
                    missed_health = missed_health.saturating_add(1);
                } else {
                    missed_health = 0;
                }
                let network = crate::netinfo::probe(&options.host, options.port).snapshot;
                if network != previous_network {
                    emit_network_changed(
                        &session_log_id,
                        event_identity(&options),
                        previous_network.as_ref(),
                        network.as_ref(),
                    );
                    previous_network = network.clone();
                }
                let timestamp_ms = epoch_ms();
                let (telemetry, health, sample) = options.telemetry.snapshot(
                    &mut telemetry_window,
                    network,
                    timestamp_ms,
                    options.profile.max_fps,
                    missed_health,
                );
                emit_health_transition(
                    &session_log_id,
                    event_identity(&options),
                    &mut health_tracker,
                    &mut degraded_since_ms,
                    timestamp_ms,
                    &health,
                    &sample,
                );
                if health_sequence % CLIENT_PROOF_INTERVALS == 0 {
                    emit_health_snapshot(
                        &session_log_id,
                        event_identity(&options),
                        &health,
                        &sample,
                    );
                    emit_drop_notices(&session_log_id, event_identity(&options));
                }
                tracing::info!(
                    target: crate::logging::target::HEALTH,
                    sid = %session_log_id,
                    health = ?health.overall,
                    missed_health,
                    "five-second client QoS snapshot",
                );
                if let Some(feedback) = ws.quic_feedback() {
                    tracing::info!(
                        target: crate::logging::target::HEALTH,
                        sid = %session_log_id,
                        rtt_us = u64::try_from(feedback.rtt.as_micros()).unwrap_or(u64::MAX),
                        congestion_window_bytes = feedback.congestion_window,
                        congestion_events = feedback.congestion_events,
                        lost_packets = feedback.lost_packets,
                        lost_bytes = feedback.lost_bytes,
                        sent_packets = feedback.sent_packets,
                        current_mtu = feedback.current_mtu,
                        black_holes_detected = feedback.black_holes_detected,
                        "five-second QUIC path snapshot",
                    );
                }
                let ping = HealthPingMsg {
                    timestamp_ms,
                    sequence: health_sequence,
                    client_state: health
                        .overall
                        .map(health_state_name)
                        .unwrap_or("unavailable")
                        .to_owned(),
                    client_telemetry: Some(telemetry),
                    ..HealthPingMsg::default()
                };
                sent_health.push_back((health_sequence, Instant::now()));
                while sent_health.len() > 8 {
                    sent_health.pop_front();
                }
                send_bounded(
                    &mut ws,
                    Message::Text(serde_json::to_string(&ping)?),
                    &cancellation,
                    &mut close_receiver,
                ).await?;
            }
            _ = microphone_stats_tick.tick(), if microphone.is_some() => {
                if let Some(runtime) = microphone.as_mut() {
                    runtime.log_stats(false, "running");
                }
            }
            completion = next_usb_hard_completion(&mut usb_hard_responder) => {
                let completion = completion.map_err(|error| {
                    ConnectSmokeError::TransportUnavailable(format!(
                        "physical Hard USB completion failed: {error}"
                    ))
                })?;
                send_bounded(
                    &mut ws,
                    Message::Binary(completion),
                    &cancellation,
                    &mut close_receiver,
                )
                .await?;
            }
            message = ws.next() => {
                last_received = tokio::time::Instant::now();
                let Some(message) = message else {
                    return Err(ConnectSmokeError::UnexpectedEof);
                };
                match message? {
                    Message::Text(text) => {
                        if !control_size_allowed(text.len()) {
                            return Err(ConnectSmokeError::ControlMessageTooLarge {
                                size: text.len(),
                                limit: MAX_INCOMING_CONTROL_SIZE,
                            });
                        }
                        let value: Value = serde_json::from_str(&text)?;
                        if msg_type(&value) == Some(HEALTH_PONG) {
                            if let Ok(pong) = serde_json::from_value::<HealthPongMsg>(value.clone()) {
                                if let Some((_, sent)) = sent_health
                                    .iter()
                                    .find(|(sequence, _)| *sequence == pong.sequence)
                                {
                                    options.telemetry.record_rtt(sent.elapsed());
                                    last_pong_sequence = last_pong_sequence.max(pong.sequence);
                                }
                            }
                        }
                        if msg_type(&value) == Some(AUTH_RESULT) {
                            let result: AuthResult = serde_json::from_value(value)?;
                            let refresh =
                                validate_authentication_refresh(result, &session_log_id)?;
                            let _ = tx.send(SessionEvent::Authenticated(refresh));
                            continue;
                        }
                        if msg_type(&value) == Some(CLIPBOARD_DATA) {
                            handle_clipboard_offer(
                                value,
                                clipboard_policy,
                                clipboard_reassembler.as_mut(),
                            );
                            continue;
                        }
                        if msg_type(&value) == Some(MICROPHONE_STREAM_RESULT) {
                            let result = serde_json::from_value::<MicrophoneStreamResultMsg>(
                                value.clone(),
                            )?;
                            tracing::info!(
                                target: crate::logging::target::AUDIO,
                                event = "mic_negotiation",
                                sid = %session_log_id,
                                platform = "macos",
                                enabled = result.enabled && options.microphone_enabled,
                                user_enabled = options.microphone_enabled,
                                codec = ?result.config.as_ref().map(|config| config.codec),
                                sample_rate_hz = 48_000u32,
                                channels = 1u8,
                                frame_duration_ms = 20u16,
                                generation = ?result.config.as_ref().map(|config| config.generation),
                                reason = ?result.reason,
                                "microphone negotiation completed"
                            );
                            if let Some(runtime) = microphone.take() {
                                stop_microphone_runtime(
                                    runtime,
                                    Some(cancellation.cleanup_deadline()),
                                    "renegotiated",
                                )
                                .await;
                            }
                            if let Some(pending) = microphone_start.take() {
                                pending
                                    .cancel_and_join(
                                        Some(cancellation.cleanup_deadline()),
                                        "renegotiated",
                                    )
                                    .await;
                            }
                            if options.microphone_enabled && result.enabled {
                                microphone_start = result.config.and_then(|config| {
                                    PendingMicrophoneStart::spawn(
                                        config,
                                        Arc::clone(&cancellation.capture),
                                        Arc::clone(&cancellation.microphone_lifecycle),
                                        session_log_id.clone(),
                                    )
                                });
                            } else {
                                microphone_start = None;
                            }
                            let _ = tx.send(SessionEvent::MicrophoneActive(false));
                            let _ = tx.send(SessionEvent::Json(value));
                            continue;
                        }
                        let _ = tx.send(SessionEvent::Json(value));
                    }
                    Message::Binary(bytes) => {
                        if !first_binary_observed {
                            first_binary_observed = true;
                            tracing::debug!(
                                target: crate::logging::target::TRANSPORT,
                                bytes = bytes.len(),
                                frame_type = bytes.first().copied(),
                                "first binary frame received",
                            );
                        }
                        if bytes.first().copied() == Some(FrameType::Clipboard as u8) {
                            handle_clipboard_chunk(
                                &bytes,
                                clipboard_policy,
                                clipboard_reassembler.as_mut(),
                                &clipboard,
                            );
                            continue;
                        }
                        if bytes.first().copied() == Some(FrameType::UsbBridgeUrbSubmit as u8) {
                            #[cfg(feature = "usb-hard-lab")]
                            {
                                let Some(responder) = usb_hard_responder.as_mut() else {
                                    return Err(ConnectSmokeError::TransportUnavailable(
                                        "host sent a Hard USB URB without a captured responder"
                                            .to_owned(),
                                    ));
                                };
                                let (header, payload) =
                                    arcen_protocol::decode_usb_urb_submit(&bytes).map_err(|error| {
                                        ConnectSmokeError::TransportUnavailable(format!(
                                            "Hard USB submit frame is invalid: {error:?}"
                                        ))
                                    })?;
                                match responder.submit(header, payload).await.map_err(|error| {
                                    ConnectSmokeError::TransportUnavailable(format!(
                                        "Hard USB submit failed: {error}"
                                    ))
                                })? {
                                    crate::usb_bridge::SubmitResult::Immediate(completion) => {
                                        send_bounded(
                                            &mut ws,
                                            Message::Binary(completion),
                                            &cancellation,
                                            &mut close_receiver,
                                        )
                                        .await?;
                                    }
                                    crate::usb_bridge::SubmitResult::Pending => {}
                                }
                                continue;
                            }
                            #[cfg(not(feature = "usb-hard-lab"))]
                            {
                                return Err(ConnectSmokeError::TransportUnavailable(
                                    "host sent a Hard USB URB to a Deck without usb-hard-lab"
                                        .to_owned(),
                                ));
                            }
                        }
                        if bytes.first().copied() == Some(FrameType::UsbBridgeUrbCancel as u8) {
                            #[cfg(feature = "usb-hard-lab")]
                            {
                                let (generation, urb_id) =
                                    arcen_protocol::decode_usb_urb_cancel(&bytes).map_err(|error| {
                                        ConnectSmokeError::TransportUnavailable(format!(
                                            "Hard USB cancel frame is invalid: {error:?}"
                                        ))
                                    })?;
                                let Some(responder) = usb_hard_responder.as_mut() else {
                                    return Err(ConnectSmokeError::TransportUnavailable(
                                        "host cancelled a Hard USB URB without a captured responder"
                                            .to_owned(),
                                    ));
                                };
                                let completion = responder
                                    .cancel(generation, urb_id)
                                    .await
                                    .map_err(|error| {
                                        ConnectSmokeError::TransportUnavailable(format!(
                                            "Hard USB cancellation failed: {error}"
                                        ))
                                    })?;
                                // `None` means the helper will deliver the
                                // terminal completion asynchronously.
                                if let Some(completion) = completion {
                                    send_bounded(
                                        &mut ws,
                                        Message::Binary(completion),
                                        &cancellation,
                                        &mut close_receiver,
                                    )
                                    .await?;
                                }
                            }
                            continue;
                        }
                        match media.enqueue_bytes(&bytes) {
                            Ok(outcome) if outcome.notify => {
                                let _ = tx.send(SessionEvent::MediaReady);
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let message = format!("{error:?}");
                                tracing::debug!(
                                    target: crate::logging::target::TRANSPORT,
                                    ?error,
                                    "discarded malformed media packet",
                                );
                                if media.record_malformed(&bytes, message).notify {
                                    let _ = tx.send(SessionEvent::MediaReady);
                                }
                            }
                        }
                    }
                    Message::Close(_) => return Ok(SessionEnd::graceful_host()),
                    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            () = clipboard.outbound_notified() => {}
            () = clipboard_send_turn(outbound.is_some()) => {
                if clipboard.policy().is_none() {
                    outbound = None;
                    continue;
                }
                let mut finished = false;
                if let Some(transfer) = outbound.as_mut() {
                    let message = transfer.next_message()?;
                    send_bounded(&mut ws, message, &cancellation, &mut close_receiver).await?;
                    finished = transfer.is_finished();
                }
                if finished {
                    outbound = None;
                }
            }
            _ = clipboard_expiry.tick() => {
                if let Some(reassembler) = clipboard_reassembler.as_mut() {
                    let _ = reassembler.expire(Instant::now());
                }
            }
            Some(hid_event) = hid_rx.recv() => {
                #[cfg(not(feature = "experimental-raw-hid"))]
                {
                    // `DisabledHidEventReceiver::recv` never resolves, so
                    // this arm is unreachable in default builds; the
                    // `Infallible` type makes that provable at compile time.
                    let _: std::convert::Infallible = hid_event;
                    unreachable!("raw-HID capture is compiled out of this build");
                }
                #[cfg(feature = "experimental-raw-hid")]
                {
                use crate::hid::HidEvent;
                use arcen_protocol::{
                    encode_hid_device_added, encode_hid_device_removed, encode_hid_report,
                    HidDeviceAddedHeader,
                };
                let frame = match hid_event {
                    HidEvent::DeviceAdded { device_id, vendor_id, product_id, descriptor } => {
                        hid_devices.insert(device_id, (vendor_id, product_id, 0, 0));
                        emit_hid_start(
                            &session_log_id,
                            event_identity(&options),
                            device_id,
                            vendor_id,
                            product_id,
                        );
                        encode_hid_device_added(
                            HidDeviceAddedHeader { device_id, vendor_id, product_id },
                            &descriptor,
                        )
                    }
                    HidEvent::DeviceRemoved { device_id } => {
                        if let Some((vendor_id, product_id, reports, errors)) =
                            hid_devices.remove(&device_id)
                        {
                            emit_hid_end(
                                &session_log_id,
                                event_identity(&options),
                                device_id,
                                vendor_id,
                                product_id,
                                reports,
                                errors,
                            );
                        }
                        encode_hid_device_removed(device_id).to_vec()
                    }
                    HidEvent::Report { device_id, data } => {
                        if let Some((_, _, reports, _)) = hid_devices.get_mut(&device_id) {
                            *reports = reports.saturating_add(1);
                        }
                        encode_hid_report(device_id, &data)
                    }
                    HidEvent::Error { device_id, reason_class } => {
                        if let Some(device_id) = device_id {
                            if let Some((_, _, _, errors)) = hid_devices.get_mut(&device_id) {
                                *errors = errors.saturating_add(1);
                            }
                        }
                        emit_hid_error(
                            &session_log_id,
                            event_identity(&options),
                            device_id,
                            reason_class,
                        );
                        continue;
                    }
                    HidEvent::PermissionGranted => {
                        emit_permission(
                            &session_log_id,
                            event_identity(&options),
                            LifecycleEventKind::PermissionGranted,
                            "input_monitoring",
                        );
                        continue;
                    }
                };
                if let Err(error) = send_bounded(
                    &mut ws,
                    Message::Binary(frame),
                    &cancellation,
                    &mut close_receiver,
                ).await {
                    emit_hid_error(
                        &session_log_id,
                        event_identity(&options),
                        None,
                        "wire_error",
                    );
                    return Err(error);
                }
                }
            }
            frame = next_microphone_frame(&mut microphone) => {
                let frame = match frame {
                    Ok(Some(frame)) => frame,
                    Ok(None) => {
                        let generation = microphone.as_ref().map(|runtime| runtime.generation);
                        tracing::warn!(
                            target: crate::logging::target::AUDIO,
                            event = "mic_deck_capture_failure",
                            sid = %session_log_id,
                            generation = ?generation,
                            reason = "capture_ended",
                            "microphone capture ended unexpectedly",
                        );
                        if let Some(runtime) = microphone.take() {
                            stop_microphone_runtime(
                                runtime,
                                Some(cancellation.cleanup_deadline()),
                                "capture_ended",
                            )
                            .await;
                        }
                        let _ = tx.send(SessionEvent::MicrophoneActive(false));
                        if let Some(generation) = generation {
                            send_bounded(
                                &mut ws,
                                microphone_stop_message(
                                    generation,
                                    MicrophoneStreamReason::CaptureFailure,
                                )?,
                                &cancellation,
                                &mut close_receiver,
                            )
                            .await?;
                        }
                        continue;
                    }
                    Err(error) => {
                        let generation = microphone.as_ref().map(|runtime| runtime.generation);
                        tracing::warn!(
                            target: crate::logging::target::AUDIO,
                            event = "mic_deck_capture_failure",
                            sid = %session_log_id,
                            generation = ?generation,
                            reason = ?error,
                            ?error,
                            "microphone capture failed",
                        );
                        if let Some(runtime) = microphone.take() {
                            stop_microphone_runtime(
                                runtime,
                                Some(cancellation.cleanup_deadline()),
                                "capture_failure",
                            )
                            .await;
                        }
                        let _ = tx.send(SessionEvent::MicrophoneActive(false));
                        if let Some(generation) = generation {
                            send_bounded(
                                &mut ws,
                                microphone_stop_message(
                                    generation,
                                    MicrophoneStreamReason::CaptureFailure,
                                )?,
                                &cancellation,
                                &mut close_receiver,
                            )
                            .await?;
                        }
                        continue;
                    }
                };
                if cancellation.is_closed() {
                    return Ok(SessionEnd::manual());
                }
                let mut frame = frame;
                let encoded = microphone
                    .as_mut()
                    .ok_or(ConnectSmokeError::Microphone("runtime disappeared"))?
                    .encode(&frame);
                frame.zeroize();
                match encoded {
                    Ok(encoded) => {
                        let encoded_bytes = encoded.len();
                        if let Err(error) = send_microphone_bounded(
                            &mut ws,
                            encoded,
                            &cancellation,
                            &mut close_receiver,
                        )
                        .await
                        {
                            if matches!(error, ConnectSmokeError::Timeout) {
                                if let Some(runtime) = microphone.as_mut() {
                                runtime.record_transport_timeout();
                                }
                            }
                            return Err(error);
                        }
                        if let Some(runtime) = microphone.as_mut() {
                            runtime.record_sent(encoded_bytes);
                        }
                    }
                    Err(error) => {
                    let generation = microphone.as_ref().map(|runtime| runtime.generation);
                    tracing::warn!(
                            target: crate::logging::target::AUDIO,
                            event = "mic_deck_capture_failure",
                            sid = %session_log_id,
                            generation = ?generation,
                            reason = "encode_failure",
                            %error,
                            "microphone encoding failed",
                        );
                    if let Some(runtime) = microphone.take() {
                        stop_microphone_runtime(
                            runtime,
                            Some(cancellation.cleanup_deadline()),
                            "encode_failure",
                        )
                        .await;
                    }
                    let _ = tx.send(SessionEvent::MicrophoneActive(false));
                        if let Some(generation) = generation {
                            send_bounded(
                                &mut ws,
                                microphone_stop_message(
                                    generation,
                                    MicrophoneStreamReason::CaptureFailure,
                                )?,
                                &cancellation,
                                &mut close_receiver,
                            )
                            .await?;
                        }
                    }
                }
            }
            started = next_microphone_start(&mut microphone_start) => {
                let generation = microphone_start.as_ref().map(|pending| pending.generation);
                match started {
                    Some(Ok(runtime)) => {
                        let activation = activate_microphone_if_open(
                            &cancellation.capture,
                            &mut microphone,
                            runtime,
                            || {
                                let _ = tx.send(SessionEvent::MicrophoneActive(true));
                                tracing::info!(
                                    target: crate::logging::target::AUDIO,
                                    event = "mic_deck_permission",
                                    sid = %session_log_id,
                                    generation = ?generation,
                                    result = "granted",
                                    "Deck microphone permission accepted"
                                );
                                tracing::info!(
                                    target: crate::logging::target::AUDIO,
                                    event = "mic_deck_capture_active",
                                    sid = %session_log_id,
                                    generation = ?generation,
                                    sample_rate_hz = 48_000u32,
                                    channels = 1u8,
                                    frame_duration_ms = 20u16,
                                    "Deck microphone capture active"
                                );
                            },
                        );
                        microphone_start = None;
                        if let Err(runtime) = activation {
                            stop_microphone_runtime(
                                runtime,
                                Some(cancellation.close_deadline()),
                                "session_cancelled",
                            )
                            .await;
                            return Ok(SessionEnd::manual());
                        }
                    }
                    Some(Err(MicrophoneStartFailure::Channel(reason, error))) => {
                        microphone = None;
                        let _ = tx.send(SessionEvent::MicrophoneActive(false));
                        tracing::warn!(
                            target: crate::logging::target::AUDIO,
                            event = "mic_deck_capture_failure",
                            sid = %session_log_id,
                            generation = ?generation,
                            reason = ?reason,
                            error,
                            "microphone capture did not start",
                        );
                        if reason == MicrophoneStreamReason::PermissionDenied {
                            tracing::warn!(
                                target: crate::logging::target::AUDIO,
                                event = "mic_deck_permission",
                                sid = %session_log_id,
                                generation = ?generation,
                                result = "denied",
                                "Deck microphone permission denied"
                            );
                        }
                        if let Some(pending) = microphone_start.as_ref() {
                            pending.log_terminal_summary("startup_failure");
                        }
                        microphone_start = None;
                        if let Some(generation) = generation {
                            send_bounded(
                                &mut ws,
                                microphone_stop_message(generation, reason)?,
                                &cancellation,
                                &mut close_receiver,
                            )
                            .await?;
                        }
                    }
                    Some(Err(MicrophoneStartFailure::Protocol(error))) => {
                        tracing::warn!(
                            target: crate::logging::target::AUDIO,
                            event = "mic_deck_capture_failure",
                            sid = %session_log_id,
                            generation = ?generation,
                            reason = "protocol",
                            "microphone startup protocol failed",
                        );
                        if let Some(pending) = microphone_start.as_ref() {
                            pending.log_terminal_summary("startup_protocol_failure");
                        }
                        microphone_start = None;
                        return Err(ConnectSmokeError::Microphone(error));
                    }
                    None => {
                        microphone_start = None;
                        microphone = None;
                        let _ = tx.send(SessionEvent::MicrophoneActive(false));
                    }
                }
            }
        }
      }
    }
    .await;

    #[cfg(feature = "experimental-raw-hid")]
    for (device_id, (vendor_id, product_id, reports, errors)) in hid_devices.drain() {
        emit_hid_end(
            &session_log_id,
            event_identity(&options),
            device_id,
            vendor_id,
            product_id,
            reports,
            errors,
        );
    }
    #[cfg(feature = "usb-hard-lab")]
    if let Some(responder) = usb_hard_responder.take() {
        responder.shutdown().await;
    }
    let microphone_stop_reason = microphone_teardown_reason(&outcome);
    cancellation.close();
    let close_deadline = cancellation.close_deadline();
    stop_microphone_capture(
        &mut microphone,
        &mut microphone_start,
        Some(close_deadline),
        microphone_stop_reason,
    )
    .await;
    let _ = tx.send(SessionEvent::MicrophoneActive(false));
    close_bounded(&mut ws, close_deadline).await;
    match outcome {
        Err(ConnectSmokeError::SessionClosed) => Ok(SessionEnd::manual()),
        other => other,
    }
}

async fn open_websocket(
    options: &ConnectOptions,
) -> Result<(DirectSessionSocket, Url, ClientFsm), ConnectSmokeError> {
    let transport = resolve_transport(options)
        .map_err(|message| ConnectSmokeError::TransportUnavailable(message.to_string()))?;
    let uri = match transport {
        #[cfg(feature = "wss-compat")]
        DirectTransportKind::WebSocket => options.uri()?,
        DirectTransportKind::Quic => {
            Url::parse(&format!("quic://{}:{}", options.host, options.port))?
        }
    };
    let mut fsm = ClientFsm::new();
    let _ = fsm.send(ClientEvent::ConnectRequested);
    tracing::info!(
        target: crate::logging::target::TRANSPORT,
        uri = %uri,
        transport = transport.label(),
        tls = options.use_tls || transport == DirectTransportKind::Quic,
        timeout_ms = options.timeout.as_millis() as u64,
        "opening direct session transport",
    );

    let connection = match transport {
        #[cfg(feature = "wss-compat")]
        DirectTransportKind::WebSocket => {
            open_wss_socket(options).await.map(DirectSessionSocket::Wss)
        }
        DirectTransportKind::Quic => open_quic_socket(options)
            .await
            .map(DirectSessionSocket::Quic),
    };
    let socket = connection.map_err(|error| map_captured_certificate_error(options, error))?;
    let _ = fsm.send(ClientEvent::TlsOk);
    tracing::info!(
        target: crate::logging::target::TRANSPORT,
        transport = transport.label(),
        tls = options.use_tls || transport == DirectTransportKind::Quic,
        "direct session transport connected",
    );
    Ok((socket, uri, fsm))
}

fn map_captured_certificate_error(
    options: &ConnectOptions,
    error: ConnectSmokeError,
) -> ConnectSmokeError {
    if is_tofu_capture_reject_message(&error.to_string()) {
        if let Some(info) = options.tls.take_captured_certificate() {
            return ConnectSmokeError::CertificateUntrusted(info);
        }
    }
    error
}

fn direct_websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_INCOMING_MESSAGE_SIZE),
        max_frame_size: Some(MAX_INCOMING_MESSAGE_SIZE),
        ..WebSocketConfig::default()
    }
}

#[cfg(feature = "wss-compat")]
async fn open_wss_socket(options: &ConnectOptions) -> Result<DirectWssSocket, ConnectSmokeError> {
    let uri = options.uri()?;
    let connector = if options.use_tls {
        options.tls.rustls_connector()?
    } else {
        None
    };
    // Every other endpoint in this workspace disables Nagle's algorithm on
    // its socket (`hosts/linux/src/net/server.rs`, `hosts/windows/src/main.rs`,
    // `clients/windows/src/wss.rs`). This dormant `wss-compat` path (see ADR
    // 0007; QUIC is the shipping direct transport) was the one omission --
    // `disable_nagle: true` asks `tokio-tungstenite` to set `TCP_NODELAY`
    // itself rather than hand-rolling the connect to reach the raw socket.
    let connect_future = connect_async_tls_with_config(
        uri.as_str(),
        Some(direct_websocket_config()),
        true,
        connector,
    );
    let connect_result = timeout(options.timeout, connect_future)
        .await
        .map_err(|_| ConnectSmokeError::Timeout)?;
    let (socket, response) = connect_result?;
    tracing::debug!(
        target: crate::logging::target::TRANSPORT,
        status = response.status().as_u16(),
        "WSS upgrade completed",
    );
    Ok(socket)
}

async fn open_quic_socket(options: &ConnectOptions) -> Result<DirectQuicSocket, ConnectSmokeError> {
    // The TLS configuration is built per attempt, below, so that each dialled
    // address captures into its own slot. Nothing shared is constructed here.
    let addresses = timeout(
        options.timeout,
        tokio::net::lookup_host((options.host.as_str(), options.port)),
    )
    .await
    .map_err(|_| ConnectSmokeError::Timeout)?
    .map_err(WebSocketError::Io)?
    .collect::<Vec<SocketAddr>>();
    if addresses.is_empty() {
        return Err(ConnectSmokeError::WebSocket(WebSocketError::Io(
            std::io::Error::new(
                std::io::ErrorKind::AddrNotAvailable,
                "QUIC host resolved to no addresses",
            ),
        )));
    }

    let mut last_error = None;
    let mut attempts = FuturesUnordered::new();
    for remote_addr in addresses {
        let bind_addr = if remote_addr.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0_u16; 8], 0))
        };
        let endpoint = match quinn::Endpoint::client(bind_addr) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                last_error = Some(ConnectSmokeError::WebSocket(WebSocketError::Io(error)));
                continue;
            }
        };
        // Every address for this hostname is dialled concurrently. Give each
        // attempt a capture slot of its own: sharing one meant the last
        // certificate written won, so the fingerprint offered to the user was
        // not necessarily the one belonging to the connection whose error was
        // returned. An attacker who can get one extra address into the answer —
        // an added record, a spoofed mDNS reply, an owned IPv6 path — could win
        // that race and have their certificate shown under the real hostname.
        let attempt_tls = options.tls.with_private_capture_slot();
        let attempt_config = match attempt_tls
            .quic_rustls_config(DIRECT_QUIC_ALPN_PROTOCOL)
            .map_err(ConnectSmokeError::Tls)
            .and_then(|rustls_config| {
                quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config).map_err(|error| {
                    ConnectSmokeError::TransportUnavailable(format!(
                        "QUIC TLS configuration is unavailable: {error}"
                    ))
                })
            }) {
            Ok(crypto) => {
                let mut config = quinn::ClientConfig::new(Arc::new(crypto));
                config.transport_config(recommended_transport_config_arc(
                    &BoundedTransportPolicy::default(),
                ));
                config
            }
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };
        attempts.push(async move {
            let outcome = connect_direct(DirectQuicDialParams {
                endpoint,
                client_config: attempt_config,
                remote_addr,
                server_name: options.host.as_str(),
            })
            .await;
            (outcome, attempt_tls, remote_addr)
        });
    }

    let deadline = Instant::now() + options.timeout;
    while !attempts.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ConnectSmokeError::Timeout);
        }
        match timeout(remaining, attempts.next()).await {
            Ok(Some((Ok(stream), _attempt_tls, _remote_addr))) => {
                tracing::debug!(
                    target: crate::logging::target::TRANSPORT,
                    remote_addr = %stream.remote_address(),
                    "QUIC TLS handshake and direct stream completed",
                );
                return Ok(WebSocketStream::from_raw_socket(
                    stream,
                    Role::Client,
                    Some(direct_websocket_config()),
                )
                .await);
            }
            Ok(Some((Err(error), attempt_tls, remote_addr))) => {
                let mapped = quic_connect_error(error);
                if is_tofu_capture_reject_message(&mapped.to_string()) {
                    // Read the capture from the attempt that actually failed,
                    // and record which peer it was, so the user is approving a
                    // fingerprint whose origin the dialog can name.
                    if let Some(mut info) = attempt_tls.take_captured_certificate() {
                        info.peer_address = Some(remote_addr.to_string());
                        return Err(ConnectSmokeError::CertificateUntrusted(info));
                    }
                    return Err(mapped);
                }
                last_error = Some(mapped);
            }
            Ok(None) => break,
            Err(_) => return Err(ConnectSmokeError::Timeout),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        ConnectSmokeError::WebSocket(WebSocketError::Io(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "QUIC connection had no usable remote address",
        )))
    }))
}

/// Maps every [`QuicTransportError`] variant to a typed, user-safe
/// [`ConnectSmokeError`], matched exhaustively (no wildcard arm) so that a
/// future transport-foundation addition fails this crate's build instead of
/// silently falling through to a generic bucket.
///
/// Classification follows the same shape as the connection failures already
/// handled here:
/// - Timeouts (`EstablishmentTimedOut`, `MonitorStreamTimedOut`) both surface
///   as [`ConnectSmokeError::Timeout`], which `classify_disconnect` already
///   treats as a transient, retryable outcome.
/// - Preface/protocol violations (`DirectPreface`, `MonitorPreface`) and the
///   other non-TLS, non-auth transport failures below all surface as an
///   opaque [`ConnectSmokeError::WebSocket`] IO error carrying only this
///   error's own `Display` text (already free of connection secrets),
///   which `classify_disconnect` treats as a terminal protocol failure --
///   consistent whether the rejected preface belonged to the primary
///   connection stream or a direct-monitor stream.
fn quic_connect_error(error: QuicTransportError) -> ConnectSmokeError {
    let message = error.to_string();
    if is_tofu_capture_reject_message(&message) {
        return ConnectSmokeError::TlsIdentity(TOFU_CAPTURE_REJECT_ERROR.to_string());
    }
    if is_tofu_pin_mismatch_message(&message) {
        return ConnectSmokeError::TlsIdentity(TOFU_PIN_MISMATCH_ERROR.to_string());
    }
    match error {
        QuicTransportError::Endpoint(error) => {
            ConnectSmokeError::WebSocket(WebSocketError::Io(error))
        }
        QuicTransportError::Connection(error) => {
            let is_tls_error = matches!(
                &error,
                quinn::ConnectionError::TransportError(error)
                    if quic_transport_code_is_tls(error.code)
            );
            if is_tls_error {
                quic_tls_identity_error(message)
            } else {
                ConnectSmokeError::WebSocket(WebSocketError::Io(error.into()))
            }
        }
        QuicTransportError::EstablishmentTimedOut => ConnectSmokeError::Timeout,
        // The direct-monitor stream preface (Carrier B; not yet the product
        // default) has its own bounded accept/parse deadline, distinct from
        // the primary connection's `EstablishmentTimedOut`, but the same
        // typed, already-transient-classified outcome applies either way.
        QuicTransportError::MonitorStreamTimedOut => ConnectSmokeError::Timeout,
        QuicTransportError::Closed => ConnectSmokeError::SessionClosed,
        QuicTransportError::Connect(error) => ConnectSmokeError::TransportUnavailable(format!(
            "QUIC connection parameters are invalid: {error}"
        )),
        QuicTransportError::Contract(_)
        | QuicTransportError::DirectPreface
        // The peer's direct-monitor stream preface (Carrier B) was rejected
        // as malformed or carrying an invalid session id -- a protocol
        // violation exactly like `DirectPreface`, so it is classified and
        // messaged identically (the specific reason is preserved in
        // `message` via this error's own `Display`).
        | QuicTransportError::MonitorPreface(_)
        | QuicTransportError::StreamWrite(_)
        | QuicTransportError::StreamRead(_)
        | QuicTransportError::DatagramSend(_)
        | QuicTransportError::Handshake(_)
        | QuicTransportError::Unauthorized(_)
        | QuicTransportError::MissingPeerIdentity
        | QuicTransportError::OutboundQueueFull
        | QuicTransportError::InboundQueueFull => {
            ConnectSmokeError::WebSocket(WebSocketError::Io(std::io::Error::other(message)))
        }
    }
}

fn quic_transport_code_is_tls(code: quinn::TransportErrorCode) -> bool {
    (0x100..0x200).contains(&u64::from(code))
}

fn quic_tls_identity_error(message: String) -> ConnectSmokeError {
    ConnectSmokeError::TlsIdentity(message)
}

/// Additive multi-monitor-v1 negotiation applied to an in-progress
/// `AuthResponse`, isolated from `send_auth` so it is directly unit-testable
/// without a mock transport.
///
/// `options.multi_monitor_topology` is only populated (see
/// `crate::ui::app::connect_options_with_stream_sizing_policy`) when the
/// local Displays setting is Match My Layout with more than one active
/// display; every other case (Primary Display Only, Windowed, or Match My
/// Layout with a single display) passes `response` through unchanged, exactly
/// preserving today's legacy `.with_displays()`-only behavior. When a
/// topology selection *is* present but the host's `AuthRequest` never
/// advertised (or advertised an insufficient) `multi_monitor_v1` offer, this
/// fails the connection attempt with
/// [`ConnectSmokeError::MultiMonitorUnsupported`] instead of `send_auth`
/// silently reporting only the primary display — the approved product
/// decision that Match My Layout must never silently serve a subset of the
/// requested layout.
fn apply_multi_monitor_negotiation(
    response: AuthResponse,
    options: &ConnectOptions,
    auth_request: &AuthRequest,
) -> Result<AuthResponse, ConnectSmokeError> {
    match &options.multi_monitor_topology {
        Some(selection) => crate::transport::multi_monitor::attach_multi_monitor_v1_with_quality(
            response,
            auth_request,
            &selection.topology,
            selection.safe_area_policy,
            &selection.full_color_display_ids,
        )
        .map_err(|error| ConnectSmokeError::MultiMonitorUnsupported(error.to_string())),
        None => Ok(response),
    }
}

async fn send_auth<S>(
    ws: &mut S,
    options: &ConnectOptions,
    auth_request: &AuthRequest,
    mut submission: AuthSubmission,
    session_log_id: &CorrelationId,
    session_auth: &SessionAuth,
    cancellation: Option<&SessionCancellation>,
    close_receiver: Option<&mut watch::Receiver<bool>>,
) -> Result<(), ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    let mode = auth_request.auth_mode.as_deref().unwrap_or("local");
    let response = if mode == "none" {
        AuthResponse::none()
    } else if mode == "pam" || auth_request.auth_methods.iter().any(|m| m == "pam") {
        AuthResponse::pam(&submission.username, &submission.password)
    } else {
        AuthResponse::password(
            &submission.username,
            hash_password(&submission.password, &auth_request.challenge),
        )
    };
    let mut response = auth_response_with_metadata(response, options);
    response.session_log_id = Some(session_log_id.to_string());
    response.disclaimer_acceptance_sha256 =
        auth_request.disclaimer.as_deref().map(disclaimer_digest);
    response = apply_multi_monitor_negotiation(response, options, auth_request)?;
    response = apply_resume_opt_in(response, auth_request, session_auth);
    submission.clear_sensitive();
    if response.monitors.is_empty() {
        tracing::info!(
            target: crate::logging::target::SESSION,
            "no client displays enumerated; host will use its default resolution",
        );
    } else {
        tracing::info!(
            target: crate::logging::target::SESSION,
            count = response.monitors.len(),
            primary = %format!("{}x{}", response.screen_width, response.screen_height),
            mode = %response.displays_mode,
            "reporting client display layout to host",
        );
    }

    let json = serde_json::to_string(&response)?;
    send_setup_message(ws, Message::Text(json), cancellation, close_receiver).await
}

fn apply_resume_opt_in(
    response: AuthResponse,
    auth_request: &AuthRequest,
    session_auth: &SessionAuth,
) -> AuthResponse {
    match session_auth {
        SessionAuth::InitialOptIn { holder_nonce } if auth_request.supports_resume() => {
            response.with_resume_requested(holder_nonce)
        }
        SessionAuth::Legacy | SessionAuth::InitialOptIn { .. } | SessionAuth::Resume(_) => response,
    }
}

/// Sends the credential-free resume `AuthResponse`.
///
/// Whenever `options.multi_monitor_topology` is `Some` (a negotiated
/// multi-monitor-v1 session), this reuses the exact same
/// [`apply_multi_monitor_negotiation`] pure function `send_auth` used on the
/// original attach, fed this resume attempt's own fresh `auth_request`
/// (carrying the host's current pre-auth multi-monitor-v1 offer). Since
/// [`apply_multi_monitor_negotiation`]'s embedded `multi_monitor_v1` sidecar
/// is a deterministic function of only `options`'s frozen, already-UI-scaled
/// `multi_monitor_topology`/carriers -- never of the offer's own field
/// values, which gate acceptance only -- this reproduces a byte-equivalent
/// `AuthMultiMonitorRequestMsg` to the one sent on the original attach for
/// every case where the resume's own offer still admits it, using the exact
/// same frozen `ConnectOptions` instance `drive_reconnect` already threads
/// through unchanged (`self.reconnect_options`'s own `credential_free_clone`).
///
/// If the resume's own `auth_request` no longer advertises a sufficient
/// multi-monitor-v1 offer (missing entirely, or now rejects the unchanged
/// local topology), this propagates
/// [`ConnectSmokeError::MultiMonitorUnsupported`] instead of ever falling
/// back to sending a bare/legacy (`multi_monitor_v1: None`) resume response --
/// which would silently downgrade an active multi-monitor session and desync
/// the host's own committed roster expectations. That error is not
/// `transient()` and is not one of `handle_connection_closed`'s specially
/// handled terminal reasons, so it already flows through the existing
/// `return_to_credentials()` fallback -- a clean resume failure that lands
/// the user back on the credentials screen for a fresh full authentication,
/// never a broken/ambiguous resumed session.
///
/// Legacy/single-monitor sessions (`options.multi_monitor_topology` is
/// `None`) are entirely unaffected: [`apply_multi_monitor_negotiation`]'s own
/// `None` branch passes `response` through completely unchanged, exactly
/// preserving today's resume behavior.
async fn send_resume_auth<S>(
    ws: &mut S,
    options: &ConnectOptions,
    session_auth: &SessionAuth,
    auth_request: &AuthRequest,
    session_log_id: &CorrelationId,
    cancellation: Option<&SessionCancellation>,
    close_receiver: Option<&mut watch::Receiver<bool>>,
) -> Result<(), ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    let SessionAuth::Resume(attempt) = session_auth else {
        return Err(ConnectSmokeError::ResumeProtocol(
            "resume response requested for a non-resume handshake",
        ));
    };
    let mut response = auth_response_with_metadata(
        AuthResponse::resume(&attempt.holder_nonce, &attempt.grant),
        options,
    );
    response = apply_multi_monitor_negotiation(response, options, auth_request)?;
    response.session_log_id = Some(session_log_id.to_string());
    tracing::info!(
        target: crate::logging::target::SESSION,
        previous_sid = %attempt.previous_sid,
        "attempting credential-free session resume",
    );
    send_setup_message(
        ws,
        Message::Text(serde_json::to_string(&response)?),
        cancellation,
        close_receiver,
    )
    .await
}

#[allow(clippy::result_large_err)]
fn validate_resume_request(auth_request: &AuthRequest) -> Result<(), ConnectSmokeError> {
    if auth_request.disclaimer.is_some() {
        return Err(ConnectSmokeError::ResumeProtocol(
            "resume handshake unexpectedly required a disclaimer",
        ));
    }
    if !auth_request.supports_resume() {
        return Err(ConnectSmokeError::ResumeRejected {
            message: "host does not advertise session resume".to_string(),
            error_code: Some(ResumeErrorCode::Unsupported),
        });
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn validate_authentication_result(
    auth: &SessionAuth,
    mut result: AuthResult,
    session_log_id: &CorrelationId,
) -> Result<Option<SessionAuthentication>, ConnectSmokeError> {
    if result.error_code.is_some() {
        if let Some(grant) = &mut result.resume_grant {
            grant.zeroize();
        }
        return Err(ConnectSmokeError::ResumeProtocol(
            "successful auth result carried a resume error code",
        ));
    }
    match auth {
        SessionAuth::Legacy => {
            if result.resumed {
                if let Some(grant) = &mut result.resume_grant {
                    grant.zeroize();
                }
                return Err(ConnectSmokeError::ResumeProtocol(
                    "legacy authentication unexpectedly reported a resumed session",
                ));
            }
            if let Some(grant) = &mut result.resume_grant {
                grant.zeroize();
            }
            Ok(None)
        }
        SessionAuth::InitialOptIn { .. } => {
            if result.resumed {
                if let Some(grant) = &mut result.resume_grant {
                    grant.zeroize();
                }
                return Err(ConnectSmokeError::ResumeProtocol(
                    "initial authentication unexpectedly reported a resumed session",
                ));
            }
            if result.resume_grant.as_deref() == Some("")
                && result.resume_window_secs.is_none_or(|window| window != 0)
            {
                if let Some(grant) = &mut result.resume_grant {
                    grant.zeroize();
                }
                return Err(ConnectSmokeError::ResumeProtocol(
                    "initial auth result carried an empty resume grant",
                ));
            }
            match (&result.resume_grant, result.resume_window_secs) {
                (Some(_), None) | (None, Some(1..=u32::MAX)) => {
                    if let Some(grant) = &mut result.resume_grant {
                        grant.zeroize();
                    }
                    return Err(ConnectSmokeError::ResumeProtocol(
                        "initial auth result had an incomplete resume grant/window pair",
                    ));
                }
                _ => {}
            }
            Ok(Some(SessionAuthentication {
                kind: AuthenticationKind::InitialOptIn,
                resume_grant: result.resume_grant.take(),
                resume_window: result
                    .resume_window_secs
                    .map(|seconds| Duration::from_secs(u64::from(seconds))),
                resumed: false,
                session_log_id: session_log_id.to_string(),
            }))
        }
        SessionAuth::Resume(_) => {
            if !result.resumed {
                if let Some(grant) = &mut result.resume_grant {
                    grant.zeroize();
                }
                return Err(ConnectSmokeError::ResumeProtocol(
                    "resume authentication did not report resumed=true",
                ));
            }
            if result.resume_grant.as_deref().is_none_or(str::is_empty)
                || result.resume_window_secs.is_none_or(|window| window == 0)
            {
                if let Some(grant) = &mut result.resume_grant {
                    grant.zeroize();
                }
                return Err(ConnectSmokeError::ResumeProtocol(
                    "resume authentication omitted the successor grant/window",
                ));
            }
            Ok(Some(SessionAuthentication {
                kind: AuthenticationKind::Resume,
                resume_grant: result.resume_grant.take(),
                resume_window: result
                    .resume_window_secs
                    .map(|seconds| Duration::from_secs(u64::from(seconds))),
                resumed: true,
                session_log_id: session_log_id.to_string(),
            }))
        }
    }
}

#[allow(clippy::result_large_err)]
fn validate_authentication_refresh(
    mut result: AuthResult,
    session_log_id: &CorrelationId,
) -> Result<SessionAuthentication, ConnectSmokeError> {
    const MAX_REFRESH_MESSAGE_CHARS: usize = 240;

    let valid = result.msg_type == AUTH_RESULT
        && result.success
        && !result.resumed
        && result.error_code.is_none()
        && result.message.chars().count() <= MAX_REFRESH_MESSAGE_CHARS
        && result
            .resume_grant
            .as_deref()
            .is_some_and(|grant| !grant.is_empty() && grant.len() <= MAX_RESUME_GRANT_BYTES)
        && result
            .resume_window_secs
            .is_some_and(|window| (1..=7_200).contains(&window));
    if !valid {
        if let Some(grant) = &mut result.resume_grant {
            grant.zeroize();
        }
        return Err(ConnectSmokeError::ResumeProtocol(
            "invalid in-band resume grant refresh",
        ));
    }

    Ok(SessionAuthentication {
        kind: AuthenticationKind::Refresh,
        resume_grant: result.resume_grant.take(),
        resume_window: result
            .resume_window_secs
            .map(|seconds| Duration::from_secs(u64::from(seconds))),
        resumed: false,
        session_log_id: session_log_id.to_string(),
    })
}

async fn await_authentication_acceptance<S>(
    ws: &mut S,
    commands: &mut mpsc::UnboundedReceiver<SessionCommand>,
    cancellation: &SessionCancellation,
) -> Result<(), ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    let command = timeout(AUTH_INTERACTION_TIMEOUT, commands.recv())
        .await
        .map_err(|_| ConnectSmokeError::Timeout)?
        .ok_or(ConnectSmokeError::AuthDeclined)?;
    match command {
        SessionCommand::AcceptAuthentication => Ok(()),
        SessionCommand::Close => {
            close_bounded(ws, cancellation.close_deadline()).await;
            Err(ConnectSmokeError::SessionClosed)
        }
        SessionCommand::DeclineDisclaimer
        | SessionCommand::SubmitAuth(_)
        | SessionCommand::Json(_)
        | SessionCommand::Binary(_)
        | SessionCommand::HardUsbPen(_) => {
            close_bounded(ws, cancellation.cleanup_deadline()).await;
            Err(ConnectSmokeError::AuthDeclined)
        }
    }
}

fn auth_response_with_metadata(response: AuthResponse, options: &ConnectOptions) -> AuthResponse {
    let mut response = response
        .with_displays(options.monitors.clone(), &options.displays_mode)
        .with_optional_timezone(options.timezone.clone())
        .with_cursor_preference(options.cursor_preference);
    let capabilities = probe_decode_capabilities();
    let quality = rust_viewer_quality_settings(&options.profile);
    // The first link of the HDR chain, stated where the ask is actually made.
    // `transfer` is what carries it: 10-bit BT.709 is an ordinary SDR request
    // for banding headroom, so depth alone must not read as HDR.
    let hdr_requested = quality.transfer == "pq" || quality.transfer == "hlg";
    tracing::info!(
        target: crate::logging::target::SESSION,
        event = "deck_color_request",
        hdr_requested,
        transfer = %quality.transfer,
        primaries = %quality.color_primaries,
        bit_depth = %quality.bit_depth,
        chroma = %quality.chroma,
        matrix = %quality.color_matrix,
        range = %quality.color_range,
        codec = %quality.codec,
        main10_decode = capabilities.main10,
        "Deck colour request",
    );
    response.initial_video = Some(InitialVideoRequestMsg {
        quality,
        capabilities: ClientVideoCapabilitiesMsg {
            h264: capabilities.h264,
            h265: capabilities.h265,
            av1: capabilities.av1,
            yuv444: capabilities.yuv444,
            main10: capabilities.main10,
            main12: capabilities.main12,
            full_range: capabilities.full_range,
            identity_matrix: capabilities.identity_matrix,
            bt601_matrix: capabilities.bt601_matrix,
            bt2020_ncl_matrix: capabilities.bt2020_ncl_matrix,
        },
    });
    // Only ever set from an explicit user choice; the host treats it as
    // authorisation to destroy a running desktop.
    response.replace_incompatible_desktop = options.replace_incompatible_desktop;
    response
}

fn validate_disclaimer(auth_request: &AuthRequest) -> Result<(), ConnectSmokeError> {
    if let Some(disclaimer) = auth_request.disclaimer.as_deref() {
        if disclaimer.is_empty() {
            return Err(ConnectSmokeError::InvalidDisclaimer("text is empty"));
        }
        if disclaimer.len() > MAX_DISCLAIMER_CONTENT_BYTES {
            return Err(ConnectSmokeError::InvalidDisclaimer("text exceeds 16 KiB"));
        }
        if disclaimer.chars().any(char::is_control) {
            return Err(ConnectSmokeError::InvalidDisclaimer(
                "text contains control characters",
            ));
        }
    }
    Ok(())
}

fn disclaimer_digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

async fn handle_hello<S>(
    uri: &str,
    ws: &mut S,
    fsm: &mut ClientFsm,
    options: &ConnectOptions,
    session_log_id: &CorrelationId,
    hello: Value,
) -> Result<ConnectSmokeResult, ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    match msg_type(&hello).unwrap_or("<missing>") {
        SERVER_HELLO => {
            let server_hello: ServerHelloMsg = serde_json::from_value(hello)?;
            validate_server_transport(options, &server_hello)?;
            validate_server_region_input(options, &server_hello)?;
            if fsm.state_id() == "authenticating" {
                let _ = fsm.send(ClientEvent::AuthOk);
            }
            let _ = fsm.send(ClientEvent::HelloReceived);
            send_client_ready(ws, options, session_log_id, None, None, None).await?;
            Ok(ConnectSmokeResult {
                uri: uri.to_string(),
                server_hello: Some(server_hello),
                broker_hello: None,
                fsm_state: fsm.state_id(),
            })
        }
        BROKER_HELLO => Ok(ConnectSmokeResult {
            uri: uri.to_string(),
            server_hello: None,
            broker_hello: Some(hello),
            fsm_state: fsm.state_id(),
        }),
        other => Err(ConnectSmokeError::UnexpectedFirstMessage(other.to_string())),
    }
}

async fn send_client_ready<S>(
    ws: &mut S,
    options: &ConnectOptions,
    session_log_id: &CorrelationId,
    usb_hard_device: Option<arcen_protocol::messages::UsbHardDeviceMsg>,
    cancellation: Option<&SessionCancellation>,
    mut close_receiver: Option<&mut watch::Receiver<bool>>,
) -> Result<(), ConnectSmokeError>
where
    S: Sink<Message> + Unpin,
    S::Error: Into<tokio_tungstenite::tungstenite::Error>,
{
    let client_hello = client_hello_with_usb_device(options, session_log_id, usb_hard_device);
    send_setup_message(
        ws,
        Message::Text(serde_json::to_string(&client_hello)?),
        cancellation,
        close_receiver.as_deref_mut(),
    )
    .await?;
    send_setup_message(
        ws,
        Message::Text(serde_json::to_string(&rust_viewer_quality_settings(
            &options.profile,
        ))?),
        cancellation,
        close_receiver,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
fn client_hello_with_metadata(
    options: &ConnectOptions,
    session_log_id: &CorrelationId,
) -> ClientHelloMsg {
    client_hello_with_usb_device(options, session_log_id, None)
}

/// Applies a real (or, in tests, fabricated) [`DecodeCapabilities`] probe
/// result onto `hello`'s codec/colour-fidelity flags and `decoder_backend`.
/// Factored out from [`client_hello_with_usb_device`] so this mapping is
/// unit-testable without a real `VTDecompressionSession` -- see
/// `client_hello_capability_flags_come_from_the_probe_not_the_requested_profile`.
///
/// Every flag here comes from `capabilities` alone: none of them are
/// re-derived from `hello`'s own connection-specific fields. A capability
/// claim describes what this Deck can decode at all, independent of what a
/// given connection's `quality_settings` happens to request next (that
/// message -- not `client_hello` -- carries the actual requested depth,
/// range and matrix). This makes every flag here consistent with the
/// older `supports_h264`/`supports_h265`/`supports_av1`/`supports_yuv444`
/// flags, which were always unconditional capability claims, rather than
/// the narrower, request-conditioned guess this replaces.
fn apply_decode_capabilities(
    mut hello: ClientHelloMsg,
    capabilities: DecodeCapabilities,
) -> ClientHelloMsg {
    hello.supports_h264 = capabilities.h264;
    hello.supports_h265 = capabilities.h265;
    hello.supports_av1 = capabilities.av1;
    hello.supports_yuv444 = capabilities.yuv444;
    hello.supports_main10 = capabilities.main10;
    hello.supports_main12 = capabilities.main12;
    hello.supports_full_range = capabilities.full_range;
    hello.supports_identity_matrix = capabilities.identity_matrix;
    hello.supports_bt601_matrix = capabilities.bt601_matrix;
    hello.supports_bt2020_ncl_matrix = capabilities.bt2020_ncl_matrix;
    hello.decoder_backend = capabilities.decoder_backend_label().to_string();
    hello
}

fn client_hello_with_usb_device(
    options: &ConnectOptions,
    session_log_id: &CorrelationId,
    usb_hard_device: Option<arcen_protocol::messages::UsbHardDeviceMsg>,
) -> ClientHelloMsg {
    let network = crate::netinfo::probe(&options.host, options.port).snapshot;
    let hello = ClientHelloMsg {
        audio_output: Some(
            arcen_media::audio::AudioPolicy {
                opus_available: true,
                pcm_available: true,
            }
            .capabilities(),
        ),
        microphone_output: options.microphone_enabled.then(|| {
            arcen_media::audio::MicrophonePolicy {
                operator_enabled: true,
                backend_available: true,
                codecs: arcen_media::audio::MicrophoneCodecAvailability {
                    opus: true,
                    pcm: true,
                },
            }
            .capabilities()
            .expect("enabled Deck microphone policy has codecs")
        }),
        session_log_id: Some(session_log_id.to_string()),
        clipboard_protocol_version: if options.clipboard_enabled {
            CLIPBOARD_PROTOCOL_VERSION
        } else {
            0
        },
        clipboard_text_c2s: options.clipboard_enabled,
        clipboard_text_s2c: options.clipboard_enabled,
        clipboard_image_c2s: options.clipboard_enabled,
        clipboard_image_s2c: options.clipboard_enabled,
        input_capabilities: macos_input_capabilities(options.tablet_input_enabled),
        tablet_mode_requested: options.tablet_mode_requested,
        tablet_mode_capabilities: macos_tablet_mode_capabilities(options.tablet_input_enabled),
        transport_capabilities: vec![selected_transport_capability(options).to_string()],
        // SEC-raw-hid: advertises only this client's own local runtime
        // opt-in for the experimental raw-HID tablet capture path. This is
        // one half of a mutual negotiation — the host independently decides
        // whether to permit raw HID based on its own opt-in AND this flag,
        // and this client independently decides whether to *start* capture
        // based on its own opt-in AND the host's advertised
        // `ServerHelloMsg.experimental_raw_hid` (see
        // `should_start_experimental_raw_hid_capture`). Default builds
        // (feature disabled) always advertise `false`.
        experimental_raw_hid: client_experimental_raw_hid_opt_in(),
        usb_hard_v1: usb_hard_device.is_some(),
        usb_hard_device,
        ..ClientHelloMsg::default()
            .with_display_size(&options.monitors)
            .with_optional_timezone(options.timezone.clone())
            .with_cursor_preference(options.cursor_preference)
            .with_network_snapshot(network)
    };
    let hello = apply_decode_capabilities(hello, probe_decode_capabilities());
    hello.with_build_identity(crate::build_identity::current())
}

fn selected_transport_capability(options: &ConnectOptions) -> &'static str {
    #[cfg(feature = "wss-compat")]
    if !options.quic_enabled {
        return arcen_protocol::CAPABILITY_TRANSPORT_WSS;
    }
    #[cfg(not(feature = "wss-compat"))]
    let _ = options;
    arcen_protocol::CAPABILITY_TRANSPORT_QUIC
}

fn validate_server_transport(
    options: &ConnectOptions,
    hello: &ServerHelloMsg,
) -> Result<(), ConnectSmokeError> {
    let expected = selected_transport_capability(options);
    let matches = match hello.negotiated_transport.as_deref() {
        Some(actual) => actual == expected,
        #[cfg(feature = "wss-compat")]
        None => expected == arcen_protocol::CAPABILITY_TRANSPORT_WSS,
        #[cfg(not(feature = "wss-compat"))]
        None => false,
    };
    if matches {
        return Ok(());
    }
    Err(ConnectSmokeError::TransportUnavailable(format!(
        "host transport mismatch: expected {expected}, received {}",
        hello
            .negotiated_transport
            .as_deref()
            .unwrap_or("legacy-wss")
    )))
}

fn validate_server_region_input(
    options: &ConnectOptions,
    hello: &ServerHelloMsg,
) -> Result<(), ConnectSmokeError> {
    if options.multi_monitor_topology.is_none()
        || supports_region_input_v1(hello.input_protocol_version, hello.input_capabilities)
    {
        return Ok(());
    }
    Err(ConnectSmokeError::MultiMonitorUnsupported(format!(
        "region input requires input protocol v{REGION_INPUT_PROTOCOL_VERSION} and \
         input_capabilities.region_input=available (host reported v{} / {:?})",
        hello.input_protocol_version, hello.input_capabilities.region_input,
    )))
}

/// Truthful input capability report for this connection attempt. Region
/// input-v1 is always available in this Deck build; `pen` (and
/// every per-axis field, mirrored to the same value — see below) is
/// `Available` only when the user has left typed tablet input enabled *and*
/// this Mac currently has Wacom vendor USB presence
/// ([`crate::tablet::wacom_usb_presence`]); `Unavailable` when the setting is
/// off (an explicit, honest "no" rather than the default `Unknown`); and
/// whatever [`crate::tablet::wacom_usb_presence`] itself reports otherwise
/// (`Unavailable` when no such device is enumerated, `Unknown` if USB
/// enumeration itself failed).
///
/// `ClientHello` is a one-shot handshake sent before the AppKit tablet
/// monitor has observed a single live sample, so there is no empirical
/// per-axis truth yet (unlike `crate::tablet::TabletCapabilityProbe`, which
/// only ever raises a claim after seeing real nonzero samples). Mirroring
/// the overall `pen` value onto every per-axis field here is a deliberate,
/// documented simplification: it is never falsely optimistic (a Mac with no
/// Wacom driver reports `Unavailable`/`Unknown` everywhere), and any
/// axis-level refinement happens later, locally, in the in-session Tablet
/// Monitor diagnostic panel — it is not re-sent over the wire mid-session.
fn macos_input_capabilities(tablet_input_enabled: bool) -> InputCapabilitiesMsg {
    let pen = if tablet_input_enabled {
        match crate::tablet::wacom_usb_presence() {
            arcen_input::CapabilityAvailability::Available => {
                InputCapabilityAvailability::Available
            }
            arcen_input::CapabilityAvailability::Unavailable => {
                InputCapabilityAvailability::Unavailable
            }
            arcen_input::CapabilityAvailability::Unknown => InputCapabilityAvailability::Unknown,
        }
    } else {
        InputCapabilityAvailability::Unavailable
    };
    InputCapabilitiesMsg {
        absolute_pointer: InputCapabilityAvailability::Available,
        relative_pointer: InputCapabilityAvailability::Available,
        host_cursor: InputCapabilityAvailability::Available,
        region_input: InputCapabilityAvailability::Available,
        pen,
        pen_pressure: pen,
        pen_tilt: pen,
        pen_rotation: pen,
        pen_eraser: pen,
        pen_proximity: pen,
    }
}

fn macos_tablet_mode_capabilities(tablet_input_enabled: bool) -> TabletModeCapabilitiesMsg {
    let local = if tablet_input_enabled {
        match crate::tablet::wacom_usb_presence() {
            arcen_input::CapabilityAvailability::Available => {
                InputCapabilityAvailability::Available
            }
            arcen_input::CapabilityAvailability::Unavailable => {
                InputCapabilityAvailability::Unavailable
            }
            arcen_input::CapabilityAvailability::Unknown => InputCapabilityAvailability::Unknown,
        }
    } else {
        InputCapabilityAvailability::Unavailable
    };
    TabletModeCapabilitiesMsg {
        local_termination: local,
        // Physical Hard USB ownership is independent of the local AppKit
        // typed-pen setting. A feature build advertises this capability only
        // after capture succeeds and exact device metadata is attached to the
        // same ClientHello.
        wacom_usb_bridge: if cfg!(feature = "usb-hard-lab") {
            InputCapabilityAvailability::Available
        } else {
            InputCapabilityAvailability::Unavailable
        },
        disabled_mouse_compat: InputCapabilityAvailability::Available,
    }
}

/// Whether this client has locally opted in to the experimental raw-HID
/// tablet capture path. Always `false` in default (feature-disabled) builds.
#[cfg(feature = "experimental-raw-hid")]
fn client_experimental_raw_hid_opt_in() -> bool {
    crate::hid::experimental_raw_hid_client_opt_in()
}

#[cfg(not(feature = "experimental-raw-hid"))]
fn client_experimental_raw_hid_opt_in() -> bool {
    false
}

/// Decide whether to actually start the experimental raw-HID capture
/// session: BOTH this client's own runtime opt-in AND the host's
/// independently negotiated `experimental_raw_hid` capability (as advertised
/// in its `ServerHelloMsg`) are required. Neither side alone is sufficient —
/// a feature-enabled-but-not-opted-in client, or a host that never
/// negotiated the capability (including every legacy/default host), must
/// never result in raw HID capture starting.
#[cfg(feature = "experimental-raw-hid")]
fn should_start_experimental_raw_hid_capture(
    client_opt_in: bool,
    host_experimental_raw_hid: bool,
) -> bool {
    client_opt_in && host_experimental_raw_hid
}

#[cfg(feature = "experimental-raw-hid")]
#[cfg(test)]
mod experimental_raw_hid_negotiation_tests {
    use super::should_start_experimental_raw_hid_capture;

    #[test]
    fn capture_never_starts_unless_both_client_and_host_opt_in() {
        assert!(!should_start_experimental_raw_hid_capture(false, false));
        assert!(!should_start_experimental_raw_hid_capture(true, false));
        assert!(!should_start_experimental_raw_hid_capture(false, true));
        assert!(should_start_experimental_raw_hid_capture(true, true));
    }
}

fn negotiate_clipboard(
    hello: &ServerHelloMsg,
    locally_enabled: bool,
) -> Option<ClipboardPolicyMsg> {
    if !locally_enabled {
        return None;
    }
    let policy = hello.clipboard?;
    let validated = media_policy(policy)?;
    if !policy.is_v1()
        || matches!(
            validated.direction,
            arcen_media::clipboard::ClipboardDirection::Disabled
        )
    {
        return None;
    }
    Some(policy)
}

fn handle_clipboard_offer(
    value: Value,
    policy: Option<ClipboardPolicyMsg>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
) {
    let (Some(wire_policy), Some(reassembler)) = (policy, reassembler) else {
        return;
    };
    let Ok(offer) = serde_json::from_value::<ClipboardDataMsg>(value) else {
        tracing::warn!(
            target: crate::logging::target::TRANSPORT,
            reason = "invalid_metadata",
            "rejected clipboard offer"
        );
        return;
    };
    let Some(policy) = media_policy(wire_policy) else {
        return;
    };
    if policy
        .check_size(
            arcen_media::clipboard::ClipboardFlow::HostToClient,
            clipboard_media_kind(offer.kind),
            usize::try_from(offer.size_bytes).unwrap_or(usize::MAX),
        )
        .is_err()
    {
        tracing::warn!(
            target: crate::logging::target::TRANSPORT,
            sequence = offer.sequence,
            kind = ?offer.kind,
            size = offer.size_bytes,
            reason = "policy",
            "rejected clipboard offer"
        );
        return;
    }
    if let Err(error) = reassembler.begin(offer.clone()) {
        tracing::warn!(
            target: crate::logging::target::TRANSPORT,
            sequence = offer.sequence,
            kind = ?offer.kind,
            size = offer.size_bytes,
            reason = %error,
            "rejected clipboard offer"
        );
    }
}

fn handle_clipboard_chunk(
    bytes: &[u8],
    wire_policy: Option<ClipboardPolicyMsg>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
    session: &ClipboardSession,
) {
    let (Some(wire_policy), Some(reassembler)) = (wire_policy, reassembler) else {
        return;
    };
    let Ok((header, payload)) = decode_clipboard_chunk(bytes) else {
        tracing::warn!(
            target: crate::logging::target::TRANSPORT,
            reason = "invalid_chunk",
            "rejected clipboard chunk"
        );
        return;
    };
    let Some(policy) = media_policy(wire_policy) else {
        return;
    };
    if policy
        .check_size(
            arcen_media::clipboard::ClipboardFlow::HostToClient,
            clipboard_media_kind(header.kind),
            usize::try_from(header.total_size).unwrap_or(usize::MAX),
        )
        .is_err()
    {
        tracing::warn!(
            target: crate::logging::target::TRANSPORT,
            sequence = header.sequence,
            kind = ?header.kind,
            size = header.total_size,
            reason = "policy",
            "rejected clipboard chunk"
        );
        reassembler.abort();
        return;
    }
    let mut completed = match reassembler.push(header, payload) {
        Ok(Some(completed)) => completed,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                target: crate::logging::target::TRANSPORT,
                sequence = header.sequence,
                kind = ?header.kind,
                size = header.total_size,
                reason = %error,
                "rejected clipboard chunk"
            );
            reassembler.abort();
            return;
        }
    };
    if !validate_completed_clipboard(&completed, policy) {
        return;
    }
    if let Some(item) = ClipboardItem::new(
        completed.sequence,
        completed.kind,
        completed.take_bytes(),
        completed.truncated,
    ) {
        let _ = session.queue_inbound(item);
    }
}

fn validate_completed_clipboard(
    completed: &arcen_protocol::clipboard::CompletedClipboardData,
    policy: arcen_media::clipboard::ClipboardPolicy,
) -> bool {
    if policy
        .check_size(
            arcen_media::clipboard::ClipboardFlow::HostToClient,
            clipboard_media_kind(completed.kind),
            completed.bytes.len(),
        )
        .is_err()
    {
        return false;
    }
    match completed.kind {
        ClipboardContentKind::TextUtf8 => std::str::from_utf8(&completed.bytes).is_ok(),
        ClipboardContentKind::ImagePng => arcen_media::clipboard::validate_png(
            &completed.bytes,
            arcen_media::clipboard::ImageLimits {
                max_encoded_bytes: policy.max_bytes,
                ..arcen_media::clipboard::ImageLimits::default()
            },
        )
        .is_ok(),
    }
}

fn clipboard_media_kind(kind: ClipboardContentKind) -> arcen_media::clipboard::ClipboardKind {
    match kind {
        ClipboardContentKind::TextUtf8 => arcen_media::clipboard::ClipboardKind::TextUtf8,
        ClipboardContentKind::ImagePng => arcen_media::clipboard::ClipboardKind::ImagePng,
    }
}

async fn clipboard_send_turn(ready: bool) {
    if ready {
        tokio::task::yield_now().await;
    } else {
        std::future::pending::<()>().await;
    }
}

struct OutboundClipboardTransfer {
    item: ClipboardItem,
    offer_sent: bool,
    offset: usize,
}

impl OutboundClipboardTransfer {
    fn new(item: ClipboardItem, wire_policy: ClipboardPolicyMsg) -> Option<Self> {
        let policy = media_policy(wire_policy)?;
        policy
            .check_size(
                arcen_media::clipboard::ClipboardFlow::ClientToHost,
                clipboard_media_kind(item.kind),
                item.bytes.len(),
            )
            .ok()?;
        let valid = match item.kind {
            ClipboardContentKind::TextUtf8 => std::str::from_utf8(&item.bytes).is_ok(),
            ClipboardContentKind::ImagePng => arcen_media::clipboard::validate_png(
                &item.bytes,
                arcen_media::clipboard::ImageLimits {
                    max_encoded_bytes: policy.max_bytes,
                    ..arcen_media::clipboard::ImageLimits::default()
                },
            )
            .is_ok(),
        };
        valid.then_some(Self {
            item,
            offer_sent: false,
            offset: 0,
        })
    }

    const fn sequence(&self) -> u64 {
        self.item.sequence
    }

    fn next_message(&mut self) -> Result<Message, ConnectSmokeError> {
        if !self.offer_sent {
            self.offer_sent = true;
            let size_bytes = u32::try_from(self.item.bytes.len())
                .map_err(|_| ConnectSmokeError::Clipboard("payload size"))?;
            let offer = ClipboardDataMsg::new(
                self.item.sequence,
                self.item.kind,
                size_bytes,
                self.item.truncated,
            );
            return Ok(Message::Text(serde_json::to_string(&offer)?));
        }
        let end = self
            .offset
            .checked_add(CHUNK_BYTES)
            .unwrap_or(self.item.bytes.len())
            .min(self.item.bytes.len());
        let total_size = u32::try_from(self.item.bytes.len())
            .map_err(|_| ConnectSmokeError::Clipboard("payload size"))?;
        let offset =
            u32::try_from(self.offset).map_err(|_| ConnectSmokeError::Clipboard("offset"))?;
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: self.item.kind,
                sequence: self.item.sequence,
                total_size,
                offset,
            },
            &self.item.bytes[self.offset..end],
        )
        .map_err(|_| ConnectSmokeError::Clipboard("frame encoding"))?;
        self.offset = end;
        Ok(Message::Binary(frame.into()))
    }

    const fn is_finished(&self) -> bool {
        self.offer_sent && self.offset == self.item.bytes.len()
    }
}

fn fresh_session_log_id() -> Result<CorrelationId, ConnectSmokeError> {
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| ConnectSmokeError::Randomness(error.to_string()))?;
    Ok(CorrelationId::from_uuid_v4_bytes(bytes))
}

fn event_identity(options: &ConnectOptions) -> crate::logging::EventIdentity {
    crate::logging::EventIdentity {
        user: (!options.username.is_empty()).then(|| options.username.clone()),
        host: Some(options.host.clone()),
        peer_addr: Some(format!("{}:{}", options.host, options.port)),
    }
}

fn emit_connect_attempt(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    transport_label: &'static str,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("transport", FieldValue::String(transport_label.to_owned()));
    crate::logging::emit(
        LifecycleEventKind::ClientConnectAttempt,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::TRANSPORT,
        "Deck connection attempt started",
        None,
    );
}

fn emit_connect_ok(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    rtt_ms: Option<u32>,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "tls_version",
        FieldValue::String("tls1.2_or_1.3".to_owned()),
    );
    if let Some(rtt_ms) = rtt_ms {
        let _ = fields.insert("rtt_ms", FieldValue::Integer(i64::from(rtt_ms)));
    }
    crate::logging::emit(
        LifecycleEventKind::ClientConnectOk,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::TRANSPORT,
        "Deck authenticated connection established",
        None,
    );
}

fn emit_session_result(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    telemetry: &ClientTelemetry,
    auth_method: &'static str,
    result: &Result<SessionEnd, ConnectSmokeError>,
) {
    let reason = match result {
        Ok(end) => disconnect_reason_class(end.reason),
        Err(error) => connect_error_class(error),
    };
    if result.is_err() {
        let error = result.as_ref().err().unwrap();
        let mut fields = StructuredFields::default();
        let _ = fields.insert(
            "reason_class",
            FieldValue::String(connect_error_class(error).to_owned()),
        );
        let _ = fields.insert("stage", FieldValue::String("session".to_owned()));
        crate::logging::emit(
            LifecycleEventKind::ClientConnectFail,
            sid.clone(),
            identity.clone(),
            fields,
            crate::logging::target::TRANSPORT,
            "Deck connection failed",
            None,
        );
        if connect_error_class(error) == "auth" {
            emit_auth_fail(sid, identity.clone(), auth_method);
        }
    }

    emit_client_session_end(sid, identity, telemetry, reason);
}

fn emit_client_session_end(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    telemetry: &ClientTelemetry,
    reason: &'static str,
) {
    let elapsed = telemetry.session_duration();
    let fields = client_session_end_fields(telemetry, elapsed, reason);
    crate::logging::emit(
        LifecycleEventKind::ClientSessionEnd,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::SESSION,
        "Deck session ended",
        None,
    );
}

fn client_session_end_fields(
    telemetry: &ClientTelemetry,
    elapsed: Duration,
    reason: &'static str,
) -> StructuredFields {
    let summary = telemetry.summary(elapsed);
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "duration_ms",
        FieldValue::Integer(i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert("reason_class", FieldValue::String(reason.to_owned()));
    let _ = fields.insert(
        "frames_decoded",
        FieldValue::Integer(i64::try_from(summary.frames_decoded).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "frames_dropped",
        FieldValue::Integer(i64::try_from(summary.frames_dropped).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "avg_fps",
        FieldValue::Integer(i64::try_from(summary.avg_fps).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "avg_rtt_ms",
        FieldValue::Integer(i64::try_from(summary.avg_rtt_ms).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "worst_health",
        FieldValue::String(
            summary
                .worst_health
                .map(health_state_name)
                .unwrap_or("unavailable")
                .to_owned(),
        ),
    );
    let _ = fields.insert(
        "reconnects",
        FieldValue::Integer(i64::try_from(summary.reconnects).unwrap_or(i64::MAX)),
    );
    fields
}

fn session_auth_method(auth: &SessionAuth) -> &'static str {
    match auth {
        SessionAuth::Resume(_) => "resume_grant",
        SessionAuth::Legacy | SessionAuth::InitialOptIn { .. } => "interactive",
    }
}

fn emit_auth_ok(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    auth_method: &'static str,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("auth_method", FieldValue::String(auth_method.to_owned()));
    let _ = fields.insert(
        "identity_binding",
        FieldValue::String("host_validated".to_owned()),
    );
    crate::logging::emit(
        LifecycleEventKind::SessionAuthOk,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::AUTH,
        "Deck authentication succeeded",
        None,
    );
}

fn emit_auth_fail(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    auth_method: &'static str,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("auth_method", FieldValue::String(auth_method.to_owned()));
    let _ = fields.insert("stage", FieldValue::String("authentication".to_owned()));
    let _ = fields.insert("reason_class", FieldValue::String("denied".to_owned()));
    crate::logging::emit(
        LifecycleEventKind::SessionAuthFail,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::AUTH,
        "Deck authentication failed",
        None,
    );
}

fn connect_error_class(error: &ConnectSmokeError) -> &'static str {
    match error {
        ConnectSmokeError::Timeout => "timeout",
        ConnectSmokeError::Tls(_)
        | ConnectSmokeError::TlsIdentity(_)
        | ConnectSmokeError::CertificateUntrusted(_) => "tls",
        ConnectSmokeError::AuthFailed(_)
        | ConnectSmokeError::AuthDeclined
        | ConnectSmokeError::DisclaimerRequired => "auth",
        ConnectSmokeError::Url(_) | ConnectSmokeError::Randomness(_) => "dns",
        ConnectSmokeError::UnexpectedEof
        | ConnectSmokeError::ClosedByHost(_)
        | ConnectSmokeError::WebSocket(_) => "tcp",
        _ => "protocol",
    }
}

fn disconnect_reason_class(reason: DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::Terminal(TerminalDisconnect::Manual) => "user_quit",
        DisconnectReason::Terminal(TerminalDisconnect::GracefulHostClose) => "host_closed",
        DisconnectReason::Terminal(TerminalDisconnect::Authentication) => "auth",
        DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity) => "tls",
        DisconnectReason::Transient(TransientTransportError::TimedOut) => "timeout",
        DisconnectReason::Transient(_) => "network",
        DisconnectReason::Terminal(TerminalDisconnect::Resume(_)) => "reconnect_failed",
        DisconnectReason::Terminal(TerminalDisconnect::Configuration) => "configuration",
        DisconnectReason::Terminal(TerminalDisconnect::Protocol) => "protocol",
    }
}

fn emit_network_active(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    snapshot: Option<&ClientNetworkSnapshotMsg>,
) {
    let Some(snapshot) = snapshot else {
        return;
    };
    let mut fields = network_fields(snapshot);
    crate::logging::emit(
        LifecycleEventKind::NetworkPathActive,
        sid.clone(),
        identity,
        std::mem::take(&mut fields),
        crate::logging::target::TRANSPORT,
        "client network path active",
        None,
    );
}

fn emit_network_changed(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    old: Option<&ClientNetworkSnapshotMsg>,
    new: Option<&ClientNetworkSnapshotMsg>,
) {
    match (old, new) {
        (Some(old), Some(new)) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "old_kind",
                FieldValue::String(interface_name(old.interface_kind()).to_owned()),
            );
            let _ = fields.insert(
                "new_kind",
                FieldValue::String(interface_name(new.interface_kind()).to_owned()),
            );
            if let Some(value) = old.link_mbps() {
                let _ = fields.insert("old_mbps", FieldValue::Integer(i64::from(value)));
            }
            if let Some(value) = new.link_mbps() {
                let _ = fields.insert("new_mbps", FieldValue::Integer(i64::from(value)));
            }
            let _ = fields.insert(
                "reason_class",
                FieldValue::String("interface_change".to_owned()),
            );
            crate::logging::emit(
                LifecycleEventKind::NetworkPathChanged,
                sid.clone(),
                identity,
                fields,
                crate::logging::target::TRANSPORT,
                "client network path changed",
                None,
            );
        }
        (Some(old), None) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "interface_kind",
                FieldValue::String(interface_name(old.interface_kind()).to_owned()),
            );
            crate::logging::emit(
                LifecycleEventKind::NetworkPathLost,
                sid.clone(),
                identity,
                fields,
                crate::logging::target::TRANSPORT,
                "client network path unavailable",
                None,
            );
        }
        (None, Some(new)) => emit_network_active(sid, identity, Some(new)),
        (None, None) => {}
    }
}

fn network_fields(snapshot: &ClientNetworkSnapshotMsg) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "interface_kind",
        FieldValue::String(interface_name(snapshot.interface_kind()).to_owned()),
    );
    let _ = fields.insert(
        "scope",
        FieldValue::String(scope_name(snapshot.scope()).to_owned()),
    );
    if let Some(value) = snapshot.link_mbps() {
        let _ = fields.insert("link_mbps", FieldValue::Integer(i64::from(value)));
    }
    if let Some(value) = snapshot.rssi_dbm() {
        let _ = fields.insert("rssi_dbm", FieldValue::Integer(i64::from(value)));
    }
    if let Some(value) = snapshot.mtu() {
        let _ = fields.insert("mtu", FieldValue::Integer(i64::from(value)));
    }
    fields
}

fn interface_name(kind: NetworkInterfaceKind) -> &'static str {
    match kind {
        NetworkInterfaceKind::Ethernet => "ethernet",
        NetworkInterfaceKind::Wifi => "wifi",
        NetworkInterfaceKind::Cellular => "cellular",
        NetworkInterfaceKind::Vpn => "vpn",
        NetworkInterfaceKind::Loopback => "loopback",
        NetworkInterfaceKind::Other => "other",
    }
}

fn scope_name(scope: NetworkScopeMsg) -> &'static str {
    match scope {
        NetworkScopeMsg::Lan => "lan",
        NetworkScopeMsg::Wan => "wan",
    }
}

fn emit_health_transition(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    tracker: &mut HealthTracker,
    degraded_since_ms: &mut Option<u64>,
    timestamp_ms: u64,
    health: &arcen_telemetry::HealthAssessment,
    sample: &arcen_telemetry::QosSample,
) {
    let Some((kind, fields, state)) =
        health_transition_fields(tracker, degraded_since_ms, timestamp_ms, health, sample)
    else {
        return;
    };
    crate::logging::emit(
        kind,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::HEALTH,
        "client health state changed",
        Some(state),
    );
}

fn health_transition_fields(
    tracker: &mut HealthTracker,
    degraded_since_ms: &mut Option<u64>,
    timestamp_ms: u64,
    health: &arcen_telemetry::HealthAssessment,
    sample: &arcen_telemetry::QosSample,
) -> Option<(LifecycleEventKind, StructuredFields, HealthState)> {
    let transition = tracker.update(timestamp_ms, health).ok()??;
    let mut fields = StructuredFields::default();
    let kind = match transition.current {
        HealthState::Ok => {
            let _ = fields.insert(
                "previous_state",
                FieldValue::String(
                    transition
                        .previous
                        .map(health_state_name)
                        .unwrap_or("unavailable")
                        .to_owned(),
                ),
            );
            let duration = degraded_since_ms
                .take()
                .map_or(0, |started| timestamp_ms.saturating_sub(started));
            let _ = fields.insert(
                "degraded_duration_ms",
                FieldValue::Integer(i64::try_from(duration).unwrap_or(i64::MAX)),
            );
            LifecycleEventKind::HealthOk
        }
        state => {
            degraded_since_ms.get_or_insert(timestamp_ms);
            let cause = health
                .client_experience
                .dominant_cause
                .unwrap_or(HealthCause::Heartbeat);
            let value = match cause {
                HealthCause::Fps => sample.fps_actual.unwrap_or(0),
                HealthCause::Rtt => sample.rtt_ms.unwrap_or(0),
                HealthCause::Loss => unpresented_basis_points(sample),
                HealthCause::InputLatency => sample.input_latency_ms.unwrap_or(0),
                HealthCause::Heartbeat => sample.heartbeat_misses.unwrap_or(0),
            };
            let _ = fields.insert(
                "dominant_cause",
                FieldValue::String(health_cause_name(cause).to_owned()),
            );
            let _ = fields.insert("value", FieldValue::Integer(i64::from(value)));
            let _ = fields.insert(
                "threshold",
                FieldValue::Integer(i64::from(health_threshold(cause, state, sample))),
            );
            if state == HealthState::Critical {
                LifecycleEventKind::HealthCritical
            } else {
                LifecycleEventKind::HealthDegraded
            }
        }
    };
    Some((kind, fields, transition.current))
}

fn health_threshold(
    cause: HealthCause,
    state: HealthState,
    sample: &arcen_telemetry::QosSample,
) -> u32 {
    let targets = arcen_telemetry::QosTargets::default();
    let critical = state == HealthState::Critical;
    match cause {
        HealthCause::Fps => {
            let percent = if critical {
                targets.fps_critical_percent()
            } else {
                targets.fps_degraded_percent()
            };
            sample
                .fps_target
                .unwrap_or(0)
                .saturating_mul(u32::from(percent))
                / 100
        }
        HealthCause::Rtt => {
            if critical {
                targets.rtt_critical_ms()
            } else {
                targets.rtt_degraded_ms()
            }
        }
        HealthCause::Loss => u32::from(if critical {
            targets.drop_critical_basis_points()
        } else {
            targets.drop_degraded_basis_points()
        }),
        HealthCause::InputLatency => {
            if critical {
                targets.input_critical_ms()
            } else {
                targets.input_degraded_ms()
            }
        }
        HealthCause::Heartbeat => targets.heartbeat_critical_misses(),
    }
}

/// Fraction of decoded frames that never reached the screen, in basis points.
///
/// **This is not transport loss**, despite feeding `HealthCause::Loss`. Every
/// frame counted here arrived intact and decoded successfully; it was then
/// superseded by a newer frame before the UI presented it, which is the normal
/// consequence of a client that always shows the latest frame. A session can
/// report a large value here with zero packet drops — one did, and the
/// resulting `loss=2602` sent a tester looking for a network fault that did
/// not exist.
///
/// It is still a useful health signal, because a viewer genuinely sees fewer
/// frames than arrived. It is simply a *presentation* signal, not a network
/// one; real packet loss is reported separately as loss epochs.
fn unpresented_basis_points(sample: &arcen_telemetry::QosSample) -> u32 {
    let decoded = sample.frames_decoded.unwrap_or(0);
    if decoded == 0 {
        return 0;
    }
    let missed = decoded.saturating_sub(sample.frames_presented.unwrap_or(0));
    u32::try_from(missed.saturating_mul(10_000) / decoded).unwrap_or(u32::MAX)
}

fn emit_health_snapshot(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    health: &arcen_telemetry::HealthAssessment,
    sample: &arcen_telemetry::QosSample,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "overall_state",
        FieldValue::String(
            health
                .overall
                .map(health_state_name)
                .unwrap_or("unavailable")
                .to_owned(),
        ),
    );
    if let Some(state) = health.client_experience.state {
        let _ = fields.insert(
            "client_state",
            FieldValue::String(health_state_name(state).to_owned()),
        );
    }
    if let Some(value) = sample.rtt_ms {
        let _ = fields.insert("rtt_ms", FieldValue::Integer(i64::from(value)));
    }
    let _ = fields.insert(
        "heartbeat_misses",
        FieldValue::Integer(i64::from(sample.heartbeat_misses.unwrap_or(0))),
    );
    crate::logging::emit(
        LifecycleEventKind::HealthSnapshot,
        sid.clone(),
        identity,
        fields,
        crate::logging::target::HEALTH,
        "sixty-second client proof of life",
        health.overall,
    );
}

fn emit_drop_notices(sid: &CorrelationId, identity: crate::logging::EventIdentity) {
    let Some(handle) = crate::logging::handle() else {
        return;
    };
    let Ok(notices) = handle.take_drop_notices(sid) else {
        return;
    };
    for notice in notices {
        crate::logging::emit(
            notice.kind(),
            sid.clone(),
            identity.clone(),
            notice.fields().clone(),
            crate::logging::target::HEALTH,
            "bounded observability sink dropped records",
            None,
        );
    }
}

#[cfg(feature = "experimental-raw-hid")]
fn emit_hid_start(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    device_id: u8,
    vendor_id: u16,
    product_id: u16,
) {
    let mut attached = StructuredFields::default();
    let _ = attached.insert("vendor_id", FieldValue::Integer(i64::from(vendor_id)));
    let _ = attached.insert("product_id", FieldValue::Integer(i64::from(product_id)));
    let _ = attached.insert("transport", FieldValue::String("usb".to_owned()));
    crate::logging::emit(
        LifecycleEventKind::HidDeviceAttached,
        sid.clone(),
        identity.clone(),
        attached,
        crate::logging::target::USB,
        "HID device attached",
        None,
    );
    let mut start = StructuredFields::default();
    let _ = start.insert("device_id", FieldValue::String(device_id.to_string()));
    let _ = start.insert("vendor_id", FieldValue::Integer(i64::from(vendor_id)));
    let _ = start.insert("product_id", FieldValue::Integer(i64::from(product_id)));
    crate::logging::emit(
        LifecycleEventKind::HidPassthroughStart,
        sid.clone(),
        identity,
        start,
        crate::logging::target::USB,
        "HID passthrough started",
        None,
    );
}

#[cfg(feature = "experimental-raw-hid")]
#[allow(clippy::too_many_arguments)]
fn emit_hid_end(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    device_id: u8,
    vendor_id: u16,
    product_id: u16,
    reports: u64,
    errors: u64,
) {
    let mut end = StructuredFields::default();
    let _ = end.insert("device_id", FieldValue::String(device_id.to_string()));
    let _ = end.insert(
        "reports_forwarded",
        FieldValue::Integer(i64::try_from(reports).unwrap_or(i64::MAX)),
    );
    let _ = end.insert(
        "errors",
        FieldValue::Integer(i64::try_from(errors).unwrap_or(i64::MAX)),
    );
    crate::logging::emit(
        LifecycleEventKind::HidPassthroughEnd,
        sid.clone(),
        identity.clone(),
        end,
        crate::logging::target::USB,
        "HID passthrough ended",
        None,
    );
    let mut detached = StructuredFields::default();
    let _ = detached.insert("vendor_id", FieldValue::Integer(i64::from(vendor_id)));
    let _ = detached.insert("product_id", FieldValue::Integer(i64::from(product_id)));
    let _ = detached.insert("transport", FieldValue::String("usb".to_owned()));
    crate::logging::emit(
        LifecycleEventKind::HidDeviceDetached,
        sid.clone(),
        identity,
        detached,
        crate::logging::target::USB,
        "HID device detached",
        None,
    );
}

#[cfg(feature = "experimental-raw-hid")]
fn emit_hid_error(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    device_id: Option<u8>,
    reason_class: &'static str,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "device_id",
        FieldValue::String(device_id.map_or_else(|| "unknown".to_owned(), |id| id.to_string())),
    );
    let _ = fields.insert("reason_class", FieldValue::String(reason_class.to_owned()));
    // Say what to do about it. `exclusive_access` is the expected failure when
    // a tablet vendor's own driver is running and holding the pen collection;
    // it was measured to clear the moment every vendor process is stopped.
    // Without this the reader gets an errno class and no way forward.
    if let Some(remedy) = hid_error_remedy(reason_class) {
        let _ = fields.insert("remedy", FieldValue::String(remedy.to_owned()));
    }
    crate::logging::emit(
        LifecycleEventKind::HidPassthroughError,
        sid.clone(),
        identity.clone(),
        fields,
        crate::logging::target::USB,
        "HID passthrough error",
        None,
    );
    // Only report a permission problem when it actually is one.
    // "permission_denied" is `IOHIDCheckAccess`'s explicit answer, and
    // "not_permitted" is the OS refusing the open. Everything else --
    // notably "exclusive_access", where the device is simply owned by
    // another driver -- must not be dressed up as a denial: doing so
    // previously printed PERMISSION_DENIED in the same second as
    // PERMISSION_GRANTED and sent the reader to re-grant a permission they
    // already had.
    if reason_class == "permission_denied" || reason_class == "not_permitted" {
        emit_permission(
            sid,
            identity,
            LifecycleEventKind::PermissionDenied,
            "input_monitoring",
        );
    }
}

/// Actionable next step for a HID failure, where one is actually known.
///
/// Returns `None` rather than inventing advice: a wrong remedy is worse than
/// none, having already cost one evening of re-granting a permission that was
/// never the problem.
#[cfg(feature = "experimental-raw-hid")]
fn hid_error_remedy(reason_class: &str) -> Option<&'static str> {
    match reason_class {
        "exclusive_access" => Some(
            "another process holds the tablet right now; restart the vendor driver \
             (e.g. Wacom's TabletDriver) or unplug and replug the tablet, then reconnect",
        ),
        "permission_denied" | "not_permitted" => Some(
            "grant Arcen Deck access under System Settings > Privacy & Security > \
             Input Monitoring, then relaunch",
        ),
        _ => None,
    }
}

#[cfg(feature = "experimental-raw-hid")]
fn emit_permission(
    sid: &CorrelationId,
    identity: crate::logging::EventIdentity,
    kind: LifecycleEventKind,
    permission_name: &'static str,
) {
    if !PROCESS_PERMISSION_EVENTS.claim(kind) {
        return;
    }
    let mut permission = StructuredFields::default();
    let _ = permission.insert("permission", FieldValue::String(permission_name.to_owned()));
    let _ = permission.insert("platform", FieldValue::String("macos".to_owned()));
    crate::logging::emit(
        kind,
        sid.clone(),
        identity,
        permission,
        crate::logging::target::AUTH,
        "macOS permission state changed",
        None,
    );
}

fn health_state_name(state: HealthState) -> &'static str {
    match state {
        HealthState::Ok => "ok",
        HealthState::Degraded => "degraded",
        HealthState::Critical => "critical",
    }
}

fn health_cause_name(cause: HealthCause) -> &'static str {
    match cause {
        HealthCause::Fps => "fps",
        HealthCause::Rtt => "rtt",
        // Not transport loss, and saying "loss" cost a tester a whole session
        // hunting a network fault that did not exist: a run reporting
        // `loss=2602` had zero packet drops and delivered essentially every
        // host frame. What this cause actually measures is decoded frames that
        // never reached the screen — see `unpresented_basis_points`. Real
        // packet loss is reported separately as "loss epochs".
        HealthCause::Loss => "unpresented",
        HealthCause::InputLatency => "input_latency",
        HealthCause::Heartbeat => "heartbeat",
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn classify_disconnect(error: &ConnectSmokeError) -> SessionEnd {
    classify_disconnect_at(error, Instant::now())
}

fn microphone_teardown_reason(outcome: &Result<SessionEnd, ConnectSmokeError>) -> &'static str {
    let reason = match outcome {
        Ok(end) => end.reason,
        Err(error) => classify_disconnect(error).reason,
    };
    match reason {
        DisconnectReason::Terminal(TerminalDisconnect::Manual) => "manual",
        DisconnectReason::Terminal(TerminalDisconnect::GracefulHostClose) => "host_closed",
        DisconnectReason::Transient(TransientTransportError::TimedOut) => "timeout",
        DisconnectReason::Transient(TransientTransportError::UnexpectedEof) => "unexpected_eof",
        DisconnectReason::Transient(_) => "transport_failure",
        DisconnectReason::Terminal(TerminalDisconnect::Authentication) => "authentication_failure",
        DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity) => "tls_failure",
        DisconnectReason::Terminal(TerminalDisconnect::Resume(_)) => "resume_failure",
        DisconnectReason::Terminal(TerminalDisconnect::Configuration) => "configuration_failure",
        DisconnectReason::Terminal(TerminalDisconnect::Protocol) => "protocol_failure",
    }
}

fn classify_disconnect_at(error: &ConnectSmokeError, observed_at: Instant) -> SessionEnd {
    use std::io::ErrorKind;
    use tokio_tungstenite::tungstenite::{error::ProtocolError, Error as WebSocketError};

    let reason = match error {
        ConnectSmokeError::SessionClosed => DisconnectReason::Terminal(TerminalDisconnect::Manual),
        ConnectSmokeError::Timeout => {
            DisconnectReason::Transient(TransientTransportError::TimedOut)
        }
        ConnectSmokeError::UnexpectedEof => {
            DisconnectReason::Transient(TransientTransportError::UnexpectedEof)
        }
        ConnectSmokeError::WebSocket(WebSocketError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake,
        )) => DisconnectReason::Transient(TransientTransportError::UnexpectedEof),
        ConnectSmokeError::WebSocket(WebSocketError::Io(error)) => {
            let transient = match error.kind() {
                ErrorKind::UnexpectedEof => TransientTransportError::UnexpectedEof,
                ErrorKind::ConnectionReset => TransientTransportError::ConnectionReset,
                ErrorKind::TimedOut => TransientTransportError::TimedOut,
                ErrorKind::ConnectionRefused => TransientTransportError::ConnectionRefused,
                ErrorKind::NetworkUnreachable => TransientTransportError::NetworkUnreachable,
                ErrorKind::HostUnreachable => TransientTransportError::HostUnreachable,
                ErrorKind::ConnectionAborted => TransientTransportError::ConnectionAborted,
                ErrorKind::BrokenPipe => TransientTransportError::BrokenPipe,
                ErrorKind::NotConnected => TransientTransportError::NotConnected,
                ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                    TransientTransportError::TransientIo
                }
                _ => {
                    return SessionEnd {
                        reason: DisconnectReason::Terminal(TerminalDisconnect::Protocol),
                        message: bounded_message(&error.to_string()),
                        observed_at,
                    };
                }
            };
            DisconnectReason::Transient(transient)
        }
        ConnectSmokeError::WebSocket(WebSocketError::ConnectionClosed)
        | ConnectSmokeError::ClosedByHost(_) => {
            DisconnectReason::Terminal(TerminalDisconnect::GracefulHostClose)
        }
        ConnectSmokeError::WebSocket(WebSocketError::Tls(_))
        | ConnectSmokeError::Tls(_)
        | ConnectSmokeError::TlsIdentity(_)
        | ConnectSmokeError::CertificateUntrusted(_) => {
            DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity)
        }
        ConnectSmokeError::ResumeRejected { error_code, .. } => {
            DisconnectReason::Terminal(TerminalDisconnect::Resume(*error_code))
        }
        ConnectSmokeError::AuthFailed(_)
        | ConnectSmokeError::AuthDeclined
        | ConnectSmokeError::DisclaimerRequired => {
            DisconnectReason::Terminal(TerminalDisconnect::Authentication)
        }
        ConnectSmokeError::Url(_)
        | ConnectSmokeError::Randomness(_)
        | ConnectSmokeError::TransportUnavailable(_)
        | ConnectSmokeError::MultiMonitorUnsupported(_) => {
            DisconnectReason::Terminal(TerminalDisconnect::Configuration)
        }
        ConnectSmokeError::Json(_)
        | ConnectSmokeError::Clipboard(_)
        | ConnectSmokeError::InvalidDisclaimer(_)
        | ConnectSmokeError::UnexpectedFirstMessage(_)
        | ConnectSmokeError::NonTextMessage
        | ConnectSmokeError::ControlMessageTooLarge { .. }
        | ConnectSmokeError::ResumeProtocol(_)
        | ConnectSmokeError::Microphone(_)
        | ConnectSmokeError::WebSocket(_) => {
            DisconnectReason::Terminal(TerminalDisconnect::Protocol)
        }
    };
    SessionEnd {
        reason,
        message: bounded_message(&error.to_string()),
        observed_at,
    }
}

fn bounded_message(message: &str) -> String {
    const LIMIT: usize = 240;
    let mut output: String = message.chars().take(LIMIT).collect();
    if message.chars().count() > LIMIT {
        output.push('…');
    }
    output
}

/// Builds the `quality_settings` message sent right after `client_hello`,
/// carrying the Deck's requested depth, range and matrix alongside the
/// existing codec/chroma/fps
/// (`docs/architecture/color-fidelity.md`'s "Where colour lives" table), plus
/// the encode intent, which is orthogonal to all of them: it tells the host
/// what to optimise for, not what to encode.
fn rust_viewer_quality_settings(profile: &StreamProfile) -> QualitySettings {
    QualitySettings {
        quality_bias: 0.3,
        max_fps: profile.max_fps.clamp(1, 240),
        max_bandwidth_mbps: 25.0,
        codec: profile.codec.clone(),
        chroma: profile.chroma.clone(),
        video_selection: profile.video_selection,
        bit_depth: profile.bit_depth.clone(),
        color_range: profile.color_range.clone(),
        color_matrix: profile.color_matrix.clone(),
        transfer: profile.transfer.clone(),
        color_primaries: profile.color_primaries.clone(),
        encode_intent: profile.encode_intent.clone(),
        force_lossless: false,
        intra_refresh: false,
        enable_audio: true,
        audio_bitrate_kbps: 128,
        ..QualitySettings::default()
    }
}

async fn recv_json<S>(ws: &mut S, wait: Duration) -> Result<Value, ConnectSmokeError>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let message = timeout(remaining, ws.next())
            .await
            .map_err(|_| ConnectSmokeError::Timeout)?
            .ok_or_else(|| ConnectSmokeError::ClosedByHost(String::new()))??;
        match message {
            Message::Text(text) if control_size_allowed(text.len()) => {
                return Ok(serde_json::from_str(&text)?);
            }
            Message::Binary(bytes) if control_size_allowed(bytes.len()) => {
                return Ok(serde_json::from_slice(&bytes)?);
            }
            Message::Text(text) => {
                return Err(ConnectSmokeError::ControlMessageTooLarge {
                    size: text.len(),
                    limit: MAX_INCOMING_CONTROL_SIZE,
                });
            }
            Message::Binary(bytes) => {
                return Err(ConnectSmokeError::ControlMessageTooLarge {
                    size: bytes.len(),
                    limit: MAX_INCOMING_CONTROL_SIZE,
                });
            }
            Message::Close(frame) => {
                let reason = frame
                    .map(|frame| frame.reason.to_string())
                    .filter(|reason| !reason.is_empty())
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default();
                return Err(ConnectSmokeError::ClosedByHost(reason));
            }
            // Pings/pongs mid-handshake are keepalive, not the reply we await;
            // tungstenite queues the pong response internally.
            Message::Ping(_) | Message::Pong(_) => continue,
            _ => return Err(ConnectSmokeError::NonTextMessage),
        }
    }
}

fn control_size_allowed(size: usize) -> bool {
    size <= MAX_INCOMING_CONTROL_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI recognises this failure by prefix in order to offer an inline
    /// recovery instead of leaving the user at a dead-end error (the
    /// pier-windows-software.example.internal field report: the host advertises `max_monitors: 1`,
    /// and the message told the user to change a setting they could not find
    /// from where they were standing). Editing the wording without updating
    /// the constant would silently remove that offer, so tie them together.
    #[test]
    fn multi_monitor_unsupported_message_keeps_its_recognisable_prefix() {
        let rendered = ConnectSmokeError::MultiMonitorUnsupported(
            "requested topology has 2 monitors but max_monitors advertises 1".to_string(),
        )
        .to_string();
        assert!(
            rendered.starts_with(MULTI_MONITOR_UNSUPPORTED_PREFIX),
            "the UI matches this failure by prefix; rendered message was: {rendered}",
        );
    }

    #[cfg(feature = "wss-compat")]
    use crate::protocol::messages::CLIENT_HELLO;
    #[cfg(feature = "wss-compat")]
    use crate::protocol::{
        encode_video_header, ChromaSubsampling, FrameType, VideoCodec, VideoHeader,
    };
    use arcen_transport::quic::MonitorStreamPrefaceError;
    use arcen_transport::tls::{PinKind, TlsPin};
    use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    #[cfg(feature = "wss-compat")]
    use tokio::net::TcpListener;
    #[cfg(feature = "wss-compat")]
    use tokio_tungstenite::accept_async;

    fn untrusted_cert_info() -> CertInfo {
        CertInfo {
            endpoint: "probe.test:18443".to_string(),
            server_name: "probe.test".to_string(),
            peer_address: None,
            certificate_sha256: TlsPin::new(PinKind::CertificateSha256, [0x24; 32]),
            certificate_sha256_display: crate::transport::tls::format_fingerprint(&[0x24; 32]),
            spki_sha256: TlsPin::new(PinKind::SubjectPublicKeyInfoSha256, [0x42; 32]),
            spki_sha256_display: crate::transport::tls::format_fingerprint(&[0x42; 32]),
            not_before_epoch_secs: 1_700_000_000,
            not_after_epoch_secs: 1_800_000_000,
            cert_der_len: 512,
        }
    }

    fn transport_test_options(quic_enabled: bool) -> ConnectOptions {
        ConnectOptions {
            host: "127.0.0.1".to_string(),
            port: if quic_enabled { 18_444 } else { 18_443 },
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(1),
            tls: TlsTrustConfig::system_ca(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: true,
            microphone_enabled: false,
            tablet_input_enabled: false,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled,
        }
    }

    fn multi_monitor_test_topology(count: usize) -> arcen_media::RequestedMonitorTopology {
        use arcen_media::{Monitor, MonitorIdentity, RequestedMonitor, Rotation};
        let monitors = (0..count)
            .map(|index| {
                let monitor = Monitor {
                    identity: MonitorIdentity {
                        // Numeric so `legacy_client_monitor_id`'s
                        // `id.parse::<u32>()` succeeds and yields distinct
                        // legacy monitor ids across the topology.
                        id: (1000 + index).to_string(),
                        name: format!("Display {index}"),
                        vendor: 1,
                        model: 2,
                        serial: index as u32,
                    },
                    x: index as i32 * 960,
                    y: 0,
                    width_px: 1920,
                    height_px: 1080,
                    scale: 2.0,
                    refresh_hz: 60,
                    rotation: Rotation::Degrees0,
                    primary: index == 0,
                    width_mm: 300.0,
                    height_mm: 200.0,
                };
                RequestedMonitor::new(monitor, 960, 540).expect("valid requested monitor")
            })
            .collect();
        arcen_media::RequestedMonitorTopology::new(monitors).expect("valid topology")
    }

    /// Same shape as [`multi_monitor_test_topology`] but with an explicit
    /// rotation and per-monitor physical size, standing in for whatever a
    /// Remote UI Scale transform already baked into `width_mm`/`height_mm`
    /// upstream (`crate::ui::app::apply_remote_ui_scale_to_requested_topology`,
    /// applied once at connect time before `options.multi_monitor_topology`
    /// is ever frozen) -- this module's own responsibility is only to prove
    /// that whatever already-final topology `options` carries survives
    /// `send_resume_auth` byte-for-byte, not to re-test the scale transform's
    /// own math (already covered where it is applied, in `ui::app`'s tests).
    fn multi_monitor_test_topology_rotated_with_mm(
        count: usize,
        rotation: arcen_media::Rotation,
        width_mm: f32,
        height_mm: f32,
    ) -> arcen_media::RequestedMonitorTopology {
        use arcen_media::{Monitor, MonitorIdentity, RequestedMonitor};
        let monitors = (0..count)
            .map(|index| {
                let monitor = Monitor {
                    identity: MonitorIdentity {
                        id: (2000 + index).to_string(),
                        name: format!("Display {index}"),
                        vendor: 1,
                        model: 2,
                        serial: index as u32,
                    },
                    x: index as i32 * 960,
                    y: 0,
                    width_px: 1920,
                    height_px: 1080,
                    scale: 2.0,
                    refresh_hz: 60,
                    rotation,
                    primary: index == 0,
                    width_mm,
                    height_mm,
                };
                RequestedMonitor::new(monitor, 960, 540).expect("valid requested monitor")
            })
            .collect();
        arcen_media::RequestedMonitorTopology::new(monitors).expect("valid topology")
    }

    /// Collects every message written through a mock `Sink<Message>` so
    /// `send_resume_auth`/`send_auth`'s actual wire output can be inspected
    /// directly, without a real socket or the `wss-compat` feature.
    fn collecting_sink() -> (
        Pin<Box<dyn Sink<Message, Error = tokio_tungstenite::tungstenite::Error>>>,
        Arc<std::sync::Mutex<Vec<Message>>>,
    ) {
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sent_for_sink = Arc::clone(&sent);
        let sink = futures_util::sink::unfold(sent_for_sink, |sent, message: Message| async move {
            sent.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(message);
            Ok::<_, tokio_tungstenite::tungstenite::Error>(sent)
        });
        (Box::pin(sink), sent)
    }

    /// Asserts exactly one text `AuthResponse` was written to `sent` and
    /// deserializes it.
    fn sole_sent_auth_response(sent: &Arc<std::sync::Mutex<Vec<Message>>>) -> AuthResponse {
        let messages = sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(messages.len(), 1, "exactly one setup message must be sent");
        let Message::Text(text) = &messages[0] else {
            panic!("expected a text auth response message");
        };
        serde_json::from_str(text).expect("valid AuthResponse JSON")
    }

    fn resume_test_attempt() -> ResumeAttempt {
        ResumeAttempt {
            holder_nonce: "test-holder-nonce".to_string(),
            grant: "test-resume-grant".to_string(),
            previous_sid: "sid-prev".to_string(),
            generation: 1,
            identity: crate::reconnect::ConnectionIdentity {
                endpoint: "wss://pier:18443".to_string(),
                security: "pin".to_string(),
                topology: "direct".to_string(),
            },
            attempt: 1,
            gap: Duration::from_millis(0),
        }
    }

    #[tokio::test]
    async fn send_resume_auth_reproduces_the_exact_multi_monitor_v1_request_sent_on_the_initial_attach(
    ) {
        // Ultimate/gate-readiness cross-client resume fix: whenever
        // `options.multi_monitor_topology` is negotiated, the resume
        // `AuthResponse` must carry a byte-equivalent `multi_monitor_v1`
        // sidecar to the one `send_auth` sent on the original attach --
        // including rotation and the already-UI-scaled physical mm size --
        // never a bare/legacy resume response that would desync the host's
        // committed multi-monitor roster expectations.
        let mut options = transport_test_options(false);
        let topology = multi_monitor_test_topology_rotated_with_mm(
            3,
            arcen_media::Rotation::Degrees90,
            111.5,
            222.25,
        );
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology,
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let auth_request = multi_monitor_test_auth_request()
            .with_multi_monitor_v1_offer(
                crate::protocol::messages::AuthMultiMonitorOfferMsg::new(
                    4,
                    vec![
                        crate::protocol::messages::RotationMsg::Degrees0,
                        crate::protocol::messages::RotationMsg::Degrees90,
                    ],
                    vec![crate::protocol::messages::MultiMonitorCarrierMsg::MuxedReliableStream],
                )
                .expect("valid offer"),
            )
            .expect("offer attaches");

        // What `send_auth` itself would produce on the *initial* attach,
        // using the identical shared `apply_multi_monitor_negotiation` core.
        let initial = apply_multi_monitor_negotiation(
            AuthResponse::password("user", "pass"),
            &options,
            &auth_request,
        )
        .expect("initial attach negotiation must succeed");

        // What `send_resume_auth` produces on a later resume, given the
        // exact same frozen `options` (as `drive_reconnect`'s own
        // `credential_free_clone` always threads through unchanged) and a
        // fresh -- but still sufficient -- resume-time offer.
        let session_auth = SessionAuth::Resume(resume_test_attempt());
        let (mut sink, sent) = collecting_sink();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([3; 16]);
        send_resume_auth(
            &mut sink,
            &options,
            &session_auth,
            &auth_request,
            &session_log_id,
            None,
            None,
        )
        .await
        .expect("resume negotiation with a sufficient offer must succeed");
        let resumed = sole_sent_auth_response(&sent);

        assert_eq!(
            serde_json::to_string(&resumed.multi_monitor_v1).unwrap(),
            serde_json::to_string(&initial.multi_monitor_v1).unwrap(),
            "resume must carry a byte-equivalent multi_monitor_v1 sidecar to the initial attach",
        );
        assert_eq!(resumed.multi_monitor_v1, initial.multi_monitor_v1);
        assert_eq!(resumed.monitors, initial.monitors);
        assert_eq!(resumed.displays_mode, initial.displays_mode);
        assert_eq!(resumed.screen_width, initial.screen_width);
        assert_eq!(resumed.screen_height, initial.screen_height);
        assert_eq!(
            resumed
                .multi_monitor_v1
                .as_ref()
                .expect("negotiated")
                .carriers(),
            initial
                .multi_monitor_v1
                .as_ref()
                .expect("negotiated")
                .carriers(),
        );
        // Still a genuine credential-free resume response, not the password
        // path it was derived alongside.
        assert_eq!(resumed.method, "resume");
        assert_eq!(
            resumed.resume_holder_nonce.as_deref(),
            Some("test-holder-nonce")
        );
    }

    #[tokio::test]
    async fn send_resume_auth_fails_closed_and_sends_nothing_when_the_resume_offer_is_missing() {
        // A host that no longer advertises multi-monitor-v1 at all by the
        // time the resume attempt's own `AuthRequest` arrives (an old/
        // rolled-back host, or a config change while disconnected) must
        // never cause a bare/legacy (`multi_monitor_v1: None`) resume
        // response to go out -- that would silently desync the host's own
        // still-committed multi-monitor roster. `send_resume_auth` must fail
        // cleanly instead, letting the caller fall back to full
        // authentication.
        let mut options = transport_test_options(false);
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology: multi_monitor_test_topology(2),
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let auth_request = multi_monitor_test_auth_request();
        let session_auth = SessionAuth::Resume(resume_test_attempt());
        let (mut sink, sent) = collecting_sink();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([4; 16]);

        let error = send_resume_auth(
            &mut sink,
            &options,
            &session_auth,
            &auth_request,
            &session_log_id,
            None,
            None,
        )
        .await
        .expect_err("a missing resume-time offer must fail the resume cleanly");

        assert!(matches!(
            error,
            ConnectSmokeError::MultiMonitorUnsupported(_)
        ));
        assert!(
            sent.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "no resume auth message -- legacy or otherwise -- may ever be sent once the \
             negotiated multi-monitor-v1 sidecar cannot be reproduced",
        );
    }

    #[tokio::test]
    async fn send_resume_auth_fails_closed_when_the_resume_offer_no_longer_covers_the_negotiated_topology(
    ) {
        // The resume-time offer is present but has shrunk below the
        // originally negotiated monitor count -- still a clean resume
        // failure (never a silent downgrade to a subset of the committed
        // roster).
        let mut options = transport_test_options(false);
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology: multi_monitor_test_topology(4),
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let auth_request = multi_monitor_test_auth_request()
            .with_multi_monitor_v1_offer(
                crate::protocol::messages::AuthMultiMonitorOfferMsg::new(
                    2,
                    vec![crate::protocol::messages::RotationMsg::Degrees0],
                    vec![crate::protocol::messages::MultiMonitorCarrierMsg::MuxedReliableStream],
                )
                .expect("valid offer"),
            )
            .expect("offer attaches");
        let session_auth = SessionAuth::Resume(resume_test_attempt());
        let (mut sink, sent) = collecting_sink();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([5; 16]);

        let error = send_resume_auth(
            &mut sink,
            &options,
            &session_auth,
            &auth_request,
            &session_log_id,
            None,
            None,
        )
        .await
        .expect_err("a since-shrunk resume-time offer must fail the resume cleanly");

        assert!(matches!(
            error,
            ConnectSmokeError::MultiMonitorUnsupported(_)
        ));
        assert!(sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty());
    }

    #[tokio::test]
    async fn send_resume_auth_is_unaffected_by_multi_monitor_negotiation_for_a_legacy_session() {
        // Primary-only/windowed/single-display sessions never populate
        // `multi_monitor_topology`, so resume must stay exactly as it was
        // before this fix -- a plain credential-free resume response with
        // no `multi_monitor_v1` sidecar at all, regardless of whatever the
        // host's resume-time offer happens to be.
        let options = transport_test_options(false);
        assert!(options.multi_monitor_topology.is_none());
        let auth_request = multi_monitor_test_auth_request();
        let session_auth = SessionAuth::Resume(resume_test_attempt());
        let (mut sink, sent) = collecting_sink();
        let session_log_id = CorrelationId::from_uuid_v4_bytes([6; 16]);

        send_resume_auth(
            &mut sink,
            &options,
            &session_auth,
            &auth_request,
            &session_log_id,
            None,
            None,
        )
        .await
        .expect("legacy/single-monitor resume must be entirely unaffected");

        let resumed = sole_sent_auth_response(&sent);
        assert!(resumed.multi_monitor_v1.is_none());
        assert_eq!(resumed.method, "resume");
        assert_eq!(
            resumed.resume_holder_nonce.as_deref(),
            Some("test-holder-nonce")
        );
    }

    fn multi_monitor_test_auth_request() -> AuthRequest {
        AuthRequest {
            msg_type: crate::protocol::messages::AUTH_REQUEST.to_string(),
            auth_methods: vec!["password".to_string()],
            challenge: "challenge".to_string(),
            salt: String::new(),
            auth_mode: None,
            disclaimer: None,
            multi_monitor_v1: None,
        }
    }

    #[test]
    fn multi_monitor_negotiation_is_a_no_op_when_not_requested() {
        // Primary-only/windowed/single-display Match Layout never populate
        // `multi_monitor_topology`, so this must be an exact passthrough —
        // legacy compatibility is unaffected by this tranche's wiring.
        let options = transport_test_options(false);
        assert!(options.multi_monitor_topology.is_none());
        let auth_request = multi_monitor_test_auth_request();
        let response = AuthResponse::password("user", "pass");
        let unchanged = apply_multi_monitor_negotiation(response.clone(), &options, &auth_request)
            .expect("no-op negotiation cannot fail");
        assert_eq!(unchanged.displays_mode, response.displays_mode);
        assert_eq!(unchanged.monitors, response.monitors);
        assert!(unchanged.multi_monitor_v1.is_none());
    }

    #[test]
    fn multi_monitor_negotiation_fails_closed_when_the_host_never_offered_it() {
        // Match My Layout with more than one local display, but the host's
        // `AuthRequest` carries no `multi_monitor_v1` offer: `send_auth` must
        // fail the connection with a typed unsupported-host error rather than
        // silently sending only the primary display.
        let mut options = transport_test_options(false);
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology: multi_monitor_test_topology(2),
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let auth_request = multi_monitor_test_auth_request();
        let response = AuthResponse::password("user", "pass");

        let error = apply_multi_monitor_negotiation(response, &options, &auth_request)
            .expect_err("a legacy host with no offer must fail the connection");
        assert!(matches!(
            error,
            ConnectSmokeError::MultiMonitorUnsupported(_)
        ));
    }

    #[test]
    fn multi_monitor_negotiation_fails_closed_when_the_offer_is_insufficient() {
        // The host offered multi-monitor-v1 but capped it below the local
        // display count: still a hard failure, never a silent downgrade to a
        // subset of the requested layout.
        let mut options = transport_test_options(false);
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology: multi_monitor_test_topology(4),
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let auth_request = multi_monitor_test_auth_request()
            .with_multi_monitor_v1_offer(
                crate::protocol::messages::AuthMultiMonitorOfferMsg::new(
                    2,
                    vec![crate::protocol::messages::RotationMsg::Degrees0],
                    vec![crate::protocol::messages::MultiMonitorCarrierMsg::MuxedReliableStream],
                )
                .expect("valid offer"),
            )
            .expect("offer attaches");
        let response = AuthResponse::password("user", "pass");

        let error = apply_multi_monitor_negotiation(response, &options, &auth_request)
            .expect_err("an offer below the local display count must still fail");
        assert!(matches!(
            error,
            ConnectSmokeError::MultiMonitorUnsupported(_)
        ));
    }

    #[test]
    fn multi_monitor_negotiation_attaches_the_sidecar_when_the_host_offer_covers_it() {
        let mut options = transport_test_options(false);
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology: multi_monitor_test_topology(4),
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let auth_request = multi_monitor_test_auth_request()
            .with_multi_monitor_v1_offer(
                crate::protocol::messages::AuthMultiMonitorOfferMsg::new(
                    4,
                    vec![crate::protocol::messages::RotationMsg::Degrees0],
                    vec![crate::protocol::messages::MultiMonitorCarrierMsg::MuxedReliableStream],
                )
                .expect("valid offer"),
            )
            .expect("offer attaches");
        let response = AuthResponse::password("user", "pass");

        let negotiated = apply_multi_monitor_negotiation(response, &options, &auth_request)
            .expect("a sufficient host offer must succeed");
        assert_eq!(negotiated.displays_mode, "match_layout");
        assert_eq!(negotiated.monitors.len(), 4);
        assert!(negotiated.multi_monitor_v1.is_some());
    }

    #[test]
    fn quic_tofu_capture_remains_a_typed_certificate_event() {
        let info = untrusted_cert_info();
        let mut options = transport_test_options(true);
        options.tls = TlsTrustConfig::tofu_probe("probe.test:18444");
        options.tls.set_captured_certificate_for_test(info.clone());

        let mapped = map_captured_certificate_error(
            &options,
            ConnectSmokeError::TlsIdentity(TOFU_CAPTURE_REJECT_ERROR.to_string()),
        );
        let (tx, mut events) = mpsc::unbounded_channel();
        publish_session_result(Err(mapped), &tx);

        assert!(matches!(
            events.try_recv(),
            Ok(SessionEvent::CertificateUntrusted(captured)) if captured == info
        ));
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn tofu_reject_without_a_capture_stays_a_tls_error() {
        let options = transport_test_options(true);
        let mapped = map_captured_certificate_error(
            &options,
            ConnectSmokeError::TlsIdentity(TOFU_CAPTURE_REJECT_ERROR.to_string()),
        );

        assert!(matches!(mapped, ConnectSmokeError::TlsIdentity(_)));
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn client_hello_advertises_only_the_selected_transport() {
        let sid = CorrelationId::from_uuid_v4_bytes([9; 16]);
        let wss = client_hello_with_metadata(&transport_test_options(false), &sid);
        let quic = client_hello_with_metadata(&transport_test_options(true), &sid);
        assert_eq!(
            wss.transport_capabilities,
            vec![arcen_protocol::CAPABILITY_TRANSPORT_WSS.to_string()]
        );
        assert_eq!(
            quic.transport_capabilities,
            vec![arcen_protocol::CAPABILITY_TRANSPORT_QUIC.to_string()]
        );
    }

    #[test]
    #[cfg(not(feature = "wss-compat"))]
    fn product_client_hello_advertises_quic_even_for_legacy_false_selector() {
        let sid = CorrelationId::from_uuid_v4_bytes([9; 16]);
        let hello = client_hello_with_metadata(&transport_test_options(false), &sid);
        assert_eq!(
            hello.transport_capabilities,
            vec![arcen_protocol::CAPABILITY_TRANSPORT_QUIC.to_string()]
        );
        assert_eq!(
            hello.input_capabilities.region_input,
            InputCapabilityAvailability::Available
        );
    }

    #[test]
    fn client_hello_capability_flags_come_from_the_probe_not_the_requested_profile() {
        // Pure-logic mapping test (no real `VTDecompressionSession` needed):
        // a fabricated probe result stands in for what a real macOS probe
        // would return, and every flag `apply_decode_capabilities` sets must
        // come from it verbatim.
        let capabilities = DecodeCapabilities {
            h264: true,
            h265: true,
            av1: false,
            yuv444: true,
            main10: true,
            main12: false,
            full_range: true,
            identity_matrix: false,
            bt601_matrix: true,
            bt2020_ncl_matrix: true,
            hardware_accelerated: Some(true),
        };
        let hello = apply_decode_capabilities(ClientHelloMsg::default(), capabilities);
        assert!(hello.supports_h264);
        assert!(hello.supports_h265);
        assert!(!hello.supports_av1);
        assert!(hello.supports_yuv444);
        assert!(hello.supports_main10);
        assert!(!hello.supports_main12);
        assert!(hello.supports_full_range);
        assert!(!hello.supports_identity_matrix);
        assert!(hello.supports_bt601_matrix);
        assert!(hello.supports_bt2020_ncl_matrix);
        assert_eq!(hello.decoder_backend, "videotoolbox-hw");
    }

    #[test]
    fn client_hello_conservatively_reports_no_capability_when_the_probe_finds_none() {
        // The honest fallback: a probe result where nothing decoded must
        // never leave any capability flag `true`, and must say so in
        // `decoder_backend` rather than the old hardcoded empty string.
        let hello =
            apply_decode_capabilities(ClientHelloMsg::default(), DecodeCapabilities::default());
        assert!(!hello.supports_h264);
        assert!(!hello.supports_h265);
        assert!(!hello.supports_av1);
        assert!(!hello.supports_yuv444);
        assert!(!hello.supports_main10);
        assert!(!hello.supports_main12);
        assert!(!hello.supports_full_range);
        assert!(!hello.supports_identity_matrix);
        assert!(!hello.supports_bt601_matrix);
        assert!(!hello.supports_bt2020_ncl_matrix);
        assert_eq!(hello.decoder_backend, "unsupported");
    }

    #[test]
    fn client_hello_capability_flags_do_not_vary_with_the_requested_profile() {
        // This is the bug `w3-real-caps` fixes: `supports_main10` etc. used
        // to read `options.profile` directly, so two connections requesting
        // different depths would advertise different capabilities even
        // though both route through the exact same, process-cached probe of
        // this one Mac. Whatever this build's real probe finds, it must not
        // depend on what THIS connection happens to request -- the actual
        // request rides `quality_settings`, sent right after `client_hello`.
        // `ConnectOptions` implements `Drop` (it zeroizes `password`), so its
        // fields are set individually here rather than via `..` update
        // syntax, which cannot move out of a `Drop` type.
        let sid = CorrelationId::from_uuid_v4_bytes([9; 16]);
        let mut ten_bit = transport_test_options(false);
        ten_bit.profile = StreamProfile {
            bit_depth: "10".to_string(),
            ..StreamProfile::default()
        };
        let eight_bit = transport_test_options(false); // default profile: 8-bit/limited/bt709

        let ten_bit_hello = client_hello_with_metadata(&ten_bit, &sid);
        let eight_bit_hello = client_hello_with_metadata(&eight_bit, &sid);

        assert_eq!(ten_bit_hello.supports_h264, eight_bit_hello.supports_h264);
        assert_eq!(ten_bit_hello.supports_h265, eight_bit_hello.supports_h265);
        assert_eq!(ten_bit_hello.supports_av1, eight_bit_hello.supports_av1);
        assert_eq!(
            ten_bit_hello.supports_yuv444,
            eight_bit_hello.supports_yuv444
        );
        assert_eq!(
            ten_bit_hello.supports_main10,
            eight_bit_hello.supports_main10
        );
        assert_eq!(
            ten_bit_hello.supports_main12,
            eight_bit_hello.supports_main12
        );
        assert_eq!(
            ten_bit_hello.supports_full_range,
            eight_bit_hello.supports_full_range
        );
        assert_eq!(
            ten_bit_hello.supports_identity_matrix,
            eight_bit_hello.supports_identity_matrix
        );
        assert_eq!(
            ten_bit_hello.supports_bt601_matrix,
            eight_bit_hello.supports_bt601_matrix
        );
        assert_eq!(
            ten_bit_hello.supports_bt2020_ncl_matrix,
            eight_bit_hello.supports_bt2020_ncl_matrix
        );
        assert_eq!(
            ten_bit_hello.decoder_backend,
            eight_bit_hello.decoder_backend
        );
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn server_transport_must_match_the_selected_socket() {
        let mut hello: ServerHelloMsg =
            serde_json::from_value(serde_json::json!({"type": SERVER_HELLO})).unwrap();
        assert!(validate_server_transport(&transport_test_options(false), &hello).is_ok());
        assert!(validate_server_transport(&transport_test_options(true), &hello).is_err());

        hello.negotiated_transport = Some(arcen_protocol::CAPABILITY_TRANSPORT_QUIC.to_string());
        assert!(validate_server_transport(&transport_test_options(true), &hello).is_ok());
        assert!(validate_server_transport(&transport_test_options(false), &hello).is_err());
    }

    #[test]
    #[cfg(not(feature = "wss-compat"))]
    fn product_server_transport_requires_explicit_quic_negotiation() {
        let mut hello: ServerHelloMsg =
            serde_json::from_value(serde_json::json!({"type": SERVER_HELLO})).unwrap();
        assert!(validate_server_transport(&transport_test_options(false), &hello).is_err());
        hello.negotiated_transport = Some(arcen_protocol::CAPABILITY_TRANSPORT_QUIC.to_string());
        assert!(validate_server_transport(&transport_test_options(false), &hello).is_ok());
    }

    /// The two failures a user can actually act on must say so, and the one
    /// that is *not* a permission problem must not be advertised as one.
    #[cfg(feature = "experimental-raw-hid")]
    #[test]
    fn hid_failures_carry_a_remedy_only_where_one_is_known() {
        let exclusive = hid_error_remedy("exclusive_access").expect("owned device has a remedy");
        assert!(
            exclusive.contains("restart") || exclusive.contains("replug"),
            "exclusive access is a transient ownership state, so the remedy is to make \
             the current holder let go, not to uninstall anything: {exclusive}",
        );
        assert!(
            !exclusive.contains("Input Monitoring"),
            "exclusive access is not a permission problem and must not send the reader \
             to re-grant a permission they already have: {exclusive}",
        );

        for denial in ["permission_denied", "not_permitted"] {
            let remedy = hid_error_remedy(denial).expect("a denial has a remedy");
            assert!(
                remedy.contains("Input Monitoring"),
                "{denial} is the permission gate: {remedy}",
            );
        }

        // No invented advice for failures whose cause is not established.
        assert_eq!(hid_error_remedy("open_failed"), None);
        assert_eq!(hid_error_remedy("open_failed_despite_grant"), None);
        assert_eq!(hid_error_remedy("matching_dictionary"), None);
    }

    #[cfg(feature = "experimental-raw-hid")]
    #[test]
    fn tcc_permission_lifecycle_is_deduplicated_per_process() {
        let deduper = PermissionEventDeduper::new();
        assert!(deduper.claim(LifecycleEventKind::PermissionDenied));
        assert!(!deduper.claim(LifecycleEventKind::PermissionDenied));
        assert!(deduper.claim(LifecycleEventKind::PermissionGranted));
        assert!(!deduper.claim(LifecycleEventKind::PermissionGranted));
    }

    #[test]
    fn match_layout_requires_server_region_input_v1_capability() {
        let mut options = transport_test_options(false);
        options.multi_monitor_topology = Some(
            crate::transport::multi_monitor::RequestedMultiMonitorSelection {
                topology: multi_monitor_test_topology(2),
                safe_area_policy: crate::protocol::messages::SafeAreaPolicyMsg::StandardFullscreen,
                full_color_display_ids: Vec::new(),
            },
        );
        let mut hello: ServerHelloMsg =
            serde_json::from_value(serde_json::json!({"type": SERVER_HELLO})).unwrap();
        assert!(validate_server_region_input(&options, &hello).is_err());

        hello.input_protocol_version = REGION_INPUT_PROTOCOL_VERSION;
        hello.input_capabilities.region_input = InputCapabilityAvailability::Available;
        assert!(validate_server_region_input(&options, &hello).is_ok());

        let legacy_options = transport_test_options(false);
        hello.input_protocol_version = 0;
        hello.input_capabilities.region_input = InputCapabilityAvailability::Unknown;
        assert!(validate_server_region_input(&legacy_options, &hello).is_ok());
    }

    #[test]
    fn reconnect_lifecycle_uses_controller_attempt_and_gap() {
        let attempt = ResumeAttempt {
            holder_nonce: "nonce".to_string(),
            grant: "grant".to_string(),
            previous_sid: "sid-1".to_string(),
            generation: 7,
            identity: crate::reconnect::ConnectionIdentity {
                endpoint: "wss://pier:18443".to_string(),
                security: "pin".to_string(),
                topology: "direct".to_string(),
            },
            attempt: 2,
            gap: Duration::from_millis(875),
        };
        let fields = reconnect_lifecycle_fields(&attempt);
        assert_eq!(
            fields.as_map().get("attempt"),
            Some(&FieldValue::Integer(2))
        );
        assert_eq!(
            fields.as_map().get("gap_ms"),
            Some(&FieldValue::Integer(875))
        );
    }

    #[test]
    fn heartbeat_transition_uses_real_value_and_recovery_duration() {
        let mut tracker = HealthTracker::default();
        let mut degraded_since_ms = None;
        let critical_sample = arcen_telemetry::QosSample {
            timestamp_ms: 1_000,
            heartbeat_misses: Some(3),
            ..arcen_telemetry::QosSample::default()
        };
        let critical = arcen_telemetry::assess_health(
            &critical_sample,
            &arcen_telemetry::QosTargets::default(),
        );
        assert!(health_transition_fields(
            &mut tracker,
            &mut degraded_since_ms,
            1_000,
            &critical,
            &critical_sample,
        )
        .is_none());
        let (kind, fields, _) = health_transition_fields(
            &mut tracker,
            &mut degraded_since_ms,
            2_000,
            &critical,
            &critical_sample,
        )
        .unwrap();
        assert_eq!(kind, LifecycleEventKind::HealthCritical);
        assert_eq!(
            fields.as_map().get("dominant_cause"),
            Some(&FieldValue::String("heartbeat".to_owned()))
        );
        assert_eq!(fields.as_map().get("value"), Some(&FieldValue::Integer(3)));
        assert_eq!(
            fields.as_map().get("threshold"),
            Some(&FieldValue::Integer(3))
        );

        let healthy_sample = arcen_telemetry::QosSample {
            timestamp_ms: 3_000,
            fps_actual: Some(60),
            fps_target: Some(60),
            frames_decoded: Some(300),
            frames_presented: Some(300),
            heartbeat_misses: Some(0),
            ..arcen_telemetry::QosSample::default()
        };
        let healthy = arcen_telemetry::assess_health(
            &healthy_sample,
            &arcen_telemetry::QosTargets::default(),
        );
        assert!(health_transition_fields(
            &mut tracker,
            &mut degraded_since_ms,
            3_000,
            &healthy,
            &healthy_sample,
        )
        .is_none());
        let (kind, fields, _) = health_transition_fields(
            &mut tracker,
            &mut degraded_since_ms,
            4_000,
            &healthy,
            &healthy_sample,
        )
        .unwrap();
        assert_eq!(kind, LifecycleEventKind::HealthOk);
        assert_eq!(
            fields.as_map().get("degraded_duration_ms"),
            Some(&FieldValue::Integer(2_000))
        );
    }

    #[test]
    fn client_session_end_contains_complete_aggregate_summary() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        telemetry.record_media(120, 120, 4, Duration::from_millis(2));
        for _ in 0..120 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        telemetry.record_rtt(Duration::from_millis(20));
        telemetry.record_rtt(Duration::from_millis(40));
        telemetry.record_reconnect();
        let _ = telemetry.snapshot(&mut window, None, 2_000, 60, 3);

        let fields = client_session_end_fields(&telemetry, Duration::from_secs(2), "user_quit");
        assert_eq!(
            fields.as_map().get("avg_fps"),
            Some(&FieldValue::Integer(60))
        );
        assert_eq!(
            fields.as_map().get("avg_rtt_ms"),
            Some(&FieldValue::Integer(30))
        );
        assert_eq!(
            fields.as_map().get("worst_health"),
            Some(&FieldValue::String("critical".to_owned()))
        );
        assert_eq!(
            fields.as_map().get("reconnects"),
            Some(&FieldValue::Integer(1))
        );
    }

    #[test]
    fn microphone_failure_control_is_typed_and_generation_bound() {
        let message = microphone_stop_message(17, MicrophoneStreamReason::CaptureFailure).unwrap();
        let Message::Text(text) = message else {
            panic!("microphone stop must be text");
        };
        let stop: MicrophoneStreamStopMsg = serde_json::from_str(text.as_ref()).unwrap();
        assert!(stop.is_valid());
        assert_eq!(stop.generation, 17);
        assert_eq!(stop.reason, MicrophoneStreamReason::CaptureFailure);
    }

    #[test]
    fn close_command_cancels_capture_before_transport_observes_command() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (cancellation, _) = SessionCancellation::new();
        cancellation.capture.lock().unwrap().generation = Some(Arc::clone(&cancel));
        let commands = SessionCommandSender {
            sender,
            cancellation: Arc::clone(&cancellation),
        };

        commands.send(SessionCommand::Close).unwrap();
        let deadline = cancellation.close_deadline();
        commands.send(SessionCommand::Close).unwrap();

        assert!(cancel.load(Ordering::Acquire));
        assert!(commands.session_closed());
        assert_eq!(deadline, cancellation.close_deadline());
        assert!(cancellation
            .capture
            .lock()
            .unwrap()
            .session_closed
            .load(Ordering::Acquire));
        assert!(matches!(receiver.try_recv(), Ok(SessionCommand::Close)));
    }

    #[test]
    fn microphone_wire_frame_masks_payload_and_zeros_plaintext_owner() {
        let mut payload = vec![0x10, 0x20, 0x30, 0x40, 0x50];
        let expected = payload.clone();
        let mask = [0x01, 0x02, 0x03, 0x04];
        let wire = masked_client_binary_frame(&mut payload, mask).unwrap();
        assert!(payload.iter().all(|byte| *byte == 0));
        assert_eq!(&wire[..6], &[0x82, 0x85, 1, 2, 3, 4]);
        let recovered = wire[6..]
            .iter()
            .enumerate()
            .map(|(index, byte)| *byte ^ mask[index % mask.len()])
            .collect::<Vec<_>>();
        assert_eq!(recovered, expected);
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn microphone_wire_frame_is_accepted_by_websocket_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut expected = encode_microphone_header(MicrophoneHeader {
            codec: crate::protocol::AudioCodec::Pcm,
            sequence: 1,
            timestamp_ms: 0,
            generation: 1,
        })
        .unwrap()
        .to_vec();
        expected.resize(
            arcen_protocol::MICROPHONE_HEADER_SIZE + arcen_protocol::MICROPHONE_PCM_BYTES,
            0x5a,
        );
        let server_expected = expected.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let Some(Ok(Message::Binary(payload))) = ws.next().await else {
                panic!("expected microphone binary frame");
            };
            assert_eq!(payload.as_ref(), server_expected);
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let mut ws = DirectSessionSocket::Wss(ws);
        let (cancellation, mut close_receiver) = SessionCancellation::new();
        send_microphone_bounded(
            &mut ws,
            Zeroizing::new(expected),
            &cancellation,
            &mut close_receiver,
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn quic_socket_uses_tls_and_raw_websocket_framing_end_to_end() {
        let key_pair = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.self_signed(&key_pair).unwrap();
        let certificate_der: CertificateDer<'static> = certificate.der().clone();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let mut rustls_config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate_der.clone()], private_key)
            .unwrap();
        rustls_config.alpn_protocols = vec![DIRECT_QUIC_ALPN_PROTOCOL.to_vec()];
        let quic_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_config).unwrap();
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
        server_config.transport_config(recommended_transport_config_arc(
            &BoundedTransportPolicy::default(),
        ));
        arcen_transport::quic::apply_direct_server_limits(&mut server_config);
        let server_endpoint =
            quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let port = server_endpoint.local_addr().unwrap().port();
        let server_endpoint_for_task = server_endpoint.clone();
        let server = tokio::spawn(async move {
            let incoming = server_endpoint_for_task.accept().await.unwrap();
            let connection = incoming.await.unwrap();
            let stream = arcen_transport::quic::accept_direct(connection)
                .await
                .unwrap();
            let mut ws = WebSocketStream::from_raw_socket(
                stream,
                Role::Server,
                Some(direct_websocket_config()),
            )
            .await;
            ws.send(Message::Text("pier-quic".into())).await.unwrap();
            assert_eq!(
                ws.next().await.unwrap().unwrap(),
                Message::Text("deck-quic".into())
            );
            let _ = ws.next().await;
        });

        let mut options = ConnectOptions {
            host: "localhost".to_string(),
            port,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::pinned_certificate(crate::transport::tls::fingerprint_sha256(
                certificate_der.as_ref(),
            )),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: false,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: true,
        };
        let mut ws = open_quic_socket(&options).await.unwrap();
        assert_eq!(
            ws.next().await.unwrap().unwrap(),
            Message::Text("pier-quic".into())
        );
        ws.send(Message::Text("deck-quic".into())).await.unwrap();
        ws.close(None).await.unwrap();
        options.password.zeroize();
        server.await.unwrap();
        server_endpoint.close(0_u32.into(), b"test complete");
        server_endpoint.wait_idle().await;
    }

    #[test]
    fn microphone_raw_write_flushes_tungstenite_buffer_first() {
        let source = include_str!("websocket.rs");
        let start = source.find("async fn send_microphone_bounded").unwrap();
        let rest = &source[start..];
        let body = &rest[..rest.find("\nasync fn send_setup_message").unwrap()];
        let tungstenite_flush = body.find("SinkExt::flush(ws)").unwrap();
        let raw_write = body.find("ws.write_raw").unwrap();
        assert!(tungstenite_flush < raw_write);
    }

    #[tokio::test]
    async fn queued_data_is_rejected_after_synchronous_close_latch() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(sender);
        let mut close_receiver = commands.cancellation.close_signal.subscribe();

        commands
            .send(SessionCommand::Json(serde_json::json!({"queued": true})))
            .unwrap();
        commands.send(SessionCommand::Close).unwrap();
        let SessionCommand::Json(value) = receiver.recv().await.unwrap() else {
            panic!("data command must remain first in the queue");
        };
        let mut sink = futures_util::sink::drain().sink_map_err(
            |never: std::convert::Infallible| -> tokio_tungstenite::tungstenite::Error {
                match never {}
            },
        );
        let result = send_bounded(
            &mut sink,
            Message::Text(value.to_string()),
            &commands.cancellation,
            &mut close_receiver,
        )
        .await;
        assert!(matches!(result, Err(ConnectSmokeError::SessionClosed)));
    }

    #[tokio::test]
    async fn close_latch_preempts_pending_setup_io() {
        let (cancellation, mut close_receiver) = SessionCancellation::new();
        cancellation.close();
        let result = cancellable_setup(
            std::future::pending::<Result<(), ConnectSmokeError>>(),
            &cancellation,
            &mut close_receiver,
        )
        .await;
        assert!(matches!(result, Err(ConnectSmokeError::SessionClosed)));
    }

    #[tokio::test]
    async fn pending_microphone_start_survives_losing_a_select_iteration() {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = release_rx.await;
            Err(MicrophoneStartFailure::Channel(
                MicrophoneStreamReason::CaptureFailure,
                "synthetic startup failure",
            ))
        });
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_slot = Arc::new(Mutex::new(CaptureCancelState {
            session_closed: Arc::new(AtomicBool::new(false)),
            generation: Some(Arc::clone(&cancel)),
        }));
        let lifecycle = Arc::new(AtomicBool::new(true));
        let mut pending = Some(PendingMicrophoneStart {
            cancel,
            cancel_slot,
            lifecycle: Arc::clone(&lifecycle),
            codec: crate::protocol::AudioCodec::Pcm,
            generation: 17,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            started_at: std::time::Instant::now(),
            task: Some(task),
            armed: true,
        });

        tokio::select! {
            biased;
            _ = next_microphone_start(&mut pending) => {
                panic!("blocked startup completed before the synthetic release")
            }
            _ = async {} => {}
        }
        assert!(pending.as_ref().is_some_and(|start| start.task.is_some()));

        release_tx.send(()).unwrap();
        assert!(matches!(
            next_microphone_start(&mut pending).await,
            Some(Err(MicrophoneStartFailure::Channel(
                MicrophoneStreamReason::CaptureFailure,
                "synthetic startup failure"
            )))
        ));
        assert!(!lifecycle.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn close_racing_successful_spawn_blocking_startup_joins_runtime() {
        let cancel = Arc::new(AtomicBool::new(false));
        let joined = Arc::new(AtomicBool::new(false));
        let capture_cancel = Arc::clone(&cancel);
        let capture_joined = Arc::clone(&joined);
        let capture_thread = std::thread::spawn(move || {
            while !capture_cancel.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            capture_joined.store(true, Ordering::Release);
        });
        let (capture, receiver) =
            crate::microphone::MicrophoneCapture::for_test(Arc::clone(&cancel), capture_thread);
        let (session_cancellation, _) = SessionCancellation::new();
        session_cancellation
            .microphone_lifecycle
            .store(true, Ordering::Release);
        session_cancellation.capture.lock().unwrap().generation = Some(Arc::clone(&cancel));
        let cancel_slot = Arc::clone(&session_cancellation.capture);
        let task_cancel = Arc::clone(&cancel);
        let task_slot = Arc::clone(&cancel_slot);
        let task_lifecycle = Arc::clone(&session_cancellation.microphone_lifecycle);
        let task = tokio::task::spawn_blocking(move || {
            Ok(UpstreamMicrophone {
                capture,
                receiver,
                cancel: task_cancel,
                cancel_slot: task_slot,
                lifecycle: task_lifecycle,
                encoder: None,
                codec: crate::protocol::AudioCodec::Pcm,
                generation: 23,
                opus: [0; arcen_media::audio::MAX_OPUS_PACKET_BYTES],
                pcm: [0; arcen_protocol::MICROPHONE_PCM_BYTES],
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
                stats: arcen_media::audio::MicrophoneStatsTracker::default(),
                started_at: std::time::Instant::now(),
                armed: true,
            })
        });
        let pending = PendingMicrophoneStart {
            cancel,
            cancel_slot,
            lifecycle: Arc::clone(&session_cancellation.microphone_lifecycle),
            codec: crate::protocol::AudioCodec::Pcm,
            generation: 23,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            started_at: std::time::Instant::now(),
            task: Some(task),
            armed: true,
        };
        let (sender, mut commands_rx) = mpsc::unbounded_channel();
        let commands = SessionCommandSender {
            sender,
            cancellation: session_cancellation,
        };

        commands.send(SessionCommand::Close).unwrap();
        pending
            .cancel_and_join(Some(commands.cancellation.close_deadline()), "manual")
            .await;

        assert!(joined.load(Ordering::Acquire));
        assert!(!commands.microphone_lifecycle_active());
        assert!(matches!(commands_rx.try_recv(), Ok(SessionCommand::Close)));
    }

    #[test]
    fn only_typed_capture_result_becomes_certificate_event() {
        let (tx, mut events) = mpsc::unbounded_channel();
        let info = untrusted_cert_info();
        publish_session_result(
            Err(ConnectSmokeError::CertificateUntrusted(info.clone())),
            &tx,
        );
        assert!(matches!(
            events.try_recv(),
            Ok(SessionEvent::CertificateUntrusted(captured)) if captured == info
        ));
        assert!(events.try_recv().is_err());

        publish_session_result(
            Err(ConnectSmokeError::Tls(
                TlsTrustError::InsecureDoubleGateMissing,
            )),
            &tx,
        );
        assert!(matches!(
            events.try_recv(),
            Ok(SessionEvent::Ended(SessionEnd {
                reason: DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity),
                ..
            }))
        ));
    }

    #[test]
    fn microphone_teardown_reason_is_typed_by_terminal_outcome() {
        assert_eq!(
            microphone_teardown_reason(&Ok(SessionEnd::manual())),
            "manual"
        );
        assert_eq!(
            microphone_teardown_reason(&Ok(SessionEnd::graceful_host())),
            "host_closed"
        );
        assert_eq!(
            microphone_teardown_reason(&Err(ConnectSmokeError::Timeout)),
            "timeout"
        );
        assert_eq!(
            microphone_teardown_reason(&Err(ConnectSmokeError::UnexpectedEof)),
            "unexpected_eof"
        );
        assert_eq!(
            microphone_teardown_reason(&Err(ConnectSmokeError::Microphone(
                "invalid microphone startup"
            ))),
            "protocol_failure"
        );
    }

    #[test]
    fn microphone_activation_and_close_are_one_serialized_transition() {
        let (cancellation, _) = SessionCancellation::new();
        let capture = Arc::clone(&cancellation.capture);
        let (active_entered, active_entered_rx) = std::sync::mpsc::channel();
        let (release_active, release_active_rx) = std::sync::mpsc::channel();
        let activation = std::thread::spawn(move || {
            let mut active = None;
            let result = activate_microphone_if_open(&capture, &mut active, 7_u8, || {
                active_entered.send(()).unwrap();
                release_active_rx.recv().unwrap();
            });
            (result, active)
        });
        active_entered_rx.recv().unwrap();
        let close = {
            let cancellation = Arc::clone(&cancellation);
            std::thread::spawn(move || cancellation.close())
        };
        std::thread::yield_now();
        assert!(!cancellation.is_closed());
        release_active.send(()).unwrap();
        assert_eq!(activation.join().unwrap(), (Ok(()), Some(7)));
        close.join().unwrap();
        assert!(cancellation.is_closed());

        let mut active = None;
        let callback_called = AtomicBool::new(false);
        assert_eq!(
            activate_microphone_if_open(&cancellation.capture, &mut active, 8_u8, || {
                callback_called.store(true, Ordering::Release);
            }),
            Err(8)
        );
        assert_eq!(active, None);
        assert!(!callback_called.load(Ordering::Acquire));
    }

    #[test]
    fn local_auth_hash_matches_python_contract() {
        assert_eq!(
            hash_password("secret", "challenge"),
            "ada2c96fa6369f7e33b8f4ec728133c80468ab52d21827349fed8bc89a15ff55"
        );
    }

    fn auth_request(methods: &[&str]) -> AuthRequest {
        AuthRequest {
            msg_type: AUTH_REQUEST.to_string(),
            auth_methods: methods.iter().map(|method| (*method).to_string()).collect(),
            challenge: "challenge".to_string(),
            salt: String::new(),
            auth_mode: Some("pam".to_string()),
            disclaimer: None,
            multi_monitor_v1: None,
        }
    }

    fn auth_result(grant: Option<&str>, window: Option<u32>, resumed: bool) -> AuthResult {
        AuthResult {
            msg_type: crate::protocol::messages::AUTH_RESULT.to_string(),
            success: true,
            message: String::new(),
            resume_grant: grant.map(str::to_string),
            resume_window_secs: window,
            resumed,
            error_code: None,
        }
    }

    #[test]
    fn initial_auth_opts_in_only_when_host_advertises_resume() {
        let auth = SessionAuth::InitialOptIn {
            holder_nonce: "nonce".to_string(),
        };
        let supported = apply_resume_opt_in(
            AuthResponse::pam("artist", "secret"),
            &auth_request(&["pam", "resume"]),
            &auth,
        );
        assert!(supported.resume_requested);
        assert_eq!(supported.resume_holder_nonce.as_deref(), Some("nonce"));

        let old_host = apply_resume_opt_in(
            AuthResponse::pam("artist", "secret"),
            &auth_request(&["pam"]),
            &auth,
        );
        assert!(!old_host.resume_requested);
        assert!(old_host.resume_holder_nonce.is_none());
    }

    #[test]
    fn validates_initial_and_resume_result_shapes() {
        let sid = CorrelationId::from_uuid_v4_bytes([7; 16]);
        let initial = validate_authentication_result(
            &SessionAuth::InitialOptIn {
                holder_nonce: "nonce".to_string(),
            },
            auth_result(Some("grant-1"), Some(30), false),
            &sid,
        )
        .unwrap()
        .unwrap();
        assert_eq!(initial.kind, AuthenticationKind::InitialOptIn);
        assert_eq!(initial.resume_window, Some(Duration::from_secs(30)));
        assert!(!format!("{initial:?}").contains("grant-1"));

        let attempt = ResumeAttempt {
            holder_nonce: "nonce".to_string(),
            grant: "grant-1".to_string(),
            previous_sid: "sid-1".to_string(),
            generation: 1,
            identity: crate::reconnect::ConnectionIdentity {
                endpoint: "wss://pier:18443".to_string(),
                security: "pin".to_string(),
                topology: "direct".to_string(),
            },
            attempt: 2,
            gap: Duration::from_millis(875),
        };
        let resumed = validate_authentication_result(
            &SessionAuth::Resume(attempt),
            auth_result(Some("grant-2"), Some(60), true),
            &sid,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resumed.kind, AuthenticationKind::Resume);

        let missing_successor = validate_authentication_result(
            &SessionAuth::Resume(ResumeAttempt {
                holder_nonce: "nonce".to_string(),
                grant: "grant-1".to_string(),
                previous_sid: "sid-1".to_string(),
                generation: 1,
                identity: crate::reconnect::ConnectionIdentity {
                    endpoint: "wss://pier:18443".to_string(),
                    security: "pin".to_string(),
                    topology: "direct".to_string(),
                },
                attempt: 2,
                gap: Duration::from_millis(875),
            }),
            auth_result(None, Some(60), true),
            &sid,
        );
        assert!(matches!(
            missing_successor,
            Err(ConnectSmokeError::ResumeProtocol(_))
        ));

        let empty_initial_grant = validate_authentication_result(
            &SessionAuth::InitialOptIn {
                holder_nonce: "nonce".to_string(),
            },
            auth_result(Some(""), Some(30), false),
            &sid,
        );
        assert!(matches!(
            empty_initial_grant,
            Err(ConnectSmokeError::ResumeProtocol(_))
        ));
    }

    #[test]
    fn validates_and_redacts_only_the_exact_refresh_shape() {
        let sid = CorrelationId::from_uuid_v4_bytes([8; 16]);
        let refresh = validate_authentication_refresh(
            auth_result(Some("grant-refresh"), Some(7_200), false),
            &sid,
        )
        .unwrap();
        assert_eq!(refresh.kind, AuthenticationKind::Refresh);
        assert_eq!(refresh.resume_window, Some(Duration::from_secs(7_200)));
        let debug = format!("{refresh:?}");
        assert!(!debug.contains("grant-refresh"));
        assert!(debug.contains("<redacted>"));

        let mut failure = auth_result(Some("grant"), Some(60), false);
        failure.success = false;
        let mut resumed = auth_result(Some("grant"), Some(60), true);
        resumed.resumed = true;
        let mut coded = auth_result(Some("grant"), Some(60), false);
        coded.error_code = Some(ResumeErrorCode::InternalFailure);
        let mut overlong_message = auth_result(Some("grant"), Some(60), false);
        overlong_message.message = "x".repeat(241);
        for malformed in [
            failure,
            resumed,
            coded,
            overlong_message,
            auth_result(None, Some(60), false),
            auth_result(Some(""), Some(60), false),
            auth_result(Some("grant"), None, false),
            auth_result(Some("grant"), Some(0), false),
            auth_result(Some("grant"), Some(7_201), false),
            auth_result(
                Some(&"x".repeat(MAX_RESUME_GRANT_BYTES + 1)),
                Some(60),
                false,
            ),
        ] {
            assert!(matches!(
                validate_authentication_refresh(malformed, &sid),
                Err(ConnectSmokeError::ResumeProtocol(_))
            ));
        }
    }

    #[test]
    fn classifies_only_transient_transport_io_for_retry() {
        use std::io::{Error as IoError, ErrorKind};
        use tokio_tungstenite::tungstenite::Error as WebSocketError;

        for error in [
            ConnectSmokeError::Timeout,
            ConnectSmokeError::UnexpectedEof,
            ConnectSmokeError::WebSocket(WebSocketError::Io(IoError::from(
                ErrorKind::ConnectionReset,
            ))),
            ConnectSmokeError::WebSocket(WebSocketError::Io(IoError::from(
                ErrorKind::ConnectionRefused,
            ))),
        ] {
            assert!(classify_disconnect(&error).transient(), "{error}");
        }

        for error in [
            ConnectSmokeError::AuthFailed("denied".to_string()),
            ConnectSmokeError::ResumeRejected {
                message: "expired".to_string(),
                error_code: Some(ResumeErrorCode::Expired),
            },
            ConnectSmokeError::ResumeProtocol("invalid shape"),
            ConnectSmokeError::ClosedByHost(String::new()),
            ConnectSmokeError::Tls(TlsTrustError::InsecureDoubleGateMissing),
        ] {
            assert!(!classify_disconnect(&error).transient(), "{error}");
        }
    }

    #[test]
    fn quic_connection_failures_preserve_resume_and_tls_classification() {
        for connection_error in [
            quinn::ConnectionError::TimedOut,
            quinn::ConnectionError::Reset,
        ] {
            let error = quic_connect_error(QuicTransportError::Connection(connection_error));
            assert!(classify_disconnect(&error).transient(), "{error}");
        }

        assert!(quic_transport_code_is_tls(
            quinn::TransportErrorCode::crypto(42)
        ));
        assert!(!quic_transport_code_is_tls(
            quinn::TransportErrorCode::NO_ERROR
        ));
        let tls_error = quic_tls_identity_error("certificate validation failed".to_string());
        assert_eq!(
            classify_disconnect(&tls_error).reason,
            DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity)
        );
    }

    #[test]
    fn quic_connect_error_maps_monitor_stream_timeout_like_establishment_timeout() {
        // The direct-monitor stream preface's own bounded deadline
        // (`MonitorStreamTimedOut`) must surface exactly like the primary
        // connection's `EstablishmentTimedOut`: a typed, transient
        // `ConnectSmokeError::Timeout`, never a generic protocol failure.
        for quic_error in [
            QuicTransportError::EstablishmentTimedOut,
            QuicTransportError::MonitorStreamTimedOut,
        ] {
            let mapped = quic_connect_error(quic_error);
            assert!(matches!(mapped, ConnectSmokeError::Timeout), "{mapped}");
            let end = classify_disconnect(&mapped);
            assert_eq!(
                end.reason,
                DisconnectReason::Transient(TransientTransportError::TimedOut)
            );
        }
    }

    #[test]
    fn quic_connect_error_maps_monitor_preface_rejection_like_direct_preface() {
        // The peer's direct-monitor stream preface (Carrier B) being
        // rejected as malformed or carrying an invalid session id must
        // classify identically to the primary connection's `DirectPreface`
        // rejection: a terminal protocol failure, never mistaken for a
        // transient network condition or a TLS/auth failure.
        for quic_error in [
            QuicTransportError::DirectPreface,
            QuicTransportError::MonitorPreface(MonitorStreamPrefaceError::Malformed),
            QuicTransportError::MonitorPreface(MonitorStreamPrefaceError::InvalidSessionId),
        ] {
            let message = quic_error.to_string();
            let mapped = quic_connect_error(quic_error);
            assert!(
                matches!(&mapped, ConnectSmokeError::WebSocket(WebSocketError::Io(_))),
                "{mapped}"
            );
            // The specific rejection reason must survive into the user-safe
            // message unchanged (only wrapped in the WebSocket/IO envelope),
            // never swallowed by a generic bucket string.
            assert!(
                mapped.to_string().contains(&message),
                "mapped message {:?} should contain original reason {:?}",
                mapped.to_string(),
                message
            );
            let end = classify_disconnect(&mapped);
            assert_eq!(
                end.reason,
                DisconnectReason::Terminal(TerminalDisconnect::Protocol)
            );
        }
    }

    #[test]
    fn quic_connect_error_exhaustive_mapping_never_confuses_monitor_and_primary_variants() {
        // A direct-monitor-stream failure must never be classified as a
        // primary-connection TLS, auth, or resume failure, and vice versa --
        // guarding the exhaustive match's grouping against future variant
        // additions being merged into the wrong bucket.
        let monitor_timeout = quic_connect_error(QuicTransportError::MonitorStreamTimedOut);
        assert_ne!(
            classify_disconnect(&monitor_timeout).reason,
            DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity)
        );
        assert_ne!(
            classify_disconnect(&monitor_timeout).reason,
            DisconnectReason::Terminal(TerminalDisconnect::Authentication)
        );

        let monitor_preface = quic_connect_error(QuicTransportError::MonitorPreface(
            MonitorStreamPrefaceError::Malformed,
        ));
        assert_ne!(
            classify_disconnect(&monitor_preface).reason,
            DisconnectReason::Transient(TransientTransportError::TimedOut)
        );
        assert_ne!(
            classify_disconnect(&monitor_preface).reason,
            DisconnectReason::Terminal(TerminalDisconnect::TlsIdentity)
        );
    }

    #[test]
    fn raw_tcp_reset_without_close_handshake_is_transient_unexpected_eof() {
        use tokio_tungstenite::tungstenite::{error::ProtocolError, Error as WebSocketError};

        let observed_at = Instant::now();
        let end = classify_disconnect_at(
            &ConnectSmokeError::WebSocket(WebSocketError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake,
            )),
            observed_at,
        );
        assert_eq!(
            end.reason,
            DisconnectReason::Transient(TransientTransportError::UnexpectedEof)
        );
        assert_eq!(end.observed_at, observed_at);
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn builds_ws_and_wss_uris() {
        let mut options = ConnectOptions {
            host: "example.test".to_string(),
            port: 4443,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(1),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        };
        assert_eq!(options.uri().unwrap().as_str(), "ws://example.test:4443/");
        options.use_tls = true;
        assert_eq!(options.uri().unwrap().as_str(), "wss://example.test:4443/");
    }

    #[test]
    fn connect_options_debug_redacts_password() {
        let options = ConnectOptions {
            host: "example.test".to_string(),
            port: 4443,
            use_tls: true,
            username: "automation".to_string(),
            password: "dummy-password".to_string(),
            timeout: Duration::from_secs(1),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        };
        let debug = format!("{options:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("dummy-password"));
    }

    #[test]
    fn hard_usb_capability_does_not_depend_on_local_typed_pen_setting() {
        assert_eq!(
            macos_tablet_mode_capabilities(false).wacom_usb_bridge,
            if cfg!(feature = "usb-hard-lab") {
                InputCapabilityAvailability::Available
            } else {
                InputCapabilityAvailability::Unavailable
            }
        );
    }

    #[test]
    fn auth_and_hello_use_the_same_captured_timezone() {
        let options = ConnectOptions {
            host: "example.test".to_string(),
            port: 4443,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(1),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: Some("Europe/Oslo".to_string()),
            cursor_preference: CursorMode::Host,
            clipboard_enabled: true,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        };
        let auth = auth_response_with_metadata(AuthResponse::none(), &options);
        let hello =
            client_hello_with_metadata(&options, &CorrelationId::from_uuid_v4_bytes([0; 16]));

        assert_eq!(auth.timezone, options.timezone);
        assert_eq!(hello.timezone, auth.timezone);
        assert_eq!(auth.cursor_preference, CursorMode::Host);
        let initial_video = auth.initial_video.expect("auth-time video request");
        assert_eq!(
            initial_video.quality,
            rust_viewer_quality_settings(&options.profile)
        );
        assert_eq!(
            initial_video.capabilities.av1, hello.supports_av1,
            "auth-time and ClientHello codec claims come from the same cached probe"
        );
        assert_eq!(hello.cursor_preference, auth.cursor_preference);
    }

    #[test]
    fn clipboard_capability_requires_local_opt_in_and_exact_host_v1() {
        let options = ConnectOptions {
            host: "example.test".to_string(),
            port: 4443,
            use_tls: true,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(1),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: true,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        };
        let hello =
            client_hello_with_metadata(&options, &CorrelationId::from_uuid_v4_bytes([0; 16]));
        assert_eq!(hello.clipboard_protocol_version, CLIPBOARD_PROTOCOL_VERSION);
        assert!(hello.clipboard_text_c2s && hello.clipboard_image_s2c);

        let mut server: ServerHelloMsg = serde_json::from_value(serde_json::json!({
            "type": SERVER_HELLO,
            "clipboard": {
                "protocol_version": CLIPBOARD_PROTOCOL_VERSION,
                "direction": "both",
                "content": "all",
                "max_bytes": 1024
            }
        }))
        .unwrap();
        assert!(negotiate_clipboard(&server, true).is_some());
        assert!(negotiate_clipboard(&server, false).is_none());
        server.clipboard.as_mut().unwrap().protocol_version = 0;
        assert!(negotiate_clipboard(&server, true).is_none());
    }

    #[test]
    fn outbound_clipboard_schedules_offer_then_one_chunk_per_turn() {
        let policy = ClipboardPolicyMsg {
            protocol_version: CLIPBOARD_PROTOCOL_VERSION,
            direction: arcen_protocol::messages::ClipboardDirectionMsg::Both,
            content: arcen_protocol::messages::ClipboardContentMsg::All,
            max_bytes: u32::try_from(CHUNK_BYTES + 1).unwrap(),
        };
        let item = ClipboardItem::new(
            4,
            ClipboardContentKind::TextUtf8,
            vec![b'x'; CHUNK_BYTES + 1],
            false,
        )
        .unwrap();
        let mut transfer = OutboundClipboardTransfer::new(item, policy).unwrap();
        assert!(matches!(transfer.next_message().unwrap(), Message::Text(_)));
        let Message::Binary(first) = transfer.next_message().unwrap() else {
            panic!("first payload turn must be binary");
        };
        assert_eq!(
            first.len(),
            crate::protocol::CLIPBOARD_HEADER_SIZE + CHUNK_BYTES
        );
        assert!(!transfer.is_finished());
        let Message::Binary(second) = transfer.next_message().unwrap() else {
            panic!("second payload turn must be binary");
        };
        assert_eq!(second.len(), crate::protocol::CLIPBOARD_HEADER_SIZE + 1);
        assert!(transfer.is_finished());
    }

    #[test]
    fn auth_submission_explicitly_clears_password_storage() {
        let mut submission = AuthSubmission {
            username: "operator".to_string(),
            password: "dummy-password".to_string(),
        };
        submission.clear_sensitive();
        assert!(submission.password.is_empty());
    }

    #[test]
    fn rust_viewer_quality_caps_native_stream_for_websocket_mvp() {
        let quality = rust_viewer_quality_settings(&StreamProfile::default());
        assert_eq!(quality.max_fps, 15);
        assert_eq!(quality.chroma, "yuv420");
        assert_eq!(quality.codec, "h264");
        assert_eq!(quality.bit_depth, "8");
        assert_eq!(quality.color_range, "limited");
        assert_eq!(quality.color_matrix, "bt709");
        assert_eq!(
            quality.encode_intent,
            arcen_media::EncodeIntent::Interactive.token(),
            "the MVP profile must stay latency-first; grading intent is only ever opt-in"
        );
        assert!(quality.enable_audio);
    }

    #[test]
    fn stream_profile_reaches_quality_settings() {
        let quality = rust_viewer_quality_settings(&StreamProfile {
            codec: "h265".to_string(),
            chroma: "yuv444".to_string(),
            video_selection: VideoSelectionIntent::ColorFidelity,
            max_fps: 60,
            bit_depth: "10".to_string(),
            color_range: "full".to_string(),
            color_matrix: "bt709".to_string(),
            transfer: "bt709".to_string(),
            color_primaries: "bt709".to_string(),
            encode_intent: "quality".to_string(),
        });
        assert_eq!(quality.codec, "h265");
        assert_eq!(quality.chroma, "yuv444");
        assert_eq!(quality.max_fps, 60);
        assert_eq!(quality.bit_depth, "10");
        assert_eq!(quality.color_range, "full");
        assert_eq!(quality.color_matrix, "bt709");
        assert_eq!(quality.video_selection, VideoSelectionIntent::ColorFidelity);
        assert_eq!(
            quality.encode_intent, "quality",
            "the requested intent must reach the wire verbatim -- the host reads this token to \
             choose its encoder preset and buffering"
        );
    }

    #[test]
    fn full_frame_gate_retries_without_storming_until_keyframe_ack() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let commands = SessionCommandSender::for_test(sender);
        let mut gate = FullFrameRequestGate::with_interval(Duration::from_millis(10));
        gate.request();
        assert!(gate.send_due(&commands));
        assert!(!gate.send_due(&commands));
        assert!(gate.is_pending());
        std::thread::sleep(Duration::from_millis(15));
        assert!(gate.send_due(&commands));
        assert!(!gate.send_due(&commands));
        assert!(gate.is_pending());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());

        gate.cancel_pending();
        std::thread::sleep(Duration::from_millis(15));
        assert!(!gate.send_due(&commands));
        assert!(!gate.is_pending());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn client_retry_interval_exceeds_host_throttle() {
        assert!(FULL_FRAME_RETRY_INTERVAL > Duration::from_millis(500));
    }

    #[test]
    fn rejects_oversized_control_before_json_allocation() {
        assert!(control_size_allowed(MAX_INCOMING_CONTROL_SIZE));
        assert!(!control_size_allowed(MAX_INCOMING_CONTROL_SIZE + 1));
    }

    #[test]
    fn rejects_disclaimer_control_characters() {
        let request: AuthRequest = serde_json::from_value(serde_json::json!({
            "type": AUTH_REQUEST,
            "auth_methods": ["pam"],
            "challenge": "",
            "salt": "",
            "auth_mode": "pam",
            "disclaimer": "Authorized use only.\n"
        }))
        .unwrap();
        assert!(matches!(
            validate_disclaimer(&request),
            Err(ConnectSmokeError::InvalidDisclaimer(
                "text contains control characters"
            ))
        ));
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn connect_smoke_receives_server_hello_and_sends_client_hello() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": SERVER_HELLO,
                    "server_name": "Mock Arcen",
                    "version": "3.0.0",
                    "codec": "h264"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            let Some(Ok(Message::Text(text))) = ws.next().await else {
                panic!("expected client_hello text frame");
            };
            let json: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(msg_type(&json), Some(CLIENT_HELLO));
            assert_eq!(json.get("capture_mode").unwrap(), "mirror_all");
            let sid = json
                .get("session_log_id")
                .and_then(Value::as_str)
                .expect("Deck sends a session log id");
            CorrelationId::parse_uuid(sid).expect("Deck sends a canonical UUID");
            let network = json
                .get("network_snapshot")
                .expect("smoke hello carries the selected local path");
            assert_eq!(network.get("interface_kind").unwrap(), "loopback");
            assert_eq!(network.get("scope").unwrap(), "lan");
            assert!(network.get("ssid").is_none());
            let Some(Ok(Message::Text(text))) = ws.next().await else {
                panic!("expected quality_settings text frame");
            };
            let json: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(json.get("type").unwrap(), "quality_settings");
            assert_eq!(json.get("max_fps").unwrap(), 15);
        });

        let result = connect_smoke(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        })
        .await
        .unwrap();

        assert_eq!(result.fsm_state, "streaming");
        assert_eq!(result.server_hello.unwrap().server_name, "Mock Arcen");
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn spawn_session_emits_server_hello_event() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": SERVER_HELLO,
                    "server_name": "Session Mock",
                    "version": "3.0.0",
                    "codec": "h264"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let _client_hello = ws.next().await;
            let _quality_settings = ws.next().await;
        });

        let mut handle = spawn_session(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        });

        let event = timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            SessionEvent::ServerHello(hello) => assert_eq!(hello.server_name, "Session Mock"),
            other => panic!("unexpected session event: {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn streaming_refresh_is_typed_and_does_not_wait_for_acceptance() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": AUTH_REQUEST,
                    "auth_methods": ["none", "resume"],
                    "challenge": "",
                    "salt": "",
                    "auth_mode": "none"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let Some(Ok(Message::Text(response))) = ws.next().await else {
                panic!("expected auth response");
            };
            let response: AuthResponse = serde_json::from_str(&response).unwrap();
            assert!(response.resume_requested);

            ws.send(Message::Text(
                serde_json::to_string(&auth_result(Some("grant-1"), Some(60), false))
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": SERVER_HELLO,
                    "server_name": "Refresh Mock",
                    "version": "3.0.0",
                    "codec": "h264"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let _client_hello = ws.next().await;
            let _quality_settings = ws.next().await;
            ws.send(Message::Text(
                serde_json::to_string(&auth_result(Some("grant-2"), Some(60), false))
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                serde_json::json!({"type": "after_refresh"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        });

        let mut handle = spawn_session_opt_in(
            ConnectOptions {
                host: "127.0.0.1".to_string(),
                port,
                use_tls: false,
                username: String::new(),
                password: String::new(),
                timeout: Duration::from_secs(5),
                tls: TlsTrustConfig::default(),
                profile: StreamProfile::default(),
                monitors: Vec::new(),
                displays_mode: String::new(),
                multi_monitor_topology: None,
                replace_incompatible_desktop: false,
                timezone: None,
                cursor_preference: CursorMode::Local,
                clipboard_enabled: false,
                microphone_enabled: false,
                tablet_input_enabled: true,
                tablet_mode_requested: TabletModeMsg::LocalTermination,
                telemetry: Arc::new(ClientTelemetry::default()),
                quic_enabled: false,
            },
            "03".repeat(32),
        );

        let initial = timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            initial,
            SessionEvent::Authenticated(SessionAuthentication {
                kind: AuthenticationKind::InitialOptIn,
                ..
            })
        ));
        handle
            .commands
            .send(SessionCommand::AcceptAuthentication)
            .unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(5), handle.events.recv())
                .await
                .unwrap()
                .unwrap(),
            SessionEvent::ServerHello(_)
        ));
        let refresh = timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            refresh,
            SessionEvent::Authenticated(SessionAuthentication {
                kind: AuthenticationKind::Refresh,
                resume_window: Some(window),
                ..
            }) if window == Duration::from_secs(60)
        ));
        assert!(matches!(
            timeout(Duration::from_secs(5), handle.events.recv())
                .await
                .unwrap()
                .unwrap(),
            SessionEvent::Json(value) if msg_type(&value) == Some("after_refresh")
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn disclaimer_waits_for_decision_then_sends_exact_digest() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (decision_ready_tx, decision_ready_rx) = tokio::sync::oneshot::channel();
        let disclaimer = "Authorized use only. Exact text.";
        let expected_digest = disclaimer_digest(disclaimer);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": AUTH_REQUEST,
                    "auth_methods": ["pam"],
                    "challenge": "",
                    "salt": "",
                    "auth_mode": "pam",
                    "disclaimer": disclaimer
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();

            assert!(
                timeout(Duration::from_millis(100), ws.next())
                    .await
                    .is_err(),
                "Deck must not send an auth frame before the user decides"
            );
            decision_ready_tx.send(()).unwrap();

            let Some(Ok(Message::Text(text))) = ws.next().await else {
                panic!("expected auth_response");
            };
            let response: AuthResponse = serde_json::from_str(&text).unwrap();
            assert_eq!(
                response.disclaimer_acceptance_sha256.as_deref(),
                Some(expected_digest.as_str())
            );
            assert_eq!(response.username, "operator");
            assert_eq!(response.credential, "dummy-password");

            ws.send(Message::Text(
                serde_json::json!({
                    "type": "auth_result",
                    "success": true,
                    "message": "Authenticated"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": SERVER_HELLO,
                    "server_name": "Disclaimer Mock",
                    "version": "3.0.0",
                    "codec": "h264"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let _client_hello = ws.next().await;
            let _quality = ws.next().await;
        });

        let mut handle = spawn_session(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        });
        let auth_request = timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        match auth_request {
            SessionEvent::AuthRequired(request) => {
                assert_eq!(request.disclaimer.as_deref(), Some(disclaimer));
            }
            other => panic!("unexpected session event: {other:?}"),
        }
        decision_ready_rx.await.unwrap();
        handle
            .commands
            .send(SessionCommand::SubmitAuth(AuthSubmission {
                username: "operator".to_string(),
                password: "dummy-password".to_string(),
            }))
            .unwrap();
        let hello = timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(hello, SessionEvent::ServerHello(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn declining_disclaimer_sends_no_auth_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (decision_ready_tx, decision_ready_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": AUTH_REQUEST,
                    "auth_methods": ["pam"],
                    "challenge": "",
                    "salt": "",
                    "auth_mode": "pam",
                    "disclaimer": "Authorized use only."
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            assert!(timeout(Duration::from_millis(100), ws.next())
                .await
                .is_err());
            decision_ready_tx.send(()).unwrap();
            match ws.next().await {
                Some(Ok(Message::Close(_))) | None => {}
                Some(Ok(Message::Text(_))) => panic!("decline must not send auth_response"),
                other => panic!("unexpected frame after decline: {other:?}"),
            }
        });

        let mut handle = spawn_session(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        });
        assert!(matches!(
            timeout(Duration::from_secs(5), handle.events.recv())
                .await
                .unwrap()
                .unwrap(),
            SessionEvent::AuthRequired(_)
        ));
        decision_ready_rx.await.unwrap();
        handle
            .commands
            .send(SessionCommand::DeclineDisclaimer)
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn connect_smoke_fails_instead_of_accepting_disclaimer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": AUTH_REQUEST,
                    "auth_methods": ["pam"],
                    "challenge": "",
                    "salt": "",
                    "auth_mode": "pam",
                    "disclaimer": "Authorized use only."
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            assert!(
                !matches!(ws.next().await, Some(Ok(Message::Text(_)))),
                "smoke client must not auto-accept"
            );
        });

        let error = connect_smoke(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: "operator".to_string(),
            password: "dummy-password".to_string(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        })
        .await
        .unwrap_err();
        assert!(matches!(error, ConnectSmokeError::DisclaimerRequired));
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn session_command_sends_json_to_server() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": SERVER_HELLO,
                    "server_name": "Command Mock",
                    "version": "3.0.0",
                    "codec": "h264"
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
            let _client_hello = ws.next().await.unwrap().unwrap();
            let quality = ws.next().await.unwrap().unwrap();
            match quality {
                Message::Text(text) => {
                    let json: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(json.get("type").unwrap(), "quality_settings");
                }
                other => panic!("unexpected quality frame: {other:?}"),
            }
            let command = ws.next().await.unwrap().unwrap();
            match command {
                Message::Text(text) => {
                    let json: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(json.get("type").unwrap(), "request_full_frame");
                }
                other => panic!("unexpected command frame: {other:?}"),
            }
        });

        let mut handle = spawn_session(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        });
        let _ = timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        handle
            .commands
            .send(SessionCommand::Json(serde_json::json!({
                "type": "request_full_frame"
            })))
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    #[cfg(feature = "wss-compat")]
    async fn media_flood_stays_bounded_without_blocking_controls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({
                    "type": SERVER_HELLO,
                    "server_name": "Flood Mock",
                    "version": "3.0.0",
                    "codec": "h264"
                })
                .to_string(),
            ))
            .await
            .unwrap();
            let _client_hello = ws.next().await.unwrap().unwrap();
            let _quality_settings = ws.next().await.unwrap().unwrap();

            for timestamp_ms in 0..10 {
                let mut bytes = encode_video_header(VideoHeader {
                    frame_type: FrameType::VideoH264,
                    codec: VideoCodec::H264,
                    chroma: ChromaSubsampling::Yuv420,
                    flags: u8::from(timestamp_ms == 0),
                    timestamp_ms,
                    monitor_id: 0,
                    topology_generation: 0,
                    stream_epoch: 0,
                })
                .to_vec();
                if timestamp_ms == 0 {
                    bytes.resize(bytes.len() + 17 * 1024 * 1024, 1);
                } else {
                    bytes.extend_from_slice(&[1, 2, 3]);
                }
                ws.send(Message::Binary(bytes)).await.unwrap();
            }
            ws.send(Message::Binary(vec![0xff])).await.unwrap();
            ws.send(Message::Text(
                serde_json::json!({"type": "health_after_flood"}).to_string(),
            ))
            .await
            .unwrap();
            ws.send(Message::Close(None)).await.unwrap();
            let _ = timeout(Duration::from_secs(2), ws.next()).await;
        });

        let mut handle = spawn_session(ConnectOptions {
            host: "127.0.0.1".to_string(),
            port,
            use_tls: false,
            username: String::new(),
            password: String::new(),
            timeout: Duration::from_secs(5),
            tls: TlsTrustConfig::default(),
            profile: StreamProfile::default(),
            monitors: Vec::new(),
            displays_mode: String::new(),
            multi_monitor_topology: None,
            replace_incompatible_desktop: false,
            timezone: None,
            cursor_preference: CursorMode::Local,
            clipboard_enabled: false,
            microphone_enabled: false,
            tablet_input_enabled: true,
            tablet_mode_requested: TabletModeMsg::LocalTermination,
            telemetry: Arc::new(ClientTelemetry::default()),
            quic_enabled: false,
        });

        let mut media_ready = 0;
        let mut health_after_flood = false;
        loop {
            let event = timeout(Duration::from_secs(5), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                SessionEvent::MediaReady => media_ready += 1,
                SessionEvent::Json(value) => {
                    health_after_flood |= msg_type(&value) == Some("health_after_flood");
                }
                SessionEvent::Ended(_) => break,
                SessionEvent::ServerHello(_)
                | SessionEvent::CertificateUntrusted(_)
                | SessionEvent::AuthRequired(_)
                | SessionEvent::Authenticated(_)
                | SessionEvent::MicrophoneActive(_)
                | SessionEvent::BrokerHello(_) => {}
            }
        }

        let snapshot = handle.media.snapshot();
        assert_eq!(media_ready, 1);
        assert!(health_after_flood);
        assert!(snapshot.video_depth <= crate::pipeline::frame_queue::VIDEO_PACKET_LIMIT);
        assert!(snapshot.video_bytes <= crate::pipeline::frame_queue::VIDEO_BYTE_LIMIT);
        let batch = handle.media.take_batch();
        assert!(batch.video.is_empty(), "overflow must clear the chain");
        assert!(batch.video_discontinuity);
        assert!(batch.idr_needed);
        assert_eq!(batch.telemetry.video_loss_epochs, 1);
        assert_eq!(batch.telemetry.malformed_packets, 1);
        assert!(batch.malformed_error.is_some());
        server.await.unwrap();
    }
}
