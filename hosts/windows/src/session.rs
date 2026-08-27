use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::{RwLock, RwLockReadGuard};
use std::time::Duration;

use arcen_identity::{DisclaimerAcceptance, PreparedDisclaimer};
use arcen_input::{
    mutual_capability, negotiate_tablet_mode, CapabilityAvailability as InputCapabilityTruth,
    InputSequenceTracker, TabletMode as InputTabletMode,
};
use arcen_media::audio::{
    AudioPolicy, AudioProtocolMode, ConfiguredAudioPolicy, MicrophonePolicy, OpusEncoder,
    ResolvedAudioStream, MAX_OPUS_PACKET_BYTES,
};
use arcen_media::clipboard::ClipboardFlow;
use arcen_media::video::{
    color_contract_is_servable, resolve_client_color_request_with_matrix_caps, ClientColorRequest,
    ColorCeiling, ColorMatrixCapabilities, EncoderBackend, ResolvedMediaPlan,
};
use arcen_media::{BitDepth, ColorMatrix, ColorRange, EncodeIntent};
use arcen_outputs::{FairRoster, RosterError};
use arcen_protocol::fsm::ServerState;
use arcen_protocol::messages::{
    msg_type, supports_region_input_v1, AudioStreamResultMsg, AuthMultiMonitorOfferMsg,
    AuthRequest, AuthResponse, AuthResult, ClientHelloMsg, CursorMode, CursorModeReason,
    CursorModeResultMsg, DisplayUpdateMsg, DisplayUpdateResultMsg, HealthPingMsg, HealthPongMsg,
    HealthStatsMsg, InputCapabilitiesMsg, InputCapabilityAvailability, KeyEventMsg,
    KeyResetModifiersMsg, MicrophoneStreamStopMsg, MouseButtonMsg, MouseMoveMsg,
    MouseMoveRelativeMsg, MouseScrollMsg, MultiMonitorCarrierMsg, PenEventMsg, QualitySettings,
    RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg, RegionPointerEnterMsg,
    RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg, RequestFullFrameMsg,
    ResumeErrorCode, ServerColorCaps, ServerHelloMsg, ServerMultiMonitorMsg,
    TabletModeCapabilitiesMsg, TabletModeMsg, TabletModeReason, TabletModeResultMsg, TextCommitMsg,
    AUTH_METHOD_RESUME, AUTH_REQUEST, AUTH_RESPONSE, AUTH_RESULT, CLIENT_HELLO, CLIPBOARD_DATA,
    DISPLAY_UPDATE, HEALTH_PING, HEALTH_PONG, HEALTH_STATS, INPUT_PROTOCOL_VERSION,
    KEY_RESET_MODIFIERS, MICROPHONE_STREAM_STOP, MOUSE_MOVE_RELATIVE, MOUSE_SCROLL, PEN_EVENT,
    REGION_INPUT_PROTOCOL_VERSION, REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER,
    REGION_POINTER_LEAVE, REGION_POINTER_MOTION, REGION_POINTER_SCROLL, REQUEST_FULL_FRAME,
    SERVER_HELLO, TEXT_COMMIT,
};
#[cfg(feature = "wss-compat")]
use arcen_protocol::CAPABILITY_TRANSPORT_WSS;
use arcen_protocol::{
    decode_clipboard_chunk, encode_audio_header, encode_video_header, negotiate_transport,
    sanitize_transport_capabilities, AudioCodec, AudioHeader, ChromaSubsampling, FrameType,
    VideoCodec, VideoHeader, CAPABILITY_TRANSPORT_QUIC,
};
use arcen_session::deskside::{
    DesksideControl, DesksideEffect, DesksideEvent, DesksideLeaseSpec, DesksideProtection,
};
use arcen_session::direct_reconnect::{
    DirectReconnect, ReconnectEvent, ReconnectPolicy, ReconnectState,
};
use arcen_session::restore_lease::{LeaseOwnerId, StateFingerprint};
use arcen_telemetry::{CorrelationId, FieldValue, LifecycleEventKind, StructuredFields};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch, Notify, OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::Instrument;
use zeroize::Zeroize;

use crate::audio::{AudioCapture, AudioPacket, AudioTelemetry, AudioTelemetrySnapshot};
use crate::capenc::{Capenc, CapencConfig, CapencStartError};
use crate::clipboard::{
    ClipboardItem, ClipboardNegotiation, ClipboardWriterQueue, WindowsClipboardRuntime,
};
use crate::display::{
    DisplayLease, DisplayManager, DisplayReport, DisplayRequest, DisplaySize, MultiDisplayLease,
};
use crate::input::{Injector, PenInjector};
use crate::latest::{LatestQueue, VideoPushResult, VideoQueue};
use crate::logging::{AUDIO, CAPENC, DISPLAY, INPUT, SESSION};
use crate::multi_monitor_input::RegionInputAdapter;
use crate::windows_session::{
    AgentAttachmentAction, AgentAttachmentCommand, AgentAttachmentState, AgentAttachmentStatus,
    AgentControl, AgentReady, AgentStart, AgentStreamingReady, WindowsSessionIdentity,
};
use crate::{HostConfig, LifecycleEmitter};

const SERVER_NAME: &str = "Arcen Windows Host (Rust)";
const AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const INTERACTIVE_AUTH_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const AUTH_VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const QUALITY_SETTINGS_TIMEOUT: Duration = Duration::from_secs(15);
const DISPLAY_UPDATE_MIN_INTERVAL: Duration = Duration::from_secs(1);
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(6);
const CONTROL_QUEUE_CAPACITY: usize = 8;
const VIDEO_QUEUE_CAPACITY: usize = 4;
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(10);
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(30);
const AGENT_ATTACHMENT_TIMEOUT: Duration = Duration::from_secs(15);
const MICROPHONE_STATS_HEALTH_TICKS: u64 =
    arcen_media::audio::MICROPHONE_STATS_INTERVAL.as_secs() / 2;
const MAX_QUEUED_ATTACHMENT_FRAMES: usize = 256;
const INTERNAL_MAX_MESSAGE: usize = 64 * 1024 * 1024;
const AGENT_ERROR_TYPE: &str = "windows_session_agent_error";

#[derive(Debug, Default)]
struct RateLimitedMicrophoneWarning {
    last_emitted: Option<std::time::Instant>,
    suppressed: u64,
}

impl RateLimitedMicrophoneWarning {
    fn observe(&mut self) -> Option<u64> {
        self.observe_at(std::time::Instant::now())
    }

    fn observe_at(&mut self, now: std::time::Instant) -> Option<u64> {
        if self.last_emitted.is_none_or(|last| {
            now.duration_since(last) >= arcen_media::audio::MICROPHONE_STATS_INTERVAL
        }) {
            self.last_emitted = Some(now);
            return Some(std::mem::take(&mut self.suppressed));
        }
        self.suppressed = self.suppressed.saturating_add(1);
        None
    }
}

/// Machine-broker ownership for the single mutable Windows display/media plane.
///
/// This permit is acquired before a session agent is spawned and remains owned
/// by the broker until that agent has exited. The agent's own DisplayManager
/// semaphore is defense-in-depth only because process-local child semaphores
/// cannot serialize sibling agents.
pub struct BrokerAgentLease {
    slot: Arc<Semaphore>,
    admission: Arc<crate::session_admission::SessionAdmissionRuntime>,
    controls: watch::Sender<AgentControl>,
    active_log: RwLock<Option<ActiveAgentLogRegistration>>,
}

struct BrokerAgentPermit {
    admission_runtime: Arc<crate::session_admission::SessionAdmissionRuntime>,
    admission: Option<crate::session_admission::SessionAdmissionLease>,
    _display: OwnedSemaphorePermit,
}

impl std::fmt::Debug for BrokerAgentPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BrokerAgentPermit(<redacted>)")
    }
}

impl Drop for BrokerAgentPermit {
    fn drop(&mut self) {
        if let Some(lease) = self.admission.take() {
            self.admission_runtime.complete(lease);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachmentError {
    Stream(String),
    FatalCleanup(String),
}

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stream(error) => formatter.write_str(error),
            Self::FatalCleanup(error) => write!(formatter, "fatal attachment cleanup: {error}"),
        }
    }
}

impl From<String> for AttachmentError {
    fn from(error: String) -> Self {
        Self::Stream(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttachmentDisposition {
    Finish(Result<(), String>),
    Reattach,
    Terminate(String),
}

fn attachment_disposition(
    detached: bool,
    result: Result<(), AttachmentError>,
) -> AttachmentDisposition {
    match result {
        Err(AttachmentError::FatalCleanup(error)) => AttachmentDisposition::Terminate(error),
        Err(AttachmentError::Stream(error)) => AttachmentDisposition::Finish(Err(error)),
        Ok(()) if detached => AttachmentDisposition::Reattach,
        Ok(()) => AttachmentDisposition::Finish(Ok(())),
    }
}

pub(crate) struct ActiveAgentLogRegistration {
    pub(crate) path: std::path::PathBuf,
    pub(crate) user_sid: String,
    pub(crate) ready: bool,
}

async fn shutdown_microphone(
    microphone: &mut Option<
        crate::microphone_input::MicrophoneIngress<crate::microphone_input::NativeMicrophoneDevice>,
    >,
    stop_reason: &'static str,
) -> Result<(), AttachmentError> {
    take_and_stop_microphone(microphone, |mut microphone| async move {
        microphone.shutdown_wait(stop_reason).await
    })
    .await
    .map_err(|error| AttachmentError::FatalCleanup(format!("microphone cleanup failed: {error:?}")))
}

fn microphone_failure_event(
    error: crate::microphone_input::MicrophoneIngressError,
) -> &'static str {
    match error {
        crate::microphone_input::MicrophoneIngressError::Device(error) => {
            microphone_device_failure_event(error)
        }
        _ => "mic_windows_feeder_failure",
    }
}

fn microphone_device_failure_event(
    error: crate::microphone_input::MicrophoneDeviceError,
) -> &'static str {
    match error {
        crate::microphone_input::MicrophoneDeviceError::Timeout => "mic_windows_feeder_timeout",
        crate::microphone_input::MicrophoneDeviceError::DeviceRemoved => {
            "mic_windows_device_removed"
        }
        _ => "mic_windows_feeder_failure",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthenticationRoute {
    ResumeRegistry,
    WindowsCredential,
}

fn authentication_route(response: &AuthResponse) -> AuthenticationRoute {
    if response.method == AUTH_METHOD_RESUME {
        AuthenticationRoute::ResumeRegistry
    } else {
        AuthenticationRoute::WindowsCredential
    }
}

fn clear_resume_secrets(response: &mut AuthResponse) {
    response.credential.zeroize();
    clear_resume_material(response);
}

fn clear_resume_material(response: &mut AuthResponse) {
    if let Some(holder_nonce) = &mut response.resume_holder_nonce {
        holder_nonce.zeroize();
    }
    if let Some(grant) = &mut response.resume_grant {
        grant.zeroize();
    }
    response.resume_holder_nonce = None;
    response.resume_grant = None;
    response.resume_requested = false;
}

fn build_auth_request(
    disclaimer: Option<&PreparedDisclaimer>,
    resume_supported: bool,
    detached_resume: bool,
    multi_monitor_v1: Option<AuthMultiMonitorOfferMsg>,
) -> AuthRequest {
    AuthRequest {
        msg_type: AUTH_REQUEST.to_string(),
        auth_methods: vec!["pam".to_string(), "token".to_string()],
        challenge: arcen_protocol::auth::generate_challenge(),
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

impl BrokerAgentLease {
    pub fn new(
        profile: arcen_telemetry::OperationalProfile,
        qos_targets: arcen_telemetry::QosTargets,
    ) -> Self {
        let (controls, _) = watch::channel(AgentControl::new(0, profile, qos_targets, true, 0));
        Self {
            slot: Arc::new(Semaphore::new(1)),
            admission: crate::session_admission::SessionAdmissionRuntime::new(),
            controls,
            active_log: RwLock::new(None),
        }
    }

    fn try_acquire(&self) -> Result<BrokerAgentPermit, String> {
        // Order matters. `BrokerAgentPermit::drop` releases the admission lease
        // and only then drops `_display`, so acquisition has to run in the same
        // order to avoid a window where one is held and the other is not.
        //
        // Taking admission first was a real defect: `SessionAdmissionLease` has
        // no `Drop` impl (the slot is freed only by an explicit `complete()`),
        // so when the display permit was unavailable the `?` below discarded the
        // lease without releasing it and the capacity-one gate stayed occupied
        // forever, refusing every later session until the service restarted.
        // Because `try_acquire` runs before credential verification, any peer
        // that completed the TLS handshake could drive that. Acquiring the
        // display permit first means the only fallible step after admission is
        // none at all. This matches the Linux Pier, which has always taken the
        // session slot before admission.
        let display = self
            .slot
            .clone()
            .try_acquire_owned()
            // "retry after it disconnects" gave no indication of how long that
            // could be, and the answer used to be up to twenty minutes because
            // a disconnected session keeps its display authority for the whole
            // resume window. Name the setting, so an operator who hits this on
            // a shared machine can find out and change it instead of guessing.
            .map_err(|_| {
                "The Windows display is already owned by another authenticated session. It is \
                 released when that session disconnects and its resume window \
                 (auth.reconnect_window_secs) expires."
                    .to_string()
            })?;
        let admission = self
            .admission
            .admit_new()
            .map_err(|error| format!("session admission denied: {error}"))?;
        Ok(BrokerAgentPermit {
            admission_runtime: Arc::clone(&self.admission),
            admission: Some(admission),
            _display: display,
        })
    }

    pub fn request_profile(
        &self,
        profile: arcen_telemetry::OperationalProfile,
        qos_targets: arcen_telemetry::QosTargets,
        use_configured_filter: bool,
    ) {
        self.controls.send_modify(|control| {
            control.sequence = control.sequence.saturating_add(1);
            control.profile_level = profile.into();
            control.qos_targets = qos_targets;
            control.use_configured_filter = use_configured_filter;
        });
    }

    pub fn request_log_reopen(&self) {
        self.controls.send_modify(|control| {
            control.sequence = control.sequence.saturating_add(1);
            control.reopen_generation = control.reopen_generation.saturating_add(1);
        });
    }

    pub fn current_profile(&self) -> arcen_telemetry::OperationalProfile {
        arcen_telemetry::OperationalProfile::try_from(self.controls.borrow().profile_level)
            .expect("broker agent control always contains a validated operational profile")
    }

    pub fn current_qos_targets(&self) -> arcen_telemetry::QosTargets {
        self.controls.borrow().qos_targets
    }

    pub(crate) fn active_log(
        &self,
    ) -> Result<RwLockReadGuard<'_, Option<ActiveAgentLogRegistration>>, String> {
        self.active_log
            .read()
            .map_err(|_| "active agent log lock is poisoned".to_string())
    }

    fn subscribe(&self) -> watch::Receiver<AgentControl> {
        self.controls.subscribe()
    }

    fn register_active_log(
        self: &Arc<Self>,
        path: std::path::PathBuf,
        user_sid: String,
    ) -> Result<ActiveAgentLog, String> {
        *self
            .active_log
            .write()
            .map_err(|_| "active agent log lock is poisoned".to_string())? =
            Some(ActiveAgentLogRegistration {
                path: path.clone(),
                user_sid: user_sid.clone(),
                ready: false,
            });
        Ok(ActiveAgentLog {
            lease: Arc::clone(self),
            path,
            log_grant: None,
        })
    }
}

struct ActiveAgentLog {
    lease: Arc<BrokerAgentLease>,
    path: std::path::PathBuf,
    log_grant: Option<crate::windows_session::SessionLogGrant>,
}

impl ActiveAgentLog {
    fn attach_grant(
        &mut self,
        grant: crate::windows_session::SessionLogGrant,
    ) -> Result<(), String> {
        if self.log_grant.is_some() {
            return Err("active agent log grant was already attached".to_string());
        }
        let mut active = self
            .lease
            .active_log
            .write()
            .map_err(|_| "active agent log lock is poisoned".to_string())?;
        let registration = active
            .as_mut()
            .filter(|registration| registration.path == self.path)
            .ok_or_else(|| "active agent log reservation disappeared".to_string())?;
        self.log_grant = Some(grant);
        registration.ready = true;
        Ok(())
    }
}

impl Drop for ActiveAgentLog {
    fn drop(&mut self) {
        if let Ok(mut active) = self.lease.active_log.write() {
            if active.as_ref().map(|entry| &entry.path) == Some(&self.path) {
                drop(self.log_grant.take());
                *active = None;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    ws: crate::resume::DirectSessionSocket,
    cfg: HostConfig,
    disclaimer: Option<Arc<PreparedDisclaimer>>,
    peer: String,
    preauth_guard: Arc<crate::auth::PreauthGuard>,
    agent_lease: Arc<BrokerAgentLease>,
    cp_coordinator: Arc<crate::cp_pipe::CpCoordinator>,
    timezone_controller: Arc<crate::timezone::TimezoneController>,
    profile: arcen_telemetry::OperationalProfile,
    emitter: LifecycleEmitter,
    resume_registry: Arc<crate::resume::ResumeRegistry>,
    host_identity: arcen_identity::HostIdentity,
    session_shutdown: watch::Receiver<bool>,
) {
    if let Err(error) = run_broker(
        ws,
        cfg,
        disclaimer,
        &peer,
        preauth_guard,
        agent_lease,
        cp_coordinator,
        timezone_controller,
        profile,
        &emitter,
        resume_registry,
        host_identity,
        session_shutdown,
    )
    .await
    {
        tracing::warn!(target: SESSION, %peer, %error, "broker session failed");
    }
}

/// Classify the authenticated account against the machine's WTS sessions,
/// moving a safe disconnected session onto or away from the physical console
/// first if that is what stands between it and a desktop.
///
/// Windows serves one interactive desktop per station and the capture pipeline
/// can only duplicate the console's adapter, so an account whose desktop is
/// parked in another session — which is what every RDP user leaves behind when
/// they close the client — used to get a flat refusal it could not act on. The
/// move is the documented `WTSConnectSessionW` operation. The classifier proves
/// by token match that the source belongs to the account being admitted, and
/// will only target another account's console session once Windows reports it
/// disconnected rather than locked-or-active.
///
/// Exactly one move is attempted. If it does not produce a bindable console the
/// caller sees an ordinary rejection, never a retry loop against the display.
fn classify_bind_with_console_takeover(
    account: &crate::auth::AuthenticatedAccount,
    peer: &str,
) -> Result<crate::windows_session::BindStatus, String> {
    let status = crate::windows_session::classify_bind(account)?;
    let crate::windows_session::BindStatus::Reconnect { source, target } = status else {
        if matches!(status, crate::windows_session::BindStatus::Rejected(_)) {
            // The field report that motivated this path produced one terse
            // reason and nothing else, so the topology had to be guessed at.
            // Record the shape whenever a bind is refused.
            tracing::warn!(
                target: SESSION,
                %peer,
                topology = %crate::windows_session::topology_summary(),
                "console bind refused; recording WTS topology"
            );
        }
        return Ok(status);
    };
    tracing::info!(
        target: SESSION,
        %peer,
        source_session = source,
        console_session = target,
        topology = %crate::windows_session::topology_summary(),
        "moving the account's existing session onto the physical console"
    );
    if let Err(error) = crate::windows_session::move_to_console(source, target) {
        tracing::warn!(
            target: SESSION,
            %peer,
            source_session = source,
            console_session = target,
            %error,
            topology = %crate::windows_session::topology_summary(),
            "could not move the account's session onto the physical console"
        );
        return Ok(crate::windows_session::BindStatus::Rejected(
            "the account's existing session could not be moved to the physical console",
        ));
    }
    let status = crate::windows_session::classify_bind(account)?;
    if matches!(status, crate::windows_session::BindStatus::Reconnect { .. }) {
        // A second move would be a loop against the display. Refuse instead.
        tracing::warn!(
            target: SESSION,
            %peer,
            topology = %crate::windows_session::topology_summary(),
            "console move completed but the session is still not on the console"
        );
        return Ok(crate::windows_session::BindStatus::Rejected(
            "the account's session did not settle on the physical console",
        ));
    }
    tracing::info!(
        target: SESSION,
        %peer,
        source_session = source,
        topology = %crate::windows_session::topology_summary(),
        "account session is now on the physical console"
    );
    Ok(status)
}

fn rejected_console_client_message(reason: &str) -> String {
    format!("Remote sign-in is unavailable: {reason}.")
}

#[allow(clippy::too_many_arguments)]
async fn run_broker(
    mut ws: crate::resume::DirectSessionSocket,
    mut cfg: HostConfig,
    disclaimer: Option<Arc<PreparedDisclaimer>>,
    peer: &str,
    preauth_guard: Arc<crate::auth::PreauthGuard>,
    agent_lease: Arc<BrokerAgentLease>,
    cp_coordinator: Arc<crate::cp_pipe::CpCoordinator>,
    timezone_controller: Arc<crate::timezone::TimezoneController>,
    profile: arcen_telemetry::OperationalProfile,
    emitter: &LifecycleEmitter,
    resume_registry: Arc<crate::resume::ResumeRegistry>,
    host_identity: arcen_identity::HostIdentity,
    session_shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    log_state(peer, ServerState::Authenticating);
    let detached_resume = cfg.reconnect_window_secs > 0
        && resume_registry
            .resume_handshake_available()
            .map_err(|error| format!("resume registry unavailable: {error:?}"))?;
    let resume_supported = cfg.reconnect_window_secs > 0;
    let mut multi_monitor_gate =
        crate::multi_monitor_gate::MultiMonitorGate::from_config(&cfg.multi_monitor);
    if cfg.iddcx.enabled {
        if let Err(error) = crate::iddcx::probe_strict_readiness(&cfg.iddcx) {
            tracing::warn!(
                target: DISPLAY,
                %error,
                "IddCx strict capability gate withheld multi_monitor_v1"
            );
            multi_monitor_gate = crate::multi_monitor_gate::MultiMonitorGate::disabled();
            cfg.multi_monitor.advertise_enabled = false;
        }
    }
    let multi_monitor_offer = crate::multi_monitor_gate::build_offer(&multi_monitor_gate);
    tracing::info!(
        target: DISPLAY,
        advertise_enabled = cfg.multi_monitor.advertise_enabled,
        nvidia_headless_enabled = cfg.multi_monitor.nvidia_headless_enabled,
        configured_max_monitors = ?cfg.multi_monitor.max_monitors,
        effective_max_monitors = multi_monitor_offer
            .as_ref()
            .map_or(0, AuthMultiMonitorOfferMsg::max_monitors),
        nvenc_session_limit = ?cfg.multi_monitor.nvenc_session_limit,
        nvenc_capacity_policy = if cfg.multi_monitor.nvenc_session_limit.is_some() {
            "operator_ceiling_then_runtime_probe"
        } else {
            "runtime_probe"
        },
        allow_software_fallback = cfg.multi_monitor.allow_software_fallback,
        allowed_adapters = ?cfg.multi_monitor.allowed_adapters,
        "effective Windows multi-monitor admission policy"
    );
    let request = build_auth_request(
        disclaimer.as_deref(),
        resume_supported,
        detached_resume,
        multi_monitor_offer,
    );
    send_json(&mut ws, &request, "auth_request").await?;

    let auth_response_timeout = if disclaimer.is_some() {
        INTERACTIVE_AUTH_RESPONSE_TIMEOUT
    } else {
        AUTH_RESPONSE_TIMEOUT
    };
    let mut response: AuthResponse =
        recv_typed(&mut ws, AUTH_RESPONSE, auth_response_timeout).await?;
    let (session_log_id, replaced) = resolve_session_log_id(response.session_log_id.as_deref())?;
    if replaced {
        tracing::warn!(
            target: SESSION,
            %peer,
            sid = %session_log_id,
            "client session log id was absent or invalid; generated host fallback"
        );
    }
    let span = tracing::info_span!(
        target: SESSION,
        "windows_connection",
        sid = %session_log_id
    );
    if authentication_route(&response) == AuthenticationRoute::ResumeRegistry {
        return async {
            if !resume_supported {
                clear_resume_secrets(&mut response);
                send_resume_rejection(
                    &mut ws,
                    "session resume is unavailable",
                    ResumeErrorCode::Unsupported,
                )
                .await
                .map_err(|error| error.to_string())?;
                return Ok(());
            }
            let prepared =
                resume_registry.prepare_resume(&response, &host_identity, &session_log_id);
            clear_resume_secrets(&mut response);
            match prepared {
                Ok(permit) => match resume_registry.handoff(permit, ws, session_log_id) {
                    Ok(()) => Ok(()),
                    Err((socket, rejection)) => {
                        let mut socket = *socket;
                        rejection.notify_terminal_owner();
                        send_resume_rejection(&mut socket, rejection.message, rejection.code).await
                    }
                },
                Err(rejection) => {
                    rejection.notify_terminal_owner();
                    send_resume_rejection(&mut ws, rejection.message, rejection.code).await
                }
            }
        }
        .instrument(span)
        .await;
    }
    if detached_resume {
        clear_resume_secrets(&mut response);
        return async {
            send_resume_rejection(
                &mut ws,
                "active session requires resume authentication",
                ResumeErrorCode::Unsupported,
            )
            .await
        }
        .instrument(span)
        .await;
    }
    let acknowledged_disclaimer = validate_disclaimer_acknowledgment(disclaimer, &response)?;
    run_correlated_broker(
        ws,
        cfg,
        peer,
        preauth_guard,
        agent_lease,
        cp_coordinator,
        timezone_controller,
        response,
        acknowledged_disclaimer,
        session_log_id,
        profile,
        emitter,
        resume_registry,
        host_identity,
        session_shutdown,
    )
    .instrument(span)
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_correlated_broker(
    mut ws: crate::resume::DirectSessionSocket,
    mut cfg: HostConfig,
    peer: &str,
    preauth_guard: Arc<crate::auth::PreauthGuard>,
    agent_lease: Arc<BrokerAgentLease>,
    cp_coordinator: Arc<crate::cp_pipe::CpCoordinator>,
    timezone_controller: Arc<crate::timezone::TimezoneController>,
    mut response: AuthResponse,
    acknowledged_disclaimer: Option<AcknowledgedDisclaimer>,
    session_log_id: CorrelationId,
    profile: arcen_telemetry::OperationalProfile,
    emitter: &LifecycleEmitter,
    resume_registry: Arc<crate::resume::ResumeRegistry>,
    host_identity: arcen_identity::HostIdentity,
    mut session_shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    let active_transport = ws.transport_capability();
    tracing::info!(target: SESSION, %peer, "session log correlation bound");
    if let Err(error) = validate_auth_response(&response) {
        // The refusal happens before authentication or any display mutation;
        // tell the Deck why instead of dropping the socket.
        send_auth_result(&mut ws, false, &error).await?;
        tracing::warn!(target: SESSION, %peer, %error, "client display plan rejected");
        return Ok(());
    }
    if let Some(request) = response.initial_video.as_ref() {
        if let Err(error) = cfg.apply_initial_video_request(request) {
            send_auth_result(&mut ws, false, &format!("Video request rejected: {error}")).await?;
            tracing::warn!(
                target: SESSION,
                %peer,
                %error,
                "auth-time video request rejected before display or encoder creation"
            );
            return Ok(());
        }
    }
    // Acquire local product authority before credential verification, CP logon,
    // display mutation, or agent launch. The non-cloneable permit remains owned
    // by this broker task across its direct reconnect loop.
    let _agent_lease = match agent_lease.try_acquire() {
        Ok(permit) => permit,
        Err(error) => {
            send_auth_result(&mut ws, false, &error).await?;
            tracing::warn!(
                target: SESSION,
                %peer,
                "rejecting new authentication before native session mutation"
            );
            return Ok(());
        }
    };
    let resume_disclaimer_binding = crate::resume::disclaimer_binding(
        acknowledged_disclaimer
            .as_ref()
            .map(|acknowledged| acknowledged.disclaimer.as_ref()),
    )
    .map_err(|_| "could not construct fixed disclaimer resume binding".to_string())?;
    let mut resume_opt_in = if cfg.reconnect_window_secs > 0 && response.resume_requested {
        response
            .resume_holder_nonce
            .as_deref()
            .and_then(crate::resume::decode_holder_nonce)
            .zip(crate::resume::TopologyBinding::from_response(&response).ok())
    } else {
        None
    };
    clear_resume_material(&mut response);
    let credential = zeroize::Zeroizing::new(std::mem::take(&mut response.credential));
    let (account, credential) = match authenticate_with_deadline(
        AUTH_VERIFY_TIMEOUT,
        crate::auth::authenticate_windows(response.username.clone(), credential, preauth_guard),
    )
    .await
    {
        Ok(pair) => pair,
        Err(error) => {
            send_auth_result(&mut ws, false, "Invalid credentials").await?;
            tracing::warn!(target: SESSION, %peer, %error, "authentication rejected");
            emit_session_auth_fail(
                emitter,
                session_log_id.clone(),
                Some(&response.username),
                peer,
                "credential_verification",
                "invalid_credentials",
            );
            return Ok(());
        }
    };

    // Existing active-session attach is unchanged. Only when no SID-matching
    // session exists (or a matching one is locked) do we hand the credential to
    // the Credential Provider for a remote first-login / unlock.
    let (identity, selected, identity_binding) =
        match classify_bind_with_console_takeover(&account, peer) {
            Ok(crate::windows_session::BindStatus::Bound(identity, selected)) => {
                // The credential is not needed for an existing session; scrub it now.
                drop(credential);
                (identity, selected, "existing_session")
            }
            Ok(crate::windows_session::BindStatus::NoSession(session_id)) => {
                let correlation_id = crate::first_login::new_correlation_id();
                match cp_coordinator
                    .first_login(
                        &account,
                        credential,
                        crate::cp_pipe::UsageScenario::Logon,
                        session_id,
                        &correlation_id,
                    )
                    .await
                {
                    Ok((identity, selected)) => {
                        emit_cp_logon_ok(
                            emitter,
                            session_log_id.clone(),
                            &identity.user,
                            peer,
                            identity.session_id,
                        );
                        (identity, selected, "cp_first_login")
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: SESSION,
                            %peer,
                            correlation_id,
                            %error,
                            "remote first-login failed"
                        );
                        emit_cp_logon_fail(
                            emitter,
                            session_log_id.clone(),
                            Some(&response.username),
                            peer,
                            &error,
                        );
                        send_auth_result(&mut ws, false, error.client_message()).await?;
                        return Ok(());
                    }
                }
            }
            Ok(crate::windows_session::BindStatus::Locked(session_id)) => {
                let correlation_id = crate::first_login::new_correlation_id();
                match cp_coordinator
                    .first_login(
                        &account,
                        credential,
                        crate::cp_pipe::UsageScenario::UnlockWorkstation,
                        session_id,
                        &correlation_id,
                    )
                    .await
                {
                    Ok((identity, selected)) => {
                        emit_cp_logon_ok(
                            emitter,
                            session_log_id.clone(),
                            &identity.user,
                            peer,
                            identity.session_id,
                        );
                        (identity, selected, "cp_exact_console_unlock")
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: SESSION,
                            %peer,
                            correlation_id,
                            windows_session_id = session_id,
                            %error,
                            "remote exact-console unlock failed"
                        );
                        emit_cp_logon_fail(
                            emitter,
                            session_log_id.clone(),
                            Some(&response.username),
                            peer,
                            &error,
                        );
                        send_auth_result(&mut ws, false, error.client_message()).await?;
                        return Ok(());
                    }
                }
            }
            Ok(crate::windows_session::BindStatus::Reconnect { source, target }) => {
                // classify_bind_with_console_takeover resolves or downgrades
                // this arm, so reaching it means the takeover contract changed.
                // Fail closed rather than fall through to a session launch.
                drop(credential);
                tracing::error!(
                    target: SESSION,
                    %peer,
                    source_session = source,
                    console_session = target,
                    "unresolved console move reached the broker"
                );
                emit_session_auth_fail(
                    emitter,
                    session_log_id.clone(),
                    Some(&response.username),
                    peer,
                    "session_bind",
                    "ineligible_console_state",
                );
                send_auth_result(
                    &mut ws,
                    false,
                    "Remote sign-in is unavailable: console takeover could not be completed.",
                )
                .await?;
                return Ok(());
            }
            Ok(crate::windows_session::BindStatus::Rejected(reason)) => {
                drop(credential);
                tracing::warn!(
                    target: SESSION,
                    %peer,
                    reason,
                    "credential provider dispatch rejected for ambiguous console state"
                );
                emit_session_auth_fail(
                    emitter,
                    session_log_id.clone(),
                    Some(&response.username),
                    peer,
                    "session_bind",
                    "ineligible_console_state",
                );
                let message = rejected_console_client_message(reason);
                send_auth_result(&mut ws, false, &message).await?;
                return Ok(());
            }
            Ok(crate::windows_session::BindStatus::Error(error)) => {
                drop(credential);
                emit_session_auth_fail(
                    emitter,
                    session_log_id.clone(),
                    Some(&response.username),
                    peer,
                    "session_bind",
                    "bind_error",
                );
                send_auth_result(&mut ws, false, "First sign-in is unavailable on this host.")
                    .await?;
                return Err(error);
            }
            Err(error) => {
                drop(credential);
                emit_session_auth_fail(
                    emitter,
                    session_log_id.clone(),
                    Some(&response.username),
                    peer,
                    "session_bind",
                    "bind_error",
                );
                send_auth_result(&mut ws, false, &error).await?;
                return Err(error);
            }
        };
    if !identity.state.eq_ignore_ascii_case("active") {
        resume_opt_in = None;
    }
    let native_user_sid = crate::windows_session::selected_user_sid(&selected).to_string();
    let active_session_id =
        arcen_identity::ActiveHostSessionId::new(format!("windows-wts:{}", identity.session_id))
            .map_err(|_| "active Windows session identity is invalid".to_string())?;
    let cleanup_active_session_id = active_session_id.clone();
    let native_principal = arcen_identity::NativePrincipal::Windows {
        sid: arcen_identity::WindowsSid::new(native_user_sid.clone())
            .map_err(|_| "bound Windows SID is invalid".to_string())?,
        wts_session_id: identity.session_id,
    };
    let timezone_lease = match timezone_controller.begin(
        cfg.timezone_redirection,
        response.timezone.as_deref(),
        session_log_id.as_str(),
    ) {
        crate::timezone::RedirectOutcome::Applied(lease) => {
            tracing::info!(
                target: SESSION,
                sid = %session_log_id,
                windows_timezone = lease.target_windows_id(),
                "system-wide client timezone applied by LocalSystem broker"
            );
            Some(lease)
        }
        crate::timezone::RedirectOutcome::Warning(error) => {
            tracing::warn!(
                target: SESSION,
                sid = %session_log_id,
                %error,
                "timezone redirection failed; authenticated streaming continues"
            );
            None
        }
        crate::timezone::RedirectOutcome::Invalid(timezone) => {
            tracing::warn!(
                target: SESSION,
                sid = %session_log_id,
                %timezone,
                "authenticated client timezone is invalid; redirection skipped"
            );
            None
        }
        crate::timezone::RedirectOutcome::Unmapped(timezone) => {
            tracing::warn!(
                target: SESSION,
                sid = %session_log_id,
                %timezone,
                "authenticated client timezone has no Windows mapping; redirection skipped"
            );
            None
        }
        crate::timezone::RedirectOutcome::RecoveryHeld => {
            tracing::warn!(
                target: SESSION,
                sid = %session_log_id,
                "timezone redirection held because recovery requires operator action"
            );
            None
        }
        crate::timezone::RedirectOutcome::AlreadyCurrent(windows_timezone) => {
            tracing::debug!(
                target: SESSION,
                sid = %session_log_id,
                %windows_timezone,
                "system timezone already matches authenticated client"
            );
            None
        }
        crate::timezone::RedirectOutcome::Disabled | crate::timezone::RedirectOutcome::Absent => {
            None
        }
    };
    let (agent_log_path, agent_log_user_sid) =
        crate::windows_session::agent_log_registration(&selected, &session_log_id)?;
    let mut active_log = agent_lease.register_active_log(agent_log_path, agent_log_user_sid)?;
    let mut launched = match crate::windows_session::spawn(
        selected,
        &session_log_id,
        profile,
        cfg.iddcx.enabled && response.multi_monitor_v1.is_some(),
    ) {
        Ok(launched) => launched,
        Err(error) => {
            let message = format!("Could not launch the target Windows session agent: {error}");
            send_auth_result(&mut ws, false, &message).await?;
            return Err(message);
        }
    };
    // Every result after spawn funnels through the cleanup below. In particular,
    // IPC send/receive/decode and auth-result failures may not rely on job Drop
    // while the system-wide timezone lease is still active.
    let result: Result<(), String> = async {
        active_log.attach_grant(launched.take_log_grant())?;
        let mut agent_controls = agent_lease.subscribe();
        let process_id = launched.process_id;
        let mut internal_config =
            tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        internal_config.max_message_size = Some(INTERNAL_MAX_MESSAGE);
        internal_config.max_frame_size = Some(INTERNAL_MAX_MESSAGE);
        let mut agent_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
            launched.take_stream(),
            Role::Client,
            Some(internal_config),
        )
        .await;
        let start = AgentStart::new(
            peer.to_string(),
            &cfg,
            response,
            identity.clone(),
            launched.log_path.clone(),
            &session_log_id,
            agent_controls.borrow().clone(),
            active_transport,
        );
        send_json(&mut agent_ws, &start, "windows_session_agent_start").await?;
        let ready_text = recv_text(&mut agent_ws, AGENT_READY_TIMEOUT).await?;
        let ready_value: serde_json::Value = serde_json::from_str(&ready_text)
            .map_err(|error| format!("decode session agent readiness: {error}"))?;
        if ready_value.get("type").and_then(serde_json::Value::as_str) == Some(AGENT_ERROR_TYPE) {
            let error = ready_value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("session agent failed before readiness")
                .to_string();
            send_auth_result(&mut ws, false, &error).await?;
            return Err(error);
        }
        let ready: AgentReady = serde_json::from_value(ready_value)
            .map_err(|error| format!("decode session agent readiness: {error}"))?;
        if ready.msg_type != AgentReady::TYPE
            || ready.process_id != process_id
            || ready.windows_session != identity
        {
            return Err("session agent readiness identity mismatch".to_string());
        }
        if let Some(acknowledged) = &acknowledged_disclaimer {
            let acceptance =
                DisclaimerAcceptance::new(&acknowledged.disclaimer, acknowledged.accepted_at);
            tracing::info!(
                target: SESSION,
                sid = %session_log_id,
                disclaimer_locale = acceptance.locale().as_str(),
                disclaimer_sha256 = %acceptance.digest().to_lower_hex(),
                disclaimer_accepted_at = acceptance.accepted_at_epoch_seconds(),
                success = true,
                "disclaimer acceptance recorded"
            );
        }
        emit_session_auth_ok(
            emitter,
            session_log_id.clone(),
            &identity.user,
            peer,
            identity_binding,
            identity.session_id,
        );
        let (resume_grant, mut resume_commands) =
            if let Some((holder_nonce, topology)) = resume_opt_in {
                let (disclaimer_digest, disclaimer_version) = resume_disclaimer_binding.clone();
                let policy = ReconnectPolicy::new(cfg.reconnect_window_secs)
                    .map_err(|error| format!("invalid reconnect policy: {error}"))?;
                let (owner, commands) = mpsc::unbounded_channel();
                let grant = resume_registry
                    .issue_initial(
                        crate::resume::ResumeBindings {
                            host_identity: host_identity.clone(),
                            active_session_id: active_session_id.clone(),
                            native_principal: native_principal.clone(),
                            holder_nonce,
                            disclaimer_digest,
                            disclaimer_version,
                            topology,
                        },
                        policy,
                        owner,
                        &session_log_id,
                    )
                    .map_err(|_| "could not issue direct resume grant".to_string())?;
                (Some(grant), Some(commands))
            } else {
                (None, None)
            };
        send_auth_result_with_resume(
            &mut ws,
            true,
            "OK",
            resume_grant.as_ref(),
            resume_grant.as_ref().map(|_| cfg.reconnect_window_secs),
            false,
            None,
        )
        .await?;
        tracing::info!(
            target: SESSION,
            %peer,
            process_id,
            windows_session_id = identity.session_id,
            windows_user = %identity.user,
            windows_domain = %identity.domain,
            "authenticated broker session is relaying to per-session agent"
        );
        let result = if let Some(commands) = resume_commands.as_mut() {
            relay_resumable_client_and_agent(
                ws,
                &mut agent_ws,
                identity,
                native_user_sid,
                active_session_id,
                &mut agent_controls,
                commands,
                &resume_registry,
                cfg.reconnect_window_secs,
                &mut session_shutdown,
            )
            .await
        } else {
            relay_client_and_agent(
                &mut ws,
                &mut agent_ws,
                identity,
                native_user_sid,
                &mut agent_controls,
                &mut session_shutdown,
            )
            .await
        };
        let _ = send_ws_with_timeout(&mut agent_ws, Message::Close(None), WS_WRITE_TIMEOUT).await;
        let _ = tokio::time::timeout(WS_WRITE_TIMEOUT, agent_ws.close(None)).await;
        result
    }
    .await;
    let cleanup_result =
        finish_resume_cleanup(&resume_registry, &cleanup_active_session_id, || {
            finish_agent_resources(launched, timezone_lease)
        })
        .await;
    match (result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(format!("resume registry cleanup failed: {error:?}")),
        (Err(error), Err(revoke_error)) => {
            tracing::error!(
                target: SESSION,
                ?revoke_error,
                "resume registry cleanup failed while handling session failure"
            );
            Err(error)
        }
    }
}

async fn finish_resume_cleanup<Cleanup, CleanupFuture>(
    registry: &crate::resume::ResumeRegistry,
    active_session_id: &arcen_identity::ActiveHostSessionId,
    cleanup: Cleanup,
) -> Result<(), crate::resume::ResumeRegistryError>
where
    Cleanup: FnOnce() -> CleanupFuture,
    CleanupFuture: std::future::Future<Output = ()>,
{
    let begin_result = registry.begin_drain(active_session_id);
    cleanup().await;
    begin_result?;
    registry.complete_drain(active_session_id)
}

async fn finish_agent_resources(
    launched: crate::windows_session::LaunchedAgent,
    timezone_lease: Option<crate::timezone::TimezoneLease>,
) {
    finish_agent_then_timezone(
        || launched.finish(),
        || {
            if let Some(lease) = timezone_lease {
                if let Err(error) = lease.finish() {
                    tracing::error!(
                        target: SESSION,
                        %error,
                        "explicit timezone restore failed after agent exit; journal retained"
                    );
                }
            }
        },
    )
    .await;
}

async fn finish_agent_then_timezone<FinishAgent, AgentFuture, FinishTimezone>(
    finish_agent: FinishAgent,
    finish_timezone: FinishTimezone,
) where
    FinishAgent: FnOnce() -> AgentFuture,
    AgentFuture: std::future::Future<Output = ()>,
    FinishTimezone: FnOnce(),
{
    finish_agent().await;
    finish_timezone();
}

struct AcknowledgedDisclaimer {
    disclaimer: Arc<PreparedDisclaimer>,
    accepted_at: u64,
}

fn validate_disclaimer_acknowledgment(
    disclaimer: Option<Arc<PreparedDisclaimer>>,
    response: &AuthResponse,
) -> Result<Option<AcknowledgedDisclaimer>, String> {
    let Some(disclaimer) = disclaimer else {
        return Ok(None);
    };
    let acknowledgment = response
        .disclaimer_acceptance_sha256
        .as_deref()
        .ok_or_else(|| "disclaimer acknowledgment is required".to_string())?;
    match disclaimer.matches_acknowledgment(acknowledgment) {
        Ok(true) => {}
        Ok(false) => return Err("disclaimer acknowledgment does not match".to_string()),
        Err(_) => return Err("disclaimer acknowledgment is invalid".to_string()),
    }
    let accepted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock precedes the Unix epoch".to_string())?
        .as_secs();
    Ok(Some(AcknowledgedDisclaimer {
        disclaimer,
        accepted_at,
    }))
}

/// Emits `SESSION_AUTH_OK` (1100) once identity has been bound and the
/// per-session agent has confirmed readiness.
fn emit_session_auth_ok(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    identity_binding: &'static str,
    os_session_id: u32,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "auth_method",
        FieldValue::String("windows_logon".to_string()),
    );
    let _ = fields.insert(
        "identity_binding",
        FieldValue::String(identity_binding.to_string()),
    );
    let _ = fields.insert(
        "os_session_id",
        FieldValue::Integer(i64::from(os_session_id)),
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::SessionAuthOk,
        session_log_id,
        fields,
        Some(user.to_owned()),
        local_hostname(),
        Some(peer.to_owned()),
        None,
    );
}

/// Emits `SESSION_AUTH_FAIL` (1101) at the broker's final auth refusal, with
/// only a safe stage/reason class — never raw credentials or OS errors.
fn emit_session_auth_fail(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: Option<&str>,
    peer: &str,
    stage: &'static str,
    reason_class: &'static str,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "auth_method",
        FieldValue::String("windows_logon".to_string()),
    );
    let _ = fields.insert("stage", FieldValue::String(stage.to_string()));
    let _ = fields.insert("reason_class", FieldValue::String(reason_class.to_string()));
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::SessionAuthFail,
        session_log_id,
        fields,
        user.map(str::to_owned),
        local_hostname(),
        Some(peer.to_owned()),
        None,
    );
}

/// Emits `CP_LOGON_OK` (1300) when the Credential Provider cold logon
/// returns a SID-matched unlocked session.
fn emit_cp_logon_ok(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    os_session_id: u32,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "stage",
        FieldValue::String("cold_logon_sid_matched".to_string()),
    );
    let _ = fields.insert(
        "os_session_id",
        FieldValue::Integer(i64::from(os_session_id)),
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::CpLogonOk,
        session_log_id,
        fields,
        Some(user.to_owned()),
        local_hostname(),
        Some(peer.to_owned()),
        None,
    );
}

/// Emits `CP_LOGON_FAIL` (1301) once at the final Credential Provider
/// failure boundary. `FirstLoginError` is mapped to a small closed set of
/// safe reason classes; its detail strings (which may carry NTSTATUS-style
/// diagnostics but never account/credential material) never reach the
/// native event.
fn emit_cp_logon_fail(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: Option<&str>,
    peer: &str,
    error: &crate::first_login::FirstLoginError,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("stage", FieldValue::String("cold_logon".to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String(cp_failure_reason_class(error).to_string()),
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::CpLogonFail,
        session_log_id,
        fields,
        user.map(str::to_owned),
        local_hostname(),
        Some(peer.to_owned()),
        None,
    );
}

/// Safe, closed reason-class mapping for [`crate::first_login::FirstLoginError`].
/// Does not alter CP DLL diagnostics or `FirstLoginError` itself.
fn cp_failure_reason_class(error: &crate::first_login::FirstLoginError) -> &'static str {
    use crate::first_login::FirstLoginError;
    match error {
        FirstLoginError::Busy => "cp_busy",
        FirstLoginError::NoCredentialProvider => "cp_not_ready",
        FirstLoginError::Payload(_) => "cp_payload_invalid",
        FirstLoginError::PushFailed(_) => "cp_push_failed",
        FirstLoginError::SessionTimeout => "cp_session_timeout",
        FirstLoginError::SessionProbe(_) => "cp_session_probe_failed",
        FirstLoginError::Unsupported => "cp_unsupported_platform",
    }
}

pub async fn run_agent<S>(
    mut ws: WebSocketStream<S>,
    expected_session_log_id: CorrelationId,
    log_controller: crate::logging::LogController,
    emitter: LifecycleEmitter,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let start_text = recv_text(&mut ws, AGENT_START_TIMEOUT).await?;
    let start: AgentStart = serde_json::from_str(&start_text)
        .map_err(|error| format!("decode session agent start: {error}"))?;
    if start.msg_type != AgentStart::TYPE {
        return Err("first agent IPC message was not a valid start packet".to_string());
    }
    let active_transport = validate_direct_transport(&start.transport_capability)?;
    let ipc_session_log_id = CorrelationId::parse_uuid(start.session_log_id.clone())
        .map_err(|error| format!("invalid session log id in agent start: {error}"))?;
    if ipc_session_log_id != expected_session_log_id {
        return Err("session agent log id does not match broker command line".to_string());
    }
    if start.log_control.msg_type != AgentControl::TYPE {
        return Err("agent start has an invalid private log control".to_string());
    }
    let initial_profile =
        arcen_telemetry::OperationalProfile::try_from(start.log_control.profile_level)
            .map_err(|error| format!("agent start log control: {error}"))?;
    let initial_filter = if start.log_control.use_configured_filter {
        log_controller.reload_configured(initial_profile)
    } else {
        log_controller.reload_profile(initial_profile)
    };
    if let Err(error) = initial_filter {
        report_log_control_error(error, &mut None);
    }
    let initial_reopen_generation = start.log_control.reopen_generation;
    let initial_qos_targets = start.log_control.qos_targets;
    let initial_applied_reopen_generation = if initial_reopen_generation == 0 {
        0
    } else {
        match log_controller.reopen_log() {
            Ok(()) => initial_reopen_generation,
            Err(error) => {
                report_log_control_error(error, &mut None);
                0
            }
        }
    };
    let mut config = match start.config.into_host() {
        Ok(config) => config,
        Err(error) => {
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
    };
    let resolved_output = match crate::display::resolve_output_selector(&config.output_selector) {
        Ok(output) => output,
        Err(error) => {
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
    };
    // Log every attached output, not just the chosen one. A positional
    // `output_index` moves when the set of attached displays changes, and
    // without the full list a wrong pick is indistinguishable from a host that
    // has no GPU output at all.
    let available_outputs = crate::display::enumerate_outputs().unwrap_or_default();
    for output in &available_outputs {
        tracing::info!(
            target: SESSION,
            global_output = output.global_index,
            adapter = %output.adapter_name,
            adapter_output = output.adapter_output_index,
            vendor_id = format_args!("0x{:04x}", output.vendor_id),
            device = %output.device_name,
            "attached desktop output"
        );
    }
    let resolved_output = match crate::display::prefer_encode_capable_output(
        &config.output_selector,
        &resolved_output,
        &available_outputs,
        DisplaySize {
            width: start.auth_response.screen_width,
            height: start.auth_response.screen_height,
        },
    ) {
        Some(preferred) => {
            tracing::warn!(
                target: SESSION,
                from_adapter = %resolved_output.adapter_name,
                from_vendor_id = format_args!("0x{:04x}", resolved_output.vendor_id),
                from_global_output = resolved_output.global_index,
                to_adapter = %preferred.adapter_name,
                to_global_output = preferred.global_index,
                requested_width = start.auth_response.screen_width,
                requested_height = start.auth_response.screen_height,
                "automatic output index did not identify the best attached NVIDIA display for \
                 this client size; selecting the matching encode-capable output — pin \
                 platform.desktop.adapter to silence this"
            );
            preferred.clone()
        }
        None => resolved_output,
    };
    config.output_index = resolved_output.global_index;
    tracing::info!(
        target: SESSION,
        adapter = %resolved_output.adapter_name,
        adapter_output = resolved_output.adapter_output_index,
        global_output = resolved_output.global_index,
        device = %resolved_output.device_name,
        "resolved configured desktop/capture GPU"
    );
    // Resolve display policy against the adapter that owns the configured
    // output. On an NVIDIA output the first child keeps `Auto`, but typed
    // pre-READY NVENC unavailability is handled here so the display can be
    // retargeted before an explicit OpenH264 child becomes authoritative.
    let display_encoder = if config.encoder == crate::capenc::EncoderSelection::Auto {
        let resolved = config
            .encoder
            .resolve_auto(resolved_output.vendor_id == 0x10de, nvenc_runtime_present());
        if resolved == crate::capenc::EncoderSelection::SoftwareH264 {
            if config.codec != VideoCodec::H264 || config.chroma != ChromaSubsampling::Yuv420 {
                tracing::warn!(
                    target: SESSION,
                    configured_codec = config.codec_name(),
                    configured_chroma = config.chroma_name(),
                    "auto encoder resolved to OpenH264; session negotiates h264/yuv420 \
                     (the software fallback encodes nothing else)"
                );
            }
        }
        tracing::info!(
            target: SESSION,
            adapter = %resolved_output.adapter_name,
            vendor_id = format_args!("0x{:04x}", resolved_output.vendor_id),
            resolved = resolved.name(),
            "auto encoder selection resolved for the configured adapter"
        );
        resolved
    } else {
        config.encoder
    };
    // Freeze the resolved adapter-local binding for every later display and
    // capture operation. Keeping the original positional selector here would
    // let display mutation fall back to global output 0 even after automatic
    // selection chose a different NVIDIA output.
    config.output_selector = crate::display::OutputSelector::Adapter {
        name: resolved_output.adapter_name.clone(),
        output_index: resolved_output.adapter_output_index,
    };
    config.apply_software_h264_backend(display_encoder)?;
    run_authenticated_agent(
        ws,
        config,
        display_encoder,
        start.peer,
        start.auth_response,
        start.windows_session,
        start.agent_log_path,
        active_transport,
        expected_session_log_id,
        Arc::new(DisplayManager::default()),
        log_controller,
        initial_reopen_generation,
        initial_applied_reopen_generation,
        initial_qos_targets,
        emitter,
    )
    .await
}

enum ActiveDisplayLease {
    Single(DisplayLease),
    Multi(MultiDisplayLease),
}

impl ActiveDisplayLease {
    fn report(&self) -> &DisplayReport {
        match self {
            Self::Single(display) => display.report(),
            Self::Multi(display) => display.report(),
        }
    }

    fn single_mut(&mut self) -> Option<&mut DisplayLease> {
        match self {
            Self::Single(display) => Some(display),
            Self::Multi(_) => None,
        }
    }

    fn is_multi(&self) -> bool {
        matches!(self, Self::Multi(_))
    }

    fn restore(&mut self) -> Result<(), String> {
        match self {
            Self::Single(display) => display.restore(),
            Self::Multi(display) => display.restore(),
        }
    }

    fn commit_multi(&mut self) -> Result<(), String> {
        match self {
            Self::Single(_) => Ok(()),
            Self::Multi(display) => display.commit(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_authenticated_agent<S>(
    mut ws: WebSocketStream<S>,
    mut cfg: HostConfig,
    mut display_encoder: crate::capenc::EncoderSelection,
    peer: String,
    response: AuthResponse,
    windows_session: WindowsSessionIdentity,
    agent_log_path: String,
    mut active_transport: &'static str,
    session_log_id: CorrelationId,
    display_manager: Arc<DisplayManager>,
    log_controller: crate::logging::LogController,
    initial_reopen_generation: u64,
    initial_applied_reopen_generation: u64,
    initial_qos_targets: arcen_telemetry::QosTargets,
    emitter: LifecycleEmitter,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let authoritative_timezone = response.timezone.clone();
    let authoritative_cursor = response.cursor_preference;
    let multi_monitor_quality = response
        .multi_monitor_v1
        .as_ref()
        .map(|request| {
            request
                .requested_topology()
                .monitors()
                .iter()
                .map(|monitor| {
                    (
                        monitor.client_display_id.as_str().to_string(),
                        monitor.quality_intent,
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let multi_monitor_gate =
        crate::multi_monitor_gate::MultiMonitorGate::from_config(&cfg.multi_monitor);
    let multi_monitor_offer = crate::multi_monitor_gate::build_offer(&multi_monitor_gate);
    let mut nvidia_headless_planning = None;
    let mut multi_monitor_committed = if response.multi_monitor_v1.is_some() {
        let requested_multi_monitor = response
            .multi_monitor_v1
            .as_ref()
            .expect("checked as present");
        let full_color_required = requested_multi_monitor
            .requested_topology()
            .monitors()
            .iter()
            .filter(|monitor| {
                monitor.quality_intent
                    == arcen_protocol::messages::MonitorQualityIntentMsg::FullColorRequired
            })
            .count();
        if full_color_required > 0 && cfg.chroma != ChromaSubsampling::Yuv444 {
            let error = format!(
                "multi-monitor quality admission failed: {full_color_required} display(s) require \
                 full-color 4:4:4, but this host profile is configured for {}",
                cfg.chroma_name()
            );
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        if cfg.iddcx.enabled {
            if let Err(error) = crate::iddcx::validate_inherited_control(true) {
                let error = format!("open inherited IddCx provider control: {error}");
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        }
        if cfg.multi_monitor.nvidia_headless_enabled {
            let adapter = cfg
                .multi_monitor
                .allowed_adapters
                .first()
                .cloned()
                .ok_or_else(|| {
                    "NVIDIA headless provisioning has no streaming adapter".to_string()
                })?;
            let requested_monitors = requested_multi_monitor.requested_topology().monitors();
            let target_count = requested_monitors.len();
            // The scale the placeholder EDIDs should imply. Taken from the
            // primary (falling back to the first monitor), because at this
            // point the spares are interchangeable and not yet assigned to
            // client monitors. Monitors that ask for a different scale are
            // corrected when their own EDID is written on the exact-timing
            // path; this only has to stop every provisioned output defaulting
            // to 96 DPI / 100% regardless of what the client asked for.
            let provisioning_scale = requested_monitors
                .iter()
                .find(|monitor| monitor.is_primary)
                .or_else(|| requested_monitors.first())
                .and_then(|monitor| arcen_media::scale120_from_scale(monitor.scale).ok())
                .unwrap_or_else(|| arcen_media::Scale120::new(120).expect("120 is a valid scale"));
            let manager = Arc::clone(&display_manager);
            let owner = session_log_id.clone();
            let lease = tokio::task::spawn_blocking(move || {
                manager.prepare_nvidia_headless_multi(
                    &adapter,
                    target_count,
                    provisioning_scale,
                    owner,
                )
            })
            .await
            .map_err(|error| format!("join NVIDIA headless provisioning: {error}"))?
            .map_err(|error| format!("provision NVIDIA headless outputs: {error}"))?;
            nvidia_headless_planning = Some(lease);
        }
        let inventory = match if cfg.iddcx.enabled {
            crate::iddcx::planning_inventory(&cfg.iddcx)
        } else {
            crate::gpu_probe::physical_output_inventory(&cfg.multi_monitor.allowed_adapters)
        } {
            Ok(inventory) => inventory,
            Err(error) => {
                let error = format!("probe multi-monitor outputs: {error}");
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        };
        match crate::multi_monitor_gate::admit_requested_topology(
            &multi_monitor_gate,
            Some(&inventory),
            multi_monitor_offer.as_ref(),
            response.multi_monitor_v1.as_ref(),
        ) {
            crate::multi_monitor_gate::MultiMonitorOutcome::Planned { plan, carrier } => {
                if cfg.iddcx.enabled {
                    let adapter = plan
                        .monitors
                        .first()
                        .map(|monitor| monitor.adapter_name.clone())
                        .ok_or_else(|| "IddCx plan contains no monitors".to_string())?;
                    cfg.multi_monitor.allowed_adapters = vec![adapter];
                }
                tracing::info!(
                    target: DISPLAY,
                    monitors = plan.monitors.len(),
                    desktop_x = plan.desktop_x,
                    desktop_y = plan.desktop_y,
                    desktop_width = plan.desktop_width,
                    desktop_height = plan.desktop_height,
                    %carrier,
                    provider = if cfg.iddcx.enabled { "iddcx" } else { "physical" },
                    "multi_monitor_v1 exact topology admitted"
                );
                Some((plan, carrier))
            }
            crate::multi_monitor_gate::MultiMonitorOutcome::Degraded(reason) => {
                let error = format!("multi-monitor topology admission failed: {reason}");
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
            crate::multi_monitor_gate::MultiMonitorOutcome::NotRequested => None,
        }
    } else {
        None
    };
    let plan = match session_display_plan(&response) {
        Ok(plan) => plan,
        Err(error) => {
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
    };
    if multi_monitor_committed.is_none() {
        if let Some(degradation) = plan.degradation.as_ref() {
            tracing::warn!(
                target: DISPLAY,
                requested_mode = %degradation.requested_mode,
                requested_monitors = degradation.requested_monitors,
                served_mode = degradation.served_mode,
                reason = degradation.reason,
                explanation = degradation.explanation,
                "display preference degraded for this Windows host session"
            );
        }
    }
    let [monitor_plan] = plan.monitors.as_slice() else {
        let error =
            "multi-monitor session plans are not yet supported on Windows hosts".to_string();
        send_agent_failure(&mut ws, &error).await;
        return Err(error);
    };
    let mut request = monitor_plan.request;
    let requested = request.size;
    if let Some((multi_plan, _)) = multi_monitor_committed.as_ref() {
        let primary = multi_plan
            .monitors
            .iter()
            .find(|monitor| monitor.primary)
            .ok_or_else(|| "multi-monitor plan has no primary output".to_string())?;
        request.size = DisplaySize {
            width: primary.width,
            height: primary.height,
        };
        request.refresh_hz = primary.refresh_hz;
        cfg.output_selector = crate::display::OutputSelector::Adapter {
            name: primary.adapter_name.clone(),
            output_index: primary.adapter_output_index,
        };
        cfg.output_index = primary.global_index;
    }
    if display_encoder == crate::capenc::EncoderSelection::SoftwareH264 {
        let fitted = match openh264_fitted_display_size(request.size) {
            Ok(fitted) => fitted,
            Err(error) => {
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        };
        if fitted != request.size {
            tracing::info!(
                target: DISPLAY,
                client_requested = %request.size,
                openh264_display = %fitted,
                "negotiated a desktop within the OpenH264 software contract"
            );
            request.size = fitted;
        }
    }
    // Per-encoder mirroring policy: direct NVENC recreates the client display
    // exactly (isolated primary or refuse); the software/paravirtualized path
    // keeps the negotiated ladder because it cannot promise exactness.
    let policy = if multi_monitor_committed.is_some() {
        crate::display::DisplayPolicy::Negotiated
    } else {
        match display_encoder {
            crate::capenc::EncoderSelection::Nvenc => crate::display::DisplayPolicy::ExactIsolated,
            crate::capenc::EncoderSelection::SoftwareH264 => {
                crate::display::DisplayPolicy::Negotiated
            }
            crate::capenc::EncoderSelection::Auto => {
                // run_agent resolves Auto before this point; if a future entry
                // path skips that, fail open to negotiation rather than refusing
                // sessions on hosts that could stream.
                tracing::warn!(
                    target: SESSION,
                    "encoder selection reached the session unresolved; using negotiated display policy"
                );
                crate::display::DisplayPolicy::Negotiated
            }
        }
    };
    let mut deskside_protection = None;
    let mut deskside_hooks = None;
    let mut deskside_recovery = None;
    let mut deskside_capture_binding = None;
    if cfg.deskside.enabled {
        if policy != crate::display::DisplayPolicy::ExactIsolated {
            let error = "deskside_refused: capture backend cannot provide exact isolated display protection"
                .to_string();
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        let capture_output = crate::display::resolve_output_selector(&cfg.output_selector)?;
        let evidence = match crate::deskside::collect_evidence(
            &cfg.deskside,
            &windows_session,
            &capture_output,
        ) {
            Ok(evidence) => evidence,
            Err(error) => {
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        };
        let decision = cfg.deskside.policy().decide(Ok(evidence.physical()));
        let owner = LeaseOwnerId::new(session_log_id.as_str().to_string())
            .map_err(|error| format!("deskside lease owner: {error}"))?;
        let mut protection = DesksideProtection::new();
        let input_original = StateFingerprint::new(b"windows-deskside-input-released-v1")
            .map_err(|error| error.to_string())?;
        let display_target = StateFingerprint::new(b"windows-exact-isolated-display-v1")
            .map_err(|error| error.to_string())?;
        if protection.begin_arm(
            decision,
            owner,
            DesksideLeaseSpec {
                original: input_original,
                protected: evidence.physical().input_fingerprint(),
            },
            DesksideLeaseSpec {
                original: evidence.physical().display_fingerprint(),
                protected: display_target,
            },
        ) != Ok(DesksideEffect::Arm(DesksideControl::LocalInput))
            || protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput))
                != DesksideEffect::Apply(DesksideControl::LocalInput)
        {
            let error = "deskside shared input arm sequencing failed".to_string();
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        let hooks = match tokio::task::spawn_blocking(crate::deskside::InputHookGuard::install)
            .await
        {
            Ok(Ok(hooks)) => hooks,
            Ok(Err(error)) => {
                let _ = protection.apply(DesksideEvent::ApplyFailed(DesksideControl::LocalInput));
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
            Err(error) => {
                let error = format!("deskside hook startup task failed: {error}");
                let _ = protection.apply(DesksideEvent::ApplyFailed(DesksideControl::LocalInput));
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        };
        if protection.apply(DesksideEvent::ApplySucceeded(DesksideControl::LocalInput))
            != DesksideEffect::Verify(DesksideControl::LocalInput)
            || hooks.verify().is_err()
            || protection.apply(DesksideEvent::VerifySucceeded(DesksideControl::LocalInput))
                != DesksideEffect::Arm(DesksideControl::LocalDisplays)
        {
            let error = "deskside input hook verification failed".to_string();
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        if protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalDisplays))
            != DesksideEffect::Apply(DesksideControl::LocalDisplays)
        {
            let error = "deskside shared display arm sequencing failed".to_string();
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        deskside_recovery = Some(evidence.recovery());
        deskside_capture_binding = Some(evidence.capture_binding());
        deskside_hooks = Some(hooks);
        deskside_protection = Some(protection);
    }
    let output_selector = cfg.output_selector.clone();
    let display_session_log_id = session_log_id.clone();
    let display_recovery = deskside_recovery.clone();
    let multi_plan_for_display = multi_monitor_committed
        .as_ref()
        .map(|(plan, _carrier)| plan.clone());
    let iddcx_config = cfg.iddcx.enabled.then(|| cfg.iddcx.clone());
    let headless_planning_for_display = nvidia_headless_planning.take();
    let mut display = match tokio::task::spawn_blocking(move || match multi_plan_for_display {
        Some(plan) => match headless_planning_for_display {
            Some(lease) => lease
                .acquire(&plan, display_session_log_id)
                .map(ActiveDisplayLease::Multi),
            None => display_manager
                .acquire_multi(&plan, iddcx_config, display_session_log_id)
                .map(ActiveDisplayLease::Multi),
        },
        None => display_manager
            .acquire_with_deskside(
                output_selector,
                request,
                policy,
                display_session_log_id,
                display_recovery,
            )
            .map(ActiveDisplayLease::Single),
    })
    .await
    {
        Ok(Ok(display)) => display,
        Ok(Err(error)) => {
            let error = format!("apply authenticated client display: {error}");
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        Err(error) => {
            let error = format!("display transaction task failed: {error}");
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
    };
    if let (Some(applied_plan), Some((committed_plan, _))) = (
        match &display {
            ActiveDisplayLease::Multi(display) => display.applied_plan().cloned(),
            ActiveDisplayLease::Single(_) => None,
        },
        multi_monitor_committed.as_mut(),
    ) {
        *committed_plan = applied_plan;
        if cfg.iddcx.enabled {
            cfg.multi_monitor.allowed_adapters = committed_plan
                .monitors
                .iter()
                .map(|monitor| monitor.adapter_name.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if let Some(primary) = committed_plan
                .monitors
                .iter()
                .find(|monitor| monitor.primary)
            {
                cfg.output_selector = crate::display::OutputSelector::Adapter {
                    name: primary.adapter_name.clone(),
                    output_index: primary.adapter_output_index,
                };
                cfg.output_index = primary.global_index;
            }
        }
    }
    if let Some(protection) = deskside_protection.as_mut() {
        if let Some(binding) = deskside_capture_binding {
            if let Err(error) =
                crate::deskside::verify_protected(&cfg.deskside, &cfg.output_selector, binding)
            {
                let _ = display.restore();
                if let Some(hooks) = deskside_hooks.as_mut() {
                    let _ = hooks.shutdown();
                }
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        }
        if protection.apply(DesksideEvent::ApplySucceeded(
            DesksideControl::LocalDisplays,
        )) != DesksideEffect::Verify(DesksideControl::LocalDisplays)
            || !display.report().exact
            || protection.apply(DesksideEvent::VerifySucceeded(
                DesksideControl::LocalDisplays,
            )) != DesksideEffect::ProtectionEstablished
        {
            let error = "deskside display verification failed".to_string();
            let _ = display.restore();
            if let Some(hooks) = deskside_hooks.as_mut() {
                let _ = hooks.shutdown();
            }
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        if let Err(error) = crate::recovery::mark_deskside_stage(
            &crate::recovery::default_path(),
            crate::recovery::DesksideRecoveryStage::Protected,
        ) {
            let restore = display.restore();
            if let Some(hooks) = deskside_hooks.as_mut() {
                let _ = hooks.shutdown();
            }
            let error = match restore {
                Ok(()) => format!("commit deskside recovery stage: {error}"),
                Err(restore) => {
                    format!("commit deskside recovery stage: {error}; display restore: {restore}")
                }
            };
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
    }
    // The negotiation may have settled on a fallback mode; never promise a
    // stream outside the OpenH264 contract.
    if display_encoder == crate::capenc::EncoderSelection::SoftwareH264 && !display.is_multi() {
        if let Err(error) = ensure_openh264_applied_size(display.report().applied) {
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
    }
    let initial_media = if let Some((multi_plan, carrier)) = multi_monitor_committed.as_ref() {
        let media = match prepare_multi_monitor_media(
            &cfg,
            display_encoder,
            multi_plan,
            *carrier,
            authoritative_cursor,
            &session_log_id,
            &multi_monitor_quality,
        )
        .await
        {
            Ok(media) => media,
            Err(error) => {
                send_agent_failure(&mut ws, &error).await;
                return Err(error);
            }
        };
        if let Err(error) = display.commit_multi() {
            let error = format!("commit verified multi-monitor display: {error}");
            send_agent_failure(&mut ws, &error).await;
            return Err(error);
        }
        media
    } else {
        let (prepared, selected_encoder) = prepare_attachment_media(
            &cfg,
            display_encoder,
            display
                .single_mut()
                .expect("single-monitor media requires a single display lease"),
            authoritative_cursor,
            &session_log_id,
            deskside_capture_binding,
            true,
        )
        .await?;
        display_encoder = selected_encoder;
        prepared
    };
    emit_display_armed(
        &emitter,
        session_log_id.clone(),
        &windows_session.user,
        &peer,
        display.report(),
        policy,
    );
    send_json(
        &mut ws,
        &AgentReady {
            msg_type: AgentReady::TYPE.to_string(),
            process_id: std::process::id(),
            windows_session: windows_session.clone(),
        },
        "windows_session_agent_ready",
    )
    .await?;
    let mut initial_media = Some(initial_media);
    let mut attachment_session_log_id = session_log_id.clone();
    let mut total_frames = 0_u64;
    let mut total_dropped = 0_u64;
    let session_started_at = std::time::Instant::now();
    let final_result = loop {
        let prepared_media = if let Some(prepared) = initial_media.take() {
            prepared
        } else {
            if let Some((multi_plan, carrier)) = multi_monitor_committed.as_ref() {
                prepare_multi_monitor_media(
                    &cfg,
                    display_encoder,
                    multi_plan,
                    *carrier,
                    authoritative_cursor,
                    &attachment_session_log_id,
                    &multi_monitor_quality,
                )
                .await?
            } else {
                let (prepared, selected_encoder) = prepare_attachment_media(
                    &cfg,
                    display_encoder,
                    display
                        .single_mut()
                        .expect("single-monitor media requires a single display lease"),
                    authoritative_cursor,
                    &attachment_session_log_id,
                    deskside_capture_binding,
                    false,
                )
                .await?;
                display_encoder = selected_encoder;
                prepared
            }
        };
        let media_plan = prepared_media.video.primary_plan();
        let pen_available = prepared_media.pen.is_some();
        let region_input_available = prepared_media.region_input.is_some();
        let multi_monitor_capability = prepared_media.video.multi_capability().cloned();
        let preferences = run_attachment_handshake(
            &mut ws,
            &cfg,
            &peer,
            &plan,
            &windows_session,
            &agent_log_path,
            &authoritative_timezone,
            authoritative_cursor,
            requested,
            display.report(),
            &attachment_session_log_id,
            &media_plan,
            pen_available,
            region_input_available,
            active_transport,
            multi_monitor_capability.as_ref(),
        )
        .await;
        let preferences = match preferences {
            Ok(preferences) => preferences,
            Err(error) => {
                drop(prepared_media);
                return Err(error);
            }
        };
        tracing::info!(
            target: INPUT,
            sid = %attachment_session_log_id,
            requested = ?preferences.tablet_mode_result.requested,
            active = ?preferences.tablet_mode_result.active,
            accepted = preferences.tablet_mode_result.accepted,
            reason = preferences.tablet_mode_result.reason.as_str(),
            reconnect_required = preferences.tablet_mode_result.reconnect_required,
            "tablet mode negotiation resolved"
        );
        let attachment = stream_session(
            ws,
            &cfg,
            &peer,
            &windows_session.user,
            preferences.audio,
            preferences.microphone,
            preferences.cursor_mode,
            preferences.clipboard,
            &mut display,
            prepared_media,
            &attachment_session_log_id,
            log_controller.clone(),
            initial_reopen_generation,
            initial_applied_reopen_generation,
            initial_qos_targets,
            deskside_hooks
                .as_ref()
                .map(crate::deskside::InputHookGuard::proof),
            deskside_capture_binding,
            &emitter,
        )
        .await
        .map_err(|error| error.to_string())?;
        ws = attachment.ws;
        total_frames = total_frames.saturating_add(attachment.sent_frames);
        total_dropped = total_dropped.saturating_add(attachment.dropped_frames);
        match attachment_disposition(attachment.detached, attachment.result) {
            AttachmentDisposition::Finish(result) => break result,
            AttachmentDisposition::Terminate(error) => {
                return Err(format!("fatal attachment cleanup: {error}"));
            }
            AttachmentDisposition::Reattach => {}
        }
        if let Some(protection) = deskside_protection.as_mut() {
            let _ = protection.apply(DesksideEvent::TransportLost { resumable: true });
        }
        send_json(
            &mut ws,
            &AgentAttachmentStatus::detached(),
            AgentAttachmentStatus::TYPE,
        )
        .await?;
        match await_attachment_command(
            &mut ws,
            &cfg,
            display_encoder,
            display.report(),
            &log_controller,
            deskside_hooks
                .as_ref()
                .map(crate::deskside::InputHookGuard::proof),
            deskside_capture_binding,
        )
        .await?
        {
            Some(next_attachment) => {
                if let Some(protection) = deskside_protection.as_mut() {
                    let _ = protection.apply(DesksideEvent::Reconnected);
                }
                attachment_session_log_id = next_attachment.session_log_id;
                active_transport = next_attachment.transport_capability;
            }
            None => break Ok(()),
        }
    };

    if let Some(protection) = deskside_protection.as_mut() {
        let _ = protection.apply(DesksideEvent::BeginDraining);
        let _ = protection.apply(DesksideEvent::RemoteInjectionStopped);
        let _ = crate::recovery::mark_deskside_stage(
            &crate::recovery::default_path(),
            crate::recovery::DesksideRecoveryStage::Restoring,
        );
    }
    let final_display_report = display.report().clone();
    let restore = tokio::task::spawn_blocking(move || display.restore())
        .await
        .map_err(|error| format!("display restore task failed: {error}"))
        .and_then(|result| result);
    if let Some(protection) = deskside_protection.as_mut() {
        let display_event = if restore.is_ok() {
            DesksideEvent::RestoreSucceeded(DesksideControl::LocalDisplays)
        } else {
            DesksideEvent::RestoreFailed(DesksideControl::LocalDisplays)
        };
        let _ = protection.apply(display_event);
    }
    let input_restore = if let Some(hooks) = deskside_hooks.as_mut() {
        hooks.shutdown()
    } else {
        Ok(())
    };
    if let Some(protection) = deskside_protection.as_mut() {
        let input_event = if input_restore.is_ok() {
            DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput)
        } else {
            DesksideEvent::RestoreFailed(DesksideControl::LocalInput)
        };
        let effect = protection.apply(input_event);
        if effect == DesksideEffect::PreserveRecoveryJournal {
            let _ = crate::recovery::mark_deskside_stage(
                &crate::recovery::default_path(),
                crate::recovery::DesksideRecoveryStage::RestoreFailed,
            );
        }
    }
    let duration_ms = i64::try_from(session_started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
    emit_session_end_or_interrupted(
        &emitter,
        attachment_session_log_id.clone(),
        &windows_session.user,
        &peer,
        &final_result,
        duration_ms,
        total_frames,
        total_dropped,
    );
    emit_display_restore_outcome(
        &emitter,
        attachment_session_log_id,
        &windows_session.user,
        &peer,
        &final_display_report,
        &restore,
    );
    let restore = match (restore, input_restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(display_error), Err(input_error)) => Err(format!(
            "{display_error}; deskside input release also failed: {input_error}"
        )),
    };
    match (final_result, restore) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(session_error), Err(restore_error)) => Err(format!(
            "{session_error}; display restore also failed: {restore_error}"
        )),
    }
}

#[allow(clippy::too_many_arguments)]
struct AttachmentPreferences {
    audio: AttachmentAudio,
    microphone: Option<
        crate::microphone_input::MicrophoneIngress<crate::microphone_input::NativeMicrophoneDevice>,
    >,
    cursor_mode: CursorMode,
    tablet_mode_result: TabletModeResultMsg,
    clipboard: Option<ClipboardNegotiation>,
}

#[derive(Clone)]
struct AttachmentAudio {
    policy: ConfiguredAudioPolicy,
    peer: Option<arcen_protocol::messages::AudioOutputCapabilitiesMsg>,
    enabled: bool,
    bitrate_kbps: u32,
}

impl AttachmentAudio {
    fn resolve(&self) -> ResolvedAudioStream {
        self.policy.resolve(self.peer.as_ref(), self.enabled)
    }

    fn update(&mut self, enabled: bool, bitrate_kbps: u32) -> ResolvedAudioStream {
        self.enabled = enabled;
        self.bitrate_kbps = bitrate_kbps;
        self.resolve()
    }
}

async fn run_attachment_handshake<S>(
    ws: &mut WebSocketStream<S>,
    cfg: &HostConfig,
    peer: &str,
    plan: &SessionDisplayPlan,
    windows_session: &WindowsSessionIdentity,
    agent_log_path: &str,
    authoritative_timezone: &Option<String>,
    authoritative_cursor: CursorMode,
    requested: DisplaySize,
    display_report: &DisplayReport,
    session_log_id: &CorrelationId,
    media_plan: &ResolvedMediaPlan,
    pen_available: bool,
    region_input_available: bool,
    active_transport: &'static str,
    multi_monitor_capability: Option<&ServerMultiMonitorMsg>,
) -> Result<AttachmentPreferences, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    log_state(peer, ServerState::CapabilityExchange);
    let microphone_backend_available =
        crate::microphone_input::backend_available_if_enabled(cfg.microphone_input_enabled)
            .await
            .map_err(|error| {
                if error.is_fatal_cleanup() {
                    format!("fatal microphone probe cleanup failure: {error:?}")
                } else {
                    format!("microphone backend probe failed: {error:?}")
                }
            })?;
    tracing::info!(
        target: AUDIO,
        event = "mic_windows_endpoint_probe",
        sid = %session_log_id,
        operator_enabled = cfg.microphone_input_enabled,
        endpoint_available = microphone_backend_available,
        reason = if cfg.microphone_input_enabled {
            if microphone_backend_available { "available" } else { "not_installed" }
        } else {
            "policy_off"
        },
        "Windows microphone endpoint probe completed"
    );
    let mut hello = build_server_hello(
        cfg,
        display_report,
        plan,
        windows_session,
        agent_log_path,
        media_plan,
        microphone_backend_available,
        pen_available,
        region_input_available,
    );
    hello.negotiated_transport = Some(active_transport.to_string());
    if let Some(capability) = multi_monitor_capability {
        hello = hello
            .with_multi_monitor_v1(capability)
            .map_err(|error| format!("attach multi-monitor ServerHello capability: {error}"))?;
    }
    send_json(ws, &hello, "server_hello").await?;
    let client_hello: ClientHelloMsg = recv_typed(ws, CLIENT_HELLO, CLIENT_HELLO_TIMEOUT).await?;
    if !multi_monitor_region_input_negotiated(
        multi_monitor_capability.is_some(),
        region_input_available,
        &client_hello,
    ) {
        return Err(format!(
            "multi-monitor input requires client input protocol v{REGION_INPUT_PROTOCOL_VERSION} \
             with region_input=available"
        ));
    }
    let _negotiated_transport = {
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
                tracing::warn!(
                    target: SESSION,
                    client_capabilities = ?client_hello.transport_capabilities,
                    "transport negotiation failed: no common transport capability"
                );
                return Err("no common transport capability".to_string());
            }
        }
    };
    if &client_hello.timezone != authoritative_timezone {
        tracing::warn!(
            target: SESSION,
            sid = %session_log_id,
            authenticated_timezone = ?authoritative_timezone,
            client_hello_timezone = ?client_hello.timezone,
            "ClientHello timezone differs from authenticated decision; retaining AuthResponse value"
        );
    }
    validate_client_cursor(&client_hello, authoritative_cursor)?;
    let client_tablet_capabilities = client_hello.effective_tablet_mode_capabilities();
    let host_tablet_capabilities = TabletModeCapabilitiesMsg {
        local_termination: if pen_available {
            InputCapabilityAvailability::Available
        } else {
            InputCapabilityAvailability::Unavailable
        },
        wacom_usb_bridge: InputCapabilityAvailability::Unavailable,
        disabled_mouse_compat: InputCapabilityAvailability::Available,
    };
    let tablet_mode_result = resolve_windows_tablet_mode_result(
        client_hello.tablet_mode_requested,
        client_tablet_capabilities,
        host_tablet_capabilities,
    );
    let client_hello_size = validate_client_hello(&client_hello, requested)?;
    let clipboard = ClipboardNegotiation::from_client(cfg.clipboard_policy, &client_hello);
    if client_hello.session_log_id.as_deref() != Some(session_log_id.as_str()) {
        tracing::warn!(
            target: SESSION,
            %peer,
            sid = %session_log_id,
            "client_hello session_log_id echo does not match attachment id"
        );
    }
    if let Some(initial) = cfg.auth_video_request.as_ref() {
        if arcen_protocol::messages::ClientVideoCapabilitiesMsg::from_client_hello(&client_hello)
            != initial.capabilities
        {
            return Err(
                "ClientHello video capabilities differ from the authenticated setup request"
                    .to_string(),
            );
        }
    }
    let quality: QualitySettings =
        recv_typed(ws, "quality_settings", QUALITY_SETTINGS_TIMEOUT).await?;
    if let Some(initial) = cfg.auth_video_request.as_ref() {
        if quality != initial.quality {
            return Err("quality_settings differ from the authenticated setup request".to_string());
        }
    }
    // Resolve the client's requested colour contract against host policy and
    // its own `client_hello` decode capability. New Decks already supplied
    // this request at auth time, before display/encoder creation; this copy is
    // a consistency echo. Legacy Decks retain the informational late path.
    let requested_bit_depth = BitDepth::from_token(&quality.bit_depth);
    if requested_bit_depth.is_none() {
        tracing::warn!(
            target: SESSION,
            sid = %session_log_id,
            token = quality.bit_depth.as_str(),
            "quality_settings bit_depth token not recognised — treating as no client preference"
        );
    }
    let requested_color_range = ColorRange::from_token(&quality.color_range);
    if requested_color_range.is_none() {
        tracing::warn!(
            target: SESSION,
            sid = %session_log_id,
            token = quality.color_range.as_str(),
            "quality_settings color_range token not recognised — treating as no client preference"
        );
    }
    let requested_color_matrix = ColorMatrix::from_token(&quality.color_matrix);
    if requested_color_matrix.is_none() {
        tracing::warn!(
            target: SESSION,
            sid = %session_log_id,
            token = quality.color_matrix.as_str(),
            "quality_settings color_matrix token not recognised — treating as no client preference"
        );
    }
    // Intent gets the same treatment as the colour axes above, for the same
    // reason: an unrecognised token means something this host does not
    // understand, and guessing at it would spend the session's latency budget
    // on an intent nobody asked for. Unlike them it has no operator-configured
    // ceiling to resolve against, so "no preference" is what capture started
    // with — and, until the encoder-recreation work lands, so is a stated
    // preference: this is reported below, never silently claimed.
    let requested_encode_intent = EncodeIntent::from_token(&quality.encode_intent);
    if requested_encode_intent.is_none() {
        tracing::warn!(
            target: SESSION,
            sid = %session_log_id,
            token = quality.encode_intent.as_str(),
            "quality_settings encode_intent token not recognised — treating as no client preference"
        );
    }
    let resolved_encode_intent = requested_encode_intent.unwrap_or_default();
    // Policy precedence, then the absolute client-capability cross-check:
    // never grant more than `client_hello` claimed this client can decode,
    // regardless of what policy would otherwise serve.
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
    // Validate coherence and backend capability: an incoherent request (e.g.
    // an identity matrix below 4:4:4) or one this concrete backend cannot
    // serve (e.g. 12-bit on NVENC) is not something the resolved contract
    // below should ever claim, even informationally.
    let resolved_color_video = arcen_media::VideoConfiguration {
        codec: media_plan.video.codec,
        chroma: media_plan.video.chroma,
        bit_depth: resolved_bit_depth,
        range: resolved_color_range,
        matrix: resolved_color_matrix,
        primaries: media_plan.video.primaries,
        transfer: media_plan.video.transfer,
    };
    let (resolved_bit_depth, resolved_color_range, resolved_color_matrix) =
        if color_contract_is_servable(resolved_color_video, media_plan) {
            (
                resolved_bit_depth,
                resolved_color_range,
                resolved_color_matrix,
            )
        } else {
            tracing::warn!(
                target: SESSION,
                sid = %session_log_id,
                want_bit_depth = resolved_bit_depth.token(),
                want_color_range = resolved_color_range.token(),
                want_color_matrix = resolved_color_matrix.token(),
                "resolved colour request is incoherent or unsupported by this backend — falling back to the active plan"
            );
            (
                media_plan.video.bit_depth,
                media_plan.video.range,
                media_plan.video.matrix,
            )
        };
    if cfg.auth_video_request.is_none() && multi_monitor_capability.is_some() {
        let adaptive = quality.video_selection
            == arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance;
        let codec_mismatch =
            !adaptive && !quality.codec.is_empty() && quality.codec != media_plan.codec_token();
        let chroma_mismatch =
            !quality.chroma.is_empty() && quality.chroma != media_plan.chroma_token();
        let color_mismatch = resolved_bit_depth != media_plan.video.bit_depth
            || resolved_color_range != media_plan.video.range
            || resolved_color_matrix != media_plan.video.matrix;
        let intent_mismatch =
            requested_encode_intent.is_some_and(|intent| intent != cfg.requested_encode_intent());
        if codec_mismatch || chroma_mismatch || color_mismatch || intent_mismatch {
            tracing::warn!(
                target: SESSION,
                sid = %session_log_id,
                "legacy multi-monitor quality change requires a whole-roster reconnect"
            );
            return Err(
                "legacy multi-monitor quality change is unsupported; reconnect with a current Arcen Deck"
                    .to_string(),
            );
        }
    }
    let mut audio = AttachmentAudio {
        policy: AudioPolicy::configured(cfg.audio_enabled, cfg.audio_compressed),
        peer: client_hello.audio_output.clone(),
        enabled: quality.enable_audio,
        bitrate_kbps: quality.audio_bitrate_kbps,
    };
    let mut audio_stream = audio.resolve();
    if audio_stream.codec == Some(AudioCodec::Opus)
        && OpusEncoder::new(audio_stream.bitrate).is_err()
    {
        tracing::warn!(
            target: AUDIO,
            event = "audio_output_codec_unavailable",
            sid = %session_log_id,
            requested_codec = "opus",
            configured_compressed = cfg.audio_compressed,
            reason = "encoder_unavailable",
            "configured audio output codec is unavailable"
        );
        audio.policy = audio.policy.without_opus();
        audio_stream = audio.resolve();
    }
    tracing::info!(
        target: AUDIO,
        event = "audio_output_negotiated",
        sid = %session_log_id,
        enabled = audio_stream.is_enabled(),
        codec = ?audio_stream.codec,
        sample_rate_hz = 48_000u32,
        channels = 2u8,
        frame_duration_ms = 20u16,
        bitrate_kbps = audio_stream.bitrate.kbps(),
        configured_compressed = cfg.audio_compressed,
        requested_bitrate_kbps = quality.audio_bitrate_kbps,
        reason = ?audio_stream.reason,
        "audio output negotiation completed"
    );
    if let Some(result) = audio_stream.result() {
        send_json(ws, &result, arcen_protocol::messages::AUDIO_STREAM_RESULT).await?;
    }
    let microphone_policy = MicrophonePolicy {
        operator_enabled: cfg.microphone_input_enabled,
        backend_available: microphone_backend_available,
        codecs: arcen_media::audio::MicrophoneCodecAvailability {
            opus: true,
            pcm: true,
        },
    };
    let microphone_generation = next_microphone_generation()?;
    let mut microphone_stream = microphone_policy.resolve(
        client_hello.microphone_output.as_ref(),
        client_hello.microphone_output.is_some(),
        microphone_generation,
        64,
    );
    let binding = crate::microphone_input::MicrophoneSessionBinding::new(
        windows_session.session_id,
        windows_session.user_sid.clone(),
        microphone_generation,
    )
    .map_err(|_| "microphone session binding is invalid".to_string())?;
    let mut microphone = if microphone_stream.is_enabled() {
        let opened =
            match crate::microphone_input::NativeMicrophoneDevice::open(binding.clone()).await {
                Ok(device) => match crate::microphone_input::MicrophoneIngress::try_new(
                    binding,
                    microphone_stream,
                    device,
                ) {
                    Ok(ingress) => Ok(ingress.with_session_log_id(session_log_id.clone())),
                    Err((_error, mut device)) => match device.shutdown_wait().await {
                        Ok(()) => {
                            Err(crate::microphone_input::MicrophoneDeviceError::DeviceUnavailable)
                        }
                        Err(error) => Err(error),
                    },
                },
                Err(error) => Err(error),
            };
        match opened {
            Ok(ingress) => {
                tracing::info!(
                    target: AUDIO,
                    event = "mic_windows_feeder_started",
                    sid = %session_log_id,
                    generation = microphone_generation,
                    codec = ?microphone_stream.codec,
                    sample_rate_hz = 48_000u32,
                    channels = 1u8,
                    frame_duration_ms = 20u16,
                    endpoint_available = true,
                    "Windows microphone feeder started"
                );
                Some(ingress)
            }
            Err(error) if error.is_fatal_cleanup() => {
                return Err(format!(
                    "fatal microphone startup cleanup failure: {error:?}"
                ));
            }
            Err(error) => {
                tracing::warn!(
                    target: AUDIO,
                    event = microphone_device_failure_event(error),
                    sid = %session_log_id,
                    generation = microphone_generation,
                    reason = ?error,
                    "Windows microphone device did not start"
                );
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
    tracing::info!(
        target: AUDIO,
        event = "mic_negotiation",
        sid = %session_log_id,
        platform = "windows",
        enabled = microphone_stream.is_enabled(),
        operator_enabled = cfg.microphone_input_enabled,
        client_capability = client_hello.microphone_output.is_some(),
        backend_available = microphone_backend_available,
        codec = ?microphone_stream.codec,
        sample_rate_hz = 48_000u32,
        channels = 1u8,
        frame_duration_ms = 20u16,
        generation = microphone_generation,
        reason = ?microphone_stream.reason,
        "microphone negotiation completed"
    );
    let post_acquisition = async {
        send_json(
            ws,
            &microphone_stream.result(),
            arcen_protocol::messages::MICROPHONE_STREAM_RESULT,
        )
        .await?;
        send_json(
            ws,
            &tablet_mode_result,
            arcen_protocol::messages::TABLET_MODE_RESULT,
        )
        .await?;
        tracing::info!(
        target: SESSION,
        %peer,
        client = %client_hello.client_name,
        client_version = %client_hello.version,
        auth_requested = %requested,
        client_hello_requested = %client_hello_size,
        display_applied = %display_report.applied,
        display_backend = display_report.backend,
        display_exact = display_report.exact,
        requested_codec = %quality.codec,
        requested_chroma = %quality.chroma,
        requested_bit_depth = %quality.bit_depth,
        requested_color_range = %quality.color_range,
        requested_color_matrix = %quality.color_matrix,
        requested_encode_intent = %quality.encode_intent,
        requested_audio = quality.enable_audio,
        actual_codec = media_plan.codec_token(),
        actual_chroma = media_plan.chroma_token(),
        resolved_bit_depth = resolved_bit_depth.token(),
        resolved_color_range = resolved_color_range.token(),
        resolved_color_matrix = resolved_color_matrix.token(),
        // Same caveat, and no `actual_` counterpart to compare against:
        // intent changes how the encoder spends its budget, not the format it
        // announces, so `media_plan` has nothing to report it as.
        resolved_encode_intent = resolved_encode_intent.token(),
        actual_bit_depth = media_plan.bit_depth_token(),
        actual_color_range = media_plan.range_token(),
        actual_color_matrix = media_plan.matrix_token(),
        actual_audio = audio_stream.is_enabled(),
        clipboard_v1 = clipboard.is_some(),
        "capability exchange complete"
        );
        send_json(
            ws,
            &AgentStreamingReady {
                msg_type: AgentStreamingReady::TYPE.to_string(),
            },
            AgentStreamingReady::TYPE,
        )
        .await
    }
    .await;
    if let Err(error) = post_acquisition {
        if let Err(cleanup_error) =
            shutdown_microphone(&mut microphone, "attachment_setup_failure").await
        {
            return Err(format!("{error}; {cleanup_error}"));
        }
        return Err(error);
    }
    Ok(AttachmentPreferences {
        audio,
        microphone,
        cursor_mode: authoritative_cursor,
        tablet_mode_result,
        clipboard,
    })
}

struct NextAttachment {
    session_log_id: CorrelationId,
    transport_capability: &'static str,
}

async fn await_attachment_command<S>(
    ws: &mut WebSocketStream<S>,
    cfg: &HostConfig,
    display_encoder: crate::capenc::EncoderSelection,
    display_report: &DisplayReport,
    log_controller: &crate::logging::LogController,
    deskside_hook_proof: Option<crate::deskside::HookProof>,
    deskside_capture_binding: Option<StateFingerprint>,
) -> Result<Option<NextAttachment>, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut supervision = tokio::time::interval(Duration::from_secs(2));
    supervision.tick().await;
    loop {
        tokio::select! {
        message = ws.next() => match message {
            Some(Ok(Message::Text(text))) => {
                if let Some(control) = AgentControl::decode(text.as_ref())? {
                    let profile =
                        arcen_telemetry::OperationalProfile::try_from(control.profile_level)
                        .map_err(|error| error.to_string())?;
                    if control.use_configured_filter {
                        log_controller.reload_configured(profile)?;
                    } else {
                        log_controller.reload_profile(profile)?;
                    }
                    if control.reopen_generation > 0 {
                        log_controller.reopen_log()?;
                    }
                    continue;
                }
                let command = AgentAttachmentCommand::decode(text.as_ref())?
                    .ok_or_else(|| "unexpected broker message while detached".to_string())?;
                match command.action {
                    AgentAttachmentAction::Detach => {
                        send_json(
                            ws,
                            &AgentAttachmentStatus::detached(),
                            AgentAttachmentStatus::TYPE,
                        )
                        .await?;
                    }
                    AgentAttachmentAction::Validate => {
                        let valid = validate_held_output(cfg, display_encoder, display_report).is_ok()
                            && verify_deskside_supervision(
                                cfg,
                                deskside_hook_proof.as_ref(),
                                deskside_capture_binding,
                            )
                            .await
                            .is_ok();
                        send_json(
                            ws,
                            &AgentAttachmentStatus::validated(valid),
                            AgentAttachmentStatus::TYPE,
                        )
                        .await?;
                        if !valid {
                            return Err("held display topology changed".to_string());
                        }
                    }
                    AgentAttachmentAction::Attach => {
                        let session_log_id = command
                            .session_log_id
                            .as_deref()
                            .ok_or_else(|| "attachment command omitted session log id".to_string())
                            .and_then(|value| {
                                CorrelationId::parse_uuid(value.to_string())
                                    .map_err(|_| "attachment session log id is invalid".to_string())
                            })?;
                        let transport_capability = command
                            .transport_capability
                            .as_deref()
                            .ok_or_else(|| {
                                "attachment command omitted transport capability".to_string()
                            })
                            .and_then(validate_direct_transport)?;
                        return Ok(Some(NextAttachment {
                            session_log_id,
                            transport_capability,
                        }));
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => return Ok(None),
            Some(Ok(Message::Ping(payload))) => {
                send_ws_with_timeout(ws, Message::Pong(payload), WS_WRITE_TIMEOUT).await?;
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                return Err(format!("broker-agent IPC failed while detached: {error}"))
            }
        },
        _ = supervision.tick() => {
            verify_deskside_supervision(
                cfg,
                deskside_hook_proof.as_ref(),
                deskside_capture_binding,
            )
            .await?;
        }
        }
    }
}

/// Emits `DISPLAY_ARMED` (1200) only when the recovery journal is armed and
/// mutation is protected — i.e. the display transaction actually changed
/// something. An already-satisfied ("unchanged") transaction arms no journal
/// and emits nothing.
fn emit_display_armed(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    report: &DisplayReport,
    policy: crate::display::DisplayPolicy,
) {
    if !report.changed {
        return;
    }
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "display_backend",
        FieldValue::String(report.backend.to_string()),
    );
    let _ = fields.insert(
        "policy",
        FieldValue::String(display_policy_name(policy).to_string()),
    );
    let _ = fields.insert("changed", FieldValue::Boolean(report.changed));
    let _ = fields.insert(
        "width",
        FieldValue::Integer(i64::from(report.applied.width)),
    );
    let _ = fields.insert(
        "height",
        FieldValue::Integer(i64::from(report.applied.height)),
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::DisplayArmed,
        session_log_id,
        fields,
        Some(user.to_owned()),
        local_hostname(),
        Some(peer.to_owned()),
        None,
    );
}

const fn display_policy_name(policy: crate::display::DisplayPolicy) -> &'static str {
    match policy {
        crate::display::DisplayPolicy::ExactIsolated => "exact_isolated",
        crate::display::DisplayPolicy::Negotiated => "negotiated",
        crate::display::DisplayPolicy::NegotiatedMacroblock16 => "negotiated_macroblock16",
    }
}

fn next_microphone_generation() -> Result<u32, String> {
    static NEXT_GENERATION: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    NEXT_GENERATION
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |generation| Some(generation.wrapping_add(1).max(1)),
        )
        .map_err(|_| "microphone generation counter unavailable".to_string())
}

fn resolve_session_log_id(value: Option<&str>) -> Result<(CorrelationId, bool), String> {
    if let Some(value) = value {
        if let Ok(id) = CorrelationId::parse_uuid(value) {
            return Ok((id, false));
        }
    }
    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| format!("generate session log id fallback: {error}"))?;
    Ok((CorrelationId::from_uuid_v4_bytes(bytes), true))
}

async fn send_agent_failure<S>(ws: &mut WebSocketStream<S>, error: &str)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let failure = serde_json::json!({
        "type": AGENT_ERROR_TYPE,
        "message": error,
    });
    let _ = send_json(ws, &failure, "windows_session_agent_error").await;
}

fn validate_auth_response(response: &AuthResponse) -> Result<SessionDisplayPlan, String> {
    if response.method == AUTH_METHOD_RESUME {
        return Err("resume authentication must use the resume registry".to_string());
    }
    if response.resume_grant.is_some() {
        return Err("initial authentication carried a resume grant".to_string());
    }
    if !response.resume_requested && response.resume_holder_nonce.is_some() {
        return Err("resume holder nonce was supplied without opt-in".to_string());
    }
    if response.username.is_empty() || response.credential.is_empty() {
        return Err("empty Windows username or credential".to_string());
    }
    if response.username.len() > 256 || response.credential.len() > 4096 {
        return Err("Windows username or credential exceeds safety limit".to_string());
    }
    session_display_plan(response)
}

/// One host display to arm for the session. Single-display milestone plans
/// exactly one; multi-monitor Match-My-Layout grows this into a per-monitor
/// loop over host outputs (one lease + capenc + `VideoHeader.monitor_id` per
/// entry).
#[derive(Debug)]
struct MonitorPlan {
    request: DisplayRequest,
    is_primary: bool,
    client_monitor_id: u32,
    name: String,
}

/// The host-side interpretation of the client's `displays_mode` + monitor list.
#[derive(Debug)]
struct SessionDisplayPlan {
    monitors: Vec<MonitorPlan>,
    degradation: Option<DisplayPlanDegradation>,
}

#[derive(Debug)]
struct DisplayPlanDegradation {
    requested_mode: String,
    requested_monitors: usize,
    served_mode: &'static str,
    reason: &'static str,
    explanation: &'static str,
}

fn session_display_plan(response: &AuthResponse) -> Result<SessionDisplayPlan, String> {
    if response.monitors.len() > 8 {
        return Err("client monitor count exceeds safety limit".to_string());
    }
    let mut degradation = None;
    match response.displays_mode.as_str() {
        // Legacy clients (including the dormant Windows Deck) send no mode;
        // treat that as streaming the primary display.
        "" | "single_primary" | "windowed" => {}
        "match_layout" => {
            if response.monitors.len() > 1 {
                degradation = Some(DisplayPlanDegradation {
                    requested_mode: response.displays_mode.clone(),
                    requested_monitors: response.monitors.len(),
                    served_mode: "single_primary",
                    reason: "multi_monitor_match_layout_degraded",
                    explanation: "Windows hosts currently mirror a single display; serving the client primary monitor instead of refusing the session",
                });
            }
        }
        other => {
            return Err(format!(
                "display mode {other:?} is not supported by this host"
            ));
        }
    }
    let mut request = DisplayRequest::new(response.screen_width, response.screen_height)?;
    let Some(monitor) = response
        .monitors
        .iter()
        .find(|monitor| monitor.is_primary)
        .or_else(|| response.monitors.first())
    else {
        // Legacy handshake without monitor enumeration.
        return Ok(SessionDisplayPlan {
            monitors: vec![MonitorPlan {
                request,
                is_primary: true,
                client_monitor_id: 0,
                name: "Primary Display".to_string(),
            }],
            degradation,
        });
    };
    if monitor.width_px != request.size.width || monitor.height_px != request.size.height {
        return Err(format!(
            "client primary monitor {}x{} disagrees with the authenticated session size {}",
            monitor.width_px, monitor.height_px, request.size
        ));
    }
    request.refresh_hz = monitor.refresh_hz.max(1);
    request.width_mm = finite_nonnegative(monitor.width_mm, "client monitor width_mm")?;
    request.height_mm = finite_nonnegative(monitor.height_mm, "client monitor height_mm")?;
    request.scale = if monitor.scale.is_finite() && monitor.scale > 0.0 {
        monitor.scale
    } else {
        1.0
    };
    request.product_id = if monitor.model == 0 {
        0x0001
    } else {
        monitor.model as u16
    };
    request.serial = monitor.serial;
    Ok(SessionDisplayPlan {
        monitors: vec![MonitorPlan {
            request,
            is_primary: true,
            client_monitor_id: monitor.id,
            name: monitor.name.clone(),
        }],
        degradation,
    })
}

fn finite_nonnegative(value: f32, name: &str) -> Result<f32, String> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(format!("{name} must be finite and non-negative"))
    }
}

fn openh264_fitted_display_size(size: DisplaySize) -> Result<DisplaySize, String> {
    let request = arcen_media::video::MediaRequest {
        encoder: arcen_media::video::EncoderRequest::SoftwareH264,
        video: arcen_media::VideoConfiguration::legacy_h264(),
        width: size.width & !1,
        height: size.height & !1,
        fps: 30,
        cursor_mode: CursorMode::Local,
    };
    let candidate = arcen_media::video::BackendCandidate {
        backend: EncoderBackend::OpenH264,
        availability: arcen_media::video::BackendAvailability::Available(
            EncoderBackend::OpenH264.contract(),
        ),
    };
    let (plan, _) = arcen_media::video::resolve_media_plan_degrading(request, &[candidate])
        .map_err(|error| format!("fit display to OpenH264 contract: {error}"))?;
    DisplaySize::validate(plan.width, plan.height)
}

fn ensure_openh264_applied_size(applied: DisplaySize) -> Result<(), String> {
    if openh264_fitted_display_size(applied)? == applied {
        Ok(())
    } else {
        Err(format!(
            "display settled at {applied}, outside the OpenH264 limit of 1920x1200p30 \
             with even 4:2:0 dimensions"
        ))
    }
}

fn openh264_fallback_retarget_size(
    applied: DisplaySize,
    requested: DisplaySize,
    allow_geometry_retarget: bool,
) -> Result<Option<DisplaySize>, String> {
    if ensure_openh264_applied_size(applied).is_ok() {
        return Ok(None);
    }
    if !allow_geometry_retarget {
        return Err(format!("held display {applied} is not valid for OpenH264"));
    }
    openh264_fitted_display_size(requested).map(Some)
}

fn capenc_encoder_for_attempt(
    configured: crate::capenc::EncoderSelection,
    active: crate::capenc::EncoderSelection,
) -> crate::capenc::EncoderSelection {
    if configured == crate::capenc::EncoderSelection::Auto
        && active == crate::capenc::EncoderSelection::SoftwareH264
    {
        crate::capenc::EncoderSelection::SoftwareH264
    } else {
        configured
    }
}

#[cfg(windows)]
fn nvenc_runtime_present() -> bool {
    crate::gpu_probe::nvenc_runtime_dll()
}

#[cfg(not(windows))]
fn nvenc_runtime_present() -> bool {
    false
}

fn validate_client_hello(
    client_hello: &ClientHelloMsg,
    authenticated: DisplaySize,
) -> Result<DisplaySize, String> {
    let size = DisplaySize::validate(client_hello.screen_width, client_hello.screen_height)?;
    if size != authenticated {
        return Err(format!(
            "client_hello display {size} disagrees with authenticated display {authenticated}"
        ));
    }
    Ok(size)
}

#[derive(Debug, Clone, Copy)]
struct DisplayUpdateAvailability {
    supported: bool,
    reason: &'static str,
    explanation: &'static str,
}

impl DisplayUpdateAvailability {
    fn rejection_message(self) -> String {
        format!("{}: {}", self.reason, self.explanation)
    }
}

fn windows_display_update_availability(display: &DisplayReport) -> DisplayUpdateAvailability {
    if !display.exact {
        return DisplayUpdateAvailability {
            supported: false,
            reason: "display_lease_not_exact",
            explanation: "live resize requires an exact display lease; this session is using a negotiated or fallback display mode",
        };
    }
    if !display.retarget_capable {
        return DisplayUpdateAvailability {
            supported: false,
            reason: "display_backend_cannot_retarget",
            explanation: "display backend cannot retarget because it did not prove NVAPI custom timings with verified rollback capability",
        };
    }
    DisplayUpdateAvailability {
        supported: true,
        reason: "available",
        explanation: "live resize is available using NVAPI custom timings with verified rollback",
    }
}

fn windows_display_update_supported(display: &DisplayReport) -> bool {
    windows_display_update_availability(display).supported
}

fn display_resize_capability(display: &DisplayReport) -> serde_json::Value {
    let resize = windows_display_update_availability(display);
    let (mechanism, scope) = if resize.supported {
        ("nvapi_custom_timing", "arbitrary_custom_timing")
    } else {
        ("none", "none")
    };
    serde_json::json!({
        "available": resize.supported,
        "reason": resize.reason,
        "explanation": resize.explanation,
        "mechanism": mechanism,
        "scope": scope,
    })
}

fn display_plan_degradation_capability(plan: &SessionDisplayPlan) -> serde_json::Value {
    match plan.degradation.as_ref() {
        Some(degradation) => serde_json::json!({
            "active": true,
            "requested_mode": degradation.requested_mode.as_str(),
            "requested_monitors": degradation.requested_monitors,
            "served_mode": degradation.served_mode,
            "reason": degradation.reason,
            "explanation": degradation.explanation,
        }),
        None => serde_json::json!({
            "active": false,
        }),
    }
}

fn parse_display_update(text: &str) -> Option<DisplayUpdateMsg> {
    let update: DisplayUpdateMsg = serde_json::from_str(text).ok()?;
    (update.msg_type == DISPLAY_UPDATE).then_some(update)
}

fn validate_display_update(
    update: &DisplayUpdateMsg,
    media_plan: &ResolvedMediaPlan,
) -> Result<DisplaySize, String> {
    if update.sequence == 0 {
        return Err("display_update sequence must start at 1".to_string());
    }
    if !update.scale.is_finite() || update.scale < 0.0 {
        return Err("display_update scale must be finite and non-negative".to_string());
    }
    if update.reason.len() > 64 || update.reason.chars().any(char::is_control) {
        return Err("display_update reason is invalid".to_string());
    }
    let size = DisplaySize::validate(update.width, update.height)?;
    if size.height % 2 != 0 {
        return Err("display_update height must be even".to_string());
    }
    if media_plan.video.chroma == arcen_media::ChromaSubsampling::Yuv444 && size.width % 4 != 0 {
        return Err("YUV444 display_update width must be divisible by 4".to_string());
    }
    Ok(size)
}

fn resize_encoder_for(media_plan: &ResolvedMediaPlan) -> Option<crate::capenc::EncoderSelection> {
    match media_plan.backend {
        EncoderBackend::NativeNvenc => Some(crate::capenc::EncoderSelection::Nvenc),
        EncoderBackend::OpenH264 => Some(crate::capenc::EncoderSelection::SoftwareH264),
        EncoderBackend::WindowsMediaFoundation | EncoderBackend::Rav1e => None,
    }
}

fn resize_contract_matches(current: &ResolvedMediaPlan, candidate: &ResolvedMediaPlan) -> bool {
    current.backend == candidate.backend
        && current.video == candidate.video
        && current.fps == candidate.fps
        && current.cursor_mode == candidate.cursor_mode
        && current.cursor_in_video == candidate.cursor_in_video
        // Capability sets compared directly; adding a codec needs no edit here.
        && current.codecs == candidate.codecs
        && current.chroma == candidate.chroma
}

fn display_update_result(
    sequence: u64,
    accepted: bool,
    size: DisplaySize,
    message: impl Into<String>,
) -> DisplayUpdateResultMsg {
    DisplayUpdateResultMsg {
        sequence,
        accepted,
        width: size.width,
        height: size.height,
        message: message.into(),
        ..DisplayUpdateResultMsg::default()
    }
}

fn validate_client_cursor(
    client_hello: &ClientHelloMsg,
    authenticated: CursorMode,
) -> Result<(), String> {
    if client_hello.cursor_preference == authenticated {
        Ok(())
    } else {
        Err(
            "client_hello cursor preference disagrees with authenticated cursor preference"
                .to_string(),
        )
    }
}

fn resolve_windows_tablet_mode_result(
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
                "local termination unavailable: Windows Ink synthetic pen backend did not initialize"
            }
            TabletModeMsg::WacomUsbBridge => {
                // Reaches the operator verbatim: the Deck shows this reason
                // rather than inventing its own, so it has to say what is
                // missing and what to do instead.
                "Native tablet (USB bridged) is not available on Windows hosts yet. It needs a signed virtual USB host-controller driver, which Arcen does not ship. Use Tablet support instead: it needs no host driver and works over any network."
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

const fn runtime_input_capability(available: bool) -> InputCapabilityAvailability {
    if available {
        InputCapabilityAvailability::Available
    } else {
        InputCapabilityAvailability::Unavailable
    }
}

fn build_server_hello(
    cfg: &HostConfig,
    display: &DisplayReport,
    plan: &SessionDisplayPlan,
    windows_session: &WindowsSessionIdentity,
    agent_log_path: &str,
    media_plan: &ResolvedMediaPlan,
    microphone_backend_available: bool,
    pen_available: bool,
    region_input_available: bool,
) -> ServerHelloMsg {
    let rect = display.desktop_rect;
    // Honest, all-or-nothing: the synthetic PT_PEN device either exists
    // (pressure/tilt/rotation/eraser/proximity all real, from the same
    // POINTER_PEN_INFO sample) or it does not (older Windows, Windows Ink
    // disabled, or CreateSyntheticPointerDevice failed) — see `PenInjector`.
    let pen_capability = if pen_available {
        InputCapabilityAvailability::Available
    } else {
        InputCapabilityAvailability::Unavailable
    };
    let display_capability = serde_json::json!({
        "requested": {
            "width": display.requested.width,
            "height": display.requested.height,
        },
        "applied": {
            "width": display.applied.width,
            "height": display.applied.height,
            "refresh_hz": display.applied_refresh_hz,
        },
        "original": {
            "width": display.original.width,
            "height": display.original.height,
            "refresh_hz": display.original_refresh_hz,
        },
        "exact": display.exact,
        "changed": display.changed,
        "backend": display.backend,
        "restore_backend": display.restore_backend,
        "device_name": display.device_name,
        "selected_output_index": cfg.output_index,
        "capture_output_index": display.capture_output_index,
        "resize": display_resize_capability(display),
        "degradation": display_plan_degradation_capability(plan),
        "desktop_coordinates": {
            "left": rect.left,
            "top": rect.top,
            "width": rect.width,
            "height": rect.height,
        },
        "effective_display_scale": {
            "source": "GetDpiForMonitor(MDT_EFFECTIVE_DPI)",
            "baseline_dpi": 96,
            "monitors": display.effective_scale_reports,
        },
    });
    ServerHelloMsg {
        msg_type: SERVER_HELLO.to_string(),
        server_name: SERVER_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        os_user: String::new(),
        session_id: String::new(),
        session_type: String::new(),
        desktop: String::new(),
        screen_width: display.applied.width,
        screen_height: display.applied.height,
        // The applied host layout, one entry per planned client monitor, keyed
        // back to the client's monitor ids (ClientMonitor field names).
        monitors: plan
            .monitors
            .iter()
            .map(|monitor| {
                serde_json::json!({
                    "id": monitor.client_monitor_id,
                    "x": rect.left,
                    "y": rect.top,
                    "width_px": display.applied.width,
                    "height_px": display.applied.height,
                    "refresh_hz": display.applied_refresh_hz,
                    "scale": monitor.request.scale,
                    "is_primary": monitor.is_primary,
                    "name": monitor.name,
                    "device_name": display.device_name,
                    "capture_output_index": display.capture_output_index,
                })
            })
            .collect(),
        supports_h264: media_plan.supports_h264(),
        supports_h265: media_plan.supports_h265(),
        supports_av1: media_plan.supports_av1(),
        supports_yuv444: media_plan.supports_yuv444(),
        supports_audio: cfg.audio_enabled,
        audio_output: cfg
            .audio_enabled
            .then(|| AudioPolicy::configured(true, cfg.audio_compressed).capabilities()),
        microphone_input: MicrophonePolicy {
            operator_enabled: cfg.microphone_input_enabled,
            backend_available: microphone_backend_available,
            codecs: arcen_media::audio::MicrophoneCodecAvailability {
                opus: true,
                pcm: true,
            },
        }
        .capabilities(),
        supports_pen: false,
        // Windows has no raw-HID backend; this is the unrelated, unimplemented
        // experimental raw-HID surface, not the typed pen backend below.
        experimental_raw_hid: false,
        usb_hard_v1: false,
        supports_display_update: windows_display_update_supported(display),
        requires_auth: true,
        encoder_backend: media_plan.backend.ready_token().to_string(),
        // Declared by the backend rather than guessed by the client from the
        // token above. Additive metadata only; encoder selection is unchanged.
        encoder_class: media_plan.backend.accelerator_class().token().to_string(),
        available_encoders: BTreeMap::new(),
        codec: media_plan.codec_token().to_string(),
        color_caps: ServerColorCaps {
            // Backend capability -- what this resolved backend *could*
            // serve -- not what is currently active; `active_*` below
            // carries the currently active truth. Previously hardcoded
            // `false`/`false` for every host, which made these decorative.
            main10: media_plan.supports_main10(),
            main12: media_plan.bit_depths.contains(BitDepth::Twelve),
            chroma_422: media_plan
                .chroma
                .contains(arcen_media::ChromaSubsampling::Yuv422),
            chroma_444: media_plan.supports_yuv444(),
            full_range: media_plan.supports_full_range(),
            // Identity-matrix encode capability is a static per-backend
            // contract fact, not something the per-GPU probe narrows (see
            // `EncoderBackend::contract`'s doc: whether the *result*
            // survives a client decoder is a separate, measured
            // probe-matrix question).
            identity_matrix: media_plan.backend.contract().identity_matrix,
            active_bit_depth: media_plan.bit_depth_token().to_string(),
            active_range: media_plan.range_token().to_string(),
            active_matrix: media_plan.matrix_token().to_string(),
            active_primaries: media_plan.primaries_token().to_string(),
            active_transfer: media_plan.transfer_token().to_string(),
            advertised_pix_fmt: media_plan.chroma_token().to_string(),
            negotiated_state: "host_authoritative".to_string(),
        },
        input_protocol_version: INPUT_PROTOCOL_VERSION,
        input_capabilities: InputCapabilitiesMsg {
            absolute_pointer: InputCapabilityAvailability::Available,
            relative_pointer: InputCapabilityAvailability::Available,
            host_cursor: InputCapabilityAvailability::Available,
            region_input: runtime_input_capability(region_input_available),
            pen: pen_capability,
            pen_pressure: pen_capability,
            pen_tilt: pen_capability,
            pen_rotation: pen_capability,
            pen_eraser: pen_capability,
            pen_proximity: pen_capability,
        },
        tablet_mode_capabilities: TabletModeCapabilitiesMsg {
            local_termination: pen_capability,
            wacom_usb_bridge: InputCapabilityAvailability::Unavailable,
            disabled_mouse_compat: InputCapabilityAvailability::Available,
        },
        clipboard: Some(crate::clipboard::policy_message(cfg.clipboard_policy)),
        device_capabilities: BTreeMap::from([
            ("display_resolution".to_string(), display_capability),
            (
                "windows_session".to_string(),
                serde_json::json!({
                    "session_id": windows_session.session_id,
                    "user": windows_session.user,
                    "domain": windows_session.domain,
                    "account": windows_session.account_name(),
                    "state": windows_session.state,
                    "launch_backend": windows_session.launch_backend,
                    "creation_policy": "existing-session-only",
                    "desktop": r"winsta0\default",
                    "agent_log": agent_log_path,
                }),
            ),
            (
                "input".to_string(),
                serde_json::json!({
                    "available": true,
                    "backend": "sendinput",
                    "selected_output_index": display.capture_output_index,
                    "pen_backend": if pen_available { "synthetic_pointer" } else { "unavailable" },
                }),
            ),
        ]),
        negotiated_transport: None, // set from the active socket before transmission
    }
    .with_build_identity(windows_build_identity())
}

fn windows_build_identity() -> arcen_protocol::messages::BuildIdentityMsg {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    use std::sync::OnceLock;

    static ARTIFACT_HASH: OnceLock<Option<String>> = OnceLock::new();
    let artifact_sha256 = ARTIFACT_HASH
        .get_or_init(|| {
            let mut file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).ok()?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Some(format!("{:x}", hasher.finalize()))
        })
        .clone();
    arcen_protocol::messages::BuildIdentityMsg {
        product: "arcen-pier-windows".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build_id: option_env!("ARCEN_BUILD_ID")
            .unwrap_or("development")
            .to_string(),
        source_revision: option_env!("ARCEN_SOURCE_REVISION")
            .unwrap_or("unknown")
            .to_string(),
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
        .to_string(),
        feature_profile: option_env!("ARCEN_FEATURE_PROFILE")
            .unwrap_or("quic-default")
            .to_string(),
        artifact_sha256,
        signing_state: option_env!("ARCEN_SIGNING_STATE").map(str::to_string),
    }
}

async fn authenticate_with_deadline<F, T>(timeout: Duration, auth: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    tokio::time::timeout(timeout, auth)
        .await
        .map_err(|_| "Windows authentication timed out".to_string())?
}

trait ManagedAudioCapture: Send {
    fn telemetry(&self) -> AudioTelemetrySnapshot;
    fn telemetry_handle(&self) -> Arc<AudioTelemetry>;
    async fn shutdown(&mut self);
}

impl ManagedAudioCapture for AudioCapture {
    fn telemetry(&self) -> AudioTelemetrySnapshot {
        AudioCapture::telemetry(self)
    }

    fn telemetry_handle(&self) -> Arc<AudioTelemetry> {
        AudioCapture::telemetry_handle(self)
    }

    async fn shutdown(&mut self) {
        AudioCapture::shutdown(self).await;
    }
}

trait AudioCaptureFactory {
    type Capture: ManagedAudioCapture;

    fn start(&mut self, queue: Arc<LatestQueue<AudioPacket>>) -> Result<Self::Capture, String>;
}

struct WasapiCaptureFactory;

impl AudioCaptureFactory for WasapiCaptureFactory {
    type Capture = AudioCapture;

    fn start(&mut self, queue: Arc<LatestQueue<AudioPacket>>) -> Result<Self::Capture, String> {
        AudioCapture::start(queue)
    }
}

struct AudioSendState {
    enabled: AtomicBool,
    telemetry: RwLock<Option<Arc<AudioTelemetry>>>,
    stream: RwLock<ResolvedAudioStream>,
    generation: AtomicU64,
    encode_failures: AtomicU64,
    codec_failed: AtomicBool,
    codec_failure_notify: Notify,
}

impl Default for AudioSendState {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            telemetry: RwLock::new(None),
            stream: RwLock::new(ResolvedAudioStream::disabled(
                AudioProtocolMode::Legacy,
                arcen_protocol::messages::AudioStreamReason::NotNegotiated,
            )),
            generation: AtomicU64::new(0),
            encode_failures: AtomicU64::new(0),
            codec_failed: AtomicBool::new(false),
            codec_failure_notify: Notify::new(),
        }
    }
}

impl AudioSendState {
    fn activate(&self, telemetry: Arc<AudioTelemetry>, stream: ResolvedAudioStream) {
        *self
            .telemetry
            .write()
            .expect("audio telemetry lock poisoned") = Some(telemetry);
        self.set_stream(stream);
        self.enabled.store(true, Ordering::Release);
    }

    fn deactivate(&self) {
        self.enabled.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        *self
            .telemetry
            .write()
            .expect("audio telemetry lock poisoned") = None;
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    fn telemetry(&self) -> Option<Arc<AudioTelemetry>> {
        self.telemetry
            .read()
            .expect("audio telemetry lock poisoned")
            .clone()
    }

    fn set_stream(&self, stream: ResolvedAudioStream) {
        *self.stream.write().expect("audio stream lock poisoned") = stream;
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn stream(&self) -> (u64, ResolvedAudioStream) {
        (
            self.generation.load(Ordering::Acquire),
            *self.stream.read().expect("audio stream lock poisoned"),
        )
    }

    fn record_encode_failure(&self) {
        self.encode_failures.fetch_add(1, Ordering::Relaxed);
    }

    fn report_codec_failure(&self) {
        self.codec_failed.store(true, Ordering::Release);
        self.codec_failure_notify.notify_one();
    }

    async fn wait_for_codec_failure(&self) {
        loop {
            if self.codec_failed.swap(false, Ordering::AcqRel) {
                return;
            }
            self.codec_failure_notify.notified().await;
        }
    }
}

struct AudioRuntime<F: AudioCaptureFactory> {
    audio: AttachmentAudio,
    client_enabled: bool,
    bitrate_kbps: u32,
    queue: Arc<LatestQueue<AudioPacket>>,
    send_state: Arc<AudioSendState>,
    factory: F,
    capture: Option<F::Capture>,
}

impl AudioRuntime<WasapiCaptureFactory> {
    async fn start(audio: AttachmentAudio, queue: Arc<LatestQueue<AudioPacket>>) -> Self {
        let client_enabled = audio.enabled;
        let bitrate_kbps = audio.bitrate_kbps;
        let mut runtime = Self::with_audio(audio, queue, WasapiCaptureFactory);
        runtime
            .set_quality(client_enabled, bitrate_kbps, None)
            .await
            .expect("initial audio negotiation has no writer barrier");
        runtime
    }
}

impl<F: AudioCaptureFactory> AudioRuntime<F> {
    fn with_factory(host_enabled: bool, queue: Arc<LatestQueue<AudioPacket>>, factory: F) -> Self {
        Self::with_audio(
            AttachmentAudio {
                policy: AudioPolicy::configured(host_enabled, false),
                peer: None,
                enabled: false,
                bitrate_kbps: 128,
            },
            queue,
            factory,
        )
    }

    fn with_audio(
        audio: AttachmentAudio,
        queue: Arc<LatestQueue<AudioPacket>>,
        factory: F,
    ) -> Self {
        Self {
            audio,
            client_enabled: false,
            bitrate_kbps: 128,
            queue,
            send_state: Arc::new(AudioSendState::default()),
            factory,
            capture: None,
        }
    }

    fn send_state(&self) -> Arc<AudioSendState> {
        Arc::clone(&self.send_state)
    }

    fn telemetry(&self) -> Option<AudioTelemetrySnapshot> {
        self.capture.as_ref().map(ManagedAudioCapture::telemetry)
    }

    async fn set_client_enabled(
        &mut self,
        client_enabled: bool,
        writer_control: Option<&mpsc::Sender<WriterControl>>,
    ) -> Result<(), String> {
        self.set_quality(client_enabled, self.bitrate_kbps, writer_control)
            .await
    }

    async fn set_quality(
        &mut self,
        client_enabled: bool,
        bitrate_kbps: u32,
        writer_control: Option<&mpsc::Sender<WriterControl>>,
    ) -> Result<(), String> {
        let was_client_enabled = self.client_enabled;
        self.client_enabled = client_enabled;
        self.bitrate_kbps = bitrate_kbps;
        let stream = self.audio.update(client_enabled, bitrate_kbps);
        let should_capture = stream.is_enabled();

        if should_capture {
            if self.capture.is_some() {
                self.send_state.set_stream(stream);
                return Ok(());
            }
            self.queue.clear();
            match self.factory.start(Arc::clone(&self.queue)) {
                Ok(capture) => {
                    self.send_state.activate(capture.telemetry_handle(), stream);
                    self.capture = Some(capture);
                    tracing::info!(
                        target: AUDIO,
                        host_enabled = self.audio.policy.is_enabled(),
                        client_enabled,
                        "WASAPI capture started after audio negotiation"
                    );
                }
                Err(error) => {
                    self.send_state.deactivate();
                    tracing::warn!(target: AUDIO, %error, "WASAPI capture did not start");
                }
            }
            return Ok(());
        }

        let was_sending = self.send_state.is_enabled();
        self.send_state.deactivate();
        let barrier_result = if was_sending {
            if let Some(control) = writer_control {
                writer_audio_barrier(control).await
            } else {
                Ok(())
            }
        } else {
            Ok(())
        };
        if let Some(mut capture) = self.capture.take() {
            capture.shutdown().await;
            tracing::info!(
                target: AUDIO,
                host_enabled = self.audio.policy.is_enabled(),
                client_enabled,
                "WASAPI capture stopped after audio negotiation"
            );
        } else if was_client_enabled != client_enabled {
            tracing::info!(
                target: AUDIO,
                host_enabled = self.audio.policy.is_enabled(),
                client_enabled,
                "WASAPI capture remains disabled by audio negotiation"
            );
        }
        self.queue.clear();
        barrier_result
    }

    async fn shutdown(&mut self, writer_control: &mpsc::Sender<WriterControl>) {
        if let Err(error) = self.set_client_enabled(false, Some(writer_control)).await {
            tracing::warn!(target: AUDIO, %error, "audio writer barrier failed during shutdown");
        }
    }

    async fn recover_after_codec_failure(
        &mut self,
        writer_control: &mpsc::Sender<WriterControl>,
    ) -> Result<(), String> {
        let current = self.audio.resolve();
        let fallback_policy = self.audio.policy.without_opus();
        let fallback = fallback_policy.resolve(self.audio.peer.as_ref(), self.client_enabled);
        if current.codec == Some(AudioCodec::Opus) && fallback.codec == Some(AudioCodec::Pcm) {
            tracing::warn!(
                target: AUDIO,
                from_codec = ?current.codec,
                to_codec = ?fallback.codec,
                "Opus runtime failure detected; falling back to PCM"
            );
            self.audio.policy = fallback_policy;
            self.send_state.set_stream(fallback);
            self.queue.clear();
            if let Some(result) = fallback.result() {
                writer_audio_result_barrier(writer_control, result).await?;
            }
            return Ok(());
        }
        self.stop_after_codec_failure().await;
        if let Some(result) = ResolvedAudioStream::disabled(
            current.mode,
            arcen_protocol::messages::AudioStreamReason::CodecFailure,
        )
        .result()
        {
            writer_audio_result_barrier(writer_control, result).await?;
        }
        Ok(())
    }

    async fn stop_after_codec_failure(&mut self) {
        tracing::warn!(
            target: AUDIO,
            "audio codec disabled after repeated failures; no PCM fallback available"
        );
        self.client_enabled = false;
        self.audio.enabled = false;
        self.send_state.deactivate();
        if let Some(mut capture) = self.capture.take() {
            capture.shutdown().await;
        }
        self.queue.clear();
    }
}

impl<F: AudioCaptureFactory> Drop for AudioRuntime<F> {
    fn drop(&mut self) {
        self.send_state.deactivate();
        self.queue.clear();
    }
}

async fn writer_audio_barrier(control: &mpsc::Sender<WriterControl>) -> Result<(), String> {
    let (ack_tx, ack_rx) = oneshot::channel();
    tokio::time::timeout(
        WS_WRITE_TIMEOUT,
        control.send(WriterControl::AudioBarrier(ack_tx)),
    )
    .await
    .map_err(|_| "audio writer barrier enqueue timed out".to_string())?
    .map_err(|_| "outbound writer closed before audio barrier".to_string())?;
    tokio::time::timeout(WS_WRITE_TIMEOUT, ack_rx)
        .await
        .map_err(|_| "audio writer barrier timed out".to_string())?
        .map_err(|_| "outbound writer dropped audio barrier".to_string())
}

async fn send_display_update_result(
    control: &mpsc::Sender<WriterControl>,
    result: DisplayUpdateResultMsg,
) -> Result<(), String> {
    let text = serde_json::to_string(&result)
        .map_err(|error| format!("serialize display_update_result: {error}"))?;
    tokio::time::timeout(
        WS_WRITE_TIMEOUT,
        control.send(WriterControl::Message(Message::Text(text.into()))),
    )
    .await
    .map_err(|_| "timed out queueing display_update_result".to_string())?
    .map_err(|_| "outbound writer closed before display_update_result".to_string())?;
    writer_audio_barrier(control).await
}

async fn writer_audio_result_barrier(
    control: &mpsc::Sender<WriterControl>,
    result: AudioStreamResultMsg,
) -> Result<(), String> {
    let text = serde_json::to_string(&result)
        .map_err(|error| format!("serialize audio result: {error}"))?;
    tokio::time::timeout(
        WS_WRITE_TIMEOUT,
        control.send(WriterControl::Message(Message::Text(text.into()))),
    )
    .await
    .map_err(|_| "timed out queueing audio result".to_string())?
    .map_err(|_| "outbound writer closed before audio result".to_string())?;
    writer_audio_barrier(control).await
}

#[allow(clippy::too_many_arguments)]
struct AttachmentRun<S> {
    ws: WebSocketStream<S>,
    detached: bool,
    result: Result<(), AttachmentError>,
    sent_frames: u64,
    dropped_frames: u64,
}

struct PreparedAttachmentMedia {
    injector: Injector,
    /// `None` when the digitizer device could not be created (older Windows,
    /// Windows Ink unavailable, or transient API failure) — the attachment
    /// still proceeds mouse-only; see `create_pen_injector`.
    pen: Option<PenInjector>,
    /// Present only for a committed multi-monitor topology. Legacy
    /// single-monitor pointer messages remain isolated in `input.rs`.
    region_input: Option<RegionInputAdapter>,
    video: PreparedVideo,
}

impl PreparedAttachmentMedia {
    fn into_single(self) -> Option<(Injector, Option<PenInjector>, PreparedVideoPipeline)> {
        if self.region_input.is_some() {
            return None;
        }
        match self.video {
            PreparedVideo::Single(pipeline) => Some((self.injector, self.pen, pipeline)),
            PreparedVideo::Multi { .. } => None,
        }
    }
}

struct PreparedVideoPipeline {
    monitor_id: u16,
    capenc: Capenc,
    frames: Arc<VideoQueue<crate::capenc::EncodedFrame>>,
    initial_frame: Option<crate::capenc::EncodedFrame>,
    plan: ResolvedMediaPlan,
}

enum PreparedVideo {
    Single(PreparedVideoPipeline),
    Multi {
        pipelines: Vec<PreparedVideoPipeline>,
        capability: ServerMultiMonitorMsg,
    },
}

impl PreparedVideo {
    fn primary_plan(&self) -> ResolvedMediaPlan {
        match self {
            Self::Single(pipeline) => pipeline.plan,
            Self::Multi { pipelines, .. } => {
                pipelines
                    .first()
                    .expect("multi-monitor media has a primary pipeline")
                    .plan
            }
        }
    }

    fn primary_pipeline_telemetry(&self) -> crate::capenc::PipelineTelemetrySnapshot {
        match self {
            Self::Single(pipeline) => pipeline.capenc.pipeline_telemetry(),
            Self::Multi { pipelines, .. } => pipelines
                .first()
                .expect("multi-monitor media has a primary pipeline")
                .capenc
                .pipeline_telemetry(),
        }
    }

    fn multi_capability(&self) -> Option<&ServerMultiMonitorMsg> {
        match self {
            Self::Single(_) => None,
            Self::Multi { capability, .. } => Some(capability),
        }
    }

    fn is_multi_monitor(&self) -> bool {
        matches!(self, Self::Multi { .. })
    }

    fn region_stream_identity(&self, monitor_id: u16) -> Result<(u64, u64), String> {
        let Self::Multi { capability, .. } = self else {
            return if monitor_id == 0 {
                Ok((0, 0))
            } else {
                Err(format!(
                    "single-monitor pipeline produced region frame for monitor {monitor_id}"
                ))
            };
        };
        let topology = capability
            .applied_topology()
            .ok_or_else(|| "multi-monitor pipeline has no applied topology".to_string())?;
        topology
            .monitors()
            .iter()
            .find(|monitor| monitor.session_monitor_id == monitor_id)
            .map(|monitor| {
                (
                    topology.topology_generation(),
                    monitor.media_plan.stream_epoch,
                )
            })
            .ok_or_else(|| {
                format!("multi-monitor pipeline produced unadvertised monitor {monitor_id}")
            })
    }

    fn pipelines(&self) -> &[PreparedVideoPipeline] {
        match self {
            Self::Single(pipeline) => std::slice::from_ref(pipeline),
            Self::Multi { pipelines, .. } => pipelines,
        }
    }

    fn single_mut(&mut self) -> Option<&mut PreparedVideoPipeline> {
        match self {
            Self::Single(pipeline) => Some(pipeline),
            Self::Multi { .. } => None,
        }
    }

    fn request_keyframe_all(&self, reason: &'static str) {
        for pipeline in self.pipelines() {
            pipeline.capenc.request_keyframe(reason);
        }
    }

    fn close_frames(&self) {
        for pipeline in self.pipelines() {
            pipeline.frames.close();
        }
    }

    async fn shutdown(&mut self) {
        match self {
            Self::Single(pipeline) => pipeline.capenc.shutdown().await,
            Self::Multi { pipelines, .. } => {
                for pipeline in pipelines {
                    pipeline.capenc.shutdown().await;
                }
            }
        }
    }
}

enum RoutedFrameEvent {
    Frame {
        monitor_id: u16,
        plan: ResolvedMediaPlan,
        frame: crate::capenc::EncodedFrame,
        idr: crate::capenc::IdrRequester,
        ack: oneshot::Sender<()>,
    },
    Closed(u16),
}

struct RoutedFrameIngress {
    receiver: mpsc::Receiver<RoutedFrameEvent>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl RoutedFrameIngress {
    fn start(video: &PreparedVideo) -> Self {
        let pipelines = video.pipelines();
        let (sender, receiver) = mpsc::channel(pipelines.len().max(1));
        let mut tasks = Vec::with_capacity(pipelines.len());
        for pipeline in pipelines {
            let sender = sender.clone();
            let frames = Arc::clone(&pipeline.frames);
            let monitor_id = pipeline.monitor_id;
            let plan = pipeline.plan;
            let idr = pipeline.capenc.idr();
            let initial_frame = pipeline.initial_frame.clone();
            tasks.push(tokio::spawn(async move {
                if let Some(frame) = initial_frame {
                    let (ack, acknowledged) = oneshot::channel();
                    if sender
                        .send(RoutedFrameEvent::Frame {
                            monitor_id,
                            plan,
                            frame,
                            idr: idr.clone(),
                            ack,
                        })
                        .await
                        .is_err()
                        || acknowledged.await.is_err()
                    {
                        return;
                    }
                }
                loop {
                    let Some(frame) = frames.pop().await else {
                        let _ = sender.send(RoutedFrameEvent::Closed(monitor_id)).await;
                        break;
                    };
                    let (ack, acknowledged) = oneshot::channel();
                    if sender
                        .send(RoutedFrameEvent::Frame {
                            monitor_id,
                            plan,
                            frame,
                            idr: idr.clone(),
                            ack,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                    if acknowledged.await.is_err() {
                        break;
                    }
                }
            }));
        }
        drop(sender);
        Self { receiver, tasks }
    }

    async fn pop(&mut self) -> Option<RoutedFrameEvent> {
        self.receiver.recv().await
    }
}

impl Drop for RoutedFrameIngress {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Probes and creates the synthetic `PT_PEN` pointer device for the selected
/// output. This call is itself the honest Windows-version/API-availability
/// probe required before `ServerHello`: on success the exact same device is
/// kept for the whole attachment (no throw-away probe-then-destroy cycle);
/// on failure, pen is logged as unavailable and mouse input is unaffected.
fn create_pen_injector(
    output_index: u32,
    expected_output: crate::display::DesktopRect,
    session_log_id: &CorrelationId,
) -> Option<PenInjector> {
    match PenInjector::new(output_index, expected_output) {
        Ok(pen) => {
            tracing::info!(
                target: INPUT,
                sid = %session_log_id,
                output_index,
                "synthetic pen pointer device available"
            );
            Some(pen)
        }
        Err(error) => {
            tracing::info!(
                target: INPUT,
                sid = %session_log_id,
                output_index,
                %error,
                "synthetic pen pointer device unavailable; mouse fallback retained"
            );
            None
        }
    }
}

fn next_adaptive_nvenc_codec(config: &HostConfig) -> Option<VideoCodec> {
    if config.video_selection != arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
    {
        return None;
    }
    let current = crate::capenc::media_codec(config.codec);
    let ladder = arcen_media::video::adaptive_codec_ladder(current);
    let next = ladder
        .iter()
        .position(|codec| *codec == current)
        .and_then(|index| ladder.get(index + 1))
        .copied()?;
    Some(crate::capenc::protocol_codec(next))
}

async fn prepare_attachment_media(
    cfg: &HostConfig,
    mut display_encoder: crate::capenc::EncoderSelection,
    display: &mut DisplayLease,
    cursor_mode: CursorMode,
    session_log_id: &CorrelationId,
    deskside_capture_binding: Option<StateFingerprint>,
    allow_geometry_retarget: bool,
) -> Result<(PreparedAttachmentMedia, crate::capenc::EncoderSelection), String> {
    let mut attempt_cfg = cfg.clone();
    loop {
        let display_report = display.report();
        let resolved_output = validate_held_output(&attempt_cfg, display_encoder, display_report)?;
        if let Some(binding) = deskside_capture_binding {
            crate::deskside::verify_protected(&cfg.deskside, &cfg.output_selector, binding)?;
        }
        let (injector, mut config, ()) = start_capture_after_display(
            &attempt_cfg,
            display_report,
            &resolved_output,
            cursor_mode,
            session_log_id,
            Injector::new,
            |config| Ok((config, ())),
            |_| {},
        )?;
        let pen = create_pen_injector(
            resolved_output.global_index,
            display_report.desktop_rect,
            session_log_id,
        );
        config.encoder = Some(capenc_encoder_for_attempt(
            attempt_cfg.encoder,
            display_encoder,
        ));
        match Capenc::spawn(config).await {
            Ok((capenc, frames, plan)) => {
                capenc.request_keyframe("display_mode_settled_capture_restart");
                return Ok((
                    PreparedAttachmentMedia {
                        injector,
                        pen,
                        region_input: None,
                        video: PreparedVideo::Single(PreparedVideoPipeline {
                            monitor_id: 0,
                            capenc,
                            frames,
                            initial_frame: None,
                            plan,
                        }),
                    },
                    display_encoder,
                ));
            }
            Err(CapencStartError::Unavailable(notice))
                if cfg.encoder == crate::capenc::EncoderSelection::Auto
                    && display_encoder == crate::capenc::EncoderSelection::Nvenc
                    && notice.backend == EncoderBackend::NativeNvenc =>
            {
                drop(injector);
                drop(pen);
                if let Some(next_codec) = next_adaptive_nvenc_codec(&attempt_cfg) {
                    tracing::info!(
                        target: CAPENC,
                        sid = %session_log_id,
                        requested_codec = attempt_cfg.codec_name(),
                        selected_codec = crate::codec_name(next_codec),
                        "retrying adaptive performance on the same NVENC adapter"
                    );
                    attempt_cfg.codec = next_codec;
                    attempt_cfg.chroma = ChromaSubsampling::Yuv420;
                    if next_codec == VideoCodec::H264 {
                        attempt_cfg.bit_depth = BitDepth::Eight;
                    }
                    continue;
                }
                tracing::warn!(
                    target: CAPENC,
                    event = "media_encoder_fallback",
                    sid = %session_log_id,
                    requested_backend = "nvenc",
                    selected_backend = "openh264",
                    reason = ?notice.reason,
                    requested_codec = attempt_cfg.codec_name(),
                    requested_chroma = attempt_cfg.chroma_name(),
                    "video encoder fallback selected"
                );
                attempt_cfg
                    .apply_software_h264_backend(crate::capenc::EncoderSelection::SoftwareH264)
                    .map_err(|error| {
                        format!(
                            "NVENC is unavailable and OpenH264 cannot preserve the exact \
                             administrator video pin: {error}"
                        )
                    })?;
                if let Some(fitted) = openh264_fallback_retarget_size(
                    display.report().applied,
                    display.report().requested,
                    allow_geometry_retarget,
                )
                .map_err(|error| {
                    format!("capenc NVENC unavailable ({:?}); {error}", notice.reason)
                })? {
                    display.retarget_exact(fitted)?;
                    ensure_openh264_applied_size(display.report().applied)?;
                }
                display_encoder = crate::capenc::EncoderSelection::SoftwareH264;
            }
            Err(error) => return Err(format!("capenc READY failed: {error}")),
        }
    }
}

async fn prepare_multi_monitor_media(
    cfg: &HostConfig,
    display_encoder: crate::capenc::EncoderSelection,
    plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
    carrier: MultiMonitorCarrierMsg,
    cursor_mode: CursorMode,
    session_log_id: &CorrelationId,
    quality_intents: &BTreeMap<String, arcen_protocol::messages::MonitorQualityIntentMsg>,
) -> Result<PreparedAttachmentMedia, String> {
    if cfg.deskside.enabled {
        return Err(
            "multi-monitor is unavailable while deskside display isolation is enabled".to_string(),
        );
    }
    // The OpenH264 software fallback encodes nothing but h264/yuv420,
    // so a software session's template is pinned here rather than trusting a
    // configured codec/chroma that a late NVENC fallback may have outlived.
    // Any 4:4:4 client intent is still refused by `plan_encoder_sets`.
    let (codec, chroma) = match display_encoder {
        crate::capenc::EncoderSelection::Nvenc => (cfg.codec, cfg.chroma),
        crate::capenc::EncoderSelection::SoftwareH264 => {
            (VideoCodec::H264, ChromaSubsampling::Yuv420)
        }
        crate::capenc::EncoderSelection::Auto => {
            return Err(
                "multi-monitor requires a resolved encoder backend before media preparation"
                    .to_string(),
            );
        }
    };
    let template_fps = if display_encoder == crate::capenc::EncoderSelection::SoftwareH264 {
        cfg.fps.min(EncoderBackend::OpenH264.contract().max_fps)
    } else {
        cfg.fps
    };
    let template = crate::multi_monitor_capenc::MonitorPipelineTemplate {
        codec,
        chroma,
        bit_depth: cfg.bit_depth,
        color_range: cfg.color_range,
        color_matrix: cfg.color_matrix,
        intent: cfg.requested_encode_intent(),
        qp_map: cfg.qp_map,
        fps: template_fps,
        encoder: Some(display_encoder),
        video_selection: cfg.video_selection,
        cursor_mode,
        session_log_id: session_log_id.clone(),
    };
    let encoder_plan = crate::encoder_admission::plan_encoder_sets(
        plan,
        &template,
        quality_intents,
        &cfg.multi_monitor.allowed_adapters,
        cfg.multi_monitor.nvenc_session_limit,
        cfg.multi_monitor.allow_software_fallback && cfg.exact_pins_allow_software_h264(),
    )
    .map_err(|error| format!("plan multi-monitor encoder set: {error}"))?;
    let fresh_inventory = if cfg.iddcx.enabled {
        crate::iddcx::active_inventory(&cfg.iddcx)
    } else {
        crate::gpu_probe::physical_output_inventory(&cfg.multi_monitor.allowed_adapters)
    }
    .map_err(|error| format!("re-probe multi-monitor capture outputs: {error}"))?;
    let thresholds = arcen_media::EncoderAdmissionThresholds::from_qos_targets(cfg.qos_targets);
    let (encoder_plan, fresh_inventory, decision) = tokio::task::spawn_blocking(move || {
        let decision = encoder_plan.admit_runtime(thresholds, &fresh_inventory);
        (encoder_plan, fresh_inventory, decision)
    })
    .await
    .map_err(|error| format!("join multi-monitor encoder admission: {error}"))?;
    let decision =
        decision.map_err(|error| format!("measure multi-monitor encoder set: {error}"))?;
    crate::encoder_admission::emit_admission_telemetry(&decision);
    // The accepted candidate's specs *and* the negotiated media roster it was
    // admitted with: the roster names each region's committed bitrate budget,
    // which the applied capability publishes verbatim rather than re-deriving
    // it from the resolved geometry.
    let (specs, negotiated_media, selected_template) = encoder_plan
        .selected_specs(&decision)
        .map(<[_]>::to_vec)
        .zip(encoder_plan.selected_media_roster(&decision).cloned())
        .zip(encoder_plan.selected_template(&decision).cloned())
        .map(|((specs, media), template)| (specs, media, template))
        .ok_or_else(|| {
            "every exact multi-monitor encoder candidate failed measured admission".to_string()
        })?;
    let supervisor = crate::multi_monitor_capenc::MultiCapencSupervisor::start(
        plan.generation,
        &specs,
        &fresh_inventory,
        &selected_template,
    )
    .await
    .map_err(|error| format!("start multi-monitor capture: {error}"))?;
    let primary_id = plan
        .monitors
        .iter()
        .find(|monitor| monitor.primary)
        .map(|monitor| monitor.session_monitor_id)
        .ok_or_else(|| "multi-monitor plan has no primary output".to_string())?;
    let mut started = supervisor.into_pipelines();
    started.sort_by_key(|pipeline| pipeline.session_monitor_id != primary_id);

    let mut media = Vec::with_capacity(started.len());
    let mut pipelines: Vec<PreparedVideoPipeline> = Vec::with_capacity(started.len());
    for pipeline in started {
        let planned = plan
            .monitors
            .iter()
            .find(|monitor| monitor.session_monitor_id == pipeline.session_monitor_id)
            .ok_or_else(|| "started pipeline is absent from topology plan".to_string())?;
        if pipeline.plan.width != planned.width || pipeline.plan.height != planned.height {
            let mut capenc = pipeline.capenc;
            capenc.shutdown().await;
            for pipeline in &mut pipelines {
                pipeline.capenc.shutdown().await;
            }
            return Err(format!(
                "monitor {} capture resolved {}x{} instead of applied {}x{}",
                planned.session_monitor_id.get(),
                pipeline.plan.width,
                pipeline.plan.height,
                planned.width,
                planned.height
            ));
        }
        media.push((pipeline.session_monitor_id, pipeline.plan));
        pipelines.push(PreparedVideoPipeline {
            monitor_id: pipeline.session_monitor_id.get(),
            capenc: pipeline.capenc,
            frames: pipeline.frames,
            initial_frame: Some(pipeline.initial_frame),
            plan: pipeline.plan,
        });
    }
    let capability = crate::multi_monitor_gate::build_applied_capability(
        plan,
        carrier,
        &media,
        &negotiated_media,
    )
    .map_err(|error| format!("build applied multi-monitor capability: {error}"))?;
    let region_input = RegionInputAdapter::from_plan(plan)
        .map_err(|error| format!("initialize shared-region input: {error}"))?;
    let injector = Injector::new_region_desktop(region_input.desktop())
        .map_err(|error| format!("initialize multi-monitor input: {error}"))?;
    let pen = match PenInjector::new_region_desktop(region_input.desktop()) {
        Ok(pen) => Some(pen),
        Err(error) => {
            tracing::info!(
                target: INPUT,
                sid = %session_log_id,
                %error,
                "multi-monitor synthetic pen unavailable; mouse fallback retained"
            );
            None
        }
    };
    let video = PreparedVideo::Multi {
        pipelines,
        capability,
    };
    video.request_keyframe_all("multi_monitor_initial_idr");
    Ok(PreparedAttachmentMedia {
        injector,
        pen,
        region_input: Some(region_input),
        video,
    })
}

async fn prepare_resized_attachment_media(
    cfg: &HostConfig,
    display: &DisplayLease,
    cursor_mode: CursorMode,
    session_log_id: &CorrelationId,
    deskside_capture_binding: Option<StateFingerprint>,
    current_plan: &ResolvedMediaPlan,
) -> Result<PreparedAttachmentMedia, String> {
    let encoder = resize_encoder_for(current_plan)
        .ok_or_else(|| "active encoder cannot be recreated for display_update".to_string())?;
    let display_report = display.report();
    let resolved_output = validate_held_output(cfg, encoder, display_report)?;
    if let Some(binding) = deskside_capture_binding {
        crate::deskside::verify_protected(&cfg.deskside, &cfg.output_selector, binding)?;
    }
    let (injector, mut config, ()) = start_capture_after_display(
        cfg,
        display_report,
        &resolved_output,
        cursor_mode,
        session_log_id,
        Injector::new,
        |config| Ok((config, ())),
        |_| {},
    )?;
    let pen = create_pen_injector(
        resolved_output.global_index,
        display_report.desktop_rect,
        session_log_id,
    );
    config.encoder = Some(encoder);
    let (mut capenc, frames, plan) = Capenc::spawn(config)
        .await
        .map_err(|error| format!("replacement capenc READY failed: {error}"))?;
    if plan.width != display_report.applied.width
        || plan.height != display_report.applied.height
        || !resize_contract_matches(current_plan, &plan)
    {
        capenc.shutdown().await;
        return Err("replacement capenc changed geometry or the active media contract".to_string());
    }
    capenc.request_keyframe("display_update_capture_restart");
    Ok(PreparedAttachmentMedia {
        injector,
        pen,
        region_input: None,
        video: PreparedVideo::Single(PreparedVideoPipeline {
            monitor_id: 0,
            capenc,
            frames,
            initial_frame: None,
            plan,
        }),
    })
}

#[allow(clippy::too_many_arguments)]
async fn recover_previous_attachment_media(
    cfg: &HostConfig,
    display: &mut DisplayLease,
    previous_size: DisplaySize,
    cursor_mode: CursorMode,
    session_log_id: &CorrelationId,
    deskside_capture_binding: Option<StateFingerprint>,
    current_plan: &ResolvedMediaPlan,
) -> Result<PreparedAttachmentMedia, String> {
    display
        .retarget_exact(previous_size)
        .map_err(|error| format!("restore previous display mode: {error}"))?;
    prepare_resized_attachment_media(
        cfg,
        display,
        cursor_mode,
        session_log_id,
        deskside_capture_binding,
        current_plan,
    )
    .await
    .map_err(|error| format!("restart previous media generation: {error}"))
}

fn validate_held_output(
    cfg: &HostConfig,
    display_encoder: crate::capenc::EncoderSelection,
    display_report: &DisplayReport,
) -> Result<crate::display::ResolvedOutput, String> {
    let resolved_output = crate::display::resolve_output_selector(&cfg.output_selector)?;
    if !resolved_output
        .device_name
        .eq_ignore_ascii_case(&display_report.device_name)
    {
        return Err("held display output identity changed".to_string());
    }
    if display_encoder == crate::capenc::EncoderSelection::Nvenc
        && (resolved_output.vendor_id == 0x1414
            || resolved_output.adapter_name.contains("Microsoft Basic"))
    {
        return Err("held display no longer resolves to the authenticated GPU".to_string());
    }
    if resolved_output.desktop_rect != display_report.desktop_rect
        || resolved_output.global_index != display_report.capture_output_index
    {
        return Err("held display topology changed".to_string());
    }
    Ok(resolved_output)
}

async fn verify_deskside_supervision(
    cfg: &HostConfig,
    hook_proof: Option<&crate::deskside::HookProof>,
    capture_binding: Option<StateFingerprint>,
) -> Result<(), String> {
    if let Some(proof) = hook_proof {
        let proof = proof.clone();
        tokio::task::spawn_blocking(move || proof.probe())
            .await
            .map_err(|error| format!("deskside hook liveness task failed: {error}"))?
            .map_err(|error| format!("deskside hook liveness failed: {error}"))?;
    }
    if let Some(binding) = capture_binding {
        let config = cfg.deskside.clone();
        let selector = cfg.output_selector.clone();
        tokio::task::spawn_blocking(move || {
            crate::deskside::verify_protected(&config, &selector, binding)
        })
        .await
        .map_err(|error| format!("deskside topology task failed: {error}"))?
        .map_err(|error| format!("deskside protected topology changed: {error}"))?;
    }
    Ok(())
}

async fn take_and_stop_microphone<M, E, Stop, Stopped>(
    microphone: &mut Option<M>,
    stop: Stop,
) -> Result<(), E>
where
    Stop: FnOnce(M) -> Stopped,
    Stopped: std::future::Future<Output = Result<(), E>>,
{
    match microphone.take() {
        Some(microphone) => stop(microphone).await,
        None => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_session<S>(
    ws: WebSocketStream<S>,
    cfg: &HostConfig,
    peer: &str,
    user: &str,
    attachment_audio: AttachmentAudio,
    mut microphone: Option<
        crate::microphone_input::MicrophoneIngress<crate::microphone_input::NativeMicrophoneDevice>,
    >,
    cursor_mode: CursorMode,
    clipboard_negotiation: Option<ClipboardNegotiation>,
    display: &mut ActiveDisplayLease,
    prepared_media: PreparedAttachmentMedia,
    session_log_id: &CorrelationId,
    log_controller: crate::logging::LogController,
    initial_reopen_generation: u64,
    initial_applied_reopen_generation: u64,
    initial_qos_targets: arcen_telemetry::QosTargets,
    deskside_hook_proof: Option<crate::deskside::HookProof>,
    deskside_capture_binding: Option<StateFingerprint>,
    emitter: &LifecycleEmitter,
) -> Result<AttachmentRun<S>, AttachmentError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let display_report = display.report().clone();
    let capture_output_index = display_report.capture_output_index;
    let PreparedAttachmentMedia {
        mut injector,
        mut pen,
        mut region_input,
        video: mut video_pipelines,
    } = prepared_media;
    let mut media_plan = video_pipelines.primary_plan();
    let mut frame_ingress = RoutedFrameIngress::start(&video_pipelines);
    let (ws_tx, mut ws_rx) = ws.split();
    let video = match OutboundVideoMux::new(
        video_pipelines
            .pipelines()
            .iter()
            .map(|pipeline| pipeline.monitor_id),
    ) {
        Ok(mux) => Arc::new(mux),
        Err(error) => {
            if let Err(cleanup_error) =
                shutdown_microphone(&mut microphone, "attachment_setup_failure").await
            {
                return Err(cleanup_error);
            }
            return Err(AttachmentError::FatalCleanup(format!(
                "construct outbound video mux: {error}"
            )));
        }
    };
    let audio = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
    let clipboard = ClipboardWriterQueue::new();
    let clipboard_runtime = clipboard_negotiation
        .map(|negotiation| WindowsClipboardRuntime::start(negotiation, Arc::clone(&clipboard)))
        .transpose();
    let mut clipboard_runtime = match clipboard_runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            if let Err(cleanup_error) =
                shutdown_microphone(&mut microphone, "attachment_setup_failure").await
            {
                return Err(cleanup_error);
            }
            return Err(error.into());
        }
    };
    if clipboard_runtime.is_none() {
        clipboard.close();
    }
    let clipboard_reassembler = clipboard_negotiation
        .map(|negotiation| {
            arcen_protocol::clipboard::ClipboardReassembler::new(negotiation.policy().max_bytes)
        })
        .transpose()
        .map_err(|error| format!("initialize clipboard reassembly: {error}"));
    let mut clipboard_reassembler = match clipboard_reassembler {
        Ok(reassembler) => reassembler,
        Err(error) => {
            if let Err(cleanup_error) =
                shutdown_microphone(&mut microphone, "attachment_setup_failure").await
            {
                return Err(cleanup_error);
            }
            return Err(error.into());
        }
    };
    let mut audio_capture = AudioRuntime::start(attachment_audio, Arc::clone(&audio)).await;
    let (control_tx, control_rx) = mpsc::channel::<WriterControl>(CONTROL_QUEUE_CAPACITY);
    let audio_send_state = audio_capture.send_state();
    let video_stats = Arc::new(WriterVideoStats::default());
    let mut writer = tokio::spawn(writer_loop(
        ws_tx,
        video.clone(),
        audio.clone(),
        Arc::clone(&clipboard),
        control_rx,
        Arc::clone(&audio_send_state),
        Arc::clone(&video_stats),
    ));
    let mut dropped_frames = 0u64;
    let mut input_events = 0u64;
    let mut last_input_type = "";
    let mut input_sequence = InputSequenceTracker::default();
    let mut session_health = crate::observability::SessionHealth::new(initial_qos_targets);
    // Cursor shape streaming: poll GetCursorInfo at ~20 Hz in a dedicated OS
    // thread; only active when cursor mode is Local.
    let mut cursor_shape_rx = if cursor_mode == CursorMode::Local {
        crate::cursor_watcher::spawn()
    } else {
        None
    };
    let mut health_sequence = 0u64;
    let mut audio_health_ticks = 0u64;
    let mut network_probe_ticks = 0u64;
    let mut network_snapshot = crate::netinfo::snapshot(peer);
    let mut network_lost_at = None;
    let mut applied_reopen_generation = initial_applied_reopen_generation;
    let mut pending_reopen_generation = initial_reopen_generation;
    let mut last_log_control_error = None;
    let mut writer_finished = false;
    let mut completed_writer = None;
    let mut detached = false;
    let mut cursor_result_sent = false;
    let mut last_display_update_sequence = 0_u64;
    let mut last_display_update_at = None;
    let microphone_started_at = std::time::Instant::now();
    let mut microphone_order_warning = RateLimitedMicrophoneWarning::default();
    let mut microphone_protocol_warning = RateLimitedMicrophoneWarning::default();
    let mut microphone_authorization_warning = RateLimitedMicrophoneWarning::default();
    let mut unauthorized_microphone_frames = 0u64;
    let mut unauthorized_microphone_frames_interval = 0u64;
    let mut health = tokio::time::interval(std::time::Duration::from_secs(2));
    let mut microphone_tick = tokio::time::interval(std::time::Duration::from_millis(20));
    microphone_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    microphone_tick.tick().await;
    let microphone_generation = microphone.as_ref().map(|ingress| ingress.generation());
    health.tick().await;
    log_state(peer, ServerState::Streaming);
    tracing::info!(
        target: SESSION,
        event = "media_plan_resolved",
        sid = %session_log_id,
        %peer,
        encoder_backend = media_plan.backend.ready_token(),
        codec = media_plan.codec_token(),
        chroma = media_plan.chroma_token(),
        fps = media_plan.fps,
        requested_resolution = %display_report.requested,
        applied_resolution = %display_report.applied,
        display_backend = display_report.backend,
        capture_output_index,
        ready = true,
        idr_requested = true,
        "streaming started after display settle with fresh capture+encode and forced IDR"
    );
    let stream_started_at = std::time::Instant::now();
    emit_session_stream_start(
        emitter,
        session_log_id.clone(),
        user,
        peer,
        &media_plan,
        &display_report,
    );
    if let Some(network) = network_snapshot.as_ref() {
        crate::emit_lifecycle_event_with_context(
            emitter,
            LifecycleEventKind::NetworkPathActive,
            session_log_id.clone(),
            crate::netinfo::lifecycle_fields(&network),
            Some(user.to_owned()),
            local_hostname(),
            Some(peer.to_owned()),
            None,
        );
    }

    let mut fatal_cleanup = None;
    let stream_result: Result<(), String> = async {
        if audio_capture.audio.resolve().is_enabled() && !audio_capture.send_state.is_enabled() {
            writer_audio_result_barrier(
                &control_tx,
                AudioStreamResultMsg::disabled(
                    arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
                ),
            )
            .await?;
        }
        loop {
        tokio::select! {
            cursor_json = async { cursor_shape_rx.as_mut()?.recv().await },
                if cursor_shape_rx.is_some() =>
            {
                match cursor_json {
                    Some(json) => {
                        // Best-effort: if the channel is full, drop this shape update.
                        let _ = control_tx.try_send(WriterControl::Message(
                            Message::Text(json.into()),
                        ));
                    }
                    None => cursor_shape_rx = None,
                }
            }
            _ = microphone_tick.tick(), if microphone.is_some() => {
                if let Some(ingress) = microphone.as_mut() {
                    if let Err(error) = ingress.playout_tick() {
                        tracing::warn!(
                            target: AUDIO,
                            event = microphone_failure_event(error),
                            sid = %session_log_id,
                            generation = ingress.generation(),
                            reason = ?error,
                            "Windows microphone playout failed"
                        );
                        if let Err(error) =
                            shutdown_microphone(&mut microphone, "playout_failure").await
                        {
                            fatal_cleanup = Some(error.to_string());
                            break Err(error.to_string());
                        }
                        if control_tx.send(WriterControl::Message(Message::Text(
                            serde_json::to_string(
                                &arcen_protocol::messages::MicrophoneStreamResultMsg::disabled(
                                    arcen_protocol::messages::MicrophoneStreamReason::BackendUnavailable,
                                ),
                            )
                            .expect("microphone result serializes"),
                        ))).await.is_err() {
                            break Err(
                                "outbound writer closed before microphone disable".to_string()
                            );
                        }
                    }
                }
            }
            () = audio_send_state.wait_for_codec_failure() => {
                if let Err(error) = audio_capture.recover_after_codec_failure(&control_tx).await {
                    break Err(error);
                }
            }
            routed = frame_ingress.pop() => {
                let Some(routed) = routed else {
                    break Err("capenc frame router exited".to_string());
                };
                let (monitor_id, plan, frame, idr, ack) = match routed {
                    RoutedFrameEvent::Frame {
                        monitor_id,
                        plan,
                        frame,
                        idr,
                        ack,
                    } => (monitor_id, plan, frame, idr, ack),
                    RoutedFrameEvent::Closed(monitor_id) => {
                        break Err(format!("capenc engine exited for monitor {monitor_id}"));
                    }
                };
                if !cursor_result_sent {
                    let result = CursorModeResultMsg {
                        requested: cursor_mode,
                        active: cursor_mode,
                        accepted: true,
                        reason: CursorModeReason::default(),
                        ..CursorModeResultMsg::default()
                    };
                    let text = serde_json::to_string(&result)
                        .map_err(|error| format!("serialize cursor mode result: {error}"))?;
                    if control_tx
                        .send(WriterControl::Message(Message::Text(text.into())))
                        .await
                        .is_err()
                    {
                        break Err("outbound writer closed before cursor confirmation".to_string());
                    }
                    cursor_result_sent = true;
                }
                let (topology_generation, stream_epoch) =
                    match video_pipelines.region_stream_identity(monitor_id) {
                        Ok(identity) => identity,
                        Err(error) => break Err(error),
                    };
                let message = frame_message(
                    &plan,
                    &frame,
                    monitor_id,
                    topology_generation,
                    stream_epoch,
                );
                match video.push(
                    monitor_id,
                    OutboundVideo {
                        message: Message::Binary(message.into()),
                    },
                    frame.keyframe,
                ) {
                    VideoPushResult::Enqueued { cleared } => {
                        dropped_frames += cleared as u64;
                    }
                    VideoPushResult::Dropped {
                        count,
                        recovery_started,
                    } => {
                        dropped_frames += count as u64;
                        let reason = if recovery_started {
                            "websocket_video_queue_drop"
                        } else {
                            "websocket_video_queue_awaiting_keyframe"
                        };
                        idr.request(reason);
                        tracing::debug!(
                            target: SESSION,
                            dropped_frames,
                            recovery_started,
                            "outbound video AU suppressed until replacement IDR"
                        );
                    }
                    VideoPushResult::Closed(_) => {
                        break Err("outbound writer closed".to_string());
                    }
                }
                let _ = ack.send(());
            }
            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                       let microphone_stop = serde_json::from_str::<serde_json::Value>(&text)
                           .ok()
                           .filter(|value| msg_type(value) == Some(MICROPHONE_STREAM_STOP));
                       if let Some(value) = microphone_stop {
                           let stop = serde_json::from_value::<MicrophoneStreamStopMsg>(value);
                           match stop {
                               Ok(stop)
                                   if stop.is_valid()
                                       && microphone_generation == Some(stop.generation) =>
                               {
                                   if let Err(error) =
                                       shutdown_microphone(&mut microphone, "client_stop").await
                                   {
                                       fatal_cleanup = Some(error.to_string());
                                       break Err(error.to_string());
                                   }
                                   tracing::info!(
                                       target: AUDIO,
                                       reason = ?stop.reason,
                                       "client stopped microphone publication"
                                   );
                                   continue;
                               }
                               _ => break Err(
                                   "invalid microphone_stream_stop for active generation".to_string()
                               ),
                           }
                       }
                       if is_clipboard_data_message(&text) {
                           handle_clipboard_offer(
                               text.as_ref(),
                               clipboard_negotiation,
                               clipboard_reassembler.as_mut(),
                           );
                           continue;
                       }
                       match AgentAttachmentCommand::decode(text.as_ref()) {
                            Ok(Some(command)) if command.action == AgentAttachmentAction::Detach => {
                                detached = true;
                                break Ok(());
                            }
                            Ok(Some(_)) => {
                                break Err(
                                    "attachment command is invalid while media is active".to_string()
                                );
                            }
                            Ok(None) => {}
                            Err(error) => break Err(error),
                        }
                        match AgentControl::decode(text.as_ref()) {
                            Ok(Some(control)) => {
                                let mut errors = Vec::new();
                                let profile = arcen_telemetry::OperationalProfile::try_from(
                                    control.profile_level,
                                )
                                    .map_err(|error| error.to_string())?;
                                let filter_result = if control.use_configured_filter {
                                    log_controller.reload_configured(profile)
                                } else {
                                    log_controller.reload_profile(profile)
                                };
                                session_health.set_targets(control.qos_targets);
                                if let Err(error) = filter_result {
                                    errors.push(error);
                                }
                                pending_reopen_generation =
                                    pending_reopen_generation.max(control.reopen_generation);
                                if pending_reopen_generation > applied_reopen_generation {
                                    match log_controller.reopen_log() {
                                        Ok(()) => {
                                            applied_reopen_generation = pending_reopen_generation;
                                        }
                                        Err(error) => errors.push(error),
                                    }
                                }
                                if errors.is_empty() {
                                    last_log_control_error = None;
                                } else {
                                    report_log_control_error(
                                        errors.join("; "),
                                        &mut last_log_control_error,
                                    );
                                }
                                tracing::info!(
                                    target: SESSION,
                                    sequence = control.sequence,
                                    profile_level = control.profile_level,
                                    configured = control.use_configured_filter,
                                    reopen_generation = control.reopen_generation,
                                    "private agent logging control applied"
                                );
                                continue;
                            }
                            Ok(None) => {}
                            Err(error) => break Err(error),
                        }
                        if let Some(update) = parse_display_update(text.as_ref()) {
                            let current_size = display.report().applied;
                            if video_pipelines.is_multi_monitor() {
                                send_display_update_result(
                                    &control_tx,
                                    display_update_result(
                                        update.sequence,
                                        false,
                                        current_size,
                                        "multi-monitor topology is fixed; reconnect after changing displays",
                                    ),
                                )
                                .await?;
                                continue;
                            }
                            let resize_availability =
                                windows_display_update_availability(display.report());
                            if !resize_availability.supported {
                                send_display_update_result(
                                    &control_tx,
                                    display_update_result(
                                        update.sequence,
                                        false,
                                        current_size,
                                        resize_availability.rejection_message(),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                            if update.sequence <= last_display_update_sequence {
                                send_display_update_result(
                                    &control_tx,
                                    display_update_result(
                                        update.sequence,
                                        false,
                                        current_size,
                                        "stale display_update sequence",
                                    ),
                                )
                                .await?;
                                continue;
                            }
                            last_display_update_sequence = update.sequence;
                            let requested = match validate_display_update(&update, &media_plan) {
                                Ok(requested) => requested,
                                Err(error) => {
                                    tracing::warn!(
                                        target: DISPLAY,
                                        sequence = update.sequence,
                                        width = update.width,
                                        height = update.height,
                                        %error,
                                        "rejected invalid display_update"
                                    );
                                    send_display_update_result(
                                        &control_tx,
                                        display_update_result(
                                            update.sequence,
                                            false,
                                            current_size,
                                            error,
                                        ),
                                    )
                                    .await?;
                                    continue;
                                }
                            };
                            if requested == current_size {
                                send_display_update_result(
                                    &control_tx,
                                    display_update_result(
                                        update.sequence,
                                        true,
                                        current_size,
                                        "",
                                    ),
                                )
                                .await?;
                                continue;
                            }
                            if let Some(last) = last_display_update_at {
                                let elapsed = std::time::Instant::now().duration_since(last);
                                if elapsed < DISPLAY_UPDATE_MIN_INTERVAL {
                                    tokio::time::sleep(DISPLAY_UPDATE_MIN_INTERVAL - elapsed).await;
                                }
                            }
                            tracing::info!(
                                target: DISPLAY,
                                sequence = update.sequence,
                                requested = %requested,
                                reason = %update.reason,
                                "mid-session stream resize begin"
                            );

                            let single = video_pipelines
                                .single_mut()
                                .expect("multi-monitor resize rejected above");
                            single.capenc.shutdown().await;
                            single.frames.close();
                            single.frames.clear();
                            dropped_frames =
                                dropped_frames.saturating_add(video.clear() as u64);
                            writer_audio_barrier(&control_tx).await?;

                            if let Err(error) = display
                                .single_mut()
                                .expect("multi-monitor display updates are rejected above")
                                .retarget_exact(requested)
                            {
                                tracing::warn!(
                                    target: DISPLAY,
                                    sequence = update.sequence,
                                    %error,
                                    "mid-session display retarget failed"
                                );
                                match recover_previous_attachment_media(
                                    cfg,
                                    display
                                        .single_mut()
                                        .expect("display-update recovery is single-monitor"),
                                    current_size,
                                    cursor_mode,
                                    session_log_id,
                                    deskside_capture_binding,
                                    &media_plan,
                                )
                                .await
                                {
                                    Ok(previous) => {
                                        let (new_injector, new_pen, pipeline) = previous
                                            .into_single()
                                            .expect("resize recovery is single-monitor");
                                        injector = new_injector;
                                        pen = new_pen;
                                        media_plan = pipeline.plan;
                                        video_pipelines = PreparedVideo::Single(pipeline);
                                        frame_ingress =
                                            RoutedFrameIngress::start(&video_pipelines);
                                        last_display_update_at =
                                            Some(std::time::Instant::now());
                                        send_display_update_result(
                                            &control_tx,
                                            display_update_result(
                                                update.sequence,
                                                false,
                                                display.report().applied,
                                                format!(
                                                    "display retarget failed: {error}"
                                                ),
                                            ),
                                        )
                                        .await?;
                                        tracing::warn!(
                                            target: DISPLAY,
                                            sequence = update.sequence,
                                            applied = %current_size,
                                            "mid-session display_update rejected; previous media restored and attachment retained"
                                        );
                                        continue;
                                    }
                                    Err(recovery_error) => {
                                        send_display_update_result(
                                            &control_tx,
                                            display_update_result(
                                                update.sequence,
                                                false,
                                                display.report().applied,
                                                format!(
                                                    "display retarget failed: {error}; recovery failed: {recovery_error}"
                                                ),
                                            ),
                                        )
                                        .await?;
                                        break Err(format!(
                                            "display_update failed and previous media could not be restored: {error}; {recovery_error}"
                                        ));
                                    }
                                }
                            }

                            let replacement = match prepare_resized_attachment_media(
                                cfg,
                                display
                                    .single_mut()
                                    .expect("display-update replacement is single-monitor"),
                                cursor_mode,
                                session_log_id,
                                deskside_capture_binding,
                                &media_plan,
                            )
                            .await
                            {
                                Ok(replacement) => replacement,
                                Err(error) => {
                                    tracing::warn!(
                                        target: CAPENC,
                                        sequence = update.sequence,
                                        %error,
                                        "replacement media generation failed after display_update"
                                    );
                                    match recover_previous_attachment_media(
                                        cfg,
                                        display
                                            .single_mut()
                                            .expect("display-update recovery is single-monitor"),
                                        current_size,
                                        cursor_mode,
                                        session_log_id,
                                        deskside_capture_binding,
                                        &media_plan,
                                    )
                                    .await
                                    {
                                        Ok(previous) => {
                                            let (new_injector, new_pen, pipeline) = previous
                                                .into_single()
                                                .expect("resize recovery is single-monitor");
                                            injector = new_injector;
                                            pen = new_pen;
                                            media_plan = pipeline.plan;
                                            video_pipelines = PreparedVideo::Single(pipeline);
                                            frame_ingress =
                                                RoutedFrameIngress::start(&video_pipelines);
                                            last_display_update_at =
                                                Some(std::time::Instant::now());
                                            send_display_update_result(
                                                &control_tx,
                                                display_update_result(
                                                    update.sequence,
                                                    false,
                                                    display.report().applied,
                                                    format!(
                                                        "capture restart failed: {error}"
                                                    ),
                                                ),
                                            )
                                            .await?;
                                            tracing::warn!(
                                                target: DISPLAY,
                                                sequence = update.sequence,
                                                applied = %current_size,
                                                "mid-session display_update rejected; previous media restored and attachment retained"
                                            );
                                            continue;
                                        }
                                        Err(recovery_error) => {
                                            send_display_update_result(
                                                &control_tx,
                                                display_update_result(
                                                    update.sequence,
                                                    false,
                                                    display.report().applied,
                                                    format!(
                                                        "capture restart failed: {error}; recovery failed: {recovery_error}"
                                                    ),
                                                ),
                                            )
                                            .await?;
                                            break Err(format!(
                                                "display_update replacement failed and previous media could not be restored: {error}; {recovery_error}"
                                            ));
                                        }
                                    }
                                }
                            };
                            let (new_injector, new_pen, pipeline) = replacement
                                .into_single()
                                .expect("display_update replacement is single-monitor");
                            injector = new_injector;
                            pen = new_pen;
                            media_plan = pipeline.plan;
                            video_pipelines = PreparedVideo::Single(pipeline);
                            frame_ingress = RoutedFrameIngress::start(&video_pipelines);
                            last_display_update_at = Some(std::time::Instant::now());
                            let applied = display.report().applied;
                            let capture_output_index = display.report().capture_output_index;
                            send_display_update_result(
                                &control_tx,
                                display_update_result(update.sequence, true, applied, ""),
                            )
                            .await?;
                            tracing::info!(
                                target: DISPLAY,
                                sequence = update.sequence,
                                requested = %requested,
                                applied = %applied,
                                capture_output_index,
                                "mid-session stream resize applied; replacement media ready"
                            );
                            continue;
                        }
                        let inbound = handle_inbound(
                            text.as_ref(),
                            &mut injector,
                            &mut pen,
                            &mut region_input,
                            &video_pipelines,
                            &mut input_sequence,
                        ).await;
                        if let Some(telemetry) = inbound.client_telemetry {
                            session_health.record_client_at(now_ms_u64(), Some(telemetry));
                        }
                        if let Some(input_type) = inbound.input_type {
                            input_events = input_events.saturating_add(1);
                            last_input_type = input_type;
                        }
                        if let Some((enabled, bitrate_kbps)) = inbound.audio_quality {
                            let target = audio_capture.audio.policy.resolve(
                                audio_capture.audio.peer.as_ref(),
                                enabled,
                            );
                            let preannounced = target.is_enabled() && target.result().is_some();
                            if preannounced {
                                writer_audio_result_barrier(
                                    &control_tx,
                                    target
                                        .result()
                                        .expect("audio-v1 enabled stream has a result"),
                                )
                                .await?;
                            }
                            if let Err(error) = audio_capture
                                .set_quality(enabled, bitrate_kbps, Some(&control_tx))
                                .await {
                                break Err(error);
                            }
                            if preannounced && !audio_capture.send_state.is_enabled() {
                                writer_audio_result_barrier(
                                    &control_tx,
                                    AudioStreamResultMsg::disabled(
                                        arcen_protocol::messages::AudioStreamReason::CaptureUnavailable,
                                    ),
                                )
                                .await?;
                            } else if !preannounced {
                                if let Some(result) = audio_capture.audio.resolve().result() {
                                    writer_audio_result_barrier(&control_tx, result).await?;
                                }
                            }
                        }
                        if let Some(reply) = inbound.reply {
                            match control_tx
                                .try_send(WriterControl::Message(Message::Text(reply.into())))
                            {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    tracing::debug!(target: SESSION, "best-effort control reply dropped");
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    break Err("outbound writer closed".to_string());
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        if bytes.first().copied() == Some(FrameType::AudioUpstream as u8) {
                            if let Some(ingress) = microphone.as_mut() {
                                match ingress.ingest(&bytes) {
                                    Ok(arcen_media::audio::MicrophoneIngestOutcome::DroppedWrongGeneration) => {
                                        if let Some(suppressed) = microphone_order_warning.observe() {
                                            tracing::warn!(
                                                target: AUDIO,
                                                event = "mic_frame_rejected",
                                                sid = %session_log_id,
                                                generation = ingress.generation(),
                                                reason = "wrong_generation",
                                                suppressed_since_last = suppressed,
                                                "microphone frame rejected"
                                            );
                                        }
                                    }
                                    Ok(arcen_media::audio::MicrophoneIngestOutcome::Reset) => {
                                        if let Some(suppressed) = microphone_order_warning.observe() {
                                            tracing::warn!(
                                                target: AUDIO,
                                                event = "mic_frame_rejected",
                                                sid = %session_log_id,
                                                generation = ingress.generation(),
                                                reason = "discontinuity",
                                                suppressed_since_last = suppressed,
                                                "microphone ordering discontinuity reset"
                                            );
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(error) => {
                                        if let Some(suppressed) =
                                            microphone_protocol_warning.observe()
                                        {
                                            tracing::warn!(
                                                target: AUDIO,
                                                event = "mic_frame_rejected",
                                                sid = %session_log_id,
                                                generation = ingress.generation(),
                                                reason = ?error,
                                                suppressed_since_last = suppressed,
                                                "malformed or unauthorized microphone frame rejected"
                                            );
                                        }
                                    }
                                }
                            } else {
                                unauthorized_microphone_frames =
                                    unauthorized_microphone_frames.saturating_add(1);
                                unauthorized_microphone_frames_interval =
                                    unauthorized_microphone_frames_interval.saturating_add(1);
                                if let Some(suppressed) =
                                    microphone_authorization_warning.observe()
                                {
                                    tracing::warn!(
                                        target: AUDIO,
                                        event = "mic_frame_rejected",
                                        sid = %session_log_id,
                                        reason = "not_negotiated",
                                        suppressed_since_last = suppressed,
                                        "unauthorized microphone frame rejected"
                                    );
                                }
                            }
                        } else if bytes.first().copied() == Some(FrameType::Clipboard as u8) {
                            handle_clipboard_chunk(
                                &bytes,
                                clipboard_negotiation,
                                clipboard_reassembler.as_mut(),
                                clipboard_runtime.as_ref(),
                            );
                        } else {
                            tracing::warn!(target: SESSION, "unexpected client binary frame dropped");
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        if control_tx
                            .try_send(WriterControl::Message(Message::Close(None)))
                            .is_err()
                        {
                            tracing::debug!(target: SESSION, "close reply not queued");
                        }
                        break Ok(());
                    }
                    None => break Ok(()),
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break Err(format!("WebSocket receive failed: {error}")),
                }
            }
            _ = health.tick() => {
                if let Err(error) = verify_deskside_supervision(
                    cfg,
                    deskside_hook_proof.as_ref(),
                    deskside_capture_binding,
                )
                .await
                {
                    break Err(error);
                }
                if let Some(reassembler) = clipboard_reassembler.as_mut() {
                    let _ = reassembler.expire(std::time::Instant::now());
                }
                if pending_reopen_generation > applied_reopen_generation {
                    match log_controller.reopen_log() {
                        Ok(()) => {
                            applied_reopen_generation = pending_reopen_generation;
                            last_log_control_error = None;
                        }
                        Err(error) => {
                            report_log_control_error(error, &mut last_log_control_error);
                        }
                    }
                }
                health_sequence += 1;
                audio_health_ticks += 1;
                network_probe_ticks += 1;
                if network_probe_ticks.is_multiple_of(5) {
                    let next_network = crate::netinfo::snapshot(peer);
                    let transition = match (&network_snapshot, &next_network) {
                        (Some(old), Some(new)) if old != new => Some((
                            LifecycleEventKind::NetworkPathChanged,
                            crate::netinfo::changed_fields(old, new),
                        )),
                        (Some(old), None) => {
                            network_lost_at = Some(now_ms_u64());
                            Some((
                                LifecycleEventKind::NetworkPathLost,
                                crate::netinfo::lost_fields(old),
                            ))
                        }
                        (None, Some(new)) => Some((
                            LifecycleEventKind::NetworkPathRestored,
                            crate::netinfo::restored_fields(
                                new,
                                network_lost_at
                                    .take()
                                    .map_or(0, |lost| now_ms_u64().saturating_sub(lost)),
                            ),
                        )),
                        _ => None,
                    };
                    if let Some((kind, fields)) = transition {
                        crate::emit_lifecycle_event_with_context(
                            emitter,
                            kind,
                            session_log_id.clone(),
                            fields,
                            Some(user.to_owned()),
                            local_hostname(),
                            Some(peer.to_owned()),
                            None,
                        );
                    }
                    network_snapshot = next_network;
                }
                let pong = HealthPongMsg {
                    msg_type: HEALTH_PONG.to_string(),
                    ping_timestamp_ms: 0,
                    sequence: health_sequence,
                    server_timestamp_ms: now_ms_u64(),
                    server_state: ServerState::Streaming.as_str().to_string(),
                };
                let timestamp_ms = now_ms_u64();
                let (sent_frames, bytes_sent) = video_stats.snapshot();
                let counters = crate::observability::HostCounters {
                    frames_sent: sent_frames,
                    frames_dropped: dropped_frames,
                    bytes_sent,
                    input_events,
                    last_input_sequence: input_sequence.last_nonzero(),
                    last_input_type,
                };
                let observation = session_health.observe(timestamp_ms, cfg.fps, counters);
                let pipeline = video_pipelines.primary_pipeline_telemetry();
                let mut stats = crate::observability::SessionHealth::health_stats(
                    &observation,
                    counters,
                    cfg.fps,
                    media_plan.codec_token(),
                    media_plan.chroma_token(),
                    display_report.applied.to_string(),
                );
                stats.capture_time_ms = pipeline.average_stage_ms;
                stats.encode_time_ms = pipeline.average_encode_ms;
                if let Some((kind, fields)) = observation.transition.as_ref() {
                    crate::emit_lifecycle_event_with_context(
                        emitter,
                        *kind,
                        session_log_id.clone(),
                        fields.clone(),
                        Some(user.to_owned()),
                        local_hostname(),
                        Some(peer.to_owned()),
                        observation.assessment.overall,
                    );
                }
                if observation.snapshot_due {
                    let snapshot_fields = crate::observability::snapshot_fields(
                        &observation.sample,
                        &observation.assessment,
                    );
                    crate::emit_lifecycle_event_with_context(
                        emitter,
                        LifecycleEventKind::HealthSnapshot,
                        session_log_id.clone(),
                        snapshot_fields,
                        Some(user.to_owned()),
                        local_hostname(),
                        Some(peer.to_owned()),
                        observation.assessment.overall,
                    );
                    // Pipeline metrics ride a Level 3 event rather than the
                    // canonical snapshot. `HEALTH_SNAPSHOT` carries a closed
                    // field schema, and these four keys are not in it, so
                    // attaching them made `ValidatedLifecycleEvent::new` reject
                    // the event and canonical delivery be skipped entirely —
                    // costing the snapshot to gain four fields nobody ever saw.
                    // Declaring them is the alternative, but that changes a
                    // schema mirrored in
                    // `scripts/observability_event_definitions.py` and gated by
                    // `scripts/validate_observability.py`.
                    tracing::debug!(
                        target: crate::logging::HEALTH,
                        sid = %session_log_id,
                        pipeline_capture_fps_milli = metric_milli(pipeline.capture_fps),
                        pipeline_encoder_fps_milli = metric_milli(pipeline.encoder_fps),
                        pipeline_stage_us = metric_milli(pipeline.average_stage_ms),
                        pipeline_encode_us = metric_milli(pipeline.average_encode_ms),
                        "capture and encode pipeline snapshot"
                    );
                    if let Some(client) = session_health.client() {
                        let qos = client.qos.as_ref();
                        let network = client.network.as_ref();
                        tracing::debug!(
                            target: crate::logging::HEALTH,
                            sid = %session_log_id,
                            client_frames_received = ?qos.and_then(|sample| sample.frames_received),
                            client_frames_decoded = ?qos.and_then(|sample| sample.frames_decoded),
                            client_frames_presented = ?qos.and_then(|sample| sample.frames_presented),
                            client_frames_dropped = ?qos.and_then(|sample| sample.frames_dropped),
                            client_decode_ms = ?qos.and_then(|sample| sample.decode_time_ms),
                            client_display_ms = ?qos.and_then(|sample| sample.display_time_ms),
                            client_input_send_ms = ?qos.and_then(|sample| sample.input_send_time_ms),
                            client_sample_age_ms = ?qos.and_then(|sample| sample.sample_age_ms),
                            client_interface = ?network.map(|snapshot| snapshot.interface_kind()),
                            client_scope = ?network.map(|snapshot| snapshot.scope()),
                            client_link_mbps = ?network.and_then(|snapshot| snapshot.link_mbps()),
                            client_rssi_dbm = ?network.and_then(|snapshot| snapshot.rssi_dbm()),
                            client_mtu = ?network.and_then(|snapshot| snapshot.mtu()),
                            "client telemetry snapshot"
                        );
                    }
                    emitter.emit_drop_notices(arcen_observability::LifecycleContext {
                        sid: session_log_id.clone(),
                        user: Some(user.to_owned()),
                        host: local_hostname(),
                        peer_addr: Some(peer.to_owned()),
                        health_state: observation.assessment.overall,
                    });
                }
                if let Ok(text) = serde_json::to_string(&pong) {
                    let _ = control_tx
                        .try_send(WriterControl::Message(Message::Text(text.into())));
                }
                if let Ok(text) = serde_json::to_string(&stats) {
                    let _ = control_tx
                        .try_send(WriterControl::Message(Message::Text(text.into())));
                }
                if let Some(audio_stats) = audio_capture.telemetry() {
                    if audio_health_ticks.is_multiple_of(MICROPHONE_STATS_HEALTH_TICKS) {
                        tracing::info!(
                            target: AUDIO,
                            packets = audio_stats.packets,
                            bytes = audio_stats.bytes,
                            sent_packets = audio_stats.sent_packets,
                            sent_bytes = audio_stats.sent_bytes,
                            queue_drops = audio_stats.queue_drops,
                            queued_packets = audio.len(),
                            capture_errors = audio_stats.capture_errors,
                            restarts = audio_stats.restarts,
                            discontinuities = audio_stats.discontinuities,
                            underruns = audio_stats.underruns,
                            silent_frames = audio_stats.silent_frames,
                            idle_periods = audio_stats.idle_periods,
                            timestamp_gap_ms = audio_stats.timestamp_gap_ms,
                            "audio capture OK working"
                        );
                    } else {
                        tracing::debug!(
                            target: AUDIO,
                            packets = audio_stats.packets,
                            sent_packets = audio_stats.sent_packets,
                            queue_drops = audio_stats.queue_drops,
                            queued_packets = audio.len(),
                            underruns = audio_stats.underruns,
                            idle_periods = audio_stats.idle_periods,
                            "audio capture heartbeat"
                        );
                    }
                }
                if audio_health_ticks.is_multiple_of(MICROPHONE_STATS_HEALTH_TICKS) {
                    let unauthorized_frames =
                        std::mem::take(&mut unauthorized_microphone_frames_interval);
                    if let Some(ingress) = microphone.as_mut() {
                        let stats = ingress.take_interval_stats();
                        tracing::info!(
                            target: AUDIO,
                            event = "mic_windows_stats",
                            sid = %session_log_id,
                            generation = ingress.generation(),
                            received_frames = stats.received_frames,
                            received_bytes = stats.received_bytes,
                            accepted_frames = stats.accepted_frames,
                            accepted_bytes = stats.accepted_bytes,
                            duplicate_frames = stats.duplicate_frames,
                            late_frames = stats.late_frames,
                            wrong_generation_frames = stats.wrong_generation_frames,
                            discontinuities = stats.discontinuities,
                            jitter_depth = ingress.jitter_depth(),
                            jitter_target = arcen_media::audio::MICROPHONE_JITTER_TARGET_FRAMES,
                            jitter_max = arcen_media::audio::MICROPHONE_JITTER_MAX_FRAMES,
                            silence_frames = stats.silence_frames,
                            underflow_frames = stats.underflow_frames,
                            decoder_resets = stats.decoder_resets,
                            decoder_errors = stats.decoder_errors,
                            feeder_mailbox_drops = stats.transport_backpressure_drops,
                            feeder_timeouts = stats.backend_timeouts,
                            device_failures = stats.backend_failures,
                            unauthorized_frames,
                            "Windows microphone interval statistics"
                        );
                    } else if unauthorized_frames != 0 {
                        tracing::info!(
                            target: AUDIO,
                            event = "mic_windows_stats",
                            sid = %session_log_id,
                            generation = ?microphone_generation,
                            unauthorized_frames,
                            "Windows microphone unauthorized-frame statistics"
                        );
                    }
                }
            }
            writer_result = &mut writer => {
                writer_finished = true;
                break match writer_result {
                    Ok(exit) => {
                        let outcome = match &exit.result {
                            Ok(()) => Err("outbound writer stopped".to_string()),
                            Err(error) => Err(error.clone()),
                        };
                        completed_writer = Some(exit);
                        outcome
                    }
                    Err(error) => Err(format!("outbound writer task failed: {error}")),
                };
            }
        }
        }
    }
    .await;
    let mut result = stream_result.map_err(AttachmentError::Stream);

    if let Err(error) = shutdown_microphone(&mut microphone, "attachment_teardown").await {
        fatal_cleanup = Some(error.to_string());
    }
    tracing::info!(
        target: AUDIO,
        event = "mic_windows_transport_teardown_summary",
        sid = %session_log_id,
        generation = ?microphone_generation,
        duration_ms = microphone_started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64,
        unauthorized_frames = unauthorized_microphone_frames,
        stop_reason = if result.is_ok() {
            "stream_complete"
        } else {
            "stream_failure"
        },
        "Windows microphone transport teardown summary"
    );
    if let Some(cleanup_error) = fatal_cleanup {
        let error = match result {
            Ok(()) => cleanup_error,
            Err(stream_error) => format!("{stream_error}; {cleanup_error}"),
        };
        result = Err(AttachmentError::FatalCleanup(error));
    }
    log_state(peer, ServerState::Draining);
    if let Some(region_input) = region_input.as_mut() {
        let _ = region_input.release_all();
    }
    injector.close();
    if let Some(pen) = pen.as_mut() {
        pen.release();
    }
    video_pipelines.close_frames();
    audio.clear();
    if let Some(runtime) = clipboard_runtime.as_mut() {
        runtime.shutdown();
    }
    clipboard.close();
    if let Some(reassembler) = clipboard_reassembler.as_mut() {
        reassembler.abort();
    }
    audio_capture.shutdown(&control_tx).await;
    // One atomic call replaces the former clear()-then-close() teardown
    // obligation: nothing between them can leave a buffered frame to
    // leak or a forgotten step to reorder.
    audio.close();
    drop(control_tx);
    video_pipelines.shutdown().await;
    let writer_exit = if writer_finished {
        completed_writer.ok_or_else(|| {
            AttachmentError::FatalCleanup("outbound writer ownership was lost".to_string())
        })?
    } else {
        match tokio::time::timeout(WRITER_SHUTDOWN_TIMEOUT, &mut writer).await {
            Ok(Ok(exit)) => {
                if let Err(error) = &exit.result {
                    tracing::warn!(target: SESSION, %error, "writer stopped with error");
                }
                exit
            }
            Ok(Err(error)) => {
                return Err(AttachmentError::FatalCleanup(format!(
                    "writer task failed during attachment cleanup: {error}"
                )));
            }
            Err(_) => {
                writer.abort();
                let _ = writer.await;
                return Err(AttachmentError::FatalCleanup(
                    "writer task did not stop before attachment cleanup timeout".to_string(),
                ));
            }
        }
    };
    let ws = writer_exit.sink.reunite(ws_rx).map_err(|_| {
        AttachmentError::FatalCleanup(
            "could not reunite broker-agent IPC after attachment cleanup".to_string(),
        )
    })?;
    let _duration_ms = i64::try_from(stream_started_at.elapsed().as_millis()).unwrap_or(i64::MAX);
    let (sent_frames, _) = video_stats.snapshot();
    tracing::info!(
        target: SESSION,
        %peer,
        sent_frames,
        detached,
        "attachment streaming stopped"
    );
    Ok(AttachmentRun {
        ws,
        detached,
        result,
        sent_frames,
        dropped_frames,
    })
}

fn handle_clipboard_offer(
    text: &str,
    negotiation: Option<ClipboardNegotiation>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
) {
    let (Some(negotiation), Some(reassembler)) = (negotiation, reassembler) else {
        return;
    };
    let Ok(offer) = serde_json::from_str::<arcen_protocol::messages::ClipboardDataMsg>(text) else {
        tracing::warn!(
            target: SESSION,
            reason = "invalid_metadata",
            "clipboard offer rejected"
        );
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
        tracing::warn!(
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
        tracing::warn!(
            target: SESSION,
            sequence = offer.sequence,
            kind = ?offer.kind,
            size = offer.size_bytes,
            reason = %error,
            "clipboard offer rejected"
        );
    }
}

fn is_clipboard_data_message(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    arcen_protocol::messages::msg_type(&value) == Some(CLIPBOARD_DATA)
}

fn handle_clipboard_chunk(
    bytes: &[u8],
    negotiation: Option<ClipboardNegotiation>,
    reassembler: Option<&mut arcen_protocol::clipboard::ClipboardReassembler>,
    runtime: Option<&WindowsClipboardRuntime>,
) {
    let (Some(negotiation), Some(reassembler), Some(runtime)) = (negotiation, reassembler, runtime)
    else {
        return;
    };
    let Ok((header, payload)) = decode_clipboard_chunk(bytes) else {
        tracing::warn!(
            target: SESSION,
            reason = "invalid_chunk",
            "clipboard chunk rejected"
        );
        return;
    };
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
        tracing::warn!(
            target: SESSION,
            sequence = header.sequence,
            kind = ?header.kind,
            size = header.total_size,
            reason = "policy",
            "clipboard chunk rejected"
        );
        return;
    }
    let mut completed = match reassembler.push(header, payload) {
        Ok(Some(completed)) => completed,
        Ok(None) => return,
        Err(error) => {
            reassembler.abort();
            tracing::warn!(
                target: SESSION,
                sequence = header.sequence,
                kind = ?header.kind,
                size = header.total_size,
                reason = %error,
                "clipboard chunk rejected"
            );
            return;
        }
    };
    let policy = negotiation.policy();
    let valid = match completed.kind {
        arcen_protocol::messages::ClipboardContentKind::TextUtf8 => {
            std::str::from_utf8(&completed.bytes).is_ok()
        }
        arcen_protocol::messages::ClipboardContentKind::ImagePng => {
            arcen_media::clipboard::validate_png(
                &completed.bytes,
                arcen_media::clipboard::ImageLimits {
                    max_encoded_bytes: policy.max_bytes,
                    ..arcen_media::clipboard::ImageLimits::default()
                },
            )
            .is_ok()
        }
    };
    if !valid
        || policy
            .check_size(
                ClipboardFlow::ClientToHost,
                clipboard_kind(completed.kind),
                completed.bytes.len(),
            )
            .is_err()
    {
        tracing::warn!(
            target: SESSION,
            sequence = completed.sequence,
            kind = ?completed.kind,
            size = completed.bytes.len(),
            reason = "content_validation",
            "clipboard item rejected before native injection"
        );
        return;
    }
    if let Some(item) = ClipboardItem::new(
        completed.sequence,
        completed.kind,
        completed.take_bytes(),
        completed.truncated,
    ) {
        let _ = runtime.inject(item);
    }
}

fn clipboard_kind(
    kind: arcen_protocol::messages::ClipboardContentKind,
) -> arcen_media::clipboard::ClipboardKind {
    match kind {
        arcen_protocol::messages::ClipboardContentKind::TextUtf8 => {
            arcen_media::clipboard::ClipboardKind::TextUtf8
        }
        arcen_protocol::messages::ClipboardContentKind::ImagePng => {
            arcen_media::clipboard::ClipboardKind::ImagePng
        }
    }
}

/// Emits `SESSION_STREAM_START` (1102) once media/input streaming becomes
/// active, after display settle and capenc readiness.
fn emit_session_stream_start(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    media_plan: &ResolvedMediaPlan,
    display_report: &DisplayReport,
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
    let _ = fields.insert(
        "display_backend",
        FieldValue::String(display_report.backend.to_string()),
    );
    crate::emit_lifecycle_event_with_context(
        emitter,
        LifecycleEventKind::SessionStreamStart,
        session_log_id,
        fields,
        Some(user.to_owned()),
        local_hostname(),
        Some(peer.to_owned()),
        None,
    );
}

fn local_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|value| !value.is_empty() && value.len() <= arcen_telemetry::MAX_IDENTITY_BYTES)
}

/// Emits `SESSION_END` (1103) for a clean client close, or
/// `SESSION_INTERRUPTED` (1104) for any other termination, preserving a safe
/// (non-raw-error) reason class.
fn emit_session_end_or_interrupted(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    result: &Result<(), String>,
    duration_ms: i64,
    frames_sent: u64,
    frames_dropped: u64,
) {
    let frames_sent = i64::try_from(frames_sent).unwrap_or(i64::MAX);
    let frames_dropped = i64::try_from(frames_dropped).unwrap_or(i64::MAX);
    match result {
        Ok(()) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "reason_class",
                FieldValue::String("client_close".to_string()),
            );
            let _ = fields.insert("duration_ms", FieldValue::Integer(duration_ms));
            let _ = fields.insert("frames_sent", FieldValue::Integer(frames_sent));
            let _ = fields.insert("frames_dropped", FieldValue::Integer(frames_dropped));
            crate::emit_lifecycle_event_with_context(
                emitter,
                LifecycleEventKind::SessionEnd,
                session_log_id,
                fields,
                Some(user.to_owned()),
                local_hostname(),
                Some(peer.to_owned()),
                None,
            );
        }
        Err(error) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert("stage", FieldValue::String("streaming".to_string()));
            let _ = fields.insert(
                "reason_class",
                FieldValue::String(classify_stream_interruption(error).to_string()),
            );
            let _ = fields.insert("duration_ms", FieldValue::Integer(duration_ms));
            let _ = fields.insert("frames_sent", FieldValue::Integer(frames_sent));
            let _ = fields.insert("frames_dropped", FieldValue::Integer(frames_dropped));
            crate::emit_lifecycle_event_with_context(
                emitter,
                LifecycleEventKind::SessionInterrupted,
                session_log_id,
                fields,
                Some(user.to_owned()),
                local_hostname(),
                Some(peer.to_owned()),
                None,
            );
        }
    }
}

/// Maps a fixed, internally-controlled termination message to a safe,
/// closed reason class. Never places the raw message itself in a native
/// event; the message text is an internal literal, not attacker-controlled
/// or secret-bearing, but the native schema only accepts small closed
/// vocabularies.
fn classify_stream_interruption(error: &str) -> &'static str {
    if error.contains("capenc engine exited") {
        "capture_engine_exit"
    } else if error.contains("outbound writer closed") {
        "writer_closed"
    } else if error.contains("outbound writer stopped") || error.contains("writer task failed") {
        "writer_task_failed"
    } else if error.contains("WebSocket receive failed") {
        "transport_error"
    } else if error.contains("audio") {
        "audio_error"
    } else {
        "stream_failure"
    }
}

/// Emits `DISPLAY_RESTORED` (1201) after a verified in-process restore, or
/// `DISPLAY_RESTORE_FAILED` (1203) when it fails. Only emitted when the
/// display transaction actually mutated something (symmetric with
/// [`emit_display_armed`]); an "unchanged" lease has nothing to restore.
fn emit_display_restore_outcome(
    emitter: &LifecycleEmitter,
    session_log_id: CorrelationId,
    user: &str,
    peer: &str,
    display_report: &DisplayReport,
    restore: &Result<(), String>,
) {
    if !display_report.changed {
        return;
    }
    match restore {
        Ok(()) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "restore_backend",
                FieldValue::String(display_report.restore_backend.to_string()),
            );
            let _ = fields.insert("changed", FieldValue::Boolean(display_report.changed));
            let _ = fields.insert(
                "width",
                FieldValue::Integer(i64::from(display_report.original.width)),
            );
            let _ = fields.insert(
                "height",
                FieldValue::Integer(i64::from(display_report.original.height)),
            );
            crate::emit_lifecycle_event_with_context(
                emitter,
                LifecycleEventKind::DisplayRestored,
                session_log_id,
                fields,
                Some(user.to_owned()),
                local_hostname(),
                Some(peer.to_owned()),
                None,
            );
        }
        Err(_error) => {
            let mut fields = StructuredFields::default();
            let _ = fields.insert(
                "restore_backend",
                FieldValue::String(display_report.restore_backend.to_string()),
            );
            let _ = fields.insert(
                "stage",
                FieldValue::String("session_cleanup_restore".to_string()),
            );
            let _ = fields.insert(
                "reason_class",
                FieldValue::String("restore_verification_failed".to_string()),
            );
            // The in-process restore path does not disarm the recovery
            // journal on failure, so it remains pending for the watchdog.
            let _ = fields.insert("journal_pending", FieldValue::Boolean(true));
            crate::emit_lifecycle_event_with_context(
                emitter,
                LifecycleEventKind::DisplayRestoreFailed,
                session_log_id,
                fields,
                Some(user.to_owned()),
                local_hostname(),
                Some(peer.to_owned()),
                None,
            );
        }
    }
}

fn report_log_control_error(error: String, previous: &mut Option<String>) {
    if previous.as_deref() != Some(error.as_str()) {
        tracing::warn!(
            target: SESSION,
            %error,
            "private agent logging control failed; active session continues"
        );
        *previous = Some(error);
    }
}

fn start_capture_after_display<I, C, F>(
    cfg: &HostConfig,
    display: &DisplayReport,
    resolved_output: &crate::display::ResolvedOutput,
    cursor_mode: CursorMode,
    session_log_id: &CorrelationId,
    initialize_input: impl FnOnce(u32, crate::display::DesktopRect) -> Result<I, String>,
    spawn_capture: impl FnOnce(CapencConfig) -> Result<(C, F), String>,
    request_idr: impl FnOnce(&C),
) -> Result<(I, C, F), String> {
    if display.desktop_rect.width != display.applied.width as i32
        || display.desktop_rect.height != display.applied.height as i32
    {
        return Err(format!(
            "display reported applied {} but selected-output geometry is {:?}",
            display.applied, display.desktop_rect
        ));
    }
    let input = initialize_input(resolved_output.global_index, display.desktop_rect)
        .map_err(|error| format!("initialize selected-output input: {error}"))?;
    let (capture, frames) = spawn_capture(CapencConfig {
        binary: cfg.capenc_bin.clone(),
        output_index: resolved_output.global_index,
        adapter_name: Some(resolved_output.adapter_name.clone()),
        adapter_output_index: Some(resolved_output.adapter_output_index),
        device_name: Some(resolved_output.device_name.clone()),
        codec: cfg.codec,
        chroma: cfg.chroma,
        bit_depth: cfg.bit_depth,
        color_range: cfg.color_range,
        color_matrix: cfg.color_matrix,
        intent: cfg.requested_encode_intent(),
        qp_map: cfg.qp_map,
        fps: cfg.fps,
        width: display.applied.width,
        height: display.applied.height,
        encoder: Some(cfg.encoder),
        cursor_mode,
        session_log_id: session_log_id.clone(),
    })
    .map_err(|error| format!("spawn capenc after display settle: {error}"))?;
    request_idr(&capture);
    Ok((input, capture, frames))
}

struct OutboundVideo {
    message: Message,
}

/// Typed rejection constructing an [`OutboundVideoMux`] — the exact same
/// validated roster rejection Linux's `MonitorMux` shares; see
/// `arcen_outputs::fairness`.
type OutboundVideoMuxError = RosterError<u16>;

/// Fairly multiplexes 1-4 per-monitor [`VideoQueue`]s onto one logical
/// outbound video source for Carrier A, on top of the shared validated
/// roster/rotating-index/atomic-teardown policy in `arcen_outputs::fairness`
/// (the same policy backing Linux's `session::monitor_mux::MonitorMux`).
/// `Notify`-based wakeup — rather than Linux's `futures_util::select_all`
/// over native queue futures — stays entirely host-local: [`FairRoster`]
/// only orders and validates the routes, never awaits one itself.
struct OutboundVideoMux {
    roster: FairRoster<u16, Arc<VideoQueue<OutboundVideo>>>,
    notify: Notify,
}

impl std::fmt::Debug for OutboundVideoMux {
    // `VideoQueue` intentionally carries no `Debug` impl (its contents are
    // encoded video bytes), so only the routed monitor ids are shown; this
    // mirrors Linux's `MonitorMux` `Debug` impl.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboundVideoMux")
            .field("monitor_ids", &self.roster.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl OutboundVideoMux {
    fn new(monitor_ids: impl IntoIterator<Item = u16>) -> Result<Self, OutboundVideoMuxError> {
        Self::new_with_capacity(monitor_ids, VIDEO_QUEUE_CAPACITY)
    }

    fn new_with_capacity(
        monitor_ids: impl IntoIterator<Item = u16>,
        capacity: usize,
    ) -> Result<Self, OutboundVideoMuxError> {
        let roster = FairRoster::new(
            monitor_ids
                .into_iter()
                .map(|monitor_id| (monitor_id, Arc::new(VideoQueue::new(capacity)))),
        )?;
        Ok(Self {
            roster,
            notify: Notify::new(),
        })
    }

    fn push(
        &self,
        monitor_id: u16,
        frame: OutboundVideo,
        keyframe: bool,
    ) -> VideoPushResult<OutboundVideo> {
        let Some(queue) = self.roster.get(monitor_id) else {
            return VideoPushResult::Closed(frame);
        };
        let result = queue.push(frame, keyframe);
        if matches!(&result, VideoPushResult::Enqueued { .. }) {
            self.notify.notify_one();
        }
        result
    }

    async fn pop(&self) -> Option<OutboundVideo> {
        loop {
            let notified = self.notify.notified();
            for (index, _monitor_id, queue) in self.roster.entries_in_service_order() {
                if let Some(frame) = queue.try_pop() {
                    self.roster.record_served(index);
                    return Some(frame);
                }
            }
            if (0..self.roster.len()).all(|index| {
                self.roster
                    .entry(index)
                    .is_some_and(|(_, queue)| queue.is_closed())
            }) {
                return None;
            }
            notified.await;
        }
    }

    /// Clears every route's buffered frames, **without** closing any of
    /// them: used only for a mid-session single-monitor resize, where the
    /// stream must drop stale frames but keep accepting new ones from the
    /// retargeted pipeline. Never use this for teardown — use
    /// [`Self::close_and_clear_all`] there.
    fn clear(&self) -> usize {
        (0..self.roster.len())
            .filter_map(|index| self.roster.entry(index))
            .map(|(_, queue)| queue.clear())
            .sum()
    }

    /// Atomically tears down every route: closes and discards any buffered
    /// frames in **every** queue this mux routes, not only the one whose
    /// pipeline actually ended.
    ///
    /// This replaces what used to be a separate `clear()`-then-`close()`
    /// caller obligation: a caller that forgot the `clear()` step, or that
    /// reordered the two calls, could let a writer still drain
    /// already-buffered frames during session teardown instead of ending
    /// immediately — the same atomic-teardown hazard
    /// `arcen_outputs::fairness` and Linux's `MonitorMux::close_and_clear_all`
    /// document. There is no longer a plain multi-route `close()` to misuse:
    /// this is the only whole-mux teardown operation.
    fn close_and_clear_all(&self) {
        self.roster
            .close_and_clear_all(|queue| queue.close_and_clear());
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    fn awaiting_keyframe(&self) -> bool {
        (0..self.roster.len())
            .filter_map(|index| self.roster.entry(index))
            .any(|(_, queue)| queue.awaiting_keyframe())
    }
}

#[derive(Default)]
struct WriterVideoStats {
    frames: AtomicU64,
    bytes: AtomicU64,
}

impl WriterVideoStats {
    fn snapshot(&self) -> (u64, u64) {
        (
            self.frames.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

enum WriterControl {
    Message(Message),
    AudioBarrier(oneshot::Sender<()>),
}

async fn writer_loop<S>(
    mut sink: S,
    video: Arc<OutboundVideoMux>,
    audio: Arc<LatestQueue<AudioPacket>>,
    clipboard: Arc<ClipboardWriterQueue>,
    mut control: mpsc::Receiver<WriterControl>,
    audio_state: Arc<AudioSendState>,
    video_stats: Arc<WriterVideoStats>,
) -> WriterExit<S>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let result = writer_loop_inner(
        &mut sink,
        video,
        audio,
        clipboard,
        &mut control,
        audio_state,
        video_stats,
    )
    .await;
    WriterExit { sink, result }
}

struct WriterExit<S> {
    sink: S,
    result: Result<(), String>,
}

#[cfg(test)]
impl<S> WriterExit<S> {
    fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    fn unwrap(self) {
        self.result.unwrap();
    }
}

async fn writer_loop_inner<S>(
    sink: &mut S,
    video: Arc<OutboundVideoMux>,
    audio: Arc<LatestQueue<AudioPacket>>,
    clipboard: Arc<ClipboardWriterQueue>,
    control: &mut mpsc::Receiver<WriterControl>,
    audio_state: Arc<AudioSendState>,
    video_stats: Arc<WriterVideoStats>,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let mut control_open = true;
    let mut video_open = true;
    let mut audio_open = true;
    let mut clipboard_open = true;
    let mut clipboard_allowed = true;
    let mut audio_encoder = AudioWireEncoder::default();
    loop {
        if !control_open && !video_open && !audio_open && !clipboard_open {
            break;
        }
        let outbound = tokio::select! {
            biased;
            control = control.recv(), if control_open => match control {
                Some(WriterControl::Message(message)) => WriterItem::Message(message),
                Some(WriterControl::AudioBarrier(ack)) => WriterItem::AudioBarrier(ack),
                None => WriterItem::Closed(WriterStream::Control),
            },
            message = clipboard.pop(), if clipboard_open && clipboard_allowed => match message? {
                Some(message) => WriterItem::Clipboard(message),
                None => WriterItem::Closed(WriterStream::Clipboard),
            },
            packet = audio.pop(), if audio_open => match packet {
                Some(packet) => WriterItem::Audio(packet),
                None => WriterItem::Closed(WriterStream::Audio),
            },
            frame = video.pop(), if video_open => match frame {
                Some(frame) => WriterItem::Video(frame.message),
                None => WriterItem::Closed(WriterStream::Video),
            },
            () = clipboard_cooldown(), if clipboard_open && !clipboard_allowed => {
                WriterItem::ClipboardCooldown
            },
        };
        let (message, audio_bytes, video_bytes) = match outbound {
            WriterItem::Message(message) => {
                clipboard_allowed = true;
                (message, None, None)
            }
            WriterItem::Video(message) => {
                clipboard_allowed = true;
                let bytes = match &message {
                    Message::Binary(bytes) => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                    _ => 0,
                };
                (message, None, Some(bytes))
            }
            WriterItem::Clipboard(message) => {
                clipboard_allowed = false;
                (message, None, None)
            }
            WriterItem::ClipboardCooldown => {
                clipboard_allowed = true;
                continue;
            }
            WriterItem::AudioBarrier(ack) => {
                clipboard_allowed = true;
                let _ = ack.send(());
                continue;
            }
            WriterItem::Audio(packet) => {
                clipboard_allowed = true;
                if !audio_state.is_enabled() {
                    continue;
                }
                match audio_encoder.encode(&packet, &audio_state) {
                    Ok(Some((message, bytes))) => {
                        (Message::Binary(message.into()), Some(bytes), None)
                    }
                    Ok(None) => continue,
                    Err(AudioEncodeFailure::Transient) => {
                        audio_state.record_encode_failure();
                        continue;
                    }
                    Err(AudioEncodeFailure::Disabled) => {
                        audio_state.record_encode_failure();
                        audio_state.report_codec_failure();
                        continue;
                    }
                }
            }
            WriterItem::Closed(WriterStream::Control) => {
                control_open = false;
                continue;
            }
            WriterItem::Closed(WriterStream::Video) => {
                video_open = false;
                continue;
            }
            WriterItem::Closed(WriterStream::Audio) => {
                audio_open = false;
                continue;
            }
            WriterItem::Closed(WriterStream::Clipboard) => {
                clipboard_open = false;
                continue;
            }
        };
        send_ws_with_timeout(sink, message, WS_WRITE_TIMEOUT).await?;
        if let Some(bytes) = video_bytes {
            video_stats.frames.fetch_add(1, Ordering::Relaxed);
            video_stats.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
        if let Some(bytes) = audio_bytes {
            if let Some(telemetry) = audio_state.telemetry() {
                telemetry.record_sent(bytes);
            }
        }
    }
    Ok(())
}

enum WriterItem {
    Message(Message),
    Video(Message),
    Clipboard(Message),
    ClipboardCooldown,
    AudioBarrier(oneshot::Sender<()>),
    Audio(AudioPacket),
    Closed(WriterStream),
}

enum WriterStream {
    Control,
    Video,
    Audio,
    Clipboard,
}

async fn clipboard_cooldown() {
    tokio::task::yield_now().await;
}

async fn send_ws_with_timeout<S>(
    sink: &mut S,
    message: Message,
    timeout: Duration,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    tokio::time::timeout(timeout, sink.send(message))
        .await
        .map_err(|_| "WebSocket send timed out".to_string())?
        .map_err(|error| format!("WebSocket send failed: {error}"))
}

fn frame_message(
    plan: &ResolvedMediaPlan,
    frame: &crate::capenc::EncodedFrame,
    monitor_id: u16,
    topology_generation: u64,
    stream_epoch: u64,
) -> Vec<u8> {
    let codec = crate::capenc::protocol_codec(plan.video.codec);
    let chroma = crate::capenc::protocol_chroma(plan.video.chroma);
    let region = monitor_id != 0;
    let frame_type = match codec {
        VideoCodec::H264 if region => FrameType::RegionVideoH264,
        VideoCodec::H265 if region => FrameType::RegionVideoH265,
        VideoCodec::Av1 if region => FrameType::RegionVideoAv1,
        VideoCodec::H264 => FrameType::VideoH264,
        VideoCodec::H265 => FrameType::VideoH265,
        VideoCodec::Av1 => FrameType::VideoAv1,
        VideoCodec::Jpeg | VideoCodec::Vp9 => return Vec::new(),
    };
    let header = encode_video_header(VideoHeader {
        frame_type,
        codec,
        chroma,
        flags: VideoHeader::encode_flags(
            frame.keyframe,
            crate::capenc::protocol_bit_depth(plan.video.bit_depth),
            crate::capenc::protocol_color_range(plan.video.range),
            crate::capenc::protocol_color_matrix(plan.video.matrix),
        ),
        timestamp_ms: frame.timestamp_ms,
        monitor_id,
        topology_generation,
        stream_epoch,
    });
    let mut message = Vec::with_capacity(header.len() + frame.data.len());
    message.extend_from_slice(&header);
    message.extend_from_slice(&frame.data);
    message
}

struct AudioWireEncoder {
    generation: u64,
    stream: Option<ResolvedAudioStream>,
    opus: Option<OpusEncoder>,
    pcm: [i16; 1_920],
    packet: [u8; MAX_OPUS_PACKET_BYTES],
    consecutive_failures: u8,
}

impl Default for AudioWireEncoder {
    fn default() -> Self {
        Self {
            generation: 0,
            stream: None,
            opus: None,
            pcm: [0; 1_920],
            packet: [0; MAX_OPUS_PACKET_BYTES],
            consecutive_failures: 0,
        }
    }
}

#[derive(Debug)]
enum AudioEncodeFailure {
    Transient,
    Disabled,
}

impl AudioWireEncoder {
    fn encode(
        &mut self,
        packet: &AudioPacket,
        state: &AudioSendState,
    ) -> Result<Option<(Vec<u8>, usize)>, AudioEncodeFailure> {
        let (generation, stream) = state.stream();
        if generation != self.generation {
            if self.reconfigure(generation, stream).is_err() {
                return Err(self.codec_failure(stream));
            }
        }
        if !stream.is_enabled() {
            return Ok(None);
        }
        match stream.codec {
            Some(AudioCodec::Pcm) => {
                let message =
                    audio_message(AudioCodec::Pcm, packet.timestamp_ms, &packet.pcm_s16le);
                Ok(Some((message, packet.pcm_s16le.len())))
            }
            Some(AudioCodec::Opus) => {
                if packet.pcm_s16le.len() != crate::audio::CHUNK_BYTES {
                    return Err(self.codec_failure(stream));
                }
                for (sample, bytes) in self.pcm.iter_mut().zip(packet.pcm_s16le.chunks_exact(2)) {
                    *sample = i16::from_le_bytes([bytes[0], bytes[1]]);
                }
                let Some(encoder) = self.opus.as_mut() else {
                    return Err(self.codec_failure(stream));
                };
                let encoded = match encoder.encode(&self.pcm, &mut self.packet) {
                    Ok(encoded) => encoded,
                    Err(_) => return Err(self.codec_failure(stream)),
                };
                self.consecutive_failures = 0;
                let message = audio_message(
                    AudioCodec::Opus,
                    packet.timestamp_ms,
                    &self.packet[..encoded],
                );
                Ok(Some((message, encoded)))
            }
            None => Ok(None),
        }
    }

    fn reconfigure(&mut self, generation: u64, stream: ResolvedAudioStream) -> Result<(), ()> {
        match (stream.codec, self.stream.and_then(|current| current.codec)) {
            (Some(AudioCodec::Opus), Some(AudioCodec::Opus)) => {
                self.opus
                    .as_mut()
                    .ok_or(())?
                    .set_bitrate(stream.bitrate)
                    .map_err(|_| ())?;
            }
            (Some(AudioCodec::Opus), _) => {
                self.opus = Some(OpusEncoder::new(stream.bitrate).map_err(|_| ())?);
            }
            _ => self.opus = None,
        }
        self.generation = generation;
        self.stream = Some(stream);
        self.consecutive_failures = 0;
        Ok(())
    }

    fn codec_failure(&mut self, stream: ResolvedAudioStream) -> AudioEncodeFailure {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.consecutive_failures < 3 {
            return AudioEncodeFailure::Transient;
        }
        self.consecutive_failures = 0;
        match stream.codec {
            Some(AudioCodec::Opus) => match OpusEncoder::new(stream.bitrate) {
                Ok(encoder) => {
                    self.opus = Some(encoder);
                    AudioEncodeFailure::Transient
                }
                Err(_) => {
                    self.opus = None;
                    AudioEncodeFailure::Disabled
                }
            },
            _ => AudioEncodeFailure::Disabled,
        }
    }
}

fn audio_message(codec: AudioCodec, timestamp_ms: u32, payload: &[u8]) -> Vec<u8> {
    let header = encode_audio_header(AudioHeader {
        codec,
        timestamp_ms,
    });
    let mut message = Vec::with_capacity(header.len() + payload.len());
    message.extend_from_slice(&header);
    message.extend_from_slice(payload);
    message
}

#[derive(Default)]
struct InboundAction {
    reply: Option<String>,
    audio_quality: Option<(bool, u32)>,
    client_telemetry: Option<arcen_protocol::messages::ClientTelemetrySnapshotMsg>,
    input_type: Option<&'static str>,
}

async fn handle_inbound(
    text: &str,
    injector: &mut Injector,
    pen: &mut Option<PenInjector>,
    region_input: &mut Option<RegionInputAdapter>,
    video: &PreparedVideo,
    sequence_tracker: &mut InputSequenceTracker,
) -> InboundAction {
    let mut action = InboundAction::default();
    let envelope: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(target: SESSION, %error, "invalid client JSON dropped");
            return action;
        }
    };
    let Some(message_type) = envelope.get("type").and_then(|value| value.as_str()) else {
        tracing::warn!(target: SESSION, "client JSON missing type");
        return action;
    };

    match message_type {
        "mouse_move" => {
            if region_input.is_some() {
                log_legacy_pointer_rejected(message_type);
            } else if let Some(message) = parse_input::<MouseMoveMsg>(text, sequence_tracker) {
                injector.move_abs(&message);
                action.input_type = Some("mouse_move");
            }
        }
        MOUSE_MOVE_RELATIVE => {
            if region_input.is_some() {
                log_legacy_pointer_rejected(message_type);
            } else if let Some(message) =
                parse_input::<MouseMoveRelativeMsg>(text, sequence_tracker)
            {
                injector.move_relative(&message);
                action.input_type = Some(MOUSE_MOVE_RELATIVE);
            }
        }
        "mouse_button" => {
            if region_input.is_some() {
                log_legacy_pointer_rejected(message_type);
            } else if let Some(message) = parse_input::<MouseButtonMsg>(text, sequence_tracker) {
                injector.button(&message);
                action.input_type = Some("mouse_button");
            }
        }
        MOUSE_SCROLL => {
            if region_input.is_some() {
                log_legacy_pointer_rejected(message_type);
            } else if let Some(message) = parse_input::<MouseScrollMsg>(text, sequence_tracker) {
                injector.scroll(&message);
                action.input_type = Some(MOUSE_SCROLL);
            }
        }
        REGION_POINTER_ENTER => {
            if let Some(adapter) = region_input.as_mut() {
                if let Some(message) =
                    parse_region_input::<RegionPointerEnterMsg>(text, sequence_tracker)
                {
                    match adapter.pointer_enter(&message) {
                        Ok(mapped) => {
                            commit_region_sequence(sequence_tracker, message.metadata.sequence);
                            injector.move_region(mapped, message.metadata.sequence);
                            action.input_type = Some(REGION_POINTER_ENTER);
                        }
                        Err(error) => log_region_input_rejected(message_type, &error),
                    }
                }
            } else {
                log_region_input_unavailable(message_type);
            }
        }
        REGION_POINTER_LEAVE => {
            if let Some(adapter) = region_input.as_mut() {
                if let Some(message) =
                    parse_region_input::<RegionPointerLeaveMsg>(text, sequence_tracker)
                {
                    match adapter.pointer_leave(&message) {
                        Ok(mapped) => {
                            commit_region_sequence(sequence_tracker, message.metadata.sequence);
                            injector.move_region(mapped, message.metadata.sequence);
                            action.input_type = Some(REGION_POINTER_LEAVE);
                        }
                        Err(error) => log_region_input_rejected(message_type, &error),
                    }
                }
            } else {
                log_region_input_unavailable(message_type);
            }
        }
        REGION_POINTER_MOTION => {
            if let Some(adapter) = region_input.as_mut() {
                if let Some(message) =
                    parse_region_input::<RegionPointerMotionMsg>(text, sequence_tracker)
                {
                    match adapter.pointer_motion(&message) {
                        Ok(mapped) => {
                            commit_region_sequence(sequence_tracker, message.metadata.sequence);
                            injector.move_region(mapped, message.metadata.sequence);
                            action.input_type = Some(REGION_POINTER_MOTION);
                        }
                        Err(error) => log_region_input_rejected(message_type, &error),
                    }
                }
            } else {
                log_region_input_unavailable(message_type);
            }
        }
        REGION_POINTER_BUTTON => {
            if let Some(adapter) = region_input.as_mut() {
                if let Some(message) =
                    parse_region_input::<RegionPointerButtonMsg>(text, sequence_tracker)
                {
                    match adapter.pointer_button(&message) {
                        Ok(mapped) => {
                            commit_region_sequence(sequence_tracker, message.metadata.sequence);
                            injector.button_region(mapped, message.metadata.sequence);
                            action.input_type = Some(REGION_POINTER_BUTTON);
                        }
                        Err(error) => log_region_input_rejected(message_type, &error),
                    }
                }
            } else {
                log_region_input_unavailable(message_type);
            }
        }
        REGION_POINTER_SCROLL => {
            if let Some(adapter) = region_input.as_mut() {
                if let Some(message) =
                    parse_region_input::<RegionPointerScrollMsg>(text, sequence_tracker)
                {
                    match adapter.pointer_scroll(&message) {
                        Ok(mapped) => {
                            commit_region_sequence(sequence_tracker, message.metadata.sequence);
                            injector.scroll_region(mapped, message.metadata.sequence);
                            action.input_type = Some(REGION_POINTER_SCROLL);
                        }
                        Err(error) => log_region_input_rejected(message_type, &error),
                    }
                }
            } else {
                log_region_input_unavailable(message_type);
            }
        }
        "key_event" => {
            if let Some(message) = parse_input::<KeyEventMsg>(text, sequence_tracker) {
                injector.key_event(&message);
                action.input_type = Some("key_event");
            }
        }
        PEN_EVENT => {
            if region_input.is_some() {
                log_legacy_pointer_rejected(message_type);
            } else if let Some(message) = parse_pen_input(text, sequence_tracker) {
                tracing::trace!(
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
                match pen.as_mut() {
                    Some(pen) => {
                        pen.dispatch(&message);
                        action.input_type = Some(PEN_EVENT);
                    }
                    None => tracing::debug!(
                        target: INPUT,
                        "pen_event received but no synthetic pen device is active; dropped"
                    ),
                }
            }
        }
        REGION_PEN_EVENT => {
            if let Some(adapter) = region_input.as_mut() {
                if let Some(message) =
                    parse_region_input::<RegionPenEventMsg>(text, sequence_tracker)
                {
                    match adapter.pen(&message) {
                        Ok(mapped) => {
                            commit_region_sequence(sequence_tracker, message.metadata.sequence);
                            match pen.as_mut() {
                                Some(pen) => {
                                    pen.dispatch_region(mapped, message.metadata.sequence);
                                    action.input_type = Some(REGION_PEN_EVENT);
                                }
                                None => tracing::debug!(
                                    target: INPUT,
                                    "region_pen_event received but no synthetic pen device is active; dropped"
                                ),
                            }
                        }
                        Err(error) => log_region_input_rejected(message_type, &error),
                    }
                }
            } else {
                log_region_input_unavailable(message_type);
            }
        }
        REQUEST_FULL_FRAME => {
            if serde_json::from_str::<RequestFullFrameMsg>(text).is_ok() {
                tracing::info!(target: CAPENC, "client requested IDR");
                video.request_keyframe_all("client_request_full_frame");
            }
        }
        KEY_RESET_MODIFIERS => {
            if serde_json::from_str::<KeyResetModifiersMsg>(text).is_ok() {
                injector.reset_modifiers();
                action.input_type = Some(KEY_RESET_MODIFIERS);
            }
        }
        TEXT_COMMIT => {
            if let Ok(message) = serde_json::from_str::<TextCommitMsg>(text) {
                injector.text_commit(&message);
                action.input_type = Some(TEXT_COMMIT);
            }
        }
        "quality_settings" => match serde_json::from_str::<QualitySettings>(text) {
            Ok(quality) => {
                let media_plan = video.primary_plan();
                tracing::info!(
                    target: SESSION,
                    requested_codec = %quality.codec,
                    requested_chroma = %quality.chroma,
                    requested_bit_depth = %quality.bit_depth,
                    requested_color_range = %quality.color_range,
                    requested_color_matrix = %quality.color_matrix,
                    requested_encode_intent = %quality.encode_intent,
                    requested_audio = quality.enable_audio,
                    active_bit_depth = media_plan.bit_depth_token(),
                    active_color_range = media_plan.range_token(),
                    active_color_matrix = media_plan.matrix_token(),
                    "updated quality settings received; video/colour remain host-authoritative"
                );
                action.audio_quality = Some((quality.enable_audio, quality.audio_bitrate_kbps));
            }
            Err(error) => {
                tracing::warn!(target: SESSION, %error, "malformed quality_settings");
            }
        },
        HEALTH_PING => action = health_ping_action(text),
        HEALTH_PONG => log_typed::<HealthPongMsg>(text, HEALTH_PONG),
        "health_stats" => log_typed::<HealthStatsMsg>(text, HEALTH_STATS),
        other => {
            tracing::debug!(target: SESSION, message_type = other, "unhandled control message")
        }
    }
    action
}

fn health_ping_action(text: &str) -> InboundAction {
    let ping: HealthPingMsg = match serde_json::from_str(text) {
        Ok(ping) => ping,
        Err(error) => {
            tracing::warn!(target: SESSION, %error, "malformed health_ping");
            return InboundAction::default();
        }
    };
    let reply = serde_json::to_string(&HealthPongMsg {
        msg_type: HEALTH_PONG.to_string(),
        ping_timestamp_ms: ping.timestamp_ms,
        sequence: ping.sequence,
        server_timestamp_ms: now_ms_u64(),
        server_state: ServerState::Streaming.as_str().to_string(),
    })
    .ok();
    InboundAction {
        reply,
        client_telemetry: ping.client_telemetry,
        ..InboundAction::default()
    }
}

#[cfg(test)]
fn health_pong_reply(text: &str) -> Option<String> {
    health_ping_action(text).reply
}

fn now_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn metric_milli(value: f64) -> i64 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    (value * 1000.0).round().min(i64::MAX as f64) as i64
}

fn log_legacy_pointer_rejected(message_type: &str) {
    tracing::warn!(
        target: INPUT,
        message_type,
        "legacy pointer input rejected for a region-authoritative multi-monitor session"
    );
}

fn log_region_input_unavailable(message_type: &str) {
    tracing::warn!(
        target: INPUT,
        message_type,
        "region input rejected because this is a legacy single-monitor session"
    );
}

fn log_region_input_rejected(message_type: &str, error: &impl std::fmt::Display) {
    tracing::warn!(
        target: INPUT,
        message_type,
        %error,
        "region input rejected without advancing the global input sequence"
    );
}

trait SequencedInput {
    fn sequence(&self) -> u64;
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

impl SequencedInput for KeyEventMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

impl SequencedInput for PenEventMsg {
    fn sequence(&self) -> u64 {
        self.sequence
    }
}

trait RegionSequencedInput: DeserializeOwned {
    fn validate_region(&self) -> Result<(), RegionInputValidationError>;
    fn region_sequence(&self) -> u64;
}

macro_rules! impl_region_sequenced_input {
    ($($message:ty),+ $(,)?) => {
        $(
            impl RegionSequencedInput for $message {
                fn validate_region(&self) -> Result<(), RegionInputValidationError> {
                    self.validate()
                }

                fn region_sequence(&self) -> u64 {
                    self.metadata.sequence
                }
            }
        )+
    };
}

impl_region_sequenced_input!(
    RegionPointerEnterMsg,
    RegionPointerLeaveMsg,
    RegionPointerMotionMsg,
    RegionPointerButtonMsg,
    RegionPointerScrollMsg,
    RegionPenEventMsg,
);

fn parse_input<T>(text: &str, sequence_tracker: &mut InputSequenceTracker) -> Option<T>
where
    T: DeserializeOwned + SequencedInput,
{
    let message: T = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(target: INPUT, %error, "malformed input message dropped");
            return None;
        }
    };
    let sequence = message.sequence();
    if !sequence_tracker.accept(sequence) {
        tracing::warn!(
            target: INPUT,
            sequence,
            previous = sequence_tracker.last_nonzero(),
            "duplicate or out-of-order input dropped"
        );
        return None;
    }
    Some(message)
}

fn parse_region_input<T>(text: &str, sequence_tracker: &InputSequenceTracker) -> Option<T>
where
    T: RegionSequencedInput,
{
    let message: T = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(target: INPUT, %error, "malformed region input message dropped");
            return None;
        }
    };
    if let Err(error) = message.validate_region() {
        tracing::warn!(
            target: INPUT,
            %error,
            "invalid region input message dropped before sequence advancement"
        );
        return None;
    }
    let sequence = message.region_sequence();
    if sequence <= sequence_tracker.last_nonzero() {
        tracing::warn!(
            target: INPUT,
            sequence,
            previous = sequence_tracker.last_nonzero(),
            "duplicate or out-of-order region input dropped"
        );
        return None;
    }
    Some(message)
}

fn commit_region_sequence(sequence_tracker: &mut InputSequenceTracker, sequence: u64) {
    let accepted = sequence_tracker.accept(sequence);
    debug_assert!(
        accepted,
        "region sequence was prevalidated immediately before commit"
    );
    if !accepted {
        tracing::error!(
            target: INPUT,
            sequence,
            previous = sequence_tracker.last_nonzero(),
            "region sequence commit invariant failed"
        );
    }
}

/// Like `parse_input`, but `PenEventMsg` carries physical ranges that
/// `serde` does not enforce (`PenEventMsg::validate`). Those ranges must be
/// checked *before* the message is allowed to advance the shared input
/// sequence, because `InputSequenceTracker::accept` mutates `last_nonzero`
/// as a side effect — an invalid payload must never reach it.
fn parse_pen_input(text: &str, sequence_tracker: &mut InputSequenceTracker) -> Option<PenEventMsg> {
    let message: PenEventMsg = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!(target: INPUT, %error, "malformed pen_event dropped");
            return None;
        }
    };
    if let Err(error) = message.validate() {
        tracing::warn!(
            target: INPUT,
            %error,
            "out-of-range pen_event dropped before sequence advancement"
        );
        return None;
    }
    let sequence = message.sequence();
    if !sequence_tracker.accept(sequence) {
        tracing::warn!(
            target: INPUT,
            sequence,
            previous = sequence_tracker.last_nonzero(),
            "duplicate or out-of-order pen_event dropped"
        );
        return None;
    }
    Some(message)
}

fn log_typed<T>(text: &str, message_type: &str)
where
    T: DeserializeOwned,
{
    if let Err(error) = serde_json::from_str::<T>(text) {
        tracing::warn!(target: SESSION, %error, message_type, "malformed control message");
    }
}

async fn send_auth_result<S>(ws: &mut S, success: bool, message: &str) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_auth_result_with_resume(ws, success, message, None, None, false, None).await
}

async fn send_resume_rejection<S>(
    ws: &mut S,
    message: &str,
    error_code: ResumeErrorCode,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_auth_result_with_resume(ws, false, message, None, None, false, Some(error_code)).await
}

#[allow(clippy::too_many_arguments)]
async fn send_auth_result_with_resume<S>(
    ws: &mut S,
    success: bool,
    message: &str,
    resume_grant: Option<&arcen_identity::DirectResumeGrantToken>,
    resume_window_secs: Option<u32>,
    resumed: bool,
    error_code: Option<ResumeErrorCode>,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_auth_result_with_resume_timeout(
        ws,
        success,
        message,
        resume_grant,
        resume_window_secs,
        resumed,
        error_code,
        WS_WRITE_TIMEOUT,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_auth_result_with_resume_timeout<S>(
    ws: &mut S,
    success: bool,
    message: &str,
    resume_grant: Option<&arcen_identity::DirectResumeGrantToken>,
    resume_window_secs: Option<u32>,
    resumed: bool,
    error_code: Option<ResumeErrorCode>,
    timeout: Duration,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    send_json_with_timeout(
        ws,
        &AuthResult {
            msg_type: AUTH_RESULT.to_string(),
            success,
            message: message.to_string(),
            resume_grant: resume_grant.map(|grant| grant.expose_for_transport().to_string()),
            resume_window_secs,
            resumed,
            error_code,
        },
        "auth_result",
        timeout,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn send_successor_auth_result_or_drain<S>(
    ws: &mut S,
    message: &str,
    successor_grant: &arcen_identity::DirectResumeGrantToken,
    window_secs: u32,
    resumed: bool,
    timeout: Duration,
    registry: &crate::resume::ResumeRegistry,
    active_session_id: &arcen_identity::ActiveHostSessionId,
) -> Result<(), crate::resume::ResumeRegistryError>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    if send_auth_result_with_resume_timeout(
        ws,
        true,
        message,
        Some(successor_grant),
        Some(window_secs),
        resumed,
        None,
        timeout,
    )
    .await
    .is_ok()
    {
        return Ok(());
    }
    registry.begin_drain(active_session_id)?;
    Err(crate::resume::ResumeRegistryError::SuccessorDelivery)
}

async fn recv_text<S>(ws: &mut WebSocketStream<S>, timeout: Duration) -> Result<String, String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(timeout, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => return Ok(text.to_string()),
                Some(Ok(Message::Ping(payload))) => {
                    send_ws_with_timeout(ws, Message::Pong(payload), WS_WRITE_TIMEOUT).await?;
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err("connection closed while waiting for text message".to_string());
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(format!("WebSocket receive failed: {error}")),
            }
        }
    })
    .await
    .map_err(|_| "timed out waiting for text message".to_string())?
}

async fn relay_client_and_agent<C, CE, A>(
    client: &mut C,
    agent: &mut WebSocketStream<A>,
    windows_session: WindowsSessionIdentity,
    native_user_sid: String,
    controls: &mut watch::Receiver<AgentControl>,
    session_shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String>
where
    C: Sink<Message> + Stream<Item = Result<Message, CE>> + Unpin,
    C::Error: std::fmt::Display,
    CE: std::fmt::Display,
    A: AsyncRead + AsyncWrite + Unpin,
{
    match relay_one_attachment(
        client,
        agent,
        &windows_session,
        &native_user_sid,
        controls,
        None,
        session_shutdown,
    )
    .await
    {
        RelayOutcome::ExplicitClose | RelayOutcome::BrokerShutdown => Ok(()),
        RelayOutcome::UnexpectedLoss(reason) => Err(reason.to_string()),
        RelayOutcome::AgentFailure(error) | RelayOutcome::NativeSessionEnded(error) => Err(error),
        RelayOutcome::ResumeAuthorityFailure(error) => {
            Err(format!("resume grant refresh failed: {error:?}"))
        }
    }
}
enum RelayOutcome {
    ExplicitClose,
    BrokerShutdown,
    UnexpectedLoss(&'static str),
    AgentFailure(String),
    NativeSessionEnded(String),
    ResumeAuthorityFailure(crate::resume::ResumeRegistryError),
}

#[derive(Clone, Copy)]
struct ResumeRefreshContext<'a> {
    registry: &'a crate::resume::ResumeRegistry,
    active_session_id: &'a arcen_identity::ActiveHostSessionId,
    window_secs: u32,
}

struct ResumableAttachment<'a> {
    commands: &'a mut mpsc::UnboundedReceiver<crate::resume::OwnerCommand>,
    refresh: ResumeRefreshContext<'a>,
}

fn resume_refresh_interval(window_secs: u32) -> Duration {
    Duration::from_millis((u64::from(window_secs) * 1_000 / 2).max(1))
}

fn resumable_write_timeout(window_secs: u32) -> Duration {
    WS_WRITE_TIMEOUT.min(resume_refresh_interval(window_secs))
}

#[allow(clippy::too_many_arguments)]
async fn relay_resumable_client_and_agent<A>(
    mut client: crate::resume::DirectSessionSocket,
    agent: &mut WebSocketStream<A>,
    windows_session: WindowsSessionIdentity,
    native_user_sid: String,
    active_session_id: arcen_identity::ActiveHostSessionId,
    controls: &mut watch::Receiver<AgentControl>,
    commands: &mut mpsc::UnboundedReceiver<crate::resume::OwnerCommand>,
    registry: &crate::resume::ResumeRegistry,
    reconnect_window_secs: u32,
    session_shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String>
where
    A: AsyncRead + AsyncWrite + Unpin,
{
    let policy = ReconnectPolicy::new(reconnect_window_secs)
        .map_err(|error| format!("invalid reconnect policy: {error}"))?;
    let mut reconnect = DirectReconnect::new(policy);
    let result = async {
        'session: loop {
        match relay_one_attachment(
            &mut client,
            agent,
            &windows_session,
            &native_user_sid,
            controls,
            Some(ResumableAttachment {
                commands,
                refresh: ResumeRefreshContext {
                    registry,
                    active_session_id: &active_session_id,
                    window_secs: reconnect_window_secs,
                },
            }),
            session_shutdown,
        )
        .await
        {
            RelayOutcome::ExplicitClose => {
                let _ =
                    reconnect.apply(ReconnectEvent::ExplicitDisconnect, registry.monotonic_now());
                break Ok(());
            }
            RelayOutcome::BrokerShutdown => {
                let _ =
                    reconnect.apply(ReconnectEvent::ExplicitDisconnect, registry.monotonic_now());
                break Ok(());
            }
            RelayOutcome::AgentFailure(error) => {
                let _ = reconnect.apply(ReconnectEvent::OwnerCrashed, registry.monotonic_now());
                break Err(error);
            }
            RelayOutcome::NativeSessionEnded(error) => {
                let _ =
                    reconnect.apply(ReconnectEvent::NativeSessionEnded, registry.monotonic_now());
                break Err(error);
            }
            RelayOutcome::ResumeAuthorityFailure(error) => {
                let _ = reconnect.apply(ReconnectEvent::OwnerCrashed, registry.monotonic_now());
                break Err(format!("resume grant refresh failed: {error:?}"));
            }
            RelayOutcome::UnexpectedLoss(reason) => {
                let actions =
                    reconnect.apply(ReconnectEvent::UnexpectedLoss, registry.monotonic_now());
                if !actions.hold_restore_leases {
                    break Err("unexpected transport loss is not resumable".to_string());
                }
                registry
                    .mark_detached(&active_session_id)
                    .map_err(|error| format!("could not expose detached resume slot: {error:?}"))?;
                send_json(
                    agent,
                    &AgentAttachmentCommand::detach(),
                    AgentAttachmentCommand::TYPE,
                )
                .await?;
                await_agent_attachment_status(
                    agent,
                    AgentAttachmentState::Detached,
                    AGENT_ATTACHMENT_TIMEOUT,
                )
                .await?;
                tracing::info!(
                    target: SESSION,
                    reason_class = reason,
                    "external direct transport detached; restore leases and broker-agent process retained"
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
                        _ => break 'session Err("invalid detached reconnect state".to_string()),
                    };
                    let now = registry.monotonic_now();
                    if now >= deadline {
                        let _ = reconnect
                            .apply(ReconnectEvent::DeadlineReached { timer_generation }, now);
                        break 'session Err("direct resume window expired".to_string());
                    }
                    let remaining_ms = deadline.get().saturating_sub(now.get());
                    let wait_ms = u64::try_from(remaining_ms).unwrap_or(u64::MAX);
                    let mut monitor = tokio::time::interval(Duration::from_secs(2));
                    monitor.tick().await;
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {
                            let _ = reconnect.apply(
                                ReconnectEvent::DeadlineReached { timer_generation },
                                registry.monotonic_now(),
                            );
                            break 'session Err("direct resume window expired".to_string());
                        }
                        command = commands.recv() => {
                            match command {
                                Some(crate::resume::OwnerCommand::Resume(handoff)) => {
                                    let mut handoff = *handoff;
                                    let _ = reconnect.apply(
                                        ReconnectEvent::BeginResume,
                                        registry.monotonic_now(),
                                    );
                                    tracing::info!(
                                        target: SESSION,
                                        sid = %handoff.session_log_id,
                                        previous_sid = %handoff.previous_session_log_id,
                                        "credential-free direct transport resume candidate consumed"
                                    );
                                    let identity = windows_session.clone();
                                    let sid = native_user_sid.clone();
                                    let observed = tokio::task::spawn_blocking(move || {
                                        crate::windows_session::observe_resumable_bound(&identity, &sid)
                                    })
                                    .await
                                    .map_err(|_| "Windows resume observation task failed".to_string())
                                    .and_then(|result| result);
                                    if observed.is_err() {
                                        let _ = send_resume_rejection(
                                            &mut handoff.socket,
                                            "native session identity changed",
                                            ResumeErrorCode::NativeIdentityChanged,
                                        ).await;
                                        break 'session Err("native session identity changed".to_string());
                                    }
                                    send_json(
                                        agent,
                                        &AgentAttachmentCommand::validate(&handoff.session_log_id),
                                        AgentAttachmentCommand::TYPE,
                                    ).await?;
                                    match await_agent_attachment_status(
                                        agent,
                                        AgentAttachmentState::Validated,
                                        AGENT_ATTACHMENT_TIMEOUT,
                                    ).await {
                                        Ok(status) if status.success => {}
                                        Ok(_) | Err(_) => {
                                            let _ = send_resume_rejection(
                                                &mut handoff.socket,
                                                "resume topology changed",
                                                ResumeErrorCode::TopologyChanged,
                                            ).await;
                                            break 'session Err("held display topology changed".to_string());
                                        }
                                    }
                                    if registry.monotonic_now() >= deadline {
                                        let _ = send_resume_rejection(
                                            &mut handoff.socket,
                                            "resume grant expired",
                                            ResumeErrorCode::Expired,
                                        ).await;
                                        break 'session Err("direct resume window expired".to_string());
                                    }
                                    if let Err(error) = send_successor_auth_result_or_drain(
                                        &mut handoff.socket,
                                        "OK",
                                        &handoff.successor_grant,
                                        handoff.window_secs,
                                        true,
                                        WS_WRITE_TIMEOUT,
                                        registry,
                                        &active_session_id,
                                    ).await {
                                        let _ = reconnect.apply(
                                            ReconnectEvent::OwnerCrashed,
                                            registry.monotonic_now(),
                                        );
                                        break 'session Err(format!(
                                            "resume successor delivery failed: {error:?}"
                                        ));
                                    }
                                    let resumed_transport =
                                        handoff.socket.transport_capability();
                                    send_json(
                                        agent,
                                        &AgentAttachmentCommand::attach(
                                            &handoff.session_log_id,
                                            resumed_transport,
                                        ),
                                        AgentAttachmentCommand::TYPE,
                                    ).await?;
                                    client = handoff.socket;
                                    registry.mark_attached(&active_session_id).map_err(|error| {
                                        format!("could not mark resumed slot attached: {error:?}")
                                    })?;
                                    let _ = reconnect.apply(
                                        ReconnectEvent::ResumeAccepted,
                                        registry.monotonic_now(),
                                    );
                                    continue 'session;
                                }
                                Some(crate::resume::OwnerCommand::Terminal) => {
                                    let _ = reconnect.apply(
                                        ReconnectEvent::OwnerCrashed,
                                        registry.monotonic_now(),
                                    );
                                    break 'session Err("resume binding became terminal".to_string());
                                }
                                Some(crate::resume::OwnerCommand::BrokerShutdown) | None => {
                                    let _ = reconnect.apply(
                                        ReconnectEvent::ExplicitDisconnect,
                                        registry.monotonic_now(),
                                    );
                                    break 'session Ok(());
                                }
                            }
                        }
                        changed = controls.changed() => {
                            changed.map_err(|_| "private agent control channel closed".to_string())?;
                            let control = controls.borrow_and_update().clone();
                            send_json(agent, &control, AgentControl::TYPE).await?;
                        }
                        outgoing = agent.next() => {
                            match outgoing {
                                Some(Ok(Message::Ping(payload))) => {
                                    send_ws_with_timeout(agent, Message::Pong(payload), WS_WRITE_TIMEOUT).await?;
                                }
                                Some(Ok(Message::Close(_))) | None => {
                                    break 'session Err("session agent IPC closed while detached".to_string());
                                }
                                Some(Err(_)) => {
                                    break 'session Err("session agent IPC failed while detached".to_string());
                                }
                                Some(Ok(_)) => {
                                    break 'session Err("session agent sent unexpected data while detached".to_string());
                                }
                            }
                        }
                        _ = monitor.tick() => {
                            let identity = windows_session.clone();
                            let sid = native_user_sid.clone();
                            tokio::task::spawn_blocking(move || {
                                crate::windows_session::observe_resumable_bound(&identity, &sid)
                            })
                            .await
                            .map_err(|_| "Windows detached-session monitor task failed".to_string())??;
                        }
                        changed = session_shutdown.changed() => {
                            if changed.is_err() || *session_shutdown.borrow() {
                                let _ = reconnect.apply(
                                    ReconnectEvent::ExplicitDisconnect,
                                    registry.monotonic_now(),
                                );
                                break 'session Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
    }
    .await;
    match registry.begin_drain(&active_session_id) {
        Ok(()) => result,
        Err(error) => {
            tracing::error!(
                target: SESSION,
                ?error,
                "resume registry final drain failed closed"
            );
            result.and(Err(format!(
                "resume registry final drain failed: {error:?}"
            )))
        }
    }
}

async fn await_agent_attachment_status<A>(
    agent: &mut WebSocketStream<A>,
    expected: AgentAttachmentState,
    timeout: Duration,
) -> Result<AgentAttachmentStatus, String>
where
    A: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(timeout, async {
        let mut discarded = 0_usize;
        loop {
            match agent.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Some(status) = AgentAttachmentStatus::decode(text.as_ref())? {
                        if status.state == expected {
                            return Ok(status);
                        }
                        return Err(
                            "session agent returned an unexpected attachment state".to_string()
                        );
                    }
                    if AgentStreamingReady::is(text.as_ref()) {
                        continue;
                    }
                    if AgentControl::is_reserved(text.as_ref())
                        || AgentAttachmentCommand::is_reserved(text.as_ref())
                    {
                        return Err(
                            "session agent returned malformed reserved attachment control"
                                .to_string(),
                        );
                    }
                    discarded = discarded.saturating_add(1);
                    if discarded > MAX_QUEUED_ATTACHMENT_FRAMES {
                        return Err(
                            "too many queued agent frames during attachment control".to_string()
                        );
                    }
                    continue;
                }
                Some(Ok(Message::Ping(payload))) => {
                    send_ws_with_timeout(agent, Message::Pong(payload), WS_WRITE_TIMEOUT).await?;
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err("session agent IPC closed during attachment control".to_string());
                }
                Some(Ok(Message::Binary(_) | Message::Pong(_) | Message::Frame(_))) => {
                    discarded = discarded.saturating_add(1);
                    if discarded > MAX_QUEUED_ATTACHMENT_FRAMES {
                        return Err(
                            "too many queued agent frames during attachment control".to_string()
                        );
                    }
                }
                Some(Err(_)) => {
                    return Err("session agent IPC failed during attachment control".to_string());
                }
            }
        }
    })
    .await
    .map_err(|_| "session agent attachment control timed out".to_string())?
}

async fn relay_one_attachment<C, CE, A>(
    client: &mut C,
    agent: &mut WebSocketStream<A>,
    windows_session: &WindowsSessionIdentity,
    native_user_sid: &str,
    controls: &mut watch::Receiver<AgentControl>,
    mut resumable: Option<ResumableAttachment<'_>>,
    session_shutdown: &mut watch::Receiver<bool>,
) -> RelayOutcome
where
    C: Sink<Message> + Stream<Item = Result<Message, CE>> + Unpin,
    C::Error: std::fmt::Display,
    CE: std::fmt::Display,
    A: AsyncRead + AsyncWrite + Unpin,
{
    let mut session_monitor = tokio::time::interval(Duration::from_secs(2));
    session_monitor.tick().await;
    let mut refresh_interval: Option<tokio::time::Interval> = None;
    let mut agent_streaming = false;
    let require_active_session = resumable.is_some();
    let write_timeout = resumable
        .as_ref()
        .map(|resumable| resumable_write_timeout(resumable.refresh.window_secs))
        .unwrap_or(WS_WRITE_TIMEOUT);
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
                let refresh = resumable
                    .as_ref()
                    .expect("refresh interval requires resumable context")
                    .refresh;
                let grant = match refresh.registry.refresh_grant(refresh.active_session_id) {
                    Ok(grant) => grant,
                    Err(error) => return RelayOutcome::ResumeAuthorityFailure(error),
                };
                if let Err(error) = send_successor_auth_result_or_drain(
                    client,
                    "Resume grant refreshed",
                    &grant.grant,
                    grant.window_secs,
                    false,
                    write_timeout,
                    refresh.registry,
                    refresh.active_session_id,
                )
                .await
                {
                    return RelayOutcome::ResumeAuthorityFailure(error);
                }
            },
            incoming = client.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if AgentControl::is_reserved(text.as_ref())
                        || AgentStreamingReady::is(text.as_ref())
                        || AgentAttachmentCommand::is_reserved(text.as_ref())
                    {
                        return RelayOutcome::AgentFailure(
                            "client attempted to send reserved broker-agent control".to_string()
                        );
                    }
                    if send_ws_with_timeout(agent, Message::Text(text), write_timeout).await.is_err() {
                        return RelayOutcome::AgentFailure("session agent IPC send failed".to_string());
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if send_ws_with_timeout(agent, Message::Binary(bytes), write_timeout).await.is_err() {
                        return RelayOutcome::AgentFailure("session agent IPC send failed".to_string());
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if send_ws_with_timeout(client, Message::Pong(payload), write_timeout).await.is_err() {
                        return RelayOutcome::UnexpectedLoss("client_transport");
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let _ = send_ws_with_timeout(agent, Message::Close(frame), write_timeout).await;
                    return RelayOutcome::ExplicitClose;
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => return RelayOutcome::UnexpectedLoss("client_transport"),
                None => return RelayOutcome::UnexpectedLoss("client_eof"),
            },
            command = async {
                match resumable.as_mut() {
                    Some(resumable) => resumable.commands.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match command {
                    Some(crate::resume::OwnerCommand::BrokerShutdown)
                    | Some(crate::resume::OwnerCommand::Terminal)
                    | None => return RelayOutcome::BrokerShutdown,
                    Some(crate::resume::OwnerCommand::Resume(_)) => {
                        return RelayOutcome::AgentFailure(
                            "resume handoff arrived while attachment was active".to_string()
                        );
                    }
                }
            },
            changed = session_shutdown.changed() => {
                if changed.is_err() || *session_shutdown.borrow() {
                    return RelayOutcome::BrokerShutdown;
                }
            },
            changed = controls.changed(), if agent_streaming => {
                if changed.is_err() {
                    return RelayOutcome::AgentFailure("private agent control channel closed".to_string());
                }
                let control = controls.borrow_and_update().clone();
                if send_json_with_timeout(agent, &control, AgentControl::TYPE, write_timeout).await.is_err() {
                    return RelayOutcome::AgentFailure("session agent control send failed".to_string());
                }
            },
            outgoing = agent.next() => match outgoing {
                Some(Ok(Message::Text(text))) => {
                    if AgentStreamingReady::is(text.as_ref()) {
                        agent_streaming = true;
                        if let Some(refresh) =
                            resumable.as_ref().map(|resumable| resumable.refresh)
                        {
                            let mut interval =
                                tokio::time::interval(resume_refresh_interval(refresh.window_secs));
                            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                            interval.tick().await;
                            refresh_interval = Some(interval);
                        }
                        let control = controls.borrow_and_update().clone();
                        if send_json_with_timeout(agent, &control, AgentControl::TYPE, write_timeout).await.is_err() {
                            return RelayOutcome::AgentFailure("session agent control send failed".to_string());
                        }
                        continue;
                    }
                    if AgentAttachmentStatus::decode(text.as_ref()).ok().flatten().is_some() {
                        return RelayOutcome::AgentFailure(
                            "session agent returned an unexpected attachment status".to_string()
                        );
                    }
                    if send_ws_with_timeout(client, Message::Text(text), write_timeout).await.is_err() {
                        return RelayOutcome::UnexpectedLoss("client_transport");
                    }
                }
                Some(Ok(Message::Binary(bytes))) => {
                    if send_ws_with_timeout(client, Message::Binary(bytes), write_timeout).await.is_err() {
                        return RelayOutcome::UnexpectedLoss("client_transport");
                    }
                }
                Some(Ok(Message::Ping(payload))) => {
                    if send_ws_with_timeout(agent, Message::Pong(payload), write_timeout).await.is_err() {
                        return RelayOutcome::AgentFailure("session agent IPC send failed".to_string());
                    }
                }
                Some(Ok(Message::Close(frame))) => {
                    let _ = send_ws_with_timeout(client, Message::Close(frame), write_timeout).await;
                    return RelayOutcome::AgentFailure("session agent closed".to_string());
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => return RelayOutcome::AgentFailure("session agent IPC failed".to_string()),
                None => return RelayOutcome::AgentFailure("session agent IPC closed".to_string()),
            },
            _ = session_monitor.tick() => {
                let identity = windows_session.clone();
                let sid = native_user_sid.to_string();
                match tokio::task::spawn_blocking(move || {
                    if require_active_session {
                        crate::windows_session::observe_resumable_bound(&identity, &sid)
                    } else {
                        crate::windows_session::observe_bound(&identity, &sid)
                    }
                }).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return RelayOutcome::NativeSessionEnded(error),
                    Err(_) => return RelayOutcome::NativeSessionEnded(
                        "Windows session monitor task failed".to_string()
                    ),
                }
            },
        }
    }
}

async fn send_json<S, T>(ws: &mut S, value: &T, kind: &'static str) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
    T: serde::Serialize,
{
    send_json_with_timeout(ws, value, kind, WS_WRITE_TIMEOUT).await
}

async fn send_json_with_timeout<S, T>(
    ws: &mut S,
    value: &T,
    kind: &'static str,
    timeout: Duration,
) -> Result<(), String>
where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
    T: serde::Serialize,
{
    let text = serde_json::to_string(value).map_err(|error| format!("serialize: {error}"))?;
    tokio::time::timeout(timeout, ws.send(Message::Text(text.into())))
        .await
        .map_err(|_| format!("{kind} send timed out"))?
        .map_err(|error| format!("{kind} send failed: {error}"))
}

async fn recv_typed<S, E, T>(
    ws: &mut S,
    expected_type: &str,
    timeout: Duration,
) -> Result<T, String>
where
    S: Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
    T: DeserializeOwned,
{
    tokio::time::timeout(timeout, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let envelope: serde_json::Value = serde_json::from_str(text.as_ref())
                        .map_err(|error| format!("invalid JSON during handshake: {error}"))?;
                    let actual = envelope
                        .get("type")
                        .and_then(|value| value.as_str())
                        .ok_or_else(|| "handshake message missing type".to_string())?;
                    if actual != expected_type {
                        return Err(format!(
                            "expected {expected_type} during handshake, received {actual}"
                        ));
                    }
                    return serde_json::from_value(envelope)
                        .map_err(|error| format!("invalid {expected_type}: {error}"));
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err("connection closed during handshake".to_string());
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(format!("WebSocket receive: {error}")),
            }
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {expected_type}"))?
}

fn log_state(peer: &str, state: ServerState) {
    tracing::info!(target: SESSION, %peer, state = state.as_str(), "session state");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorPolicy;
    use arcen_protocol::messages::ClientMonitor;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;
    use std::task::{Context, Poll};

    fn tagged_outbound_video(tag: u8) -> OutboundVideo {
        OutboundVideo {
            message: Message::Binary(vec![tag].into()),
        }
    }

    fn outbound_video_tag(frame: OutboundVideo) -> u8 {
        match frame.message {
            Message::Binary(payload) => payload[0],
            other => panic!("expected binary video, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_an_empty_route_set() {
        let error = OutboundVideoMux::new_with_capacity(Vec::<u16>::new(), 1).unwrap_err();
        assert_eq!(error, RosterError::Empty);
    }

    #[test]
    fn new_rejects_a_duplicate_monitor_id() {
        let error = OutboundVideoMux::new_with_capacity([1, 2, 1], 1).unwrap_err();
        assert_eq!(error, RosterError::Duplicate(1));
    }

    #[test]
    fn new_rejects_more_routes_than_the_shared_maximum() {
        // The exact same bound Linux's `MonitorMux` inherits from the shared
        // `arcen_outputs::FairRoster` — one validated-roster policy shared
        // by both hosts, not a Windows-only cap.
        let count =
            u16::try_from(arcen_media::MAX_MULTI_MONITOR_COUNT + 1).expect("bounded monitor count");
        let error = OutboundVideoMux::new_with_capacity(1..=count, 1).unwrap_err();
        assert_eq!(
            error,
            RosterError::TooMany {
                count: usize::from(count),
                limit: arcen_media::MAX_MULTI_MONITOR_COUNT,
            }
        );
    }

    #[tokio::test]
    async fn carrier_a_three_head_baseline_is_strictly_round_robin() {
        let mux = OutboundVideoMux::new_with_capacity([1, 2, 3], 4).expect("valid three-route mux");
        for (monitor_id, first, second) in [(1, 10, 11), (2, 20, 21), (3, 30, 31)] {
            assert!(matches!(
                mux.push(monitor_id, tagged_outbound_video(first), true),
                VideoPushResult::Enqueued { cleared: 0 }
            ));
            assert!(matches!(
                mux.push(monitor_id, tagged_outbound_video(second), false),
                VideoPushResult::Enqueued { cleared: 0 }
            ));
        }

        let mut order = Vec::new();
        for _ in 0..6 {
            order.push(outbound_video_tag(mux.pop().await.unwrap()));
        }
        assert_eq!(order, [10, 20, 30, 11, 21, 31]);
    }

    #[tokio::test]
    async fn carrier_a_closing_one_route_leaves_buffered_siblings_live() {
        // Member failure: one route's own queue closing directly (not
        // through the mux) must not immediately end delivery for a sibling
        // route that still has a buffered frame ready — Windows' `pop()`
        // always finishes scanning every route for a ready frame before it
        // ever concludes "every route is closed", unlike a race that could
        // pick whichever route happens to resolve first.
        let mux = OutboundVideoMux::new_with_capacity([1, 2], 2).expect("valid two-route mux");
        assert!(matches!(
            mux.push(2, tagged_outbound_video(20), true),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        mux.roster.get(1).expect("known route").close();

        assert_eq!(outbound_video_tag(mux.pop().await.unwrap()), 20);
        assert!(matches!(
            mux.push(1, tagged_outbound_video(10), true),
            VideoPushResult::Closed(_)
        ));
        assert!(matches!(
            mux.push(2, tagged_outbound_video(21), true),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        mux.close_and_clear_all();
    }

    #[tokio::test]
    async fn close_and_clear_all_discards_every_buffered_frame_instead_of_draining_it() {
        // This is the misuse the atomic operation exists to prevent: the old
        // two-call `clear()`-then-`close()` obligation could be forgotten or
        // reordered by a future caller — e.g. calling only `close()` would
        // still let the writer drain every already-buffered frame first
        // (as the now-removed `close_only` fixture used to prove) instead of
        // ending the session's video stream immediately. There is no longer
        // a plain multi-route `close()` on `OutboundVideoMux` to call by
        // mistake; `close_and_clear_all` is the only whole-mux teardown.
        let mux = OutboundVideoMux::new_with_capacity([1, 2, 3], 2).expect("valid three-route mux");
        for (monitor_id, tag) in [(1, 10), (2, 20), (3, 30)] {
            assert!(matches!(
                mux.push(monitor_id, tagged_outbound_video(tag), true),
                VideoPushResult::Enqueued { cleared: 0 }
            ));
        }

        mux.close_and_clear_all();

        assert!(
            mux.pop().await.is_none(),
            "close_and_clear_all must discard every buffered frame, not drain it"
        );
        assert!(matches!(
            mux.push(2, tagged_outbound_video(41), true),
            VideoPushResult::Closed(_)
        ));
    }

    #[tokio::test]
    async fn close_and_clear_all_is_idempotent() {
        let mux = OutboundVideoMux::new_with_capacity([1, 2], 2).expect("valid two-route mux");
        assert!(matches!(
            mux.push(1, tagged_outbound_video(1), true),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        mux.close_and_clear_all();
        mux.close_and_clear_all();
        assert!(mux.pop().await.is_none());
    }

    /// A rejection reason is shown to the operator verbatim by the Deck, so it
    /// has to actually arrive and actually help.
    ///
    /// The empty case is the one worth pinning. `TabletModeReason::try_from`
    /// refuses anything over `MAX_TABLET_MODE_REASON_BYTES`, and the call site
    /// ends in `unwrap_or_default()` — so a reason edited past the limit does
    /// not fail loudly, it silently becomes the empty string and the operator
    /// is told nothing at all. The Windows bridge reason is the longest one
    /// here and has the least headroom.
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
    fn windows_tablet_mode_resolution_keeps_native_bridge_explicit() {
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
        let local =
            resolve_windows_tablet_mode_result(TabletModeMsg::LocalTermination, client, host);
        assert!(local.accepted);
        assert_eq!(local.active, TabletModeMsg::LocalTermination);

        let bridge =
            resolve_windows_tablet_mode_result(TabletModeMsg::WacomUsbBridge, client, host);
        assert!(!bridge.accepted);
        assert_eq!(bridge.active, TabletModeMsg::DisabledMouseCompat);
        assert_reason_reaches_the_operator(bridge.reason.as_str());
        assert!(bridge.reconnect_required);

        let disabled =
            resolve_windows_tablet_mode_result(TabletModeMsg::DisabledMouseCompat, client, host);
        assert!(disabled.accepted);
        assert_eq!(disabled.active, TabletModeMsg::DisabledMouseCompat);

        let unavailable_client = TabletModeCapabilitiesMsg {
            local_termination: InputCapabilityAvailability::Unavailable,
            ..client
        };
        let rejected_local = resolve_windows_tablet_mode_result(
            TabletModeMsg::LocalTermination,
            unavailable_client,
            host,
        );
        assert!(!rejected_local.accepted);
        assert_eq!(rejected_local.active, TabletModeMsg::DisabledMouseCompat);
        assert!(rejected_local.reason.as_str().contains("client did not"));
    }

    #[test]
    fn windows_display_update_protocol_seams_validate_and_preserve_media_contract() {
        let current = test_media_plan(
            EncoderBackend::NativeNvenc,
            arcen_media::VideoCodec::H265,
            arcen_media::ChromaSubsampling::Yuv444,
            1800,
            1168,
            60,
        );
        let update = parse_display_update(
            r#"{"type":"display_update","sequence":7,"width":1396,"height":760,"scale":1.0,"reason":"resize"}"#,
        )
        .expect("display_update");
        assert_eq!(
            validate_display_update(&update, &current).unwrap(),
            DisplaySize {
                width: 1396,
                height: 760,
            }
        );
        assert!(parse_display_update(r#"{"type":"health_ping"}"#).is_none());

        let mut invalid = update.clone();
        invalid.height = 759;
        assert!(validate_display_update(&invalid, &current).is_err());
        invalid = update.clone();
        invalid.width = 1398;
        assert!(validate_display_update(&invalid, &current).is_err());

        let mut resized = current;
        resized.width = 1396;
        resized.height = 760;
        assert!(resize_contract_matches(&current, &resized));
        resized.backend = EncoderBackend::OpenH264;
        assert!(!resize_contract_matches(&current, &resized));

        let result = display_update_result(
            update.sequence,
            true,
            DisplaySize {
                width: 1396,
                height: 760,
            },
            "",
        );
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            r#"{"type":"display_update_result","sequence":7,"accepted":true,"width":1396,"height":760,"message":""}"#
        );
    }

    #[test]
    fn microphone_warning_limiter_reports_suppressed_count_once_per_interval() {
        let start = std::time::Instant::now();
        let mut limiter = RateLimitedMicrophoneWarning::default();
        assert_eq!(limiter.observe_at(start), Some(0));
        assert_eq!(
            limiter.observe_at(start + std::time::Duration::from_secs(1)),
            None
        );
        assert_eq!(
            limiter.observe_at(start + std::time::Duration::from_secs(9)),
            None
        );
        assert_eq!(
            limiter.observe_at(start + arcen_media::audio::MICROPHONE_STATS_INTERVAL),
            Some(2)
        );
    }

    #[tokio::test]
    async fn microphone_teardown_completes_before_unrelated_awaits() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut microphone = Some(());
        let stopped = Arc::clone(&calls);
        take_and_stop_microphone(&mut microphone, move |()| async move {
            stopped.lock().unwrap().push("stop");
            tokio::task::yield_now().await;
            stopped.lock().unwrap().push("joined");
            Ok::<_, ()>(())
        })
        .await
        .unwrap();
        assert!(microphone.is_none());
        calls.lock().unwrap().push("other");
        assert_eq!(*calls.lock().unwrap(), ["stop", "joined", "other"]);
    }

    async fn assert_microphone_cleanup_order(trigger: &'static str) {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let mut microphone = Some(());
        let stopped = Arc::clone(&calls);
        take_and_stop_microphone(&mut microphone, move |()| async move {
            stopped.lock().unwrap().push("stop");
            tokio::task::yield_now().await;
            stopped.lock().unwrap().push("joined");
            Ok::<_, ()>(())
        })
        .await
        .unwrap();
        calls.lock().unwrap().push(trigger);
        assert_eq!(*calls.lock().unwrap(), ["stop", "joined", trigger]);
    }

    #[tokio::test]
    async fn client_microphone_stop_awaits_worker_before_continuing() {
        assert_microphone_cleanup_order("client_stop").await;
    }

    #[tokio::test]
    async fn microphone_playout_failure_awaits_worker_before_disable() {
        assert_microphone_cleanup_order("playout_disable").await;
    }

    #[tokio::test]
    async fn post_acquisition_error_awaits_worker_before_return() {
        assert_microphone_cleanup_order("early_return").await;
    }

    #[test]
    fn detached_cleanup_failure_terminates_before_rebind() {
        let disposition = attachment_disposition(
            true,
            Err(AttachmentError::FatalCleanup(
                "worker did not reap".to_string(),
            )),
        );
        let mut rebinds = 0;
        match disposition {
            AttachmentDisposition::Reattach => rebinds += 1,
            AttachmentDisposition::Terminate(error) => {
                assert_eq!(error, "worker did not reap");
            }
            AttachmentDisposition::Finish(result) => panic!("unexpected result: {result:?}"),
        }
        assert_eq!(rebinds, 0);
    }

    #[test]
    fn production_paths_remain_coupled_to_awaited_microphone_cleanup() {
        let source = include_str!("session.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source precedes tests");
        assert_eq!(
            production
                .matches("shutdown_microphone(&mut microphone,")
                .count(),
            7,
            "post-acquisition, mux/clipboard setup, playout, client-stop, and final teardown paths must stay awaited"
        );
        assert!(
            production.contains("take_and_stop_microphone(microphone, |mut microphone| async move")
                && production.contains("microphone.shutdown_wait(stop_reason).await"),
            "the production cleanup wrapper must await the native feeder join"
        );

        let final_cleanup = production
            .rfind("shutdown_microphone(&mut microphone, \"attachment_teardown\")")
            .expect("stream cleanup path");
        let unrelated_cleanup = production[final_cleanup..]
            .find("log_state(peer, ServerState::Draining)")
            .expect("unrelated attachment cleanup")
            + final_cleanup;
        assert!(
            final_cleanup < unrelated_cleanup,
            "microphone teardown must precede unrelated attachment cleanup"
        );

        let disposition = production
            .find("match attachment_disposition(attachment.detached, attachment.result)")
            .expect("attachment disposition");
        let fatal = production[disposition..]
            .find("AttachmentDisposition::Terminate(error)")
            .expect("fatal cleanup disposition")
            + disposition;
        let reattach = production[fatal..]
            .find("await_attachment_command(")
            .expect("reattachment command")
            + fatal;
        assert!(
            fatal < reattach,
            "fatal cleanup must terminate before any reattachment command"
        );
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
        assert_eq!(resumable_write_timeout(60), WS_WRITE_TIMEOUT);
    }

    #[test]
    fn agent_start_json_contains_no_resume_material() {
        let mut response = AuthResponse::pam("artist", "password");
        response.resume_requested = true;
        response.resume_holder_nonce = Some("holder-secret".to_string());
        response.resume_grant = Some("grant-secret".to_string());
        clear_resume_material(&mut response);

        let cfg = HostConfig {
            capenc_bin: "capenc".to_string(),
            output_selector: crate::display::OutputSelector::GlobalIndex(0),
            output_index: 0,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            color_policy: ColorPolicy::DefaultOff,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 30,
            encoder: crate::capenc::EncoderSelection::Auto,
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            reconnect_window_secs: 30,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        let start = AgentStart::new(
            "peer".to_string(),
            &cfg,
            response,
            WindowsSessionIdentity {
                session_id: 7,
                user_sid: "S-1-5-21-1000".to_string(),
                user: "artist".to_string(),
                domain: "STUDIO".to_string(),
                state: "active".to_string(),
                launch_backend: "wts".to_string(),
            },
            "agent.log".to_string(),
            &CorrelationId::from_uuid_v4_bytes([7; 16]),
            AgentControl::new(
                1,
                arcen_telemetry::OperationalProfile::Info,
                arcen_telemetry::QosTargets::default(),
                false,
                0,
            ),
            CAPABILITY_TRANSPORT_QUIC,
        );
        let json = serde_json::to_string(&start).unwrap();
        assert!(!json.contains("holder-secret"));
        assert!(!json.contains("grant-secret"));
        assert!(!json.contains("password"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let auth = &value["auth_response"];
        assert!(auth
            .get("resume_requested")
            .is_none_or(|value| value == false));
        assert!(auth["resume_holder_nonce"].is_null());
        assert!(auth["resume_grant"].is_null());
        assert_eq!(auth["credential"], "");
        assert_eq!(
            value["transport_capability"],
            arcen_protocol::CAPABILITY_TRANSPORT_QUIC
        );
    }

    #[tokio::test]
    async fn attachment_status_discards_queued_health_and_media_frames() {
        let (broker_io, agent_io) = tokio::io::duplex(4_096);
        let mut broker = WebSocketStream::from_raw_socket(broker_io, Role::Client, None).await;
        let mut agent = WebSocketStream::from_raw_socket(agent_io, Role::Server, None).await;
        let sender = tokio::spawn(async move {
            agent
                .send(Message::Text(
                    r#"{"type":"health_stats","frames":1}"#.into(),
                ))
                .await
                .unwrap();
            agent.send(Message::Binary(vec![1, 2, 3])).await.unwrap();
            agent
                .send(Message::Text(
                    serde_json::to_string(&AgentAttachmentStatus::detached()).unwrap(),
                ))
                .await
                .unwrap();
        });

        let status = await_agent_attachment_status(
            &mut broker,
            AgentAttachmentState::Detached,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert!(status.success);
        sender.await.unwrap();
    }

    #[test]
    fn resume_auth_route_is_credential_free_and_never_enters_windows_auth() {
        let response = AuthResponse::resume("03".repeat(32), "opaque");
        assert_eq!(
            authentication_route(&response),
            AuthenticationRoute::ResumeRegistry
        );
        assert!(response.username.is_empty());
        assert!(response.credential.is_empty());
        assert!(validate_auth_response(&response).is_err());

        let password = AuthResponse::pam("artist", "password");
        assert_eq!(
            authentication_route(&password),
            AuthenticationRoute::WindowsCredential
        );
    }

    #[test]
    fn resume_advertisement_tracks_wiring_window_and_detached_disclaimer_state() {
        let disclaimer = PreparedDisclaimer::from_bytes(
            arcen_identity::DisclaimerLocale::new("en_US").unwrap(),
            b"Authorized use only",
        )
        .unwrap();
        let initial = build_auth_request(Some(&disclaimer), true, false, None);
        assert!(initial.supports_resume());
        assert_eq!(initial.disclaimer.as_deref(), Some("Authorized use only"));
        assert!(initial.multi_monitor_v1_offer().is_none());
        assert!(!serde_json::to_value(&initial)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("multi_monitor_v1"));

        let detached = build_auth_request(Some(&disclaimer), true, true, None);
        assert!(detached.supports_resume());
        assert!(detached.disclaimer.is_none());

        let disabled = build_auth_request(Some(&disclaimer), false, false, None);
        assert!(!disabled.supports_resume());
        assert!(disabled.disclaimer.is_some());
    }

    #[test]
    fn cp_failure_reason_classes_are_closed_and_safe() {
        use crate::first_login::FirstLoginError;

        assert_eq!(cp_failure_reason_class(&FirstLoginError::Busy), "cp_busy");
        assert_eq!(
            cp_failure_reason_class(&FirstLoginError::NoCredentialProvider),
            "cp_not_ready"
        );
        assert_eq!(
            cp_failure_reason_class(&FirstLoginError::Payload("secret detail".to_string())),
            "cp_payload_invalid"
        );
        assert_eq!(
            cp_failure_reason_class(&FirstLoginError::PushFailed("secret detail".to_string())),
            "cp_push_failed"
        );
        assert_eq!(
            cp_failure_reason_class(&FirstLoginError::SessionTimeout),
            "cp_session_timeout"
        );
        assert_eq!(
            cp_failure_reason_class(&FirstLoginError::SessionProbe("S-1-5-21-...".to_string())),
            "cp_session_probe_failed"
        );
        assert_eq!(
            cp_failure_reason_class(&FirstLoginError::Unsupported),
            "cp_unsupported_platform"
        );
    }

    #[test]
    fn display_policy_names_are_stable() {
        assert_eq!(
            display_policy_name(crate::display::DisplayPolicy::ExactIsolated),
            "exact_isolated"
        );
        assert_eq!(
            display_policy_name(crate::display::DisplayPolicy::Negotiated),
            "negotiated"
        );
        assert_eq!(
            display_policy_name(crate::display::DisplayPolicy::NegotiatedMacroblock16),
            "negotiated_macroblock16"
        );
    }

    #[test]
    fn stream_interruption_reason_classes_never_carry_the_raw_message() {
        assert_eq!(
            classify_stream_interruption("capenc engine exited"),
            "capture_engine_exit"
        );
        assert_eq!(
            classify_stream_interruption("outbound writer closed"),
            "writer_closed"
        );
        assert_eq!(
            classify_stream_interruption("outbound writer stopped"),
            "writer_task_failed"
        );
        assert_eq!(
            classify_stream_interruption("outbound writer task failed: boom"),
            "writer_task_failed"
        );
        assert_eq!(
            classify_stream_interruption("WebSocket receive failed: boom"),
            "transport_error"
        );
        assert_eq!(
            classify_stream_interruption("audio capture restart exhausted retries"),
            "audio_error"
        );
        assert_eq!(
            classify_stream_interruption("some other failure"),
            "stream_failure"
        );
    }

    #[test]
    fn session_lifecycle_emitters_never_panic_on_a_disabled_emitter() {
        let emitter = LifecycleEmitter::disabled();
        let correlation_id =
            CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef").expect("uuid");
        emit_session_auth_ok(
            &emitter,
            correlation_id.clone(),
            "artist",
            "192.0.2.10:5000",
            "existing_session",
            7,
        );
        emit_session_auth_fail(
            &emitter,
            correlation_id.clone(),
            Some("artist"),
            "192.0.2.10:5000",
            "session_bind",
            "bind_error",
        );
        emit_cp_logon_ok(
            &emitter,
            correlation_id.clone(),
            "artist",
            "192.0.2.10:5000",
            7,
        );
        emit_cp_logon_fail(
            &emitter,
            correlation_id.clone(),
            Some("artist"),
            "192.0.2.10:5000",
            &crate::first_login::FirstLoginError::SessionTimeout,
        );
        emit_session_end_or_interrupted(
            &emitter,
            correlation_id.clone(),
            "artist",
            "192.0.2.10:5000",
            &Ok(()),
            10,
            5,
            1,
        );
        emit_session_end_or_interrupted(
            &emitter,
            correlation_id,
            "artist",
            "192.0.2.10:5000",
            &Err("boom".to_string()),
            10,
            5,
            1,
        );
    }

    struct PendingSink;

    impl Sink<Message> for PendingSink {
        type Error = &'static str;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            unreachable!("pending sink never becomes ready")
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Pending
        }

        fn poll_close(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    fn registered_windows_resume() -> (
        Arc<crate::resume::ResumeRegistry>,
        crate::resume::ResumeBindings,
        arcen_identity::ActiveHostSessionId,
    ) {
        let registry = crate::resume::ResumeRegistry::new().unwrap();
        let response = AuthResponse::resume("03".repeat(32), "placeholder");
        let (disclaimer_digest, disclaimer_version) =
            crate::resume::no_disclaimer_binding().unwrap();
        let bindings = crate::resume::ResumeBindings {
            host_identity: arcen_identity::HostIdentity::new("spki-sha256:test-host").unwrap(),
            active_session_id: arcen_identity::ActiveHostSessionId::new("windows-wts:7").unwrap(),
            native_principal: arcen_identity::NativePrincipal::Windows {
                sid: arcen_identity::WindowsSid::new("S-1-5-21-1-2-3-1001").unwrap(),
                wts_session_id: 7,
            },
            holder_nonce: arcen_identity::DeckHolderNonce::new([3; 32]),
            disclaimer_digest,
            disclaimer_version,
            topology: crate::resume::TopologyBinding::from_response(&response).unwrap(),
        };
        let active_session_id = bindings.active_session_id.clone();
        let (owner, _commands) = mpsc::unbounded_channel();
        registry
            .issue_initial(
                bindings.clone(),
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        (registry, bindings, active_session_id)
    }

    struct RecordingSink(Arc<StdMutex<Vec<Message>>>);

    struct RejectingSink;

    impl Sink<Message> for RejectingSink {
        type Error = &'static str;

        fn poll_ready(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Err("rejected")
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

    impl Sink<Message> for RecordingSink {
        type Error = &'static str;

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

    fn legacy_pcm_stream() -> ResolvedAudioStream {
        AudioPolicy {
            opus_available: true,
            pcm_available: true,
        }
        .resolve(None, true, 128)
    }

    #[derive(Clone, Default)]
    struct TestCaptureFactory {
        starts: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        clean_shutdowns: Arc<AtomicUsize>,
    }

    struct TestCapture {
        telemetry: Arc<AudioTelemetry>,
        active: Arc<AtomicUsize>,
        clean_shutdowns: Arc<AtomicUsize>,
        stopped: bool,
    }

    impl ManagedAudioCapture for TestCapture {
        fn telemetry(&self) -> AudioTelemetrySnapshot {
            self.telemetry.snapshot()
        }

        fn telemetry_handle(&self) -> Arc<AudioTelemetry> {
            Arc::clone(&self.telemetry)
        }

        async fn shutdown(&mut self) {
            if !self.stopped {
                self.stopped = true;
                self.active.fetch_sub(1, Ordering::SeqCst);
                self.clean_shutdowns.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl Drop for TestCapture {
        fn drop(&mut self) {
            if !self.stopped {
                self.active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    impl AudioCaptureFactory for TestCaptureFactory {
        type Capture = TestCapture;

        fn start(&mut self, queue: Arc<LatestQueue<AudioPacket>>) -> Result<Self::Capture, String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            queue
                .push(AudioPacket {
                    pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
                    timestamp_ms: 1,
                })
                .expect("test audio queue must be open");
            Ok(TestCapture {
                telemetry: Arc::new(AudioTelemetry::default()),
                active: Arc::clone(&self.active),
                clean_shutdowns: Arc::clone(&self.clean_shutdowns),
                stopped: false,
            })
        }
    }

    #[test]
    fn shared_header_is_exactly_ten_bytes() {
        let frame = crate::capenc::EncodedFrame {
            data: vec![0xAA, 0xBB],
            keyframe: true,
            timestamp_ms: 0x0102_0304,
        };
        let media_plan = test_media_plan(
            arcen_media::video::EncoderBackend::NativeNvenc,
            arcen_media::VideoCodec::H264,
            arcen_media::ChromaSubsampling::Yuv420,
            1920,
            1080,
            30,
        );
        let message = frame_message(&media_plan, &frame, 0, 0, 0);
        assert_eq!(message.len(), arcen_protocol::VIDEO_HEADER_SIZE + 2);
        assert_eq!(&message[..10], &[3, 1, 0, 1, 1, 2, 3, 4, 0, 0]);
    }

    #[test]
    fn region_header_carries_wire_generation_and_epoch() {
        let frame = crate::capenc::EncodedFrame {
            data: vec![0xAA, 0xBB],
            keyframe: true,
            timestamp_ms: 0x0102_0304,
        };
        let media_plan = test_media_plan(
            arcen_media::video::EncoderBackend::NativeNvenc,
            arcen_media::VideoCodec::H264,
            arcen_media::ChromaSubsampling::Yuv420,
            1920,
            1080,
            30,
        );
        let message = frame_message(&media_plan, &frame, 2, 7, 11);
        let header = arcen_protocol::decode_video_header(&message).expect("region header");
        assert_eq!(header.frame_type, FrameType::RegionVideoH264);
        assert_eq!(header.monitor_id, 2);
        assert_eq!(header.topology_generation, 7);
        assert_eq!(header.stream_epoch, 11);
    }

    #[test]
    fn every_frame_carries_the_resolved_color_contract() {
        let frame = crate::capenc::EncodedFrame {
            data: vec![0xAA, 0xBB],
            keyframe: false,
            timestamp_ms: 0x0102_0304,
        };
        let mut media_plan = test_media_plan(
            arcen_media::video::EncoderBackend::NativeNvenc,
            arcen_media::VideoCodec::H265,
            arcen_media::ChromaSubsampling::Yuv444,
            1920,
            1080,
            30,
        );
        media_plan.video = arcen_media::VideoConfiguration::grading_reference();
        let message = frame_message(&media_plan, &frame, 0, 0, 0);
        let header = arcen_protocol::decode_video_header(&message).expect("video header");
        assert!(!header.is_keyframe());
        assert_eq!(header.bit_depth(), Ok(arcen_protocol::wire::BitDepth::Ten));
        assert_eq!(header.color_range(), arcen_protocol::wire::ColorRange::Full);
        assert_eq!(
            header.color_matrix(),
            Ok(arcen_protocol::wire::ColorMatrix::Bt709)
        );
    }

    #[test]
    fn validates_auth_limits() {
        let mut response = AuthResponse::pam("user", "password");
        response.screen_width = 3600;
        response.screen_height = 2338;
        assert_eq!(
            validate_auth_response(&response).unwrap().monitors[0]
                .request
                .size,
            DisplaySize {
                width: 3600,
                height: 2338
            }
        );

        let response = AuthResponse::pam("user", "password");
        assert!(validate_auth_response(&response).is_err());

        let mut response = AuthResponse::pam("user", "password");
        response.screen_width = 20_000;
        response.screen_height = 1080;
        assert!(validate_auth_response(&response).is_err());

        let mut response = AuthResponse::pam("user", "password");
        response.credential = "x".repeat(4097);
        assert!(validate_auth_response(&response).is_err());
    }

    fn client_monitor(width_px: u32, height_px: u32, is_primary: bool) -> ClientMonitor {
        ClientMonitor {
            id: 42,
            width_px,
            height_px,
            refresh_hz: 75,
            scale: 2.0,
            width_mm: 344.0,
            height_mm: 223.0,
            model: 0x1234,
            serial: 0x5678_9abc,
            is_primary,
            name: "Built-in Display".to_string(),
            ..Default::default()
        }
    }

    fn display_response(mode: &str, monitors: Vec<ClientMonitor>) -> AuthResponse {
        let mut response = AuthResponse::pam("user", "password");
        response.screen_width = 3600;
        response.screen_height = 2338;
        response.displays_mode = mode.to_string();
        response.monitors = monitors;
        response
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
    fn disclaimer_gate_rejects_every_negative_acknowledgment_before_auth() {
        let disclaimer = prepared_disclaimer();
        let mut response = AuthResponse::pam("user", "password");
        assert!(validate_disclaimer_acknowledgment(Some(disclaimer.clone()), &response).is_err());

        response.disclaimer_acceptance_sha256 = Some("0".repeat(64));
        assert!(validate_disclaimer_acknowledgment(Some(disclaimer.clone()), &response).is_err());

        response.disclaimer_acceptance_sha256 = Some("A".repeat(64));
        assert!(validate_disclaimer_acknowledgment(Some(disclaimer), &response).is_err());
    }

    #[test]
    fn disclaimer_gate_accepts_only_the_exact_digest() {
        let disclaimer = prepared_disclaimer();
        let mut response = AuthResponse::pam("user", "password");
        response.disclaimer_acceptance_sha256 = Some(disclaimer.digest().to_lower_hex());
        let acknowledged = validate_disclaimer_acknowledgment(Some(disclaimer.clone()), &response)
            .expect("matching acknowledgment")
            .expect("acknowledged disclaimer");
        assert_eq!(
            acknowledged.disclaimer.digest().to_lower_hex(),
            disclaimer.digest().to_lower_hex()
        );
        assert!(
            validate_disclaimer_acknowledgment(None, &AuthResponse::pam("user", "password"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn primary_monitor_attributes_feed_the_exact_display_request() {
        let response = display_response("match_layout", vec![client_monitor(3600, 2338, true)]);

        let plan = session_display_plan(&response).unwrap();

        let [monitor] = plan.monitors.as_slice() else {
            panic!("single-display plan expected");
        };
        assert_eq!(
            monitor.request.size,
            DisplaySize::validate(3600, 2338).unwrap()
        );
        assert_eq!(monitor.request.refresh_hz, 75);
        assert_eq!(monitor.request.width_mm, 344.0);
        assert_eq!(monitor.request.height_mm, 223.0);
        assert_eq!(monitor.request.scale, 2.0);
        assert_eq!(monitor.request.product_id, 0x1234);
        assert_eq!(monitor.request.serial, 0x5678_9abc);
        assert_eq!(monitor.client_monitor_id, 42);
        assert!(monitor.is_primary);
        assert_eq!(monitor.name, "Built-in Display");
    }

    #[test]
    fn all_single_display_modes_plan_the_same_primary_mirror() {
        for mode in ["", "single_primary", "windowed", "match_layout"] {
            let response = display_response(mode, vec![client_monitor(3600, 2338, true)]);
            let plan = session_display_plan(&response)
                .unwrap_or_else(|error| panic!("mode {mode:?} must plan: {error}"));
            assert_eq!(plan.monitors.len(), 1, "mode {mode:?}");
            assert_eq!(
                plan.monitors[0].request.size,
                DisplaySize::validate(3600, 2338).unwrap(),
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn match_layout_with_multiple_monitors_degrades_to_primary_display() {
        let response = display_response(
            "match_layout",
            vec![
                client_monitor(2560, 1440, false),
                client_monitor(3600, 2338, true),
            ],
        );
        let plan = session_display_plan(&response).unwrap();

        assert_eq!(plan.monitors.len(), 1);
        assert_eq!(
            plan.monitors[0].request.size,
            DisplaySize::validate(3600, 2338).unwrap()
        );
        assert_eq!(plan.monitors[0].client_monitor_id, 42);
        let degradation = plan.degradation.as_ref().expect("degradation");
        assert_eq!(degradation.requested_mode, "match_layout");
        assert_eq!(degradation.requested_monitors, 2);
        assert_eq!(degradation.served_mode, "single_primary");
        assert_eq!(degradation.reason, "multi_monitor_match_layout_degraded");
        assert_eq!(
            display_plan_degradation_capability(&plan)["reason"],
            "multi_monitor_match_layout_degraded"
        );
    }

    #[test]
    fn single_primary_with_multiple_monitors_streams_only_the_primary() {
        let response = display_response(
            "single_primary",
            vec![
                client_monitor(2560, 1440, false),
                client_monitor(3600, 2338, true),
            ],
        );
        let plan = session_display_plan(&response).unwrap();
        assert_eq!(plan.monitors.len(), 1);
        assert_eq!(
            plan.monitors[0].request.size,
            DisplaySize::validate(3600, 2338).unwrap()
        );
    }

    #[test]
    fn unknown_display_mode_is_refused() {
        let response = display_response("pick", vec![client_monitor(3600, 2338, true)]);
        let error = session_display_plan(&response).unwrap_err();
        assert!(error.contains("not supported"));
    }

    #[test]
    fn excessive_client_monitor_count_is_refused() {
        let response = display_response(
            "match_layout",
            (0..9)
                .map(|index| {
                    let mut monitor = client_monitor(3600, 2338, index == 0);
                    monitor.id = index;
                    monitor
                })
                .collect(),
        );
        let error = session_display_plan(&response).unwrap_err();
        assert!(error.contains("safety limit"));
    }

    #[test]
    fn primary_monitor_disagreeing_with_session_size_is_refused() {
        let response = display_response("match_layout", vec![client_monitor(2560, 1440, true)]);
        let error = session_display_plan(&response).unwrap_err();
        assert!(error.contains("disagrees with the authenticated session size"));
    }

    #[test]
    fn client_hello_must_repeat_authenticated_dimensions() {
        let authenticated = DisplaySize {
            width: 3600,
            height: 2338,
        };
        let matching = ClientHelloMsg {
            screen_width: 3600,
            screen_height: 2338,
            ..ClientHelloMsg::default()
        };
        assert_eq!(
            validate_client_hello(&matching, authenticated).unwrap(),
            authenticated
        );

        let mismatched = ClientHelloMsg {
            screen_width: 1920,
            screen_height: 1080,
            ..ClientHelloMsg::default()
        };
        assert!(validate_client_hello(&mismatched, authenticated).is_err());

        let odd = ClientHelloMsg {
            screen_width: 3599,
            screen_height: 2338,
            ..ClientHelloMsg::default()
        };
        assert!(validate_client_hello(&odd, authenticated).is_err());
    }

    #[test]
    fn session_log_id_accepts_deck_uuid_and_replaces_invalid_text() {
        let value = "01234567-89ab-4def-8123-456789abcdef";
        let (accepted, replaced) = resolve_session_log_id(Some(value)).unwrap();
        assert_eq!(accepted.as_str(), value);
        assert!(!replaced);

        let (fallback, replaced) = resolve_session_log_id(Some("bad\nsid")).unwrap();
        assert!(replaced);
        CorrelationId::parse_uuid(fallback.to_string()).unwrap();
    }

    #[test]
    fn openh264_display_negotiation_preserves_valid_sizes_and_fits_oversize() {
        assert_eq!(
            openh264_fitted_display_size(DisplaySize {
                width: 1801,
                height: 1169,
            })
            .unwrap(),
            DisplaySize {
                width: 1800,
                height: 1168,
            }
        );
        assert_eq!(
            openh264_fitted_display_size(DisplaySize {
                width: 3840,
                height: 2160,
            })
            .unwrap(),
            DisplaySize {
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn openh264_refuses_an_oversize_applied_display_before_server_hello() {
        assert!(ensure_openh264_applied_size(DisplaySize {
            width: 1920,
            height: 1080,
        })
        .is_ok());
        let error = ensure_openh264_applied_size(DisplaySize {
            width: 2560,
            height: 1440,
        })
        .unwrap_err();
        assert!(error.contains("OpenH264"));
        assert!(error.contains("2560x1440"));
    }

    #[test]
    fn windows_auto_nvenc_failure_retargets_before_openh264_capture() {
        let requested = DisplaySize {
            width: 2560,
            height: 1440,
        };
        assert_eq!(
            openh264_fallback_retarget_size(requested, requested, true).unwrap(),
            Some(DisplaySize {
                width: 1920,
                height: 1080,
            })
        );
        let fitted = DisplaySize {
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            openh264_fallback_retarget_size(fitted, requested, false).unwrap(),
            None
        );
        assert_eq!(
            capenc_encoder_for_attempt(
                crate::capenc::EncoderSelection::Auto,
                crate::capenc::EncoderSelection::Nvenc,
            ),
            crate::capenc::EncoderSelection::Auto
        );
        assert_eq!(
            capenc_encoder_for_attempt(
                crate::capenc::EncoderSelection::Auto,
                crate::capenc::EncoderSelection::SoftwareH264,
            ),
            crate::capenc::EncoderSelection::SoftwareH264
        );
        assert!(
            openh264_fallback_retarget_size(requested, requested, false).is_err(),
            "reconnect must not hide a second display modeset"
        );
    }

    #[test]
    fn adaptive_nvenc_codec_order_is_av1_then_hevc_then_h264() {
        let mut config = HostConfig {
            capenc_bin: "capenc".to_string(),
            output_selector: crate::display::OutputSelector::GlobalIndex(0),
            output_index: 0,
            codec: VideoCodec::Av1,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            color_policy: ColorPolicy::AlwaysOn,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 60,
            encoder: crate::capenc::EncoderSelection::Auto,
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            reconnect_window_secs: 0,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        assert_eq!(next_adaptive_nvenc_codec(&config), Some(VideoCodec::H265));
        config.codec = VideoCodec::H265;
        assert_eq!(next_adaptive_nvenc_codec(&config), Some(VideoCodec::H264));
        config.codec = VideoCodec::H264;
        assert_eq!(next_adaptive_nvenc_codec(&config), None);
        config.codec = VideoCodec::H265;
        config.video_selection = arcen_protocol::messages::VideoSelectionIntent::ColorFidelity;
        assert_eq!(next_adaptive_nvenc_codec(&config), None);
    }

    #[test]
    fn server_hello_reports_applied_display_and_backend() {
        let cfg = HostConfig {
            capenc_bin: "capenc".to_string(),
            output_selector: crate::display::OutputSelector::GlobalIndex(2),
            output_index: 2,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            color_policy: ColorPolicy::DefaultOff,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 30,
            encoder: crate::capenc::EncoderSelection::Auto,
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            reconnect_window_secs: 0,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        let report = DisplayReport {
            requested: DisplaySize {
                width: 3600,
                height: 2338,
            },
            applied: DisplaySize {
                width: 2560,
                height: 1600,
            },
            original: DisplaySize {
                width: 1680,
                height: 1050,
            },
            original_refresh_hz: 59,
            applied_refresh_hz: 60,
            exact: false,
            changed: true,
            retarget_capable: false,
            backend: "change-display-settings-ex-temporary-fallback",
            restore_backend: "set-display-config-plus-exact-devmode",
            device_name: r"\\.\DISPLAY6".to_string(),
            capture_output_index: 1,
            desktop_rect: crate::display::DesktopRect {
                left: 1920,
                top: 0,
                width: 2560,
                height: 1600,
            },
            effective_scale_reports: vec![crate::display::DisplayScaleReport {
                client_display_id: "philips-secondary".to_string(),
                session_monitor_id: 2,
                device_name: r"\\.\DISPLAY6".to_string(),
                requested_scale_percent: 200,
                effective_dpi_x: 96,
                effective_dpi_y: 96,
                effective_scale_percent: 100,
                matches_requested: false,
            }],
        };

        let identity = WindowsSessionIdentity {
            session_id: 7,
            user_sid: "S-1-5-21-1000".to_string(),
            user: "artist".to_string(),
            domain: "STUDIO".to_string(),
            state: "active".to_string(),
            launch_backend: "wts-query-user-token-create-process-as-user".to_string(),
        };
        let plan = SessionDisplayPlan {
            monitors: vec![MonitorPlan {
                request: {
                    let mut request = DisplayRequest::new(3600, 2338).unwrap();
                    request.scale = 2.0;
                    request
                },
                is_primary: true,
                client_monitor_id: 42,
                name: "Built-in Display".to_string(),
            }],
            degradation: None,
        };
        let native_media = test_media_plan(
            arcen_media::video::EncoderBackend::NativeNvenc,
            arcen_media::VideoCodec::H265,
            arcen_media::ChromaSubsampling::Yuv444,
            report.applied.width,
            report.applied.height,
            30,
        );
        let hello = build_server_hello(
            &cfg,
            &report,
            &plan,
            &identity,
            r"C:\logs\arcen-session-agent.log",
            &native_media,
            false,
            false,
            false,
        );
        assert_eq!((hello.screen_width, hello.screen_height), (2560, 1600));
        assert_eq!(hello.monitors.len(), 1);
        assert_eq!(hello.monitors[0]["id"], 42);
        assert_eq!(hello.monitors[0]["x"], 1920);
        assert_eq!(hello.monitors[0]["width_px"], 2560);
        assert_eq!(hello.monitors[0]["height_px"], 1600);
        assert_eq!(hello.monitors[0]["refresh_hz"], 60);
        assert_eq!(hello.monitors[0]["scale"], 2.0);
        assert_eq!(hello.monitors[0]["is_primary"], true);
        assert_eq!(hello.monitors[0]["name"], "Built-in Display");
        assert_eq!(hello.monitors[0]["capture_output_index"], 1);
        let display = &hello.device_capabilities["display_resolution"];
        assert_eq!(display["requested"]["width"], 3600);
        assert_eq!(display["applied"]["height"], 1600);
        assert_eq!(
            display["backend"],
            "change-display-settings-ex-temporary-fallback"
        );
        assert_eq!(display["exact"], false);
        assert_eq!(display["selected_output_index"], 2);
        assert_eq!(display["capture_output_index"], 1);
        assert_eq!(display["resize"]["available"], false);
        assert_eq!(display["resize"]["reason"], "display_lease_not_exact");
        assert_eq!(display["resize"]["mechanism"], "none");
        assert_eq!(display["resize"]["scope"], "none");
        assert_eq!(display["degradation"]["active"], false);
        assert_eq!(
            display["effective_display_scale"]["source"],
            "GetDpiForMonitor(MDT_EFFECTIVE_DPI)"
        );
        assert_eq!(
            display["effective_display_scale"]["monitors"][0]["client_display_id"],
            "philips-secondary"
        );
        assert_eq!(
            display["effective_display_scale"]["monitors"][0]["requested_scale_percent"],
            200
        );
        assert_eq!(
            display["effective_display_scale"]["monitors"][0]["effective_scale_percent"],
            100
        );
        assert_eq!(
            display["effective_display_scale"]["monitors"][0]["matches_requested"],
            false
        );
        assert!(display["resize"]["explanation"]
            .as_str()
            .expect("resize explanation")
            .contains("exact display lease"));
        assert_eq!(hello.device_capabilities["input"]["available"], true);
        assert_eq!(hello.encoder_backend, "native-nvenc");
        assert!(
            hello.supports_h264,
            "capability flags describe the resolved backend roster, not only the active codec"
        );
        assert!(hello.supports_h265);
        assert!(!hello.supports_av1);
        assert!(hello.supports_yuv444);
        assert!(
            !hello.supports_display_update,
            "negotiated/fallback display leases must not advertise live resize"
        );
        let exact_report = DisplayReport {
            exact: true,
            ..report.clone()
        };
        assert!(
            !windows_display_update_supported(&exact_report),
            "exact geometry without a proven retarget backend must not advertise resize"
        );
        let retarget_capable_report = DisplayReport {
            retarget_capable: true,
            backend: "nvidia-nvapi-edid-saved-custom-timing",
            restore_backend: "nvapi-purge-plus-set-display-config-exact",
            ..exact_report
        };
        assert!(windows_display_update_supported(&retarget_capable_report));
        assert_eq!(hello.input_protocol_version, INPUT_PROTOCOL_VERSION);
        assert_eq!(
            hello.input_capabilities.relative_pointer,
            InputCapabilityAvailability::Available
        );
        assert_eq!(
            hello.input_capabilities.host_cursor,
            InputCapabilityAvailability::Available
        );
        assert_eq!(
            hello.input_capabilities.region_input,
            InputCapabilityAvailability::Unavailable
        );
        let region_hello = build_server_hello(
            &cfg,
            &report,
            &plan,
            &identity,
            r"C:\logs\arcen-session-agent.log",
            &native_media,
            false,
            false,
            true,
        );
        assert_eq!(
            region_hello.input_capabilities.region_input,
            InputCapabilityAvailability::Available,
        );
        assert!(region_hello.input_protocol_version >= REGION_INPUT_PROTOCOL_VERSION);
        assert!(supports_region_input_v1(
            region_hello.input_protocol_version,
            region_hello.input_capabilities,
        ));
        assert_eq!(
            hello.input_capabilities.pen,
            InputCapabilityAvailability::Unavailable,
            "pen_available=false must honestly advertise Unavailable, never Unknown"
        );
        assert_eq!(
            hello.input_capabilities.pen_pressure,
            InputCapabilityAvailability::Unavailable
        );
        assert_eq!(
            hello.device_capabilities["input"]["pen_backend"],
            "unavailable"
        );
        assert_eq!(
            hello.audio_output.as_ref().unwrap().codecs,
            vec![AudioCodec::Pcm]
        );
        let compressed_hello = build_server_hello(
            &HostConfig {
                audio_compressed: true,
                ..cfg.clone()
            },
            &report,
            &plan,
            &identity,
            r"C:\logs\arcen-session-agent.log",
            &native_media,
            false,
            false,
            false,
        );
        assert_eq!(
            compressed_hello.audio_output.unwrap().codecs,
            vec![AudioCodec::Opus]
        );

        let mf_cfg = HostConfig {
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            encoder: crate::capenc::EncoderSelection::SoftwareH264,
            ..cfg
        };
        let mf_media = test_media_plan(
            arcen_media::video::EncoderBackend::OpenH264,
            arcen_media::VideoCodec::H264,
            arcen_media::ChromaSubsampling::Yuv420,
            report.applied.width,
            report.applied.height,
            30,
        );
        let mf_hello = build_server_hello(
            &mf_cfg,
            &report,
            &plan,
            &identity,
            r"C:\logs\mf-session-agent.log",
            &mf_media,
            false,
            false,
            false,
        );
        assert!(mf_hello.supports_h264);
        assert!(!mf_hello.supports_h265);
        assert!(!mf_hello.supports_yuv444);
        assert_eq!(mf_hello.encoder_backend, "openh264-sw-h264");
        assert!(mf_hello.available_encoders.is_empty());

        let openh264_media = test_media_plan(
            arcen_media::video::EncoderBackend::OpenH264,
            arcen_media::VideoCodec::H264,
            arcen_media::ChromaSubsampling::Yuv420,
            1920,
            1080,
            30,
        );
        let openh264_hello = build_server_hello(
            &mf_cfg,
            &report,
            &plan,
            &identity,
            r"C:\logs\openh264-session-agent.log",
            &openh264_media,
            false,
            false,
            false,
        );
        assert_eq!(openh264_hello.encoder_backend, "openh264-sw-h264");
        assert!(openh264_hello.supports_h264);
        assert!(!openh264_hello.supports_h265);
        assert!(!openh264_hello.supports_yuv444);
        assert_eq!(openh264_hello.codec, "h264");
        assert_eq!(openh264_hello.color_caps.advertised_pix_fmt, "yuv420");
        assert_eq!(
            hello.device_capabilities["input"]["selected_output_index"],
            1
        );
        assert_eq!(
            hello.device_capabilities["windows_session"]["session_id"],
            7
        );
        assert_eq!(
            hello.device_capabilities["windows_session"]["account"],
            r"STUDIO\artist"
        );
        assert_eq!(
            hello.device_capabilities["windows_session"]["creation_policy"],
            "existing-session-only"
        );
        assert_eq!(
            hello.device_capabilities["windows_session"]["agent_log"],
            r"C:\logs\arcen-session-agent.log"
        );
        let pen_hello = build_server_hello(
            &mf_cfg,
            &report,
            &plan,
            &identity,
            r"C:\logs\arcen-session-agent.log",
            &native_media,
            false,
            true,
            false,
        );
        for available in [
            pen_hello.input_capabilities.pen,
            pen_hello.input_capabilities.pen_pressure,
            pen_hello.input_capabilities.pen_tilt,
            pen_hello.input_capabilities.pen_rotation,
            pen_hello.input_capabilities.pen_eraser,
            pen_hello.input_capabilities.pen_proximity,
        ] {
            assert_eq!(
                available,
                InputCapabilityAvailability::Available,
                "pen_available=true must advertise every pen sub-capability as Available"
            );
        }
        assert_eq!(
            pen_hello.device_capabilities["input"]["pen_backend"],
            "synthetic_pointer"
        );
    }

    #[test]
    fn region_input_capability_tracks_the_prepared_runtime_adapter() {
        assert_eq!(
            runtime_input_capability(false),
            InputCapabilityAvailability::Unavailable,
        );
        assert_eq!(
            runtime_input_capability(true),
            InputCapabilityAvailability::Available,
        );
    }

    #[test]
    fn multi_monitor_region_input_requires_client_v4_and_explicit_availability() {
        let mut hello = ClientHelloMsg::default();
        assert!(multi_monitor_region_input_negotiated(false, false, &hello));
        assert!(!multi_monitor_region_input_negotiated(true, false, &hello));
        assert!(!multi_monitor_region_input_negotiated(true, true, &hello));

        hello.input_capabilities.region_input = InputCapabilityAvailability::Available;
        hello.input_protocol_version = REGION_INPUT_PROTOCOL_VERSION - 1;
        assert!(!multi_monitor_region_input_negotiated(true, true, &hello));

        hello.input_protocol_version = REGION_INPUT_PROTOCOL_VERSION;
        assert!(multi_monitor_region_input_negotiated(true, true, &hello));
    }

    #[test]
    fn windows_inbound_accepts_every_direct_region_wire_shape_without_legacy_coordinates() {
        fn assert_accepted<T>(
            message: &T,
            expected_type: &'static str,
            tracker: &mut InputSequenceTracker,
        ) where
            T: serde::Serialize + RegionSequencedInput,
        {
            let value = serde_json::to_value(message).expect("serialize region input");
            assert_eq!(value["type"], expected_type);
            assert!(value.get("server_x").is_none());
            assert!(value.get("server_y").is_none());
            assert!(value.get("logical_x").is_some());
            assert!(value.get("logical_y").is_some());

            let parsed = parse_region_input::<T>(
                &serde_json::to_string(&value).expect("serialize region JSON"),
                tracker,
            )
            .expect("Windows inbound parser accepts direct Region* input");
            commit_region_sequence(tracker, parsed.region_sequence());
        }

        let position = arcen_protocol::messages::RegionInputPositionMsg {
            region_generation: 17,
            region_id: 2,
            logical_x: 48_000,
            logical_y: 24_000,
        };
        let metadata = |sequence| arcen_protocol::messages::RegionInputMetadataMsg {
            sequence,
            timestamp_ns: sequence * 1_000,
            coalescable: false,
        };
        let mut tracker = InputSequenceTracker::default();

        assert_accepted(
            &RegionPointerEnterMsg {
                msg_type: REGION_POINTER_ENTER.to_owned(),
                position,
                metadata: metadata(1),
            },
            REGION_POINTER_ENTER,
            &mut tracker,
        );
        assert_accepted(
            &RegionPointerMotionMsg {
                msg_type: REGION_POINTER_MOTION.to_owned(),
                position,
                metadata: metadata(2),
            },
            REGION_POINTER_MOTION,
            &mut tracker,
        );
        assert_accepted(
            &RegionPointerButtonMsg {
                msg_type: REGION_POINTER_BUTTON.to_owned(),
                position,
                button: 3,
                pressed: true,
                metadata: metadata(3),
            },
            REGION_POINTER_BUTTON,
            &mut tracker,
        );
        assert_accepted(
            &RegionPointerScrollMsg {
                msg_type: REGION_POINTER_SCROLL.to_owned(),
                position,
                delta_x: -120,
                delta_y: 240,
                metadata: metadata(4),
            },
            REGION_POINTER_SCROLL,
            &mut tracker,
        );
        assert_accepted(
            &RegionPenEventMsg {
                msg_type: REGION_PEN_EVENT.to_owned(),
                position,
                pressure: 0.75,
                tilt_x_degrees: -15.0,
                tilt_y_degrees: 20.0,
                rotation_degrees: 270.0,
                tool: arcen_protocol::messages::PenToolMsg::Tip,
                in_proximity: true,
                touching: true,
                buttons: 1,
                metadata: metadata(5),
            },
            REGION_PEN_EVENT,
            &mut tracker,
        );
        assert_accepted(
            &RegionPointerLeaveMsg {
                msg_type: REGION_POINTER_LEAVE.to_owned(),
                position,
                metadata: metadata(6),
            },
            REGION_POINTER_LEAVE,
            &mut tracker,
        );
        assert_eq!(tracker.last_nonzero(), 6);
    }

    #[test]
    fn display_update_requires_exact_retarget_capability_not_nvenc() {
        let report = DisplayReport {
            requested: DisplaySize {
                width: 1920,
                height: 1080,
            },
            applied: DisplaySize {
                width: 1920,
                height: 1080,
            },
            original: DisplaySize {
                width: 1920,
                height: 1080,
            },
            original_refresh_hz: 60,
            applied_refresh_hz: 60,
            exact: true,
            changed: false,
            retarget_capable: false,
            backend: "change-display-settings-ex-temporary",
            restore_backend: "set-display-config-plus-exact-devmode",
            device_name: r"\\.\DISPLAY1".to_string(),
            capture_output_index: 0,
            desktop_rect: crate::display::DesktopRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
            effective_scale_reports: Vec::new(),
        };
        assert!(!windows_display_update_supported(&report));
        let unavailable = windows_display_update_availability(&report);
        assert_eq!(unavailable.reason, "display_backend_cannot_retarget");
        assert!(unavailable
            .rejection_message()
            .contains("NVAPI custom timings"));

        let negotiated = DisplayReport {
            exact: false,
            ..report.clone()
        };
        let negotiated_availability = windows_display_update_availability(&negotiated);
        assert!(!negotiated_availability.supported);
        assert_eq!(negotiated_availability.reason, "display_lease_not_exact");

        let capable = DisplayReport {
            retarget_capable: true,
            backend: "nvidia-nvapi-edid-saved-custom-timing",
            restore_backend: "nvapi-purge-plus-set-display-config-exact",
            ..report
        };
        assert!(windows_display_update_supported(&capable));
        assert_eq!(
            display_resize_capability(&capable),
            serde_json::json!({
                "available": true,
                "reason": "available",
                "explanation": "live resize is available using NVAPI custom timings with verified rollback",
                "mechanism": "nvapi_custom_timing",
                "scope": "arbitrary_custom_timing",
            })
        );
        let cds_exact = DisplayReport {
            backend: "change-display-settings-ex-temporary",
            restore_backend: "set-display-config-plus-exact-devmode",
            retarget_capable: false,
            ..capable.clone()
        };
        assert_eq!(
            display_resize_capability(&cds_exact),
            serde_json::json!({
                "available": false,
                "reason": "display_backend_cannot_retarget",
                "explanation": "display backend cannot retarget because it did not prove NVAPI custom timings with verified rollback capability",
                "mechanism": "none",
                "scope": "none",
            })
        );

        for (name, backend, codec, chroma) in [
            (
                "native-nvenc",
                EncoderBackend::NativeNvenc,
                arcen_media::VideoCodec::H265,
                arcen_media::ChromaSubsampling::Yuv444,
            ),
            (
                "openh264",
                EncoderBackend::OpenH264,
                arcen_media::VideoCodec::H264,
                arcen_media::ChromaSubsampling::Yuv420,
            ),
        ] {
            let media = test_media_plan(backend, codec, chroma, 1920, 1080, 30);
            assert!(
                resize_encoder_for(&media).is_some(),
                "{name} must be recreatable for live resize"
            );
            assert!(
                windows_display_update_supported(&capable),
                "{name} must not gate display retarget support"
            );
        }
    }

    #[test]
    fn repeated_client_cursor_preference_must_match_authenticated_setup() {
        let local = ClientHelloMsg::default();
        assert!(validate_client_cursor(&local, CursorMode::Local).is_ok());
        assert!(validate_client_cursor(&local, CursorMode::Host).is_err());
        let host = ClientHelloMsg {
            cursor_preference: CursorMode::Host,
            ..ClientHelloMsg::default()
        };
        assert!(validate_client_cursor(&host, CursorMode::Host).is_ok());
    }

    #[test]
    fn display_settle_precedes_capture_reinit_and_forced_idr() {
        let cfg = HostConfig {
            capenc_bin: "capenc".to_string(),
            output_selector: crate::display::OutputSelector::GlobalIndex(2),
            output_index: 2,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            color_policy: ColorPolicy::DefaultOff,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 60,
            encoder: crate::capenc::EncoderSelection::Auto,
            audio_enabled: true,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            reconnect_window_secs: 0,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        let report = DisplayReport {
            requested: DisplaySize {
                width: 3600,
                height: 2338,
            },
            applied: DisplaySize {
                width: 2560,
                height: 1600,
            },
            original: DisplaySize {
                width: 1680,
                height: 1050,
            },
            original_refresh_hz: 59,
            applied_refresh_hz: 60,
            exact: false,
            changed: true,
            retarget_capable: false,
            backend: "change-display-settings-ex-temporary-fallback",
            restore_backend: "set-display-config-plus-exact-devmode",
            device_name: r"\\.\DISPLAY6".to_string(),
            capture_output_index: 1,
            desktop_rect: crate::display::DesktopRect {
                left: 1920,
                top: 0,
                width: 2560,
                height: 1600,
            },
            effective_scale_reports: Vec::new(),
        };
        let events = Arc::new(StdMutex::new(vec!["display-settled"]));
        let input_events = Arc::clone(&events);
        let capture_events = Arc::clone(&events);
        let idr_events = Arc::clone(&events);
        let expected_rect = report.desktop_rect;

        let _ = start_capture_after_display(
            &cfg,
            &report,
            &crate::display::ResolvedOutput {
                global_index: report.capture_output_index,
                adapter_name: "NVIDIA GRID V100D-16Q".to_string(),
                adapter_output_index: 0,
                device_name: report.device_name.clone(),
                vendor_id: 0x10de,
                desktop_rect: report.desktop_rect,
            },
            CursorMode::Host,
            &CorrelationId::from_uuid_v4_bytes([0; 16]),
            move |output_index, rect| {
                assert_eq!(output_index, 1);
                assert_eq!(rect, expected_rect);
                input_events.lock().unwrap().push("input-remapped");
                Ok(())
            },
            move |capture_cfg| {
                assert_eq!(capture_cfg.output_index, 1);
                assert_eq!(capture_cfg.fps, 60);
                capture_events.lock().unwrap().push("capture-reinitialized");
                Ok(((), ()))
            },
            move |_| idr_events.lock().unwrap().push("idr-requested"),
        )
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "display-settled",
                "input-remapped",
                "capture-reinitialized",
                "idr-requested"
            ]
        );
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "mutates the live Windows display and exercises native capture/input/audio"]
    async fn live_native_pipeline_round_trip() {
        assert_eq!(
            std::env::var("ARCEN_LIVE_PIPELINE_TEST").as_deref(),
            Ok("1"),
            "set ARCEN_LIVE_PIPELINE_TEST=1 to authorize the live native pipeline smoke"
        );
        crate::logging::init(
            arcen_telemetry::OperationalProfile::Debug,
            crate::logging::COMPONENT_SESSION_AGENT,
            None,
            false,
        )
        .expect("live test logging");
        let capenc_bin = std::env::var("ARCEN_LIVE_CAPENC_BIN")
            .unwrap_or_else(|_| r"C:\arcen\target\release\arcen-capenc.exe".to_string());
        let mut request = DisplayRequest::new(3600, 2338).unwrap();
        request.refresh_hz = 60;
        request.width_mm = 344.0;
        request.height_mm = 223.0;
        request.scale = 2.0;
        request.product_id = 0x3600;
        request.serial = 0x2338;
        let manager = DisplayManager::default();
        let mut display = tokio::task::spawn_blocking(move || {
            manager.acquire(
                crate::display::OutputSelector::GlobalIndex(0),
                request,
                crate::display::DisplayPolicy::ExactIsolated,
                crate::eventlog::random_correlation_id(),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let report = display.report().clone();
        assert!(report.exact, "live pipeline requires an exact NVAPI lease");
        assert_eq!(report.applied, DisplaySize::validate(3600, 2338).unwrap());

        let mut injector = Injector::new(report.capture_output_index, report.desktop_rect).unwrap();
        let mut pointer = arcen_protocol::messages::MouseMoveMsg::default();
        pointer.x = 0.5;
        pointer.y = 0.5;
        pointer.sequence = 1;
        injector.move_abs(&pointer);

        for (codec, chroma) in [
            (VideoCodec::H264, ChromaSubsampling::Yuv420),
            (VideoCodec::H265, ChromaSubsampling::Yuv444),
        ] {
            let (mut capenc, frames, media_plan) = Capenc::spawn(CapencConfig {
                binary: capenc_bin.clone(),
                output_index: report.capture_output_index,
                adapter_name: None,
                adapter_output_index: None,
                device_name: Some(report.device_name.clone()),
                codec,
                chroma,
                bit_depth: BitDepth::Eight,
                color_range: ColorRange::Limited,
                color_matrix: ColorMatrix::Bt709,
                intent: arcen_media::EncodeIntent::default(),
                qp_map: arcen_media::video::QpMapPolicy::default(),
                fps: 30,
                width: report.applied.width,
                height: report.applied.height,
                encoder: None,
                cursor_mode: CursorMode::Local,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            })
            .await
            .unwrap();
            assert_eq!(crate::capenc::protocol_codec(media_plan.video.codec), codec);
            assert_eq!(
                crate::capenc::protocol_chroma(media_plan.video.chroma),
                chroma
            );
            capenc.request_keyframe("live_native_pipeline_round_trip");
            let frame = tokio::time::timeout(Duration::from_secs(20), frames.pop())
                .await
                .expect("timed out waiting for native encoded frame")
                .expect("capture engine exited without a frame");
            println!(
                "live video smoke: codec={} chroma={} bytes={} keyframe={}",
                crate::codec_name(codec),
                crate::chroma_name(chroma),
                frame.data.len(),
                frame.keyframe
            );
            assert!(!frame.data.is_empty());
            assert!(frame.keyframe, "fresh capture must begin with an IDR");
            capenc.shutdown().await;
        }

        let audio_queue = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
        let mut audio = AudioCapture::start(Arc::clone(&audio_queue)).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let telemetry = audio.telemetry();
        println!("live audio smoke: {telemetry:?}");
        assert_eq!(telemetry.capture_errors, 0);
        assert!(
            telemetry.packets > 0 || telemetry.idle_periods > 0,
            "WASAPI must either capture rendered audio or report an idle endpoint"
        );
        audio.shutdown().await;
        injector.close();

        tokio::task::spawn_blocking(move || display.restore())
            .await
            .unwrap()
            .unwrap();
        assert!(!crate::recovery::default_path().exists());
    }

    #[test]
    fn inconsistent_settled_geometry_blocks_capture_start() {
        let cfg = HostConfig {
            capenc_bin: "capenc".to_string(),
            output_selector: crate::display::OutputSelector::GlobalIndex(0),
            output_index: 0,
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            color_policy: ColorPolicy::DefaultOff,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            auth_video_request: None,
            fps: 30,
            encoder: crate::capenc::EncoderSelection::Auto,
            audio_enabled: false,
            audio_compressed: false,
            microphone_input_enabled: false,
            clipboard_policy: arcen_media::clipboard::ClipboardPolicy::default(),
            timezone_redirection: false,
            reconnect_window_secs: 0,
            qos_targets: arcen_telemetry::QosTargets::default(),
            deskside: crate::deskside::DesksideConfig::default(),
            iddcx: crate::config::WindowsIddCxConfig::default(),
            multi_monitor: crate::config::WindowsMultiMonitorConfig::default(),
        };
        let report = DisplayReport {
            requested: DisplaySize {
                width: 1920,
                height: 1080,
            },
            applied: DisplaySize {
                width: 1920,
                height: 1080,
            },
            original: DisplaySize {
                width: 1680,
                height: 1050,
            },
            original_refresh_hz: 59,
            applied_refresh_hz: 60,
            exact: true,
            changed: true,
            retarget_capable: false,
            backend: "change-display-settings-ex-temporary",
            restore_backend: "set-display-config-plus-exact-devmode",
            device_name: r"\\.\DISPLAY6".to_string(),
            capture_output_index: 0,
            desktop_rect: crate::display::DesktopRect {
                left: 0,
                top: 0,
                width: 1680,
                height: 1050,
            },
            effective_scale_reports: Vec::new(),
        };

        let error = start_capture_after_display(
            &cfg,
            &report,
            &crate::display::ResolvedOutput {
                global_index: report.capture_output_index,
                adapter_name: "NVIDIA GRID V100D-16Q".to_string(),
                adapter_output_index: 0,
                device_name: report.device_name.clone(),
                vendor_id: 0x10de,
                desktop_rect: report.desktop_rect,
            },
            CursorMode::Local,
            &CorrelationId::from_uuid_v4_bytes([0; 16]),
            |_, _| Ok(()),
            |_| Ok(((), ())),
            |_| {},
        )
        .unwrap_err();

        assert!(error.contains("selected-output geometry"));
    }

    #[test]
    fn shared_health_ping_produces_shared_health_pong() {
        let ping = HealthPingMsg {
            timestamp_ms: 1_700_000_000_000,
            sequence: 42,
            client_state: "streaming".to_string(),
            ..HealthPingMsg::default()
        };
        let reply = health_pong_reply(&serde_json::to_string(&ping).unwrap()).unwrap();
        let pong: HealthPongMsg = serde_json::from_str(&reply).unwrap();
        assert_eq!(pong.msg_type, HEALTH_PONG);
        assert_eq!(pong.ping_timestamp_ms, ping.timestamp_ms);
        assert_eq!(pong.sequence, ping.sequence);
        assert_eq!(pong.server_state, ServerState::Streaming.as_str());
    }

    #[test]
    fn health_ping_consumes_optional_client_telemetry() {
        let action = health_ping_action(
            r#"{
                "type":"health_ping",
                "timestamp_ms":1700000000000,
                "sequence":42,
                "client_state":"streaming",
                "client_telemetry":{
                    "qos":{
                        "frames_decoded":9,
                        "sample_window_secs":2,
                        "sample_age_ms":100
                    },
                    "network":{
                        "interface_kind":"wifi",
                        "scope":"lan",
                        "link_mbps":100,
                        "mtu":1500
                    }
                }
            }"#,
        );
        let telemetry = action.client_telemetry.expect("client telemetry");
        let qos = telemetry.qos.expect("client QoS");
        assert_eq!(qos.frames_decoded, Some(9));
        assert_eq!(qos.sample_age_ms, Some(100));
        let network = telemetry.network.expect("client network");
        assert_eq!(network.link_mbps(), Some(100));
        assert_eq!(network.mtu(), Some(1_500));
        assert!(action.reply.is_some());
    }

    #[test]
    fn one_sequence_tracker_orders_keyboard_absolute_relative_button_and_wheel() {
        let mut tracker = InputSequenceTracker::default();
        let absolute = MouseMoveMsg {
            sequence: 10,
            ..MouseMoveMsg::default()
        };
        assert!(parse_input::<MouseMoveMsg>(
            &serde_json::to_string(&absolute).unwrap(),
            &mut tracker
        )
        .is_some());

        let key = KeyEventMsg {
            sequence: 11,
            ..KeyEventMsg::default()
        };
        assert!(
            parse_input::<KeyEventMsg>(&serde_json::to_string(&key).unwrap(), &mut tracker)
                .is_some()
        );

        let relative = MouseMoveRelativeMsg {
            sequence: 12,
            ..MouseMoveRelativeMsg::default()
        };
        assert!(parse_input::<MouseMoveRelativeMsg>(
            &serde_json::to_string(&relative).unwrap(),
            &mut tracker
        )
        .is_some());

        let stale_button = MouseButtonMsg {
            sequence: 11,
            ..MouseButtonMsg::default()
        };
        assert!(parse_input::<MouseButtonMsg>(
            &serde_json::to_string(&stale_button).unwrap(),
            &mut tracker
        )
        .is_none());
        assert_eq!(tracker.last_nonzero(), 12);

        let wheel = MouseScrollMsg {
            sequence: 13,
            ..MouseScrollMsg::default()
        };
        assert!(parse_input::<MouseScrollMsg>(
            &serde_json::to_string(&wheel).unwrap(),
            &mut tracker
        )
        .is_some());
        assert!(parse_input::<MouseButtonMsg>(
            &serde_json::to_string(&MouseButtonMsg::default()).unwrap(),
            &mut tracker
        )
        .is_some());
        assert_eq!(tracker.last_nonzero(), 13);
    }

    #[test]
    fn region_sequence_preflight_is_atomic_with_global_input_ordering() {
        let mut tracker = InputSequenceTracker::default();
        assert!(parse_input::<KeyEventMsg>(
            &serde_json::to_string(&KeyEventMsg {
                sequence: 10,
                ..KeyEventMsg::default()
            })
            .unwrap(),
            &mut tracker,
        )
        .is_some());

        let enter = RegionPointerEnterMsg {
            msg_type: REGION_POINTER_ENTER.to_owned(),
            position: arcen_protocol::messages::RegionInputPositionMsg {
                region_generation: 7,
                region_id: 1,
                logical_x: 0,
                logical_y: 0,
            },
            metadata: arcen_protocol::messages::RegionInputMetadataMsg {
                sequence: 11,
                timestamp_ns: 0,
                coalescable: false,
            },
        };
        let parsed = parse_region_input::<RegionPointerEnterMsg>(
            &serde_json::to_string(&enter).unwrap(),
            &tracker,
        )
        .expect("valid region preflight");
        assert_eq!(tracker.last_nonzero(), 10);
        commit_region_sequence(&mut tracker, parsed.metadata.sequence);
        assert_eq!(tracker.last_nonzero(), 11);

        let invalid_generation = RegionPointerEnterMsg {
            position: arcen_protocol::messages::RegionInputPositionMsg {
                region_generation: 0,
                ..enter.position
            },
            metadata: arcen_protocol::messages::RegionInputMetadataMsg {
                sequence: 12,
                ..enter.metadata
            },
            ..enter.clone()
        };
        assert!(parse_region_input::<RegionPointerEnterMsg>(
            &serde_json::to_string(&invalid_generation).unwrap(),
            &tracker,
        )
        .is_none());
        assert_eq!(tracker.last_nonzero(), 11);

        assert!(parse_region_input::<RegionPointerEnterMsg>(
            &serde_json::to_string(&enter).unwrap(),
            &tracker,
        )
        .is_none());
        assert_eq!(tracker.last_nonzero(), 11);
    }

    #[tokio::test]
    async fn slow_client_write_has_a_hard_deadline() {
        let result = send_ws_with_timeout(
            &mut PendingSink,
            Message::Binary(vec![1, 2, 3].into()),
            Duration::from_millis(5),
        )
        .await;
        assert_eq!(result.unwrap_err(), "WebSocket send timed out");
    }

    #[tokio::test]
    async fn refresh_successor_send_failure_drains_and_reopens_after_cleanup() {
        let (registry, bindings, active_session_id) = registered_windows_resume();
        let successor = registry.refresh_grant(&active_session_id).unwrap();
        assert_eq!(
            send_successor_auth_result_or_drain(
                &mut PendingSink,
                "Resume grant refreshed",
                &successor.grant,
                successor.window_secs,
                false,
                Duration::from_millis(5),
                &registry,
                &active_session_id,
            )
            .await
            .unwrap_err(),
            crate::resume::ResumeRegistryError::SuccessorDelivery
        );
        assert!(!registry.resume_handshake_available().unwrap());

        registry.complete_drain(&active_session_id).unwrap();
        let (owner, _commands) = mpsc::unbounded_channel();
        assert!(registry
            .issue_initial(
                bindings,
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .is_ok());
    }

    #[tokio::test]
    async fn resumed_successor_send_failure_drains_instead_of_redetaching() {
        let (registry, bindings, active_session_id) = registered_windows_resume();
        let successor = registry.refresh_grant(&active_session_id).unwrap();
        assert_eq!(
            send_successor_auth_result_or_drain(
                &mut PendingSink,
                "OK",
                &successor.grant,
                successor.window_secs,
                true,
                Duration::from_millis(5),
                &registry,
                &active_session_id,
            )
            .await
            .unwrap_err(),
            crate::resume::ResumeRegistryError::SuccessorDelivery
        );
        assert!(!registry.resume_handshake_available().unwrap());
        assert_eq!(
            registry.mark_detached(&active_session_id).unwrap_err(),
            crate::resume::ResumeRegistryError::SlotUnavailable
        );

        registry.complete_drain(&active_session_id).unwrap();
        let (owner, _commands) = mpsc::unbounded_channel();
        assert!(registry
            .issue_initial(
                bindings,
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .is_ok());
    }

    #[tokio::test]
    async fn stalled_auth_has_a_hard_deadline() {
        let result = authenticate_with_deadline(
            Duration::from_millis(5),
            std::future::pending::<Result<bool, String>>(),
        )
        .await;
        assert_eq!(result.unwrap_err(), "Windows authentication timed out");
    }

    #[tokio::test]
    async fn closed_queues_stop_writer_without_waiting_for_client_io() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        let audio = Arc::new(LatestQueue::new(1));
        video.close_and_clear_all();
        audio.close();
        let (control_tx, control_rx) = mpsc::channel(1);
        drop(control_tx);
        let writer = writer_loop(
            futures_util::sink::drain(),
            video,
            audio,
            closed_clipboard_queue(),
            control_rx,
            Arc::new(AudioSendState::default()),
            Arc::new(WriterVideoStats::default()),
        );
        let result = tokio::time::timeout(Duration::from_millis(50), writer)
            .await
            .expect("writer must observe queue closure");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn writer_video_stats_count_only_successful_writes() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        assert!(matches!(
            video.push(
                0,
                OutboundVideo {
                    message: Message::Binary(vec![FrameType::VideoH264 as u8, 1, 2, 3].into()),
                },
                true,
            ),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        let audio = Arc::new(LatestQueue::new(1));
        audio.close();
        let (control_tx, control_rx) = mpsc::channel(1);
        drop(control_tx);
        let rejected_stats = Arc::new(WriterVideoStats::default());

        assert!(writer_loop(
            RejectingSink,
            video,
            audio,
            closed_clipboard_queue(),
            control_rx,
            Arc::new(AudioSendState::default()),
            Arc::clone(&rejected_stats),
        )
        .await
        .result
        .is_err());
        assert_eq!(rejected_stats.snapshot(), (0, 0));

        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        assert!(matches!(
            video.push(
                0,
                OutboundVideo {
                    message: Message::Binary(vec![FrameType::VideoH264 as u8, 1, 2, 3].into()),
                },
                true,
            ),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        let audio = Arc::new(LatestQueue::new(1));
        audio.close();
        let (control_tx, control_rx) = mpsc::channel(1);
        drop(control_tx);
        let delivered_stats = Arc::new(WriterVideoStats::default());

        let writer = tokio::spawn(writer_loop(
            futures_util::sink::drain(),
            Arc::clone(&video),
            audio,
            closed_clipboard_queue(),
            control_rx,
            Arc::new(AudioSendState::default()),
            Arc::clone(&delivered_stats),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while delivered_stats.snapshot() != (1, 4) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer must record the delivered video frame");
        video.close_and_clear_all();
        writer.await.unwrap().unwrap();
        assert_eq!(delivered_stats.snapshot(), (1, 4));
    }

    #[tokio::test]
    async fn awaiting_idr_keeps_control_audio_and_shutdown_independent() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        assert!(matches!(
            video.push(
                0,
                OutboundVideo {
                    message: Message::Binary(vec![FrameType::VideoH264 as u8, 1].into()),
                },
                false,
            ),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert!(matches!(
            video.push(
                0,
                OutboundVideo {
                    message: Message::Binary(vec![FrameType::VideoH264 as u8, 2].into()),
                },
                false,
            ),
            VideoPushResult::Dropped {
                recovery_started: true,
                ..
            }
        ));
        assert!(video.awaiting_keyframe());

        let audio = Arc::new(LatestQueue::new(1));
        audio
            .push(AudioPacket {
                pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
                timestamp_ms: 7,
            })
            .unwrap();
        let (control_tx, control_rx) = mpsc::channel(1);
        control_tx
            .send(WriterControl::Message(Message::Text("health".into())))
            .await
            .unwrap();
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let audio_state = Arc::new(AudioSendState::default());
        audio_state.activate(Arc::new(AudioTelemetry::default()), legacy_pcm_stream());
        let writer = tokio::spawn(writer_loop(
            RecordingSink(Arc::clone(&sent)),
            Arc::clone(&video),
            Arc::clone(&audio),
            closed_clipboard_queue(),
            control_rx,
            audio_state,
            Arc::new(WriterVideoStats::default()),
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

        let sent = sent.lock().unwrap();
        assert!(sent
            .iter()
            .any(|message| matches!(message, Message::Text(_))));
        assert!(sent.iter().any(
            |message| matches!(message, Message::Binary(bytes) if bytes[0] == FrameType::Audio as u8)
        ));
        assert!(
            !sent.iter().any(
                |message| matches!(message, Message::Binary(bytes) if bytes[0] == FrameType::VideoH264 as u8)
            ),
            "no unsafe P-frame may reach the writer before IDR"
        );
        drop(sent);

        video.close_and_clear_all();
        audio.close();
        drop(control_tx);
        tokio::time::timeout(Duration::from_millis(100), writer)
            .await
            .expect("closed queues must stop the writer")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn audio_result_barrier_precedes_activation_and_binary_media() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        let audio = Arc::new(LatestQueue::new(1));
        let (control_tx, control_rx) = mpsc::channel(2);
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let audio_state = Arc::new(AudioSendState::default());
        let writer = tokio::spawn(writer_loop(
            RecordingSink(Arc::clone(&sent)),
            Arc::clone(&video),
            Arc::clone(&audio),
            closed_clipboard_queue(),
            control_rx,
            Arc::clone(&audio_state),
            Arc::new(WriterVideoStats::default()),
        ));
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let peer = policy.capabilities();
        let stream = policy.resolve(Some(&peer), true, 128);

        writer_audio_result_barrier(&control_tx, stream.result().expect("audio-v1 result"))
            .await
            .unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1);

        audio_state.activate(Arc::new(AudioTelemetry::default()), stream);
        audio
            .push(AudioPacket {
                pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
                timestamp_ms: 7,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_millis(100), async {
            while sent.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("audio media must be sent");
        let sent_messages = sent.lock().unwrap();
        assert!(matches!(sent_messages[0], Message::Text(_)));
        assert!(matches!(
            &sent_messages[1],
            Message::Binary(bytes) if bytes[1] == AudioCodec::Opus as u8
        ));
        drop(sent_messages);

        video.close_and_clear_all();
        audio.close();
        drop(control_tx);
        writer.await.unwrap().unwrap();
    }

    #[test]
    fn critical_control_mailbox_is_bounded() {
        let (control_tx, _control_rx) = mpsc::channel(1);
        assert!(control_tx
            .try_send(WriterControl::Message(Message::Close(None)))
            .is_ok());
        assert!(matches!(
            control_tx.try_send(WriterControl::Message(Message::Close(None))),
            Err(mpsc::error::TrySendError::Full(_))
        ));
    }

    #[tokio::test]
    async fn client_disabled_never_starts_audio_or_queues_packets() {
        let queue = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
        let factory = TestCaptureFactory::default();
        let starts = Arc::clone(&factory.starts);
        let active = Arc::clone(&factory.active);
        let mut runtime = AudioRuntime::with_factory(true, Arc::clone(&queue), factory);

        runtime.set_client_enabled(false, None).await.unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(queue.len(), 0);
        assert!(!runtime.send_state().is_enabled());
    }

    #[tokio::test]
    async fn client_enabled_starts_audio_and_makes_packets_sendable() {
        let queue = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
        let factory = TestCaptureFactory::default();
        let starts = Arc::clone(&factory.starts);
        let active = Arc::clone(&factory.active);
        let clean_shutdowns = Arc::clone(&factory.clean_shutdowns);
        let mut runtime = AudioRuntime::with_factory(true, Arc::clone(&queue), factory);

        runtime.set_client_enabled(true, None).await.unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(active.load(Ordering::SeqCst), 1);
        assert_eq!(queue.len(), 1);
        assert!(runtime.send_state().is_enabled());

        runtime.set_client_enabled(false, None).await.unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(clean_shutdowns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn host_disabled_overrides_client_audio_request() {
        let queue = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
        let factory = TestCaptureFactory::default();
        let starts = Arc::clone(&factory.starts);
        let mut runtime = AudioRuntime::with_factory(false, Arc::clone(&queue), factory);

        runtime.set_client_enabled(true, None).await.unwrap();

        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(queue.len(), 0);
        assert!(!runtime.send_state().is_enabled());
    }

    #[tokio::test]
    async fn quality_updates_stop_restart_and_cleanly_shutdown_audio() {
        let queue = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
        let factory = TestCaptureFactory::default();
        let starts = Arc::clone(&factory.starts);
        let active = Arc::clone(&factory.active);
        let clean_shutdowns = Arc::clone(&factory.clean_shutdowns);
        let mut runtime = AudioRuntime::with_factory(true, Arc::clone(&queue), factory);

        runtime.set_client_enabled(true, None).await.unwrap();
        runtime.set_client_enabled(false, None).await.unwrap();
        assert_eq!(queue.len(), 0);
        assert!(!runtime.send_state().is_enabled());

        runtime.set_client_enabled(true, None).await.unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 1);

        runtime.set_client_enabled(false, None).await.unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(clean_shutdowns.load(Ordering::SeqCst), 2);
        assert_eq!(queue.len(), 0);
        assert!(!runtime.send_state().is_enabled());
    }

    #[tokio::test]
    async fn disabled_audio_state_suppresses_queued_packets() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        let audio = Arc::new(LatestQueue::new(1));
        audio
            .push(AudioPacket {
                pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
                timestamp_ms: 7,
            })
            .unwrap();
        video.close_and_clear_all();
        audio.close();
        let (control_tx, control_rx) = mpsc::channel(1);
        drop(control_tx);
        let sent = Arc::new(StdMutex::new(Vec::new()));

        writer_loop(
            RecordingSink(Arc::clone(&sent)),
            video,
            audio,
            closed_clipboard_queue(),
            control_rx,
            Arc::new(AudioSendState::default()),
            Arc::new(WriterVideoStats::default()),
        )
        .await
        .unwrap();

        assert!(sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disable_barrier_drains_writer_before_clean_capture_shutdown() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        let audio = Arc::new(LatestQueue::new(crate::audio::QUEUE_CAPACITY));
        let factory = TestCaptureFactory::default();
        let active = Arc::clone(&factory.active);
        let clean_shutdowns = Arc::clone(&factory.clean_shutdowns);
        let mut runtime = AudioRuntime::with_factory(true, Arc::clone(&audio), factory);
        runtime.set_client_enabled(true, None).await.unwrap();

        video.close_and_clear_all();
        let (control_tx, control_rx) = mpsc::channel(1);
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let writer = tokio::spawn(writer_loop(
            RecordingSink(Arc::clone(&sent)),
            video,
            Arc::clone(&audio),
            closed_clipboard_queue(),
            control_rx,
            runtime.send_state(),
            Arc::new(WriterVideoStats::default()),
        ));

        runtime
            .set_client_enabled(false, Some(&control_tx))
            .await
            .unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(clean_shutdowns.load(Ordering::SeqCst), 1);
        assert_eq!(audio.len(), 0);

        let sent_before_disable_completed = sent.lock().unwrap().len();
        audio
            .push(AudioPacket {
                pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
                timestamp_ms: 2,
            })
            .unwrap();
        audio.close();
        drop(control_tx);
        writer.await.unwrap().unwrap();
        assert_eq!(sent.lock().unwrap().len(), sent_before_disable_completed);
    }

    #[test]
    fn shared_audio_header_is_exactly_eight_bytes_and_pcm_tagged() {
        let packet = AudioPacket {
            pcm_s16le: vec![0x34, 0x12, 0x78, 0x56],
            timestamp_ms: 0x0102_0304,
        };
        let message = audio_message(AudioCodec::Pcm, packet.timestamp_ms, &packet.pcm_s16le);
        assert_eq!(message.len(), arcen_protocol::AUDIO_HEADER_SIZE + 4);
        assert_eq!(&message[..8], &[0x10, 0x01, 0, 0, 1, 2, 3, 4]);
        assert_eq!(&message[8..], packet.pcm_s16le);
    }

    #[test]
    fn audio_wire_encoder_preserves_legacy_pcm_and_emits_bounded_v1_opus() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let packet = AudioPacket {
            pcm_s16le: (0..crate::audio::CHUNK_BYTES)
                .map(|index| (index % 251) as u8)
                .collect(),
            timestamp_ms: 0x1122_3344,
        };
        let state = AudioSendState::default();
        let mut encoder = AudioWireEncoder::default();

        state.set_stream(policy.resolve(None, true, 128));
        let (legacy, legacy_bytes) = encoder
            .encode(&packet, &state)
            .expect("legacy encode")
            .expect("legacy frame");
        assert_eq!(&legacy[..8], &[0x10, 0x01, 0, 0, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&legacy[8..], packet.pcm_s16le);
        assert_eq!(legacy_bytes, crate::audio::CHUNK_BYTES);

        let peer = policy.capabilities();
        state.set_stream(policy.resolve(Some(&peer), true, 128));
        let (opus, encoded_bytes) = encoder
            .encode(&packet, &state)
            .expect("Opus encode")
            .expect("Opus frame");
        assert_eq!(&opus[..8], &[0x10, 0x00, 0, 0, 0x11, 0x22, 0x33, 0x44]);
        assert_eq!(
            encoded_bytes,
            opus.len() - arcen_protocol::AUDIO_HEADER_SIZE
        );
        assert!((1..=MAX_OPUS_PACKET_BYTES).contains(&encoded_bytes));

        let mut decoder = arcen_media::audio::OpusDecoder::new().expect("decoder");
        let mut decoded = [0i16; 1_920];
        decoder
            .decode(&opus[arcen_protocol::AUDIO_HEADER_SIZE..], &mut decoded)
            .expect("decode host packet");

        state.set_stream(policy.resolve(Some(&peer), true, 64));
        encoder
            .encode(&packet, &state)
            .expect("bitrate update")
            .expect("updated frame");
        assert_eq!(
            encoder.opus.as_ref().expect("encoder").bitrate(),
            arcen_media::audio::AudioBitrateTier::Kbps64
        );
        state.set_stream(policy.resolve(Some(&peer), false, 64));
        assert!(encoder.encode(&packet, &state).expect("disable").is_none());
        assert!(encoder.opus.is_none());
    }

    #[test]
    fn audio_wire_encoder_recreates_after_three_consecutive_failures() {
        let policy = AudioPolicy {
            opus_available: true,
            pcm_available: true,
        };
        let peer = policy.capabilities();
        let state = AudioSendState::default();
        state.set_stream(policy.resolve(Some(&peer), true, 128));
        let malformed = AudioPacket {
            pcm_s16le: Vec::new(),
            timestamp_ms: 1,
        };
        let mut encoder = AudioWireEncoder::default();

        for _ in 0..3 {
            assert!(matches!(
                encoder.encode(&malformed, &state),
                Err(AudioEncodeFailure::Transient)
            ));
        }
        assert!(encoder.opus.is_some());
        assert_eq!(encoder.consecutive_failures, 0);
    }

    #[tokio::test]
    async fn audio_backpressure_drops_oldest_without_consuming_video_capacity() {
        let audio = LatestQueue::new(2);
        let packet = |timestamp_ms| AudioPacket {
            pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
            timestamp_ms,
        };
        assert!(audio.push(packet(1)).unwrap().is_none());
        assert!(audio.push(packet(2)).unwrap().is_none());
        assert_eq!(audio.push(packet(3)).unwrap().unwrap().timestamp_ms, 1);

        let video = VideoQueue::new(1);
        assert!(matches!(
            video.push(
                OutboundVideo {
                    message: Message::Binary(vec![7].into()),
                },
                false,
            ),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        assert_eq!(audio.pop().await.unwrap().timestamp_ms, 2);
        assert_eq!(video.len(), 1);
    }

    /// Audio wins over video when both are ready, and the writer survives the
    /// audio queue closing under it.
    ///
    /// This asserted the opposite until now — video first — because that is
    /// what the writer did when the Windows Pier was migrated. #121 then
    /// deliberately put audio ahead of video in the host writer loops on both
    /// platforms, to stop sustained video load starving the audio sender
    /// (#115). The behaviour changed; this test did not, and it went on failing
    /// unnoticed because the Windows CI job was dying several steps earlier on
    /// a missing cl.exe.
    ///
    /// Audio is the stream that cannot absorb delay: a late packet is an
    /// audible dropout, whereas a frame arriving a moment later is not visible.
    /// So the ordering is deliberate and this test now pins it, rather than
    /// pinning the pre-#121 behaviour nothing implements.
    #[tokio::test]
    async fn writer_prioritizes_ready_audio_and_survives_audio_closure() {
        let video = Arc::new(
            OutboundVideoMux::new_with_capacity([0], 1)
                .expect("valid single-route outbound video mux"),
        );
        let audio = Arc::new(LatestQueue::new(1));
        assert!(matches!(
            video.push(
                0,
                OutboundVideo {
                    message: Message::Binary(vec![FrameType::VideoH264 as u8].into()),
                },
                true,
            ),
            VideoPushResult::Enqueued { cleared: 0 }
        ));
        audio
            .push(AudioPacket {
                pcm_s16le: vec![0; crate::audio::CHUNK_BYTES],
                timestamp_ms: 7,
            })
            .unwrap();
        audio.close();
        let (control_tx, control_rx) = mpsc::channel(1);
        drop(control_tx);
        let sent = Arc::new(StdMutex::new(Vec::new()));
        let audio_state = Arc::new(AudioSendState::default());
        audio_state.activate(Arc::new(AudioTelemetry::default()), legacy_pcm_stream());

        let writer = tokio::spawn(writer_loop(
            RecordingSink(Arc::clone(&sent)),
            Arc::clone(&video),
            audio,
            closed_clipboard_queue(),
            control_rx,
            audio_state,
            Arc::new(WriterVideoStats::default()),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while sent.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("audio and video must both reach the writer");
        video.close_and_clear_all();
        writer.await.unwrap().unwrap();

        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(
            matches!(&sent[0], Message::Binary(bytes) if bytes[0] == FrameType::Audio as u8),
            "audio must be sent before video when both are ready (see #115/#121)"
        );
        assert!(
            matches!(&sent[1], Message::Binary(bytes) if bytes[0] == FrameType::VideoH264 as u8)
        );
    }

    #[test]
    fn broker_lease_rejects_a_concurrent_authenticated_agent() {
        let lease = BrokerAgentLease::new(
            arcen_telemetry::OperationalProfile::Info,
            arcen_telemetry::QosTargets::default(),
        );
        let first = lease.try_acquire().expect("first agent owns display");
        let error = lease.try_acquire().unwrap_err();
        assert!(
            error.contains("already owned by another authenticated session"),
            "{error}"
        );
        // The old message said only "retry after it disconnects", which gave no
        // hint that the wait is bounded by a configurable resume window rather
        // than by the other user's behaviour. Assert the setting is named, so
        // it cannot quietly go missing again.
        assert!(error.contains("auth.reconnect_window_secs"), "{error}");
        drop(first);
        assert!(lease.try_acquire().is_ok());
    }

    #[test]
    fn broker_lease_does_not_leak_admission_when_the_display_is_unavailable() {
        // Regression: `try_acquire` used to take the admission lease first and
        // the display permit second. `SessionAdmissionLease` has no `Drop`
        // impl — the capacity-one slot is freed only by an explicit
        // `complete()` — so when the display permit was unavailable the early
        // return discarded the lease without releasing it, and the Pier then
        // refused every subsequent session until the service was restarted.
        // `try_acquire` runs before credential verification, so an
        // unauthenticated peer could drive it.
        let lease = BrokerAgentLease::new(
            arcen_telemetry::OperationalProfile::Info,
            arcen_telemetry::QosTargets::default(),
        );

        // Occupy the display directly, which leaves admission free and
        // reproduces the exact state the old ordering leaked in.
        let display_holder = Arc::clone(&lease.slot)
            .try_acquire_owned()
            .expect("display slot is initially free");

        let error = lease.try_acquire().unwrap_err();
        assert!(
            error.contains("already owned by another authenticated session"),
            "{error}"
        );

        // The failed attempt must not have consumed admission capacity.
        drop(display_holder);
        assert!(
            lease.try_acquire().is_ok(),
            "a failed display acquisition leaked the admission lease"
        );
    }

    #[tokio::test]
    async fn broker_lease_releases_when_agent_owner_task_is_aborted() {
        let lease = Arc::new(BrokerAgentLease::new(
            arcen_telemetry::OperationalProfile::Info,
            arcen_telemetry::QosTargets::default(),
        ));
        let worker_lease = Arc::clone(&lease);
        let (acquired_tx, acquired_rx) = oneshot::channel();
        let owner = tokio::spawn(async move {
            let _permit = worker_lease.try_acquire().expect("agent lease");
            let _ = acquired_tx.send(());
            std::future::pending::<()>().await;
        });
        acquired_rx.await.expect("agent acquired broker lease");
        assert!(lease.try_acquire().is_err());

        owner.abort();
        let _ = owner.await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if lease.try_acquire().is_ok() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_ok(),
            "task cancellation must release the broker-owned agent lease"
        );
    }

    #[test]
    fn broker_restart_cannot_leave_a_stale_process_local_lease() {
        let previous_broker = BrokerAgentLease::new(
            arcen_telemetry::OperationalProfile::Info,
            arcen_telemetry::QosTargets::default(),
        );
        let permit = previous_broker.try_acquire().expect("old broker lease");
        drop(permit);
        drop(previous_broker);

        let restarted_broker = BrokerAgentLease::new(
            arcen_telemetry::OperationalProfile::Info,
            arcen_telemetry::QosTargets::default(),
        );
        assert!(restarted_broker.try_acquire().is_ok());
    }

    #[test]
    fn broker_log_control_keeps_reopen_sticky_across_profile_changes() {
        let lease = BrokerAgentLease::new(
            arcen_telemetry::OperationalProfile::Info,
            arcen_telemetry::QosTargets::default(),
        );
        lease.request_log_reopen();
        lease.request_profile(
            arcen_telemetry::OperationalProfile::Debug,
            arcen_telemetry::QosTargets::default(),
            false,
        );

        let control = lease.controls.borrow().clone();
        assert_eq!(
            control.profile_level,
            u8::from(arcen_telemetry::OperationalProfile::Debug)
        );
        assert!(!control.use_configured_filter);
        assert_eq!(control.reopen_generation, 1);
        assert_eq!(
            lease.current_profile(),
            arcen_telemetry::OperationalProfile::Debug
        );
    }

    #[tokio::test]
    async fn resume_registry_remains_draining_until_agent_and_timezone_cleanup_complete() {
        let (registry, bindings, active_session_id) = registered_windows_resume();
        let cleanup_calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = Arc::clone(&cleanup_calls);

        finish_resume_cleanup(&registry, &active_session_id, || async {
            observed_calls.fetch_add(1, Ordering::AcqRel);
            assert!(!registry.resume_handshake_available().unwrap());
            let (owner, _commands) = mpsc::unbounded_channel();
            assert_eq!(
                registry
                    .issue_initial(
                        bindings.clone(),
                        ReconnectPolicy::new(1).unwrap(),
                        owner,
                        &CorrelationId::from_uuid_v4_bytes([2; 16]),
                    )
                    .unwrap_err(),
                crate::resume::ResumeRegistryError::Busy
            );
            tokio::task::yield_now().await;
        })
        .await
        .unwrap();

        assert_eq!(cleanup_calls.load(Ordering::Acquire), 1);
        let (owner, _commands) = mpsc::unbounded_channel();
        assert!(registry
            .issue_initial(
                bindings,
                ReconnectPolicy::new(1).unwrap(),
                owner,
                &CorrelationId::from_uuid_v4_bytes([3; 16]),
            )
            .is_ok());
    }

    #[tokio::test]
    async fn agent_teardown_completes_before_timezone_restore() {
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let agent_calls = Arc::clone(&calls);
        let timezone_calls = Arc::clone(&calls);

        finish_agent_then_timezone(
            move || async move {
                tokio::task::yield_now().await;
                agent_calls.lock().expect("calls").push("agent");
            },
            move || timezone_calls.lock().expect("calls").push("timezone"),
        )
        .await;

        assert_eq!(*calls.lock().expect("calls"), ["agent", "timezone"]);
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn transport_negotiation_sanitizes_unknown_entries_and_keeps_wss() {
        let input = [
            "unknown:cap".to_string(),
            CAPABILITY_TRANSPORT_WSS.to_string(),
            arcen_protocol::CAPABILITY_TRANSPORT_QUIC.to_string(),
            "transport:bogus-v9".to_string(),
        ];
        assert_eq!(
            negotiate_client_transport(&input, CAPABILITY_TRANSPORT_WSS),
            Some(CAPABILITY_TRANSPORT_WSS.to_string())
        );
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn transport_negotiation_returns_none_for_no_common_capability() {
        let input = [CAPABILITY_TRANSPORT_QUIC.to_string()];
        assert!(negotiate_client_transport(&input, CAPABILITY_TRANSPORT_WSS).is_none());
    }

    #[test]
    fn quic_transport_requires_and_accepts_the_quic_capability() {
        let input = [CAPABILITY_TRANSPORT_QUIC.to_string()];
        assert_eq!(
            negotiate_client_transport(&input, CAPABILITY_TRANSPORT_QUIC),
            Some(CAPABILITY_TRANSPORT_QUIC.to_string())
        );
        assert!(negotiate_client_transport(&[], CAPABILITY_TRANSPORT_QUIC).is_none());
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn legacy_client_without_transport_capabilities_uses_wss() {
        assert_eq!(
            negotiate_client_transport(&[], CAPABILITY_TRANSPORT_WSS),
            Some(CAPABILITY_TRANSPORT_WSS.to_string())
        );
    }

    mod color_negotiation {
        use super::*;
        use crate::ColorPolicy;
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
            let plan = test_media_plan(
                arcen_media::video::EncoderBackend::NativeNvenc,
                arcen_media::VideoCodec::H265,
                arcen_media::ChromaSubsampling::Yuv420,
                3840,
                2160,
                60,
            );
            let video = arcen_media::VideoConfiguration {
                codec: arcen_media::VideoCodec::H265,
                chroma: arcen_media::ChromaSubsampling::Yuv420,
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
            // NVENC's contract has no 12-bit entry at all -- no NVIDIA GPU
            // encodes 12-bit at any subsampling.
            let plan = test_media_plan(
                arcen_media::video::EncoderBackend::NativeNvenc,
                arcen_media::VideoCodec::H265,
                arcen_media::ChromaSubsampling::Yuv444,
                3840,
                2160,
                60,
            );
            let video = arcen_media::VideoConfiguration {
                codec: arcen_media::VideoCodec::H265,
                chroma: arcen_media::ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Twelve,
                range: ColorRange::Full,
                matrix: ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            };
            assert!(!color_contract_is_servable(video, &plan));
        }

        #[test]
        fn coherent_and_backend_supported_contract_is_servable() {
            let plan = test_media_plan(
                arcen_media::video::EncoderBackend::NativeNvenc,
                arcen_media::VideoCodec::H265,
                arcen_media::ChromaSubsampling::Yuv444,
                3840,
                2160,
                60,
            );
            let video = arcen_media::VideoConfiguration {
                codec: arcen_media::VideoCodec::H265,
                chroma: arcen_media::ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Ten,
                range: ColorRange::Full,
                matrix: ColorMatrix::Bt709,
                primaries: arcen_media::ColorPrimaries::Bt709,
                transfer: arcen_media::TransferCharacteristics::Bt709,
            };
            assert!(color_contract_is_servable(video, &plan));
        }
    }
}
#[cfg(test)]
fn test_media_plan(
    backend: arcen_media::video::EncoderBackend,
    codec: arcen_media::VideoCodec,
    chroma: arcen_media::ChromaSubsampling,
    width: u32,
    height: u32,
    fps: u32,
) -> ResolvedMediaPlan {
    let software = matches!(
        backend,
        arcen_media::video::EncoderBackend::WindowsMediaFoundation
            | arcen_media::video::EncoderBackend::OpenH264
    );
    ResolvedMediaPlan {
        backend,
        video: arcen_media::VideoConfiguration {
            codec,
            chroma,
            bit_depth: arcen_media::BitDepth::Eight,
            range: arcen_media::ColorRange::Limited,
            matrix: arcen_media::ColorMatrix::Bt709,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        },
        width,
        height,
        fps,
        cursor_mode: CursorMode::Local,
        cursor_in_video: false,
        // Derived from the backend's declared contract rather than restated,
        // so this test helper cannot drift from what the backend can do.
        codecs: if software {
            arcen_media::CodecSet::from_slice(&[arcen_media::VideoCodec::H264])
        } else {
            arcen_media::CodecSet::from_slice(&[
                arcen_media::VideoCodec::H264,
                arcen_media::VideoCodec::H265,
            ])
        },
        chroma: if software {
            arcen_media::ChromaSet::from_slice(&[arcen_media::ChromaSubsampling::Yuv420])
        } else {
            arcen_media::ChromaSet::from_slice(&[
                arcen_media::ChromaSubsampling::Yuv420,
                arcen_media::ChromaSubsampling::Yuv444,
            ])
        },
        bit_depths: backend.contract().bit_depths,
        ranges: backend.contract().ranges,
    }
}
fn validate_direct_transport(value: &str) -> Result<&'static str, String> {
    match value {
        #[cfg(feature = "wss-compat")]
        CAPABILITY_TRANSPORT_WSS => Ok(CAPABILITY_TRANSPORT_WSS),
        CAPABILITY_TRANSPORT_QUIC => Ok(CAPABILITY_TRANSPORT_QUIC),
        _ => Err("direct transport capability is invalid".to_string()),
    }
}

fn multi_monitor_region_input_negotiated(
    multi_monitor_active: bool,
    region_input_available: bool,
    client_hello: &ClientHelloMsg,
) -> bool {
    !multi_monitor_active
        || (region_input_available
            && supports_region_input_v1(
                client_hello.input_protocol_version,
                client_hello.input_capabilities,
            ))
}

fn negotiate_client_transport(
    client_capabilities: &[String],
    active_transport: &'static str,
) -> Option<String> {
    if client_capabilities.is_empty() {
        #[cfg(feature = "wss-compat")]
        if active_transport == CAPABILITY_TRANSPORT_WSS {
            return Some(CAPABILITY_TRANSPORT_WSS.to_string());
        }
        return None;
    }
    let sanitized = sanitize_transport_capabilities(client_capabilities);
    negotiate_transport(&sanitized, &[active_transport]).map(str::to_string)
}
