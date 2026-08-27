//! Pure-Rust Arcen Windows host.

mod audio;
mod auth;
mod capenc;
#[cfg(windows)]
mod clipboard;
/// Non-Windows stub: mirrors enough of `clipboard.rs`'s public API to allow `session.rs`
/// (which is not platform-gated) to compile for cross-platform testing. Nothing here runs.
#[cfg(not(windows))]
mod clipboard {
    use arcen_media::clipboard::{ClipboardFlow, ClipboardPolicy};
    use arcen_protocol::messages::{ClientHelloMsg, ClipboardContentKind, ClipboardPolicyMsg};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;
    use tokio_tungstenite::tungstenite::Message;

    #[derive(Clone, Copy)]
    pub struct ClipboardNegotiation {
        pub(crate) policy: ClipboardPolicy,
    }
    impl ClipboardNegotiation {
        pub fn from_client(_policy: ClipboardPolicy, _hello: &ClientHelloMsg) -> Option<Self> {
            None
        }
        pub const fn policy(self) -> ClipboardPolicy {
            self.policy
        }
        pub fn allows(self, _flow: ClipboardFlow, _kind: ClipboardContentKind) -> bool {
            false
        }
    }

    pub fn policy_message(_policy: ClipboardPolicy) -> ClipboardPolicyMsg {
        unreachable!("clipboard is Windows-only")
    }

    pub struct ClipboardItem {
        pub sequence: u64,
        pub kind: ClipboardContentKind,
        pub bytes: Vec<u8>,
        pub truncated: bool,
    }
    impl ClipboardItem {
        pub fn new(
            _seq: u64,
            _kind: ClipboardContentKind,
            _bytes: Vec<u8>,
            _truncated: bool,
        ) -> Option<Self> {
            None
        }
    }

    pub struct ClipboardWriterQueue {
        closed: AtomicBool,
        notify: Notify,
    }
    impl ClipboardWriterQueue {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                closed: AtomicBool::new(false),
                notify: Notify::new(),
            })
        }
        pub async fn pop(&self) -> Result<Option<Message>, String> {
            loop {
                let notified = self.notify.notified();
                if self.closed.load(Ordering::Acquire) {
                    return Ok(None);
                }
                notified.await;
            }
        }
        pub fn close(&self) {
            self.closed.store(true, Ordering::Release);
            self.notify.notify_waiters();
        }
        pub fn enqueue(&self, _item: ClipboardItem) -> bool {
            false
        }
    }

    pub struct WindowsClipboardRuntime;
    impl WindowsClipboardRuntime {
        pub fn start(
            _negotiation: ClipboardNegotiation,
            _outbound: Arc<ClipboardWriterQueue>,
        ) -> Result<Self, String> {
            Err("Windows clipboard not available on this platform".to_string())
        }
        pub fn inject(&self, _item: ClipboardItem) -> bool {
            false
        }
        pub fn shutdown(&mut self) {}
    }
}
mod config;
mod cp_pipe;
mod cursor_watcher;
mod deskside;
mod display;
mod edid;
mod encoder_admission;
#[cfg(windows)]
mod eventlog;
mod first_login;
#[cfg(windows)]
mod gpu_probe;
#[cfg(not(windows))]
mod gpu_probe {
    pub fn physical_output_inventory(
        _allowed_adapters: &[String],
    ) -> Result<crate::multi_monitor_topology::PhysicalOutputInventory, String> {
        Err("Windows GPU probing is unavailable on this platform".to_owned())
    }

    pub const fn nvenc_runtime_dll() -> bool {
        false
    }
}
mod iddcx;
mod input;
mod ipc;
mod latest;
#[cfg(windows)]
mod log_maintenance;
mod logging;
#[cfg(windows)]
mod logon_activation;
mod microphone_input;
mod multi_monitor_capenc;
mod multi_monitor_gate;
mod multi_monitor_input;
mod multi_monitor_recovery;
mod multi_monitor_topology;
mod netinfo;
mod nvapi;
#[cfg_attr(not(windows), allow(dead_code))]
mod nvapi_headless;
// Only the Windows maintenance command consumes this module; the portable
// classification and its unit tests still build everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod nvapi_inventory;
mod observability;
mod output_provider;
#[cfg(windows)]
mod paths;
#[cfg(not(windows))]
mod paths {
    use std::path::PathBuf;

    pub(crate) fn arcen_data_root() -> PathBuf {
        PathBuf::from("target/arcen-windows-test")
    }

    pub(crate) fn recovery_dir() -> PathBuf {
        arcen_data_root().join("recovery")
    }

    pub(crate) fn agent_runtime_dir() -> PathBuf {
        arcen_data_root().join("agent")
    }
}
mod quic;
mod recovery;
mod resume;
#[cfg(windows)]
mod service;
/// Non-Windows stub: provides the same surface as `service.rs` that `run_host` references
/// so the crate compiles for cross-platform testing. Futures never resolve.
#[cfg(not(windows))]
mod service {
    pub(crate) enum ServiceControlRequest {
        TemporaryDebug,
        ReloadConfigured,
        ReloadTls,
    }
    pub(crate) async fn next_control_request() -> ServiceControlRequest {
        std::future::pending::<ServiceControlRequest>().await
    }
}
mod session;
mod session_admission;
#[cfg(windows)]
mod support_bundle;
mod timezone;
mod tls;
mod tz_map;
mod windows_session;

use std::sync::Arc;
use std::time::Duration;

use arcen_media::{BitDepth, ColorMatrix, ColorRange};
use arcen_protocol::messages::{InitialVideoRequestMsg, VideoSelectionIntent};
use arcen_protocol::{ChromaSubsampling, VideoCodec};
#[cfg(feature = "wss-compat")]
use tokio::net::TcpListener;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
#[cfg(feature = "wss-compat")]
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
#[cfg(windows)]
use tokio_tungstenite::WebSocketStream;

use crate::logging::NET;

const PREAUTH_CAPACITY: usize = 16;
/// Canonical location of the corresponding source, surfaced in the startup
/// banner to satisfy AGPL-3.0 section 13: users of this Pier interact with it
/// over a network rather than running it themselves, so the source offer has to
/// be reachable from the running program, not just from the repository.
const SOURCE_URL: &str = "https://github.com/Aanerud/arcen_public";
/// AGPL-3.0 section 13 source offer, surfaced by `--version`.
const SOURCE_OFFER: &str =
    "Arcen is free software under the GNU AGPL-3.0. It comes with ABSOLUTELY NO WARRANTY. \
     You may redistribute it under the terms of that licence. If you run a modified version \
     that others connect to over a network, you must offer them its corresponding source.";
#[cfg(feature = "wss-compat")]
const TLS_VERSION_REQUIREMENT: &str = "TLS1.2 or TLS1.3";
#[cfg(not(feature = "wss-compat"))]
const TLS_VERSION_REQUIREMENT: &str = "TLS1.3";
const TCP_TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WEBSOCKET_UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_INBOUND_CONTROL_MESSAGE: usize = 64 * 1024;
const MAX_INBOUND_MESSAGE: usize = if MAX_INBOUND_CONTROL_MESSAGE
    > arcen_protocol::CLIPBOARD_HEADER_SIZE + arcen_protocol::CHUNK_BYTES
{
    MAX_INBOUND_CONTROL_MESSAGE
} else {
    arcen_protocol::CLIPBOARD_HEADER_SIZE + arcen_protocol::CHUNK_BYTES
};

/// Process-local Windows Event Log lifecycle emitter.
///
/// Cross-platform modules ([`session`], [`display`]) hold this type without
/// their own `cfg(windows)` forking. On Windows it wraps the real
/// [`eventlog::LifecycleEmitter`]; off Windows it is a guaranteed no-op so the
/// crate keeps type-checking on a non-Windows dev box.
#[cfg(windows)]
pub(crate) use eventlog::LifecycleEmitter;

/// No-op stand-in for [`LifecycleEmitter`] on non-Windows builds.
#[cfg(not(windows))]
#[derive(Clone, Default)]
pub(crate) struct LifecycleEmitter;

#[cfg(not(windows))]
impl LifecycleEmitter {
    pub(crate) fn disabled() -> Self {
        Self
    }

    pub(crate) fn emit(&self, _event: &arcen_telemetry::ValidatedLifecycleEvent) {}

    pub(crate) fn emit_context(
        &self,
        _event: &arcen_telemetry::ValidatedLifecycleEvent,
        _context: arcen_observability::LifecycleContext,
    ) {
    }

    pub(crate) fn emit_drop_notices(&self, _context: arcen_observability::LifecycleContext) {}
}

#[cfg(test)]
mod tls_startup_tests {
    use super::*;

    #[test]
    fn tls_initialization_and_lifecycle_event_precede_bind() {
        let calls = std::sync::Mutex::new(Vec::new());
        let result = build_runtime()
            .expect("runtime")
            .block_on(initialize_tls_before_bind(
                || {
                    calls.lock().expect("calls").push("load");
                    Ok::<_, String>("tls")
                },
                |_| calls.lock().expect("calls").push("event"),
                || async {
                    calls.lock().expect("calls").push("bind");
                    Ok::<_, String>("listener")
                },
            ))
            .expect("ordered initialization");
        assert_eq!(result, ("tls", "listener"));
        assert_eq!(*calls.lock().expect("calls"), ["load", "event", "bind"]);
    }
}

/// Validates and emits one lifecycle event.
///
/// This never returns an error and never affects the caller's own outcome:
/// an unexpected schema-validation failure (which should not happen for the
/// field sets built by this crate) is logged once and native delivery is
/// skipped for that event; a native sink failure is handled the same way
/// inside [`LifecycleEmitter::emit`].
pub(crate) fn emit_lifecycle_event(
    emitter: &LifecycleEmitter,
    kind: arcen_telemetry::LifecycleEventKind,
    correlation_id: arcen_telemetry::CorrelationId,
    fields: arcen_telemetry::StructuredFields,
) {
    match arcen_telemetry::ValidatedLifecycleEvent::new(kind, correlation_id, fields) {
        Ok(event) => emitter.emit(&event),
        Err(error) => tracing::debug!(
            target: NET,
            %error,
            event_id = kind.id(),
            "lifecycle event schema validation failed; native delivery skipped"
        ),
    }
}

pub(crate) fn emit_lifecycle_event_with_context(
    emitter: &LifecycleEmitter,
    kind: arcen_telemetry::LifecycleEventKind,
    correlation_id: arcen_telemetry::CorrelationId,
    fields: arcen_telemetry::StructuredFields,
    user: Option<String>,
    host: Option<String>,
    peer_addr: Option<String>,
    health_state: Option<arcen_telemetry::HealthState>,
) {
    match arcen_telemetry::ValidatedLifecycleEvent::new(kind, correlation_id.clone(), fields) {
        Ok(event) => emitter.emit_context(
            &event,
            arcen_observability::LifecycleContext {
                sid: correlation_id,
                user,
                host,
                peer_addr,
                health_state,
            },
        ),
        Err(error) => tracing::debug!(
            target: NET,
            %error,
            event_id = kind.id(),
            "lifecycle event schema validation failed; canonical delivery skipped"
        ),
    }
}

#[derive(Clone)]
pub struct HostConfig {
    pub capenc_bin: String,
    pub output_selector: display::OutputSelector,
    pub output_index: u32,
    pub codec: VideoCodec,
    pub chroma: ChromaSubsampling,
    /// Ceiling coded component depth; unsupported tokens fail closed.
    /// `video.variant`, when set, overrides this together with `codec`,
    /// `chroma`, `color_range` and `color_matrix`.
    pub bit_depth: BitDepth,
    /// Ceiling coded sample range. Defaults to `limited` so an existing
    /// deployment's wire output does not change until an operator opts in.
    pub color_range: ColorRange,
    /// Ceiling matrix coefficients used to derive luma/chroma from RGB.
    pub color_matrix: ColorMatrix,
    /// Governs how far a negotiating client may deviate from `bit_depth`/
    /// `color_range`/`color_matrix`. See [`ColorPolicy::resolve_bit_depth`].
    pub color_policy: ColorPolicy,
    /// Damage-driven QP biasing for this host's encoders. Operator-owned
    /// rather than client-negotiated: it redistributes bits within a frame
    /// without changing the format a client decodes, so there is nothing to
    /// negotiate. See `docs/architecture/qp-maps.md`.
    pub qp_map: arcen_media::video::QpMapPolicy,
    pub video_selection: VideoSelectionIntent,
    /// Explicit administrator codec pin. The internal default codec remains
    /// only a legacy-client fallback when no auth-time request is available.
    pub codec_pinned: bool,
    pub variant_pinned: bool,
    pub auth_video_request: Option<InitialVideoRequestMsg>,
    pub fps: u32,
    pub encoder: crate::capenc::EncoderSelection,
    pub audio_enabled: bool,
    pub audio_compressed: bool,
    pub microphone_input_enabled: bool,
    pub clipboard_policy: arcen_media::clipboard::ClipboardPolicy,
    pub timezone_redirection: bool,
    pub reconnect_window_secs: u32,
    pub qos_targets: arcen_telemetry::QosTargets,
    pub deskside: deskside::DesksideConfig,
    pub iddcx: config::WindowsIddCxConfig,
    pub multi_monitor: config::WindowsMultiMonitorConfig,
}

impl HostConfig {
    pub fn codec_name(&self) -> &'static str {
        codec_name(self.codec)
    }

    pub fn chroma_name(&self) -> &'static str {
        chroma_name(self.chroma)
    }

    pub(crate) fn requested_encode_intent(&self) -> arcen_media::EncodeIntent {
        self.auth_video_request
            .as_ref()
            .and_then(|request| {
                arcen_media::EncodeIntent::from_token(&request.quality.encode_intent)
            })
            .unwrap_or_default()
    }

    pub(crate) fn apply_software_h264_backend(
        &mut self,
        active: crate::capenc::EncoderSelection,
    ) -> Result<(), String> {
        if active != crate::capenc::EncoderSelection::SoftwareH264 {
            return Ok(());
        }
        if self.variant_pinned && !self.exact_pins_allow_software_h264() {
            return Err(
                "video.variant is incompatible with the exact OpenH264 backend pin".to_string(),
            );
        }
        if self.codec_pinned && self.codec != VideoCodec::H264 {
            return Err(format!(
                "administrator codec pin {} is incompatible with OpenH264",
                self.codec_name()
            ));
        }
        self.codec = VideoCodec::H264;
        self.chroma = ChromaSubsampling::Yuv420;
        self.bit_depth = BitDepth::Eight;
        if self.color_matrix.is_identity() {
            self.color_matrix = ColorMatrix::Bt709;
        }
        self.fps = self.fps.min(
            arcen_media::video::EncoderBackend::OpenH264
                .contract()
                .max_fps,
        );
        Ok(())
    }

    pub(crate) fn exact_pins_allow_software_h264(&self) -> bool {
        if self.codec_pinned && self.codec != VideoCodec::H264 {
            return false;
        }
        !self.variant_pinned
            || (self.codec == VideoCodec::H264
                && self.chroma == ChromaSubsampling::Yuv420
                && self.bit_depth == BitDepth::Eight
                && !self.color_matrix.is_identity()
                && self.fps
                    <= arcen_media::video::EncoderBackend::OpenH264
                        .contract()
                        .max_fps)
    }

    pub(crate) fn apply_initial_video_request(
        &mut self,
        request: &InitialVideoRequestMsg,
    ) -> Result<(), String> {
        let client = arcen_media::video::resolve_client_video_request(request)
            .map_err(|error| format!("initial video request: {error}"))?;
        let current = arcen_media::VideoConfiguration {
            codec: crate::capenc::media_codec(self.codec),
            chroma: crate::capenc::media_chroma(self.chroma),
            bit_depth: self.bit_depth,
            range: self.color_range,
            matrix: self.color_matrix,
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        };
        let resolved = arcen_media::video::resolve_host_initial_video(
            client,
            arcen_media::video::HostInitialVideoPolicy {
                current,
                color_policy: self.color_policy,
                codec_pinned: self.codec_pinned,
                variant_pinned: self.variant_pinned,
                max_fps: self.fps,
            },
        )
        .map_err(|error| format!("initial video request: {error}"))?;
        self.auth_video_request = Some(request.clone());
        self.fps = resolved.max_fps;
        self.codec = crate::capenc::protocol_codec(resolved.video.codec);
        self.chroma = crate::capenc::protocol_chroma(resolved.video.chroma);
        self.bit_depth = resolved.video.bit_depth;
        self.color_range = resolved.video.range;
        self.color_matrix = resolved.video.matrix;
        if !self.variant_pinned {
            self.color_policy = ColorPolicy::AlwaysOn;
        }
        self.video_selection = resolved.selection;
        Ok(())
    }
}

pub(crate) struct Args {
    host: String,
    /// Canonical direct-session QUIC UDP port.
    port: u16,
    tls: tls::TlsFileSource,
    first_login_timeout: Duration,
    profile: arcen_telemetry::OperationalProfile,
    profile_override: Option<arcen_telemetry::OperationalProfile>,
    profile_source: arcen_session::pier_config::LoggingProfileSource,
    log_policy: arcen_telemetry::RetentionPolicy,
    retention_was_clamped: bool,
    config_path: Option<std::path::PathBuf>,
    config_disabled: bool,
    disclaimer: Option<Arc<arcen_identity::PreparedDisclaimer>>,
    config: HostConfig,
    /// Deprecated legacy alias for the QUIC UDP port.
    quic_port: Option<u16>,
    /// Dormant compatibility listener, absent from product binaries.
    #[cfg(feature = "wss-compat")]
    wss_port: Option<u16>,
}

impl Args {
    fn direct_quic_port(&self) -> u16 {
        self.quic_port.unwrap_or(self.port)
    }
}

enum MaintenanceCommand {
    Capenc,
    RestoreDisplay {
        journal: Option<std::path::PathBuf>,
    },
    MigrateDisplayJournal {
        journal: Option<std::path::PathBuf>,
    },
    RestoreTimezone {
        journal: Option<std::path::PathBuf>,
    },
    RestoreWatchdog {
        resource: recovery::WatchdogResource,
        parent_handle: isize,
        ready_handle: isize,
        journal: std::path::PathBuf,
        session_log_id: arcen_telemetry::CorrelationId,
    },
    #[cfg(all(windows, debug_assertions))]
    CrashDisplayTest,
    SessionAgent {
        read_handle: isize,
        write_handle: isize,
        iddcx_control_handle: Option<isize>,
        session_log_id: arcen_telemetry::CorrelationId,
        log_path: std::path::PathBuf,
        profile: arcen_telemetry::OperationalProfile,
    },
    #[cfg(windows)]
    Service,
    #[cfg(windows)]
    ValidateConfig,
    #[cfg(windows)]
    DiagnoseHost {
        json: bool,
    },
    #[cfg(windows)]
    NvapiInventory {
        json: bool,
    },
    #[cfg(windows)]
    NvapiHeadlessProbe {
        request: nvapi_headless::ProbeRequest,
        json: bool,
    },
    #[cfg(windows)]
    NvapiHeadlessRestore {
        journal: Option<std::path::PathBuf>,
    },
    #[cfg(windows)]
    SupportBundle(support_bundle::SupportBundleOptions),
}

fn parse_u32_argument(value: &str, name: &str) -> Result<u32, String> {
    let parsed = value
        .strip_prefix("0x")
        .map_or_else(|| value.parse::<u32>(), |hex| u32::from_str_radix(hex, 16));
    parsed.map_err(|_| format!("invalid {name}: {value}"))
}

fn parse_maintenance_command() -> Result<Option<MaintenanceCommand>, String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Ok(None);
    };
    // Answer `--version` before anything else. Without this the Pier treated the
    // flag as an unknown command and started listening instead of exiting, and
    // it left the AGPL source offer reachable only from the startup banner.
    if command == "--version" || command == "-V" {
        println!("arcen-pier {}", env!("CARGO_PKG_VERSION"));
        println!("{SOURCE_OFFER}");
        println!("Source: {SOURCE_URL}");
        std::process::exit(0);
    }
    let remaining = arguments.collect::<Vec<_>>();
    let _ = remaining.as_slice();
    let mut arguments = remaining.into_iter();
    match command.as_str() {
        "capenc" => Ok(Some(MaintenanceCommand::Capenc)),
        "restore-display" => {
            let mut journal = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--journal" => {
                        journal = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--journal requires a path".to_string())?,
                        ));
                    }
                    "-h" | "--help" => {
                        eprintln!(
                            "USAGE:\n  arcen-pier restore-display \
                             [--journal <PATH>]"
                        );
                        std::process::exit(0);
                    }
                    other => return Err(format!("unknown restore-display argument: {other}")),
                }
            }
            Ok(Some(MaintenanceCommand::RestoreDisplay { journal }))
        }
        "migrate-display-journal" => {
            let mut journal = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--journal" => {
                        journal = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--journal requires a path".to_string())?,
                        ));
                    }
                    "-h" | "--help" => {
                        eprintln!(
                            "USAGE:\n  arcen-pier migrate-display-journal \
                             [--journal <PATH>]"
                        );
                        std::process::exit(0);
                    }
                    other => {
                        return Err(format!("unknown migrate-display-journal argument: {other}"));
                    }
                }
            }
            Ok(Some(MaintenanceCommand::MigrateDisplayJournal { journal }))
        }
        "restore-timezone" => {
            let mut journal = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--journal" => {
                        journal = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--journal requires a path".to_string())?,
                        ));
                    }
                    "-h" | "--help" => {
                        eprintln!("USAGE:\n  arcen-pier restore-timezone [--journal <PATH>]");
                        std::process::exit(0);
                    }
                    other => return Err(format!("unknown restore-timezone argument: {other}")),
                }
            }
            Ok(Some(MaintenanceCommand::RestoreTimezone { journal }))
        }
        "restore-watchdog" => {
            let mut resource = None;
            let mut parent_handle = None;
            let mut ready_handle = None;
            let mut journal = None;
            let mut session_log_id = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--resource" => {
                        resource = Some(recovery::WatchdogResource::parse(
                            &arguments
                                .next()
                                .ok_or_else(|| "--resource requires a value".to_string())?,
                        )?);
                    }
                    "--parent-handle" => {
                        parent_handle = Some(
                            arguments
                                .next()
                                .ok_or_else(|| "--parent-handle requires a value".to_string())?
                                .parse()
                                .map_err(|_| "invalid --parent-handle".to_string())?,
                        );
                    }
                    "--ready-handle" => {
                        ready_handle = Some(
                            arguments
                                .next()
                                .ok_or_else(|| "--ready-handle requires a value".to_string())?
                                .parse()
                                .map_err(|_| "invalid --ready-handle".to_string())?,
                        );
                    }
                    "--journal" => {
                        journal = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--journal requires a path".to_string())?,
                        ));
                    }
                    "--session-log-id" => {
                        session_log_id = Some(
                            arcen_telemetry::CorrelationId::parse_uuid(
                                arguments.next().ok_or_else(|| {
                                    "--session-log-id requires a value".to_string()
                                })?,
                            )
                            .map_err(|_| "invalid --session-log-id".to_string())?,
                        );
                    }
                    other => return Err(format!("unknown restore-watchdog argument: {other}")),
                }
            }
            Ok(Some(MaintenanceCommand::RestoreWatchdog {
                resource: resource
                    .ok_or_else(|| "restore-watchdog requires --resource".to_string())?,
                parent_handle: parent_handle
                    .ok_or_else(|| "restore-watchdog requires --parent-handle".to_string())?,
                ready_handle: ready_handle
                    .ok_or_else(|| "restore-watchdog requires --ready-handle".to_string())?,
                journal: journal
                    .ok_or_else(|| "restore-watchdog requires --journal".to_string())?,
                session_log_id: session_log_id
                    .ok_or_else(|| "restore-watchdog requires --session-log-id".to_string())?,
            }))
        }
        #[cfg(all(windows, debug_assertions))]
        "crash-display-test" => {
            if arguments.next().is_some() {
                return Err("crash-display-test accepts no arguments".to_string());
            }
            Ok(Some(MaintenanceCommand::CrashDisplayTest))
        }
        "session-agent" => {
            let mut read_handle = None;
            let mut write_handle = None;
            let mut iddcx_control_handle = None;
            let mut session_log_id = None;
            let mut log_path = None;
            let mut profile = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--ipc-read" => {
                        read_handle = Some(
                            arguments
                                .next()
                                .ok_or_else(|| "--ipc-read requires a value".to_string())?
                                .parse()
                                .map_err(|_| "invalid --ipc-read".to_string())?,
                        );
                    }
                    "--ipc-write" => {
                        write_handle = Some(
                            arguments
                                .next()
                                .ok_or_else(|| "--ipc-write requires a value".to_string())?
                                .parse()
                                .map_err(|_| "invalid --ipc-write".to_string())?,
                        );
                    }
                    "--iddcx-control" => {
                        iddcx_control_handle = Some(
                            arguments
                                .next()
                                .ok_or_else(|| "--iddcx-control requires a value".to_string())?
                                .parse()
                                .map_err(|_| "invalid --iddcx-control".to_string())?,
                        );
                    }
                    "--session-log-id" => {
                        let value = arguments
                            .next()
                            .ok_or_else(|| "--session-log-id requires a value".to_string())?;
                        session_log_id = Some(
                            arcen_telemetry::CorrelationId::parse_uuid(value)
                                .map_err(|error| error.to_string())?,
                        );
                    }
                    "--log-path" => {
                        log_path = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--log-path requires a value".to_string())?,
                        ));
                    }
                    "--profile" => {
                        let value = arguments
                            .next()
                            .ok_or_else(|| "--profile requires a value".to_string())?
                            .parse::<u8>()
                            .map_err(|_| "--profile must be an integer from 0 through 3")?;
                        profile = Some(
                            arcen_telemetry::OperationalProfile::try_from(value)
                                .map_err(|error| error.to_string())?,
                        );
                    }
                    other => return Err(format!("unknown session-agent argument: {other}")),
                }
            }
            Ok(Some(MaintenanceCommand::SessionAgent {
                read_handle: read_handle
                    .ok_or_else(|| "session-agent requires --ipc-read".to_string())?,
                write_handle: write_handle
                    .ok_or_else(|| "session-agent requires --ipc-write".to_string())?,
                iddcx_control_handle,
                session_log_id: session_log_id
                    .ok_or_else(|| "session-agent requires --session-log-id".to_string())?,
                log_path: log_path
                    .ok_or_else(|| "session-agent requires --log-path".to_string())?,
                profile: profile.ok_or_else(|| "session-agent requires --profile".to_string())?,
            }))
        }
        #[cfg(windows)]
        "service" => Ok(Some(MaintenanceCommand::Service)),
        #[cfg(windows)]
        "validate-config" => Ok(Some(MaintenanceCommand::ValidateConfig)),
        #[cfg(windows)]
        "diagnose-host" => {
            let mut json = false;
            for argument in arguments {
                match argument.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => {
                        eprintln!("USAGE:\n  arcen-pier diagnose-host [--json]");
                        std::process::exit(0);
                    }
                    other => return Err(format!("unknown diagnose-host argument: {other}")),
                }
            }
            Ok(Some(MaintenanceCommand::DiagnoseHost { json }))
        }
        #[cfg(windows)]
        "nvapi-inventory" => {
            let mut json = false;
            for argument in arguments {
                match argument.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => {
                        eprintln!(
                            "USAGE:\n  arcen-pier nvapi-inventory [--json]\n\n\
                             Reports the NVIDIA display targets this host exposes. Read-only:\n\
                             it never writes an EDID, a custom timing, or a display configuration."
                        );
                        std::process::exit(0);
                    }
                    other => return Err(format!("unknown nvapi-inventory argument: {other}")),
                }
            }
            Ok(Some(MaintenanceCommand::NvapiInventory { json }))
        }
        #[cfg(windows)]
        "nvapi-headless-probe" => {
            let mut display_id = None;
            let mut width = None;
            let mut height = None;
            let mut refresh_hz = 60;
            let mut hold_ms = 2_000;
            let mut journal = None;
            let mut json = false;
            let mut acknowledged = false;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--display-id" => {
                        let value = arguments
                            .next()
                            .ok_or_else(|| "--display-id requires a value".to_string())?;
                        display_id = Some(parse_u32_argument(&value, "--display-id")?);
                    }
                    "--width" => {
                        let value = arguments
                            .next()
                            .ok_or_else(|| "--width requires a value".to_string())?;
                        width = Some(parse_u32_argument(&value, "--width")?);
                    }
                    "--height" => {
                        let value = arguments
                            .next()
                            .ok_or_else(|| "--height requires a value".to_string())?;
                        height = Some(parse_u32_argument(&value, "--height")?);
                    }
                    "--refresh-hz" => {
                        let value = arguments
                            .next()
                            .ok_or_else(|| "--refresh-hz requires a value".to_string())?;
                        refresh_hz = parse_u32_argument(&value, "--refresh-hz")?;
                    }
                    "--hold-ms" => {
                        hold_ms = arguments
                            .next()
                            .ok_or_else(|| "--hold-ms requires a value".to_string())?
                            .parse()
                            .map_err(|_| "invalid --hold-ms".to_string())?;
                    }
                    "--journal" => {
                        journal = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--journal requires a path".to_string())?,
                        ));
                    }
                    "--json" => json = true,
                    "--acknowledge-temporary-display-mutation" => acknowledged = true,
                    "-h" | "--help" => {
                        eprintln!(
                            "USAGE:\n  arcen-pier nvapi-headless-probe \
                             --display-id <ID> --width <PX> --height <PX> \
                             [--refresh-hz <HZ>] [--hold-ms <MS>] [--journal <PATH>] [--json] \
                             --acknowledge-temporary-display-mutation\n\n\
                             Lab-only guarded proof. Writes one temporary EDID to an inactive \
                             NVIDIA display ID, verifies Windows enumeration, then removes it. \
                             It arms an out-of-process rollback watchdog before mutation."
                        );
                        std::process::exit(0);
                    }
                    other => {
                        return Err(format!("unknown nvapi-headless-probe argument: {other}"));
                    }
                }
            }
            if !acknowledged {
                return Err(
                    "nvapi-headless-probe requires --acknowledge-temporary-display-mutation"
                        .to_string(),
                );
            }
            Ok(Some(MaintenanceCommand::NvapiHeadlessProbe {
                request: nvapi_headless::ProbeRequest {
                    display_id: display_id
                        .ok_or_else(|| "nvapi-headless-probe requires --display-id".to_string())?,
                    width: width
                        .ok_or_else(|| "nvapi-headless-probe requires --width".to_string())?,
                    height: height
                        .ok_or_else(|| "nvapi-headless-probe requires --height".to_string())?,
                    refresh_hz,
                    hold_ms,
                    journal,
                },
                json,
            }))
        }
        #[cfg(windows)]
        "nvapi-headless-restore" => {
            let mut journal = None;
            while let Some(argument) = arguments.next() {
                match argument.as_str() {
                    "--journal" => {
                        journal = Some(std::path::PathBuf::from(
                            arguments
                                .next()
                                .ok_or_else(|| "--journal requires a path".to_string())?,
                        ));
                    }
                    "-h" | "--help" => {
                        eprintln!("USAGE:\n  arcen-pier nvapi-headless-restore [--journal <PATH>]");
                        std::process::exit(0);
                    }
                    other => {
                        return Err(format!("unknown nvapi-headless-restore argument: {other}"));
                    }
                }
            }
            Ok(Some(MaintenanceCommand::NvapiHeadlessRestore { journal }))
        }
        #[cfg(windows)]
        "support-bundle" => {
            let arguments = arguments.collect::<Vec<_>>();
            support_bundle::parse_options(&arguments)
                .map(|options| Some(MaintenanceCommand::SupportBundle(options)))
        }
        _ => Ok(None),
    }
}

fn default_capenc_bin() -> String {
    if cfg!(windows) {
        std::env::current_exe().map_or_else(
            |_| "arcen-pier.exe".to_string(),
            |path| path.to_string_lossy().into_owned(),
        )
    } else {
        "target/release/arcen-capenc".to_string()
    }
}

fn warn_ignored_capenc_binary(source: &str, value: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        eprintln!(
            "warning: ignoring {source}={value:?}; SEC-151 is closed by spawning current_exe() with the capenc subcommand"
        );
    }
}

fn parse_codec(value: &str) -> Result<VideoCodec, String> {
    match value.to_ascii_lowercase().as_str() {
        "h264" | "avc" => Ok(VideoCodec::H264),
        "h265" | "hevc" => Ok(VideoCodec::H265),
        "av1" => Ok(VideoCodec::Av1),
        other => Err(format!("unsupported codec: {other}")),
    }
}

fn parse_chroma(value: &str) -> Result<ChromaSubsampling, String> {
    match value.to_ascii_lowercase().as_str() {
        "yuv420" | "420" => Ok(ChromaSubsampling::Yuv420),
        "yuv444" | "444" => Ok(ChromaSubsampling::Yuv444),
        other => Err(format!("unsupported chroma: {other}")),
    }
}

fn parse_encoder(value: &str) -> Result<crate::capenc::EncoderSelection, String> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(crate::capenc::EncoderSelection::Auto),
        "nvenc" => Ok(crate::capenc::EncoderSelection::Nvenc),
        "software-h264" | "openh264" => Ok(crate::capenc::EncoderSelection::SoftwareH264),
        other => Err(format!(
            "unsupported encoder: {other} (want auto|nvenc|software-h264)"
        )),
    }
}

fn parse_bit_depth(value: &str) -> Result<BitDepth, String> {
    BitDepth::from_token(value)
        .ok_or_else(|| format!("unsupported bit depth: {value} (want 8|10|12)"))
}

fn parse_color_range(value: &str) -> Result<ColorRange, String> {
    ColorRange::from_token(value)
        .ok_or_else(|| format!("unsupported colour range: {value} (want limited|full)"))
}

fn parse_color_matrix(value: &str) -> Result<ColorMatrix, String> {
    ColorMatrix::from_token(value).ok_or_else(|| {
        format!("unsupported colour matrix: {value} (want identity|bt709|bt601|bt2020ncl)")
    })
}

fn parse_color_policy(value: &str) -> Result<ColorPolicy, String> {
    ColorPolicy::from_token(value).ok_or_else(|| {
        format!(
            "unsupported colour policy: {value} (want always-on|always-off|default-on|default-off)"
        )
    })
}

pub use arcen_media::video::ColorPolicy;

/// Apply a `video.variant` id, overriding `codec`, `chroma`, `bit_depth`,
/// `color_range` and `color_matrix` together. This is how an operator pins a
/// host to one exact row of the probe matrix (see `arcen_media::video::PROBE_MATRIX`)
/// instead of setting five keys by hand. Precedence: this always wins over
/// whatever the individual keys set, regardless of which was applied first.
fn apply_variant(
    id: &str,
    codec: &mut VideoCodec,
    chroma: &mut ChromaSubsampling,
    bit_depth: &mut BitDepth,
    color_range: &mut ColorRange,
    color_matrix: &mut ColorMatrix,
) -> Result<(), String> {
    let variant = arcen_media::video::VideoVariant::from_id(id)
        .map_err(|error| format!("unsupported variant {id:?}: {error}"))?;
    // Resolve every component before mutating the caller's state, so a
    // variant that is coherent (offered by `arcen_media`) but not yet wired
    // into this host's own codec/chroma pipeline (for example AV1 4:4:4 or a
    // 4:2:2 row) leaves the prior configuration completely untouched instead of
    // half-applied, and fails clearly now rather than confusingly at session
    // start. Routing through `parse_codec`/`parse_chroma` (the same functions
    // that validate the individual `video.codec`/`video.chroma` keys) keeps
    // the two paths in lockstep automatically as those functions gain codecs.
    let resolved_codec = parse_codec(variant.video.codec.token())
        .map_err(|error| format!("variant {id:?} selects a codec this host cannot run: {error}"))?;
    let resolved_chroma = parse_chroma(variant.video.chroma.token()).map_err(|error| {
        format!("variant {id:?} selects a chroma this host cannot run: {error}")
    })?;
    *codec = resolved_codec;
    *chroma = resolved_chroma;
    *bit_depth = variant.video.bit_depth;
    *color_range = variant.video.range;
    *color_matrix = variant.video.matrix;
    Ok(())
}

fn parse_clipboard_direction(
    value: &str,
) -> Result<arcen_media::clipboard::ClipboardDirection, String> {
    use arcen_media::clipboard::ClipboardDirection;
    match value.to_ascii_lowercase().as_str() {
        "both" => Ok(ClipboardDirection::Both),
        "client_to_host" => Ok(ClipboardDirection::ClientToHost),
        "host_to_client" => Ok(ClipboardDirection::HostToClient),
        "disabled" => Ok(ClipboardDirection::Disabled),
        _ => Err(
            "clipboard direction must be both, client_to_host, host_to_client, or disabled"
                .to_string(),
        ),
    }
}

fn parse_clipboard_content(
    value: &str,
) -> Result<arcen_media::clipboard::ClipboardContent, String> {
    use arcen_media::clipboard::ClipboardContent;
    match value.to_ascii_lowercase().as_str() {
        "all" => Ok(ClipboardContent::All),
        "text" => Ok(ClipboardContent::Text),
        "image" => Ok(ClipboardContent::Image),
        _ => Err("clipboard content must be all, text, or image".to_string()),
    }
}

fn validated_clipboard_policy(
    direction: arcen_media::clipboard::ClipboardDirection,
    content: arcen_media::clipboard::ClipboardContent,
    max_bytes: usize,
) -> Result<arcen_media::clipboard::ClipboardPolicy, String> {
    if !(1024 * 1024..=arcen_media::clipboard::HARD_MAX_CLIPBOARD_BYTES).contains(&max_bytes) {
        return Err("clipboard max_bytes must be from 1 MiB through 20 MiB".to_string());
    }
    arcen_media::clipboard::ClipboardPolicy::new(direction, content, max_bytes)
        .map_err(|error| format!("clipboard policy: {error}"))
}

pub fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::Av1 => "av1",
        VideoCodec::Jpeg | VideoCodec::Vp9 => "unsupported",
    }
}

pub fn chroma_name(chroma: ChromaSubsampling) -> &'static str {
    match chroma {
        ChromaSubsampling::Yuv420 => "yuv420",
        ChromaSubsampling::Yuv422 => "yuv422",
        ChromaSubsampling::Yuv444 => "yuv444",
    }
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

#[cfg(windows)]
pub(crate) fn parse_service_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(2))
}

fn parse_args_from<I>(it: I) -> Result<Args, String>
where
    I: Iterator<Item = String>,
{
    let raw_args = it.collect::<Vec<_>>();
    let config_request = find_config_request(&raw_args)?;
    let config_disabled = config_request.disabled;
    let loaded = if config_request.disabled {
        None
    } else {
        config::load(config_request.path)?
    };
    let mut config_path = None;
    let mut host = "0.0.0.0".to_string();
    let mut port = 18_444u16;
    let mut tls_cert = String::new();
    let mut tls_key = String::new();
    let mut tls_minimum_version = arcen_transport::tls::TlsVersionFloor::Tls13;
    let mut tls_disabled_cipher_suites = Vec::new();
    let mut tls_expiry_warning_days = 30_u64;
    let mut tls_expected_sans = Vec::new();
    let mut profile = arcen_telemetry::OperationalProfile::Critical;
    let mut profile_override = None;
    let mut profile_source = arcen_session::pier_config::LoggingProfileSource::ProductionDefault;
    let mut qos_targets = arcen_telemetry::QosTargets::default();
    let mut rotate_mb = arcen_telemetry::DEFAULT_ROTATE_BYTES / (1024 * 1024);
    let mut retention_days = arcen_telemetry::DEFAULT_RETENTION_DAYS;
    let mut first_login_timeout = Duration::from_secs(5 * 60);
    let capenc_bin = default_capenc_bin();
    let mut codec = VideoCodec::H264;
    let mut chroma = ChromaSubsampling::Yuv420;
    let mut bit_depth = BitDepth::Eight;
    let mut color_range = ColorRange::Limited;
    let mut color_matrix = ColorMatrix::Bt709;
    let mut color_policy = ColorPolicy::DefaultOff;
    let mut qp_map = arcen_media::video::QpMapPolicy::default();
    let mut video_selection = VideoSelectionIntent::Exact;
    let mut codec_pinned = false;
    let mut variant_pinned = false;
    let mut fps = 30u32;
    let mut encoder = crate::capenc::EncoderSelection::Auto;
    let mut audio_enabled = true;
    let mut audio_compressed = false;
    let mut microphone_input_enabled = false;
    let mut clipboard_policy = arcen_media::clipboard::ClipboardPolicy::default();
    let mut timezone_redirection = false;
    let mut reconnect_window_secs =
        arcen_session::direct_reconnect::DEFAULT_RECONNECT_WINDOW_SECONDS;
    let mut deskside = deskside::DesksideConfig::default();
    let mut iddcx = config::WindowsIddCxConfig::default();
    let mut multi_monitor = config::WindowsMultiMonitorConfig::default();
    let mut output_selector = display::OutputSelector::GlobalIndex(0);
    let mut disclaimer_config = config::DisclaimerConfig::default();
    let mut quic_port: Option<u16> = None;
    #[cfg(feature = "wss-compat")]
    let mut wss_port: Option<u16> = None;

    if let Some(loaded) = loaded {
        config_path = Some(loaded.path);
        let file = loaded.value;
        let resolved = file
            .logging
            .resolved_profile()
            .map_err(|error| format!("Pier config logging profile: {error}"))?;
        profile = resolved.profile;
        profile_source = resolved.source;
        qos_targets = file.logging.qos_targets;
        if let Some(value) = file.platform.logging.rotate_mb {
            rotate_mb = value;
        }
        if let Some(value) = file.logging.retention_days {
            retention_days = value;
        }
        if let Some(value) = file.listen.host {
            host = value;
        }
        if let Some(value) = file.listen.port {
            port = value;
        }
        if let Some(value) = file.listen.quic_port {
            quic_port = Some(value);
        }
        if file
            .tls
            .mode
            .as_deref()
            .is_some_and(|mode| !mode.eq_ignore_ascii_case("pem"))
        {
            return Err("Pier config tls.mode supports only \"pem\"".to_string());
        }
        if let Some(value) = file.tls.cert {
            tls_cert = value;
        }
        if let Some(value) = file.tls.key {
            tls_key = value;
        }
        if let Some(value) = file.tls.minimum_version {
            tls_minimum_version = value.parse().map_err(|_| {
                format!("Pier config tls.minimum_version must be {TLS_VERSION_REQUIREMENT}")
            })?;
        }
        for value in file.tls.disabled_cipher_suites {
            tls_disabled_cipher_suites.push(value.parse().map_err(|_| {
                "Pier config tls.disabled_cipher_suites contains an unsupported ring suite"
            })?);
        }
        if let Some(value) = file.tls.expiry_warning_days {
            if value > 3_650 {
                return Err(
                    "Pier config tls.expiry_warning_days must be between 0 and 3650".to_string(),
                );
            }
            tls_expiry_warning_days = value;
        }
        if file.tls.expected_sans.len() > 64
            || file.tls.expected_sans.iter().any(|name| {
                name.is_empty() || name.len() > 253 || name.chars().any(char::is_control)
            })
        {
            return Err(
                "Pier config tls.expected_sans must contain at most 64 bounded DNS/IP names"
                    .to_string(),
            );
        }
        tls_expected_sans = file.tls.expected_sans;
        if let Some(value) = file.capture.binary {
            warn_ignored_capenc_binary("capture.binary", &value);
        }
        if let Some(value) = file.video.codec {
            codec =
                parse_codec(&value).map_err(|error| format!("Pier config video.codec: {error}"))?;
            codec_pinned = true;
        }
        if let Some(value) = file.video.chroma {
            chroma = parse_chroma(&value)
                .map_err(|error| format!("Pier config video.chroma: {error}"))?;
        }
        if let Some(value) = file.video.bit_depth {
            bit_depth = parse_bit_depth(&value)
                .map_err(|error| format!("Pier config video.bit_depth: {error}"))?;
        }
        if let Some(value) = file.video.color_range {
            color_range = parse_color_range(&value)
                .map_err(|error| format!("Pier config video.color_range: {error}"))?;
        }
        if let Some(value) = file.video.color_matrix {
            color_matrix = parse_color_matrix(&value)
                .map_err(|error| format!("Pier config video.color_matrix: {error}"))?;
        }
        if let Some(value) = file.video.color_policy {
            color_policy = parse_color_policy(&value)
                .map_err(|error| format!("Pier config video.color_policy: {error}"))?;
        }
        if let Some(value) = file.video.qp_map {
            qp_map = arcen_media::video::QpMapPolicy::from_token(&value.to_ascii_lowercase())
                .ok_or_else(|| {
                    let known = arcen_media::video::QpMapPolicy::ALL
                        .iter()
                        .map(|policy| policy.token())
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("Pier config video.qp_map {value:?}: expected one of {known}")
                })?;
        }
        if let Some(value) = file.video.variant {
            apply_variant(
                &value,
                &mut codec,
                &mut chroma,
                &mut bit_depth,
                &mut color_range,
                &mut color_matrix,
            )
            .map_err(|error| format!("Pier config video.variant: {error}"))?;
            color_policy = ColorPolicy::AlwaysOn;
            video_selection = VideoSelectionIntent::Exact;
            variant_pinned = true;
        }
        if let Some(value) = file.video.fps {
            fps = value;
        }
        if let Some(value) = file.video.encoder {
            encoder = parse_encoder(&value)
                .map_err(|error| format!("Pier config video.encoder: {error}"))?;
        }
        audio_enabled = file.audio.enabled;
        audio_compressed = file.audio.compressed;
        microphone_input_enabled = file.microphone_input.enabled;
        if let Some(value) = file.clipboard.direction {
            clipboard_policy.direction = parse_clipboard_direction(&value)?;
        }
        if let Some(value) = file.clipboard.content {
            clipboard_policy.content = parse_clipboard_content(&value)?;
        }
        if let Some(value) = file.clipboard.max_bytes {
            clipboard_policy = validated_clipboard_policy(
                clipboard_policy.direction,
                clipboard_policy.content,
                value,
            )?;
        }
        if let Some(value) = file.platform.first_login_timeout_secs {
            first_login_timeout = validated_first_login_timeout(value)?;
        }
        if let Some(value) = file.redirection.timezone {
            timezone_redirection = value;
        }
        multi_monitor = file.platform.multi_monitor.clone();
        iddcx = file.platform.iddcx.clone();
        iddcx.validate(&multi_monitor)?;
        let desktop = file.platform.desktop;
        if multi_monitor.allowed_adapters.is_empty() {
            if let Some(adapter) = desktop.adapter.as_ref() {
                multi_monitor.allowed_adapters.push(adapter.clone());
            }
        } else if let Some(desktop_adapter) = desktop.adapter.as_ref() {
            // `allowed_adapters` documents that a host pinned to one GPU never
            // silently borrows another GPU reserved for compute -- but that
            // only held for the empty case above, which inherits the desktop
            // adapter. A non-empty list naming some *other* GPU was accepted
            // in silence, which is how a card documented as reserved ended up
            // encoding a remote session with nothing anywhere saying so.
            //
            // Warn rather than reject: a genuine multi-GPU streaming host is a
            // legitimate configuration, and failing closed here would take
            // working hosts down on upgrade. The point is that the decision
            // becomes visible and attributable, not that it becomes impossible.
            let borrowed =
                adapters_beyond_desktop(&multi_monitor.allowed_adapters, desktop_adapter);
            if !borrowed.is_empty() {
                tracing::warn!(
                    target: crate::logging::DISPLAY,
                    desktop_adapter = %desktop_adapter,
                    borrowed_adapters = ?borrowed,
                    "platform.multi_monitor.allowed_adapters permits GPUs other than the \
                     configured desktop adapter; multi-monitor sessions may capture and encode \
                     on a GPU that is reserved for other work"
                );
            }
        }
        desktop.deskside.validate()?;
        deskside = desktop.deskside.clone();
        if let Some(value) = file.auth.reconnect_window_secs {
            reconnect_window_secs = validated_reconnect_window(value)?;
        }
        disclaimer_config = file.auth.disclaimer;
        output_selector = selector_from_file(desktop)?;
    }

    let mut it = raw_args.into_iter();
    let mut cli_global_output = None;
    let mut cli_adapter_name = None;
    let mut cli_adapter_output = None;
    while let Some(arg) = it.next() {
        let mut next = |name: &str| it.next().ok_or_else(|| format!("{name} requires a value"));
        match arg.as_str() {
            "--config" => {
                let _ = next("--config")?;
            }
            "--no-config" => {}
            "--host" => host = next("--host")?,
            "--port" => port = next("--port")?.parse().map_err(|_| "bad --port")?,
            "--quic-port" => {
                quic_port = Some(
                    next("--quic-port")?
                        .parse()
                        .map_err(|_| "bad --quic-port")?,
                )
            }
            #[cfg(feature = "wss-compat")]
            "--wss-port" => {
                wss_port = Some(next("--wss-port")?.parse().map_err(|_| "bad --wss-port")?)
            }
            "--tls-cert" => tls_cert = next("--tls-cert")?,
            "--tls-key" => tls_key = next("--tls-key")?,
            "--first-login-timeout-secs" => {
                let seconds: u64 = next("--first-login-timeout-secs")?
                    .parse()
                    .map_err(|_| "bad --first-login-timeout-secs")?;
                first_login_timeout = validated_first_login_timeout(seconds)?;
            }
            "--capenc-bin" => {
                let value = next("--capenc-bin")?;
                warn_ignored_capenc_binary("--capenc-bin", &value);
            }
            "--output-index" => {
                cli_global_output = Some(
                    next("--output-index")?
                        .parse()
                        .map_err(|_| "bad --output-index")?,
                )
            }
            "--adapter-name" => cli_adapter_name = Some(next("--adapter-name")?),
            "--adapter-output-index" => {
                cli_adapter_output = Some(
                    next("--adapter-output-index")?
                        .parse()
                        .map_err(|_| "bad --adapter-output-index")?,
                )
            }
            "--codec" => {
                codec = parse_codec(&next("--codec")?)?;
                codec_pinned = true;
            }
            "--chroma" => chroma = parse_chroma(&next("--chroma")?)?,
            "--bit-depth" => bit_depth = parse_bit_depth(&next("--bit-depth")?)?,
            "--color-range" => color_range = parse_color_range(&next("--color-range")?)?,
            "--color-matrix" => color_matrix = parse_color_matrix(&next("--color-matrix")?)?,
            "--color-policy" => color_policy = parse_color_policy(&next("--color-policy")?)?,
            "--variant" => {
                apply_variant(
                    &next("--variant")?,
                    &mut codec,
                    &mut chroma,
                    &mut bit_depth,
                    &mut color_range,
                    &mut color_matrix,
                )?;
                color_policy = ColorPolicy::AlwaysOn;
                video_selection = VideoSelectionIntent::Exact;
                variant_pinned = true;
            }
            "--fps" => fps = next("--fps")?.parse().map_err(|_| "bad --fps")?,
            "--encoder" => encoder = parse_encoder(&next("--encoder")?)?,
            "--audio" => audio_enabled = true,
            "--no-audio" => audio_enabled = false,
            "--audio-compressed" => audio_compressed = true,
            "--audio-uncompressed" => audio_compressed = false,
            "--microphone-input" => microphone_input_enabled = true,
            "--no-microphone-input" => microphone_input_enabled = false,
            "--clipboard-direction" => {
                clipboard_policy.direction =
                    parse_clipboard_direction(&next("--clipboard-direction")?)?;
            }
            "--clipboard-content" => {
                clipboard_policy.content = parse_clipboard_content(&next("--clipboard-content")?)?;
            }
            "--clipboard-max-bytes" => {
                let maximum = next("--clipboard-max-bytes")?
                    .parse::<usize>()
                    .map_err(|_| "bad --clipboard-max-bytes")?;
                clipboard_policy = validated_clipboard_policy(
                    clipboard_policy.direction,
                    clipboard_policy.content,
                    maximum,
                )?;
            }
            "--no-clipboard" => {
                clipboard_policy.direction = arcen_media::clipboard::ClipboardDirection::Disabled;
            }
            "--timezone-redirection" => timezone_redirection = true,
            "--no-timezone-redirection" => timezone_redirection = false,
            "--log-level" => {
                let value = next("--log-level")?
                    .parse::<u8>()
                    .map_err(|_| "--log-level must be an integer from 0 through 3")?;
                let selected = arcen_telemetry::OperationalProfile::try_from(value)
                    .map_err(|error| error.to_string())?;
                profile = selected;
                profile_override = Some(selected);
                profile_source = arcen_session::pier_config::LoggingProfileSource::Level;
            }
            "--verbosity" => {
                let value = next("--verbosity")?
                    .parse::<u8>()
                    .map_err(|_| "--verbosity must be an integer from 0 through 3")?;
                let selected = arcen_session::pier_config::LoggingConfig {
                    verbosity: Some(value),
                    ..arcen_session::pier_config::LoggingConfig::default()
                }
                .resolved_profile()
                .map_err(|error| error.to_string())?
                .profile;
                profile = selected;
                profile_override = Some(selected);
                profile_source = arcen_session::pier_config::LoggingProfileSource::LegacyVerbosity;
            }
            "-v" | "--verbose" => {
                profile = arcen_telemetry::OperationalProfile::Debug;
                profile_override = Some(profile);
                profile_source = arcen_session::pier_config::LoggingProfileSource::Level;
            }
            "--quiet" => {
                profile = arcen_telemetry::OperationalProfile::Critical;
                profile_override = Some(profile);
                profile_source = arcen_session::pier_config::LoggingProfileSource::Level;
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    output_selector = merge_output_selector(
        output_selector,
        cli_global_output,
        cli_adapter_name,
        cli_adapter_output,
    )?;
    if tls_cert.is_empty() || tls_key.is_empty() {
        return Err("--tls-cert and --tls-key are required".to_string());
    }
    let direct_quic_port = quic_port.unwrap_or(port);
    if direct_quic_port == 0 {
        return Err("direct QUIC requires a nonzero UDP port".to_string());
    }
    #[cfg(feature = "wss-compat")]
    if wss_port == Some(direct_quic_port) {
        return Err("dormant WSS compatibility cannot share the QUIC UDP port".to_string());
    }
    if fps == 0 || fps > 240 {
        return Err("--fps must be between 1 and 240".to_string());
    }
    if chroma == ChromaSubsampling::Yuv444 && codec != VideoCodec::H265 {
        return Err(format!(
            "yuv444 requires h265 on the macOS VideoToolbox path; {} supports only yuv420",
            codec_name(codec)
        ));
    }
    if bit_depth == BitDepth::Twelve && encoder == crate::capenc::EncoderSelection::Nvenc {
        return Err(
            "video.bit_depth 12 cannot work with --encoder nvenc: NVENC has no 12-bit mode at \
             any subsampling; use the software tier for 12-bit"
                .to_string(),
        );
    }
    if encoder == crate::capenc::EncoderSelection::SoftwareH264
        && (codec != VideoCodec::H264 || chroma != ChromaSubsampling::Yuv420)
    {
        return Err("OpenH264 software encoding requires h264 + yuv420".to_string());
    }
    if multi_monitor.advertise_enabled {
        if multi_monitor.allowed_adapters.is_empty() {
            return Err(
                "platform.multi_monitor.advertise_enabled requires at least one allowed adapter"
                    .to_string(),
            );
        }
        if multi_monitor
            .allowed_adapters
            .iter()
            .any(|adapter| adapter.trim().is_empty())
        {
            return Err(
                "platform.multi_monitor.allowed_adapters must not contain empty names".to_string(),
            );
        }
        let mut unique = std::collections::BTreeSet::new();
        if multi_monitor
            .allowed_adapters
            .iter()
            .any(|adapter| !unique.insert(adapter.to_ascii_lowercase()))
        {
            return Err(
                "platform.multi_monitor.allowed_adapters contains a duplicate adapter".to_string(),
            );
        }
        if multi_monitor.nvenc_session_limit == Some(0) {
            return Err(
                "platform.multi_monitor.nvenc_session_limit must be at least 1".to_string(),
            );
        }
        if let Some(max_monitors) = multi_monitor.max_monitors {
            let ceiling = u8::try_from(arcen_media::MAX_MULTI_MONITOR_COUNT).unwrap_or(u8::MAX);
            if max_monitors == 0 || max_monitors > ceiling {
                return Err(format!(
                    "platform.multi_monitor.max_monitors must be between 1 and {ceiling}"
                ));
            }
        }
        if multi_monitor.nvidia_headless_enabled && multi_monitor.allowed_adapters.len() != 1 {
            return Err(
                "platform.multi_monitor.nvidia_headless_enabled requires exactly one allowed \
                 display/stream adapter"
                    .to_string(),
            );
        }
        if multi_monitor.nvidia_headless_enabled && iddcx.enabled {
            return Err(
                "NVIDIA headless provisioning and platform.iddcx.enabled are mutually exclusive"
                    .to_string(),
            );
        }
    }
    let rotate_bytes = rotate_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "logging.rotate_mb exceeds the supported range".to_string())?;
    let log_policy = arcen_telemetry::RetentionPolicy::new(rotate_bytes, retention_days)
        .map_err(|error| format!("logging policy: {error}"))?;
    let retention_was_clamped = log_policy.retention_days() != retention_days;
    let disclaimer = prepare_disclaimer(&disclaimer_config)?.map(Arc::new);
    let tls_posture =
        arcen_transport::tls::TlsPosture::new(tls_minimum_version, tls_disabled_cipher_suites)
            .map_err(|error| format!("Pier config TLS posture: {error}"))?;
    let warning_window_secs = tls_expiry_warning_days
        .checked_mul(24 * 60 * 60)
        .ok_or_else(|| "Pier config tls.expiry_warning_days overflows".to_string())?;

    let mut config = HostConfig {
        capenc_bin,
        output_selector,
        output_index: 0,
        codec,
        chroma,
        bit_depth,
        color_range,
        color_matrix,
        color_policy,
        qp_map,
        video_selection,
        codec_pinned,
        variant_pinned,
        auth_video_request: None,
        fps,
        encoder,
        audio_enabled,
        audio_compressed,
        microphone_input_enabled,
        clipboard_policy,
        timezone_redirection,
        reconnect_window_secs,
        qos_targets,
        deskside,
        iddcx,
        multi_monitor,
    };
    if config.encoder == crate::capenc::EncoderSelection::SoftwareH264 {
        config.apply_software_h264_backend(config.encoder)?;
    }

    Ok(Args {
        host,
        port,
        tls: tls::TlsFileSource {
            certificate_path: tls_cert.into(),
            private_key_path: tls_key.into(),
            expected_sans: tls_expected_sans,
            posture: tls_posture,
            key_policy: arcen_transport::tls::CertificateKeyPolicy::default(),
            time_policy: arcen_transport::tls::CertificateTimePolicy {
                warning_window_secs,
            },
        },
        first_login_timeout,
        profile,
        profile_override,
        profile_source,
        log_policy,
        retention_was_clamped,
        config_path,
        config_disabled,
        disclaimer,
        config,
        quic_port,
        #[cfg(feature = "wss-compat")]
        wss_port,
    })
}

fn prepare_disclaimer(
    config: &config::DisclaimerConfig,
) -> Result<Option<arcen_identity::PreparedDisclaimer>, String> {
    if !config.enabled {
        return Ok(None);
    }
    let locale = arcen_identity::DisclaimerLocale::new(
        config.locale.as_deref().unwrap_or("en_US").to_string(),
    )
    .map_err(|error| format!("Pier config auth.disclaimer.locale: {error}"))?;
    let directory = config.directory.as_ref().map_or_else(
        || {
            #[cfg(windows)]
            {
                crate::paths::arcen_data_root().join("disclaimers")
            }
            #[cfg(not(windows))]
            {
                std::path::PathBuf::from("disclaimers")
            }
        },
        std::path::PathBuf::from,
    );
    let path = directory.join(format!("{}.txt", locale.as_str()));
    let bytes = read_bounded_disclaimer(&path)?;
    arcen_identity::PreparedDisclaimer::from_bytes(locale, &bytes)
        .map(Some)
        .map_err(|error| format!("validate disclaimer {}: {error}", path.display()))
}

fn read_bounded_disclaimer(path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let file = std::fs::File::open(path)
        .map_err(|error| format!("read disclaimer {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(arcen_identity::MAX_DISCLAIMER_CONTENT_BYTES + 1);
    file.take((arcen_identity::MAX_DISCLAIMER_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read disclaimer {}: {error}", path.display()))?;
    Ok(bytes)
}

fn reload_configured_profile(
    args: &Args,
) -> Result<
    (
        arcen_telemetry::OperationalProfile,
        arcen_session::pier_config::LoggingProfileSource,
        arcen_telemetry::QosTargets,
    ),
    String,
> {
    if let Some(profile) = args.profile_override {
        return Ok((profile, args.profile_source, args.config.qos_targets));
    }
    if args.config_disabled {
        return Ok((args.profile, args.profile_source, args.config.qos_targets));
    }
    let Some(loaded) = config::load(args.config_path.clone())? else {
        return Ok((
            arcen_telemetry::OperationalProfile::Critical,
            arcen_session::pier_config::LoggingProfileSource::ProductionDefault,
            arcen_telemetry::QosTargets::default(),
        ));
    };
    let resolved = loaded
        .value
        .logging
        .resolved_profile()
        .map_err(|error| format!("Pier config logging profile: {error}"))?;
    Ok((
        resolved.profile,
        resolved.source,
        loaded.value.logging.qos_targets,
    ))
}

struct ConfigRequest {
    path: Option<std::path::PathBuf>,
    disabled: bool,
}

fn find_config_request(args: &[String]) -> Result<ConfigRequest, String> {
    let mut found = None;
    let mut disabled = false;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--config" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--config requires a value".to_string())?;
            if found.replace(std::path::PathBuf::from(value)).is_some() {
                return Err("--config may be supplied only once".to_string());
            }
            index += 2;
        } else if args[index] == "--no-config" {
            disabled = true;
            index += 1;
        } else {
            index += 1;
        }
    }
    if disabled && found.is_some() {
        return Err("--config and --no-config are mutually exclusive".to_string());
    }
    Ok(ConfigRequest {
        path: found,
        disabled,
    })
}

fn selector_from_file(desktop: config::DesktopConfig) -> Result<display::OutputSelector, String> {
    match (desktop.adapter, desktop.output_index, desktop.output) {
        (Some(_), Some(_), _) => Err(
            "Pier config desktop.adapter and desktop.output_index are mutually exclusive"
                .to_string(),
        ),
        (Some(name), None, output) if !name.trim().is_empty() => {
            Ok(display::OutputSelector::Adapter {
                name,
                output_index: output.unwrap_or(0),
            })
        }
        (Some(_), None, _) => Err("Pier config desktop.adapter must not be empty".to_string()),
        (None, Some(index), None) => Ok(display::OutputSelector::GlobalIndex(index)),
        (None, Some(_), Some(_)) => {
            Err("Pier config desktop.output is valid only with desktop.adapter".to_string())
        }
        (None, None, Some(_)) => {
            Err("Pier config desktop.output requires desktop.adapter".to_string())
        }
        (None, None, None) => Ok(display::OutputSelector::GlobalIndex(0)),
    }
}

fn merge_output_selector(
    base: display::OutputSelector,
    global: Option<u32>,
    adapter_name: Option<String>,
    adapter_output: Option<u32>,
) -> Result<display::OutputSelector, String> {
    if global.is_some() && (adapter_name.is_some() || adapter_output.is_some()) {
        return Err(
            "--output-index cannot be combined with --adapter-name/--adapter-output-index"
                .to_string(),
        );
    }
    if let Some(index) = global {
        return Ok(display::OutputSelector::GlobalIndex(index));
    }
    if adapter_name.is_none() && adapter_output.is_none() {
        return Ok(base);
    }
    let (base_name, base_output) = match base {
        display::OutputSelector::Adapter { name, output_index } => (Some(name), output_index),
        display::OutputSelector::GlobalIndex(_) => (None, 0),
    };
    let name = adapter_name
        .or(base_name)
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| "--adapter-output-index requires --adapter-name".to_string())?;
    Ok(display::OutputSelector::Adapter {
        name,
        output_index: adapter_output.unwrap_or(base_output),
    })
}

/// Adapters multi-monitor may consume that are not the configured desktop
/// adapter — that is, GPUs a session could capture and encode on even though
/// the host is nominally pinned elsewhere.
///
/// Case-insensitive because DXGI adapter descriptions are matched that way
/// everywhere else in this host, so a config differing only in case must not
/// be reported as a different GPU.
fn adapters_beyond_desktop(allowed: &[String], desktop_adapter: &str) -> Vec<String> {
    allowed
        .iter()
        .filter(|adapter| !adapter.eq_ignore_ascii_case(desktop_adapter))
        .cloned()
        .collect()
}

fn validated_first_login_timeout(seconds: u64) -> Result<Duration, String> {
    if !(30..=1_800).contains(&seconds) {
        return Err("--first-login-timeout-secs must be between 30 and 1800".to_string());
    }
    Ok(Duration::from_secs(seconds))
}

fn validated_reconnect_window(seconds: u32) -> Result<u32, String> {
    arcen_session::direct_reconnect::ReconnectPolicy::new(seconds)
        .map(|policy| policy.window_secs())
        .map_err(|error| format!("Pier config auth.reconnect_window_secs: {error}"))
}

fn print_help() {
    eprintln!(
        "arcen-pier\n\n\
         USAGE:\n  arcen-pier [--config <JSON>] [options]\n\n\
         RECOVERY:\n  arcen-pier restore-display [--journal <PATH>]\n\
                   arcen-pier migrate-display-journal [--journal <PATH>]\n\
                   arcen-pier restore-timezone [--journal <PATH>]\n\n\
         SUPPORT:\n  arcen-pier support-bundle [--out <DIR>]\n\n\
         VALIDATE:\n  arcen-pier validate-config [--schema-only] [--config <JSON>] [overrides]\n\n\
         OPTIONS:\n\
           --config <PATH>        JSON settings (default %ProgramData%\\Arcen\\pier.json)\n\
           --no-config           ignore the default settings file\n\
           --host <ADDR>          bind address (default 0.0.0.0)\n\
           --port <PORT>          QUIC UDP port (default 18444)\n\
           --quic-port <PORT>     deprecated QUIC UDP port alias\n\
           --tls-cert <PATH>      QUIC TLS 1.3 certificate PEM\n\
           --tls-key <PATH>       TLS private key PEM (PKCS#8 or PKCS#1)\n\
           --first-login-timeout-secs <N>\n\
                                 profile/session wait, 30-1800 (default 300)\n\
           --capenc-bin <PATH>    deprecated; ignored, capenc runs via current_exe()\n\
           --output-index <N>     global attached-output override\n\
           --adapter-name <NAME>  exact DXGI adapter description override\n\
           --adapter-output-index <N>\n\
                                  output ordinal on selected adapter (default 0)\n\
           --codec <h264|h265>    encoder codec (default h264)\n\
           --chroma <yuv420|yuv444> (default yuv420)\n\
           --bit-depth <8|10|12>  coded component depth (default 8; 12 needs the software tier)\n\
           --color-range <limited|full>  coded sample range (default limited)\n\
           --color-matrix <identity|bt709|bt601|bt2020ncl>  matrix coefficients (default bt709)\n\
           --color-policy <always-on|always-off|default-on|default-off>\n\
                                  colour fidelity ceiling/default vs. client negotiation (default default-off)\n\
           --variant <id>         probe-matrix variant id; overrides codec/chroma/bit-depth/range/matrix\n\
           --fps <N>              target FPS (default 30)\n\
           --encoder <auto|nvenc|software-h264>\n\
                                  backend (auto remains NVENC then OpenH264)\n\
           --audio / --no-audio  enable/disable WASAPI loopback\n\
           --audio-compressed / --audio-uncompressed\n\
                                  force Opus 128 kbps or uncompressed PCM\n\
           --microphone-input / --no-microphone-input\n\
                                  enable/disable optional Deck microphone policy\n\
           --clipboard-direction <both|client_to_host|host_to_client|disabled>\n\
           --clipboard-content <all|text|image>\n\
           --clipboard-max-bytes <BYTES> encoded cap from 1 MiB through 20 MiB\n\
           --no-clipboard         disable advertisement and clipboard watcher\n\
           --timezone-redirection / --no-timezone-redirection\n\
                                 enable/disable system-wide client time-zone redirection\n\
           --log-level <0..3>     critical, error, info, or debug profile\n\
           --verbosity <0..3>     one-release legacy verbosity mapping\n\
           -v, --verbose          debug logging tier\n\
           --quiet                override verbose config\n\
        \n\
        {SOURCE_OFFER}\n\
        Source: {SOURCE_URL}\n"
    );
}

fn main() {
    match parse_maintenance_command() {
        Ok(Some(MaintenanceCommand::Capenc)) => {
            let mut raw = std::env::args();
            let program = raw.next().unwrap_or_else(|| "arcen-pier".to_string());
            let _subcommand = raw.next();
            let args = std::iter::once(program).chain(raw).collect::<Vec<_>>();
            arcen_capenc::run_with_args(args);
            return;
        }
        Ok(Some(MaintenanceCommand::SessionAgent {
            read_handle,
            write_handle,
            iddcx_control_handle,
            session_log_id,
            log_path,
            profile,
        })) => {
            #[cfg(not(windows))]
            {
                let _ = (
                    read_handle,
                    write_handle,
                    iddcx_control_handle,
                    session_log_id,
                    log_path,
                    profile,
                );
                unreachable!("session-agent subcommand is Windows-only");
            }
            #[cfg(windows)]
            {
                let log_file = match service::WindowsLogFile::open_existing(log_path) {
                    Ok(log_file) => log_file,
                    Err(error) => {
                        eprintln!("session-agent log setup failed: {error}");
                        std::process::exit(1);
                    }
                };
                let log_controller = match logging::init(
                    profile,
                    logging::COMPONENT_SESSION_AGENT,
                    Some(log_file),
                    true,
                ) {
                    Ok(controller) => controller,
                    Err(error) => {
                        eprintln!("session-agent tracing setup failed: {error}");
                        std::process::exit(1);
                    }
                };
                let span = tracing::info_span!(
                    target: logging::SESSION,
                    "windows_session_agent",
                    sid = %session_log_id
                );
                let result = build_runtime().and_then(|runtime| {
                    use tracing::Instrument;
                    runtime.block_on(
                        run_session_agent(
                            read_handle,
                            write_handle,
                            iddcx_control_handle,
                            session_log_id,
                            log_controller.clone(),
                        )
                        .instrument(span),
                    )
                });
                let exit_code = match result {
                    Ok(()) => 0,
                    Err(error) => {
                        tracing::error!(target: logging::SESSION, %error, "session agent failed");
                        1
                    }
                };
                if let Err(error) = log_controller.flush_log() {
                    eprintln!("session-agent observability flush failed: {error}");
                }
                std::process::exit(exit_code);
            }
        }
        #[cfg(windows)]
        Ok(Some(MaintenanceCommand::SupportBundle(options))) => {
            match support_bundle::run(&options) {
                Ok(result) => {
                    println!("{}", result.path.display());
                    if result.omission_count != 0 {
                        eprintln!(
                            "support bundle completed with {} unavailable or omitted sources",
                            result.omission_count
                        );
                    }
                }
                Err(error) => {
                    eprintln!("support bundle failed: {error}");
                    std::process::exit(1);
                }
            }
            return;
        }
        #[cfg(windows)]
        Ok(Some(MaintenanceCommand::Service)) => {
            if let Err(error) = service::run_dispatcher() {
                eprintln!("service dispatcher failed: {error}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(windows)]
        Ok(Some(MaintenanceCommand::ValidateConfig)) => {
            let mut raw_args = std::env::args().skip(2).collect::<Vec<_>>();
            let schema_only_count = raw_args
                .iter()
                .filter(|argument| argument.as_str() == "--schema-only")
                .count();
            if schema_only_count > 1 {
                eprintln!("config validation failed: --schema-only may be supplied only once");
                std::process::exit(2);
            }
            let schema_only = schema_only_count == 1;
            raw_args.retain(|argument| argument != "--schema-only");
            let args = match parse_args_from(raw_args.into_iter()) {
                Ok(args) => args,
                Err(error) => {
                    eprintln!("config validation failed: {error}");
                    std::process::exit(2);
                }
            };
            if let Err(error) = tls::TlsLifecycle::load(args.tls.clone()) {
                eprintln!("config validation failed: TLS configuration: {error}");
                std::process::exit(1);
            }
            if schema_only {
                println!("Pier configuration schema and TLS material are valid.");
                return;
            }
            let resolved = match display::resolve_output_selector(&args.config.output_selector) {
                Ok(resolved) => resolved,
                Err(error) => {
                    eprintln!("config validation failed: {error}");
                    std::process::exit(1);
                }
            };
            println!(
                "config={} adapter={} adapter_output={} global_output={} device={}",
                args.config_path.as_ref().map_or_else(
                    || "<cli/defaults>".to_string(),
                    |path| path.display().to_string()
                ),
                resolved.adapter_name,
                resolved.adapter_output_index,
                resolved.global_index,
                resolved.device_name
            );
            return;
        }
        #[cfg(windows)]
        Ok(Some(MaintenanceCommand::DiagnoseHost { json })) => {
            let _log_controller = logging::init(
                arcen_telemetry::OperationalProfile::Debug,
                logging::COMPONENT_DIAGNOSTIC,
                None,
                true,
            )
            .unwrap_or_else(|error| {
                eprintln!("logging setup failed: {error}");
                std::process::exit(1);
            });
            match gpu_probe::probe() {
                Ok(report) if json => match serde_json::to_string_pretty(&report) {
                    Ok(value) => println!("{value}"),
                    Err(error) => {
                        eprintln!("host diagnosis serialization failed: {error}");
                        if let Err(error) = _log_controller.flush_log() {
                            eprintln!("diagnostic observability flush failed: {error}");
                        }
                        std::process::exit(1);
                    }
                },
                Ok(report) => print!("{}", report.human_summary()),
                Err(error) => {
                    eprintln!("host diagnosis failed: {error}");
                    if let Err(error) = _log_controller.flush_log() {
                        eprintln!("diagnostic observability flush failed: {error}");
                    }
                    std::process::exit(1);
                }
            }
            if let Err(error) = _log_controller.flush_log() {
                eprintln!("diagnostic observability flush failed: {error}");
            }
            return;
        }
        #[cfg(windows)]
        Ok(Some(MaintenanceCommand::NvapiInventory { json })) => {
            let _log_controller = logging::init(
                arcen_telemetry::OperationalProfile::Debug,
                logging::COMPONENT_DIAGNOSTIC,
                None,
                true,
            )
            .unwrap_or_else(|error| {
                eprintln!("logging setup failed: {error}");
                std::process::exit(1);
            });
            let exit_code = match nvapi_inventory::inventory() {
                Ok(report) if json => match serde_json::to_string_pretty(&report) {
                    Ok(value) => {
                        println!("{value}");
                        0
                    }
                    Err(error) => {
                        eprintln!("NVAPI inventory serialization failed: {error}");
                        1
                    }
                },
                Ok(report) => {
                    print!("{}", nvapi_inventory::render_summary(&report));
                    0
                }
                Err(error) => {
                    eprintln!("NVAPI inventory failed: {error}");
                    1
                }
            };
            if let Err(error) = _log_controller.flush_log() {
                eprintln!("diagnostic observability flush failed: {error}");
            }
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return;
        }
        #[cfg(windows)]
        Ok(Some(MaintenanceCommand::NvapiHeadlessProbe { request, json })) => {
            let _log_controller = logging::init(
                arcen_telemetry::OperationalProfile::Debug,
                logging::COMPONENT_DIAGNOSTIC,
                None,
                true,
            )
            .unwrap_or_else(|error| {
                eprintln!("logging setup failed: {error}");
                std::process::exit(1);
            });
            let owner = random_correlation_id();
            let exit_code = match nvapi_headless::probe(request, &owner) {
                Ok(report) if json => match serde_json::to_string_pretty(&report) {
                    Ok(value) => {
                        println!("{value}");
                        0
                    }
                    Err(error) => {
                        eprintln!("NVAPI headless probe serialization failed: {error}");
                        1
                    }
                },
                Ok(report) => {
                    print!("{}", nvapi_headless::summary(&report));
                    0
                }
                Err(error) => {
                    eprintln!("NVAPI headless probe failed: {error}");
                    1
                }
            };
            if let Err(error) = _log_controller.flush_log() {
                eprintln!("diagnostic observability flush failed: {error}");
            }
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            return;
        }
        Ok(Some(command)) => {
            let _log_controller = logging::init(
                arcen_telemetry::OperationalProfile::Debug,
                logging::COMPONENT_DIAGNOSTIC,
                None,
                cfg!(windows),
            )
            .unwrap_or_else(|error| {
                eprintln!("logging setup failed: {error}");
                std::process::exit(1);
            });
            #[cfg(windows)]
            let emitter = eventlog::LifecycleEmitter::init_process_local(_log_controller.handle());
            #[cfg(not(windows))]
            let emitter = LifecycleEmitter::disabled();
            let result = match command {
                MaintenanceCommand::RestoreDisplay { journal } => {
                    display::restore_from_journal(journal, &emitter)
                }
                MaintenanceCommand::MigrateDisplayJournal { journal } => {
                    display::migrate_legacy_journal(journal, &emitter)
                }
                MaintenanceCommand::RestoreTimezone { journal } => {
                    timezone::restore_from_journal(journal)
                }
                #[cfg(windows)]
                MaintenanceCommand::NvapiHeadlessRestore { journal } => {
                    nvapi_headless::restore(journal)
                }
                MaintenanceCommand::RestoreWatchdog {
                    resource,
                    parent_handle,
                    ready_handle,
                    journal,
                    session_log_id,
                } => match resource {
                    recovery::WatchdogResource::Display => display::run_restore_watchdog(
                        parent_handle,
                        ready_handle,
                        journal,
                        session_log_id,
                        &emitter,
                    ),
                    recovery::WatchdogResource::NvapiHeadless => {
                        nvapi_headless::run_restore_watchdog(
                            parent_handle,
                            ready_handle,
                            journal,
                            session_log_id,
                        )
                    }
                    recovery::WatchdogResource::Timezone => timezone::run_restore_watchdog(
                        parent_handle,
                        ready_handle,
                        journal,
                        session_log_id,
                    ),
                },
                #[cfg(all(windows, debug_assertions))]
                MaintenanceCommand::CrashDisplayTest => display::run_live_watchdog_crash_test(),
                MaintenanceCommand::SessionAgent { .. } => unreachable!("handled above"),
                #[cfg(windows)]
                MaintenanceCommand::Service => unreachable!("handled above"),
                #[cfg(windows)]
                MaintenanceCommand::ValidateConfig => unreachable!("handled above"),
                #[cfg(windows)]
                MaintenanceCommand::DiagnoseHost { .. } => unreachable!("handled above"),
                #[cfg(windows)]
                MaintenanceCommand::NvapiInventory { .. } => unreachable!("handled above"),
                #[cfg(windows)]
                MaintenanceCommand::NvapiHeadlessProbe { .. } => unreachable!("handled above"),
                #[cfg(windows)]
                MaintenanceCommand::SupportBundle(_) => unreachable!("handled above"),
                MaintenanceCommand::Capenc => unreachable!("handled above"),
            };
            let exit_code = match result {
                Ok(()) => 0,
                Err(error) => {
                    tracing::error!(target: logging::DISPLAY, %error, "display recovery failed");
                    eprintln!("display recovery failed: {error}");
                    1
                }
            };
            if let Err(error) = _log_controller.flush_log() {
                eprintln!("maintenance observability flush failed: {error}");
            }
            std::process::exit(exit_code);
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(2);
        }
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("error: {error}\n");
            print_help();
            std::process::exit(2);
        }
    };
    let log_controller =
        match logging::init(args.profile, logging::COMPONENT_BROKER, None, cfg!(windows)) {
            Ok(controller) => controller,
            Err(error) => {
                eprintln!("logging setup failed: {error}");
                std::process::exit(1);
            }
        };
    let result = build_runtime().and_then(|runtime| {
        #[cfg(windows)]
        let emitter = eventlog::LifecycleEmitter::init_process_local(log_controller.handle());
        #[cfg(not(windows))]
        let emitter = LifecycleEmitter::disabled();
        runtime.block_on(run_host(
            args,
            log_controller.clone(),
            console_shutdown(),
            || {},
            emitter,
        ))
    });
    if let Err(error) = result {
        tracing::error!(target: NET, %error, "Windows Pier stopped with an error");
        if let Err(flush_error) = log_controller.flush_log() {
            eprintln!("broker observability flush failed: {flush_error}");
        }
        std::process::exit(1);
    }
    if let Err(error) = log_controller.flush_log() {
        eprintln!("broker observability flush failed: {error}");
    }
}

fn build_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("create Tokio runtime: {error}"))
}

async fn console_shutdown() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!(target: NET, "shutdown signal received"),
        Err(error) => tracing::warn!(target: NET, %error, "shutdown signal failed"),
    }
}

#[cfg(feature = "wss-compat")]
type CompatibilityListener = Option<TcpListener>;
#[cfg(not(feature = "wss-compat"))]
type CompatibilityListener = ();

#[cfg(feature = "wss-compat")]
async fn accept_compatibility(
    listener: &CompatibilityListener,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

#[cfg(not(feature = "wss-compat"))]
async fn accept_compatibility(
    _listener: &CompatibilityListener,
) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    std::future::pending().await
}

#[cfg(feature = "wss-compat")]
#[allow(clippy::too_many_arguments)]
fn spawn_compatibility_session(
    stream: tokio::net::TcpStream,
    peer: std::net::SocketAddr,
    args: &Args,
    tls_lifecycle: &Arc<tls::TlsLifecycle>,
    agent_lease: &Arc<session::BrokerAgentLease>,
    cp_coordinator: &Arc<cp_pipe::CpCoordinator>,
    timezone_controller: &Arc<timezone::TimezoneController>,
    emitter: &LifecycleEmitter,
    resume_registry: &Arc<resume::ResumeRegistry>,
    session_shutdown: &watch::Sender<bool>,
    preauth_slots: &Arc<Semaphore>,
    sessions: &mut JoinSet<()>,
) {
    let acceptor = tls_lifecycle.acceptor();
    let connection_tls = Arc::clone(tls_lifecycle);
    let config = args.config.clone();
    let disclaimer = args.disclaimer.clone();
    let profile = agent_lease.current_profile();
    let agent_lease = Arc::clone(agent_lease);
    let cp_coordinator = Arc::clone(cp_coordinator);
    let timezone_controller = Arc::clone(timezone_controller);
    let emitter = emitter.clone();
    let resume_registry = Arc::clone(resume_registry);
    let session_shutdown = session_shutdown.subscribe();
    let preauth_permit = match Arc::clone(preauth_slots).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            tracing::warn!(
                target: NET,
                %peer,
                capacity = PREAUTH_CAPACITY,
                "rejecting dormant compatibility connection: pre-authentication capacity exhausted"
            );
            return;
        }
    };
    let preauth_guard = auth::PreauthGuard::new(preauth_permit);
    sessions.spawn(async move {
        let _ = stream.set_nodelay(true);
        tracing::info!(target: NET, %peer, "dormant WSS compatibility TCP accepted");
        let tls_stream =
            match tokio::time::timeout(TCP_TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream))
            .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                tracing::warn!(target: NET, %peer, %error, "dormant compatibility TLS handshake failed");
                return;
            }
            Err(_) => {
                tracing::warn!(
                    target: NET,
                    %peer,
                    "dormant compatibility TCP/TLS handshake timed out"
                );
                return;
            }
        };
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(MAX_INBOUND_MESSAGE);
        ws_config.max_frame_size = Some(MAX_INBOUND_MESSAGE);
        let websocket = match tokio::time::timeout(
            WEBSOCKET_UPGRADE_TIMEOUT,
            accept_async_with_config(tls_stream, Some(ws_config)),
        )
        .await
        {
            Ok(Ok(websocket)) => websocket,
            Ok(Err(error)) => {
                tracing::warn!(target: NET, %peer, %error, "dormant compatibility WebSocket handshake failed");
                return;
            }
            Err(_) => {
                tracing::warn!(target: NET, %peer, "dormant compatibility WebSocket upgrade timed out");
                return;
            }
        };
        let host_identity = match connection_tls.host_identity() {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(
                    target: NET,
                    %peer,
                    reason_class = error.reason_class(),
                    "stable TLS host identity unavailable after dormant compatibility handshake"
                );
                return;
            }
        };
        session::run(
            crate::resume::DirectSessionSocket::wss(websocket),
            config,
            disclaimer,
            peer.to_string(),
            preauth_guard,
            agent_lease,
            cp_coordinator,
            timezone_controller,
            profile,
            emitter,
            resume_registry,
            host_identity,
            session_shutdown,
        )
        .await;
    });
}

pub(crate) async fn run_host<F, S>(
    args: Args,
    log_controller: logging::LogController,
    shutdown: F,
    on_started: S,
    emitter: LifecycleEmitter,
) -> Result<(), String>
where
    F: std::future::Future<Output = ()>,
    S: FnOnce(),
{
    // Reconcile before TLS or any other fallible listener initialization. Service
    // entry also performs this hook before configuration parsing.
    let timezone_controller = Arc::new(timezone::TimezoneController::startup());
    let resume_registry = resume::ResumeRegistry::new()
        .map_err(|_| "initialize process-local resume registry".to_string())?;
    tracing::info!(
        target: NET,
        config = args
            .config_path
            .as_ref()
            .map_or_else(|| "<cli/defaults>".to_string(), |path| path.display().to_string()),
        output_selector = ?args.config.output_selector,
        "resolved Windows Pier settings"
    );
    if args.retention_was_clamped {
        tracing::warn!(
            target: NET,
            retention_days = args.log_policy.retention_days(),
            "configured log retention was clamped to the supported range"
        );
    }
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        tracing::debug!(target: NET, "rustls crypto provider already installed");
    }
    let tls_lifecycle = Arc::new(
        tls::TlsLifecycle::load(args.tls.clone())
            .map_err(|error| format!("tls configuration failed: {error}"))?,
    );
    tls_lifecycle.emit_startup(&emitter);
    let quic_bind = quic::resolve_bind_addr(&args.host, args.direct_quic_port()).await?;
    let quic_config = quic::build_quic_server_config(&tls_lifecycle)?;
    let quic_endpoint = quic::bind_endpoint(quic_bind, quic_config)?;
    tracing::info!(
        target: NET,
        addr = %quic_bind,
        version = env!("CARGO_PKG_VERSION"),
        license = "AGPL-3.0-only",
        source = SOURCE_URL,
        capenc = %args.config.capenc_bin,
        codec = args.config.codec_name(),
        chroma = args.config.chroma_name(),
        fps = args.config.fps,
        audio = args.config.audio_enabled,
        audio_compressed = args.config.audio_compressed,
        microphone_input = args.config.microphone_input_enabled,
        protocol = arcen_protocol::PROTOCOL_VERSION,
        "QUIC direct-session endpoint listening"
    );

    #[cfg(feature = "wss-compat")]
    let compatibility_listener = if let Some(wss_port) = args.wss_port {
        let bind = format!("{}:{wss_port}", args.host);
        let listener = TcpListener::bind(&bind)
            .await
            .map_err(|error| format!("bind dormant WSS compatibility {bind}: {error}"))?;
        tracing::warn!(
            target: NET,
            %bind,
            "dormant WSS compatibility listener enabled"
        );
        Some(listener)
    } else {
        None
    };
    #[cfg(not(feature = "wss-compat"))]
    let compatibility_listener = ();
    let preauth_slots = Arc::new(Semaphore::new(PREAUTH_CAPACITY));
    let agent_lease = Arc::new(session::BrokerAgentLease::new(
        args.profile,
        args.config.qos_targets,
    ));
    // Start the SYSTEM-only Credential Provider control pipe before accepting any
    // remote client, so a CP loaded by LogonUI can connect and be ready to serve
    // a first-login the moment one is requested.
    let cp_coordinator = cp_pipe::CpCoordinator::start(cp_pipe::CpCoordinatorConfig {
        session_timeout: args.first_login_timeout,
        ..cp_pipe::CpCoordinatorConfig::default()
    });
    let (session_shutdown, _) = watch::channel(false);
    let mut sessions = JoinSet::new();
    tokio::pin!(shutdown);
    run_log_maintenance(args.log_policy, &agent_lease).await;
    let mut log_maintenance_interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
    log_maintenance_interval.tick().await;
    let mut tls_health_interval = tokio::time::interval(Duration::from_secs(60));
    tls_health_interval.tick().await;
    on_started();
    emit_effective_profile(
        &emitter,
        args.profile,
        profile_source_name(args.profile_source),
    );

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                break;
            }
            request = service::next_control_request() => {
                match request {
                    service::ServiceControlRequest::TemporaryDebug => {
                        match log_controller.reload_profile(
                            arcen_telemetry::OperationalProfile::Debug,
                        ) {
                            Ok(()) => {
                                agent_lease.request_profile(
                                    arcen_telemetry::OperationalProfile::Debug,
                                    agent_lease.current_qos_targets(),
                                    false,
                                );
                                emit_effective_profile(
                                    &emitter,
                                    arcen_telemetry::OperationalProfile::Debug,
                                    "scm_temporary_debug",
                                );
                                tracing::info!(
                                    target: NET,
                                    "SCM temporary debug logging enabled"
                                );
                            }
                            Err(error) => tracing::warn!(
                                target: NET,
                                %error,
                                "SCM temporary debug logging failed"
                            ),
                        }
                    }
                    service::ServiceControlRequest::ReloadConfigured => {
                        match reload_configured_profile(&args) {
                            Ok((profile, source, qos_targets)) => {
                                match log_controller.reload_configured(profile) {
                                Ok(()) => {
                                    agent_lease.request_profile(profile, qos_targets, true);
                                    emit_effective_profile(
                                        &emitter,
                                        profile,
                                        profile_source_name(source),
                                    );
                                    tracing::info!(
                                        target: NET,
                                        "SCM configured logging reloaded"
                                    );
                                }
                                Err(error) => tracing::warn!(
                                    target: NET,
                                    %error,
                                    "SCM configured logging reload failed; last good filter retained"
                                ),
                                }
                            }
                            Err(error) => tracing::warn!(
                                target: NET,
                                %error,
                                "SCM configured logging reload failed; last good filter retained"
                            ),
                        }
                    }
                    service::ServiceControlRequest::ReloadTls => {
                        let lifecycle = Arc::clone(&tls_lifecycle);
                        let reload_emitter = emitter.clone();
                        match tokio::task::spawn_blocking(move || {
                            lifecycle.reload(&reload_emitter)
                        }).await {
                            Ok(Ok(())) => tracing::info!(
                                target: NET,
                                "SCM TLS certificate reload completed"
                            ),
                            Ok(Err(error)) => tracing::warn!(
                                target: NET,
                                %error,
                                reason_class = error.reason_class(),
                                "SCM TLS certificate reload failed; last good certificate retained"
                            ),
                            Err(error) => tracing::warn!(
                                target: NET,
                                %error,
                                "SCM TLS certificate reload task failed; last good certificate retained"
                            ),
                        }
                    }
                }
                continue;
            }
            _ = tls_health_interval.tick() => {
                tls_lifecycle.check_and_emit_status(&emitter);
                emit_service_health_snapshot(&emitter);
                continue;
            }
            _ = log_maintenance_interval.tick() => {
                run_log_maintenance(args.log_policy, &agent_lease).await;
                continue;
            }
            Some(result) = sessions.join_next(), if !sessions.is_empty() => {
                if let Err(error) = result {
                    tracing::error!(target: NET, %error, "session task panicked or was cancelled");
                }
                continue;
            }
            // QUIC accept arm — launch the same authenticated session path
            // through one raw WebSocket-framed QUIC stream.
            incoming = quic_endpoint.accept() => {
                if let Some(incoming) = incoming {
                    let peer = incoming.remote_address();
                    let preauth_permit = match preauth_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            incoming.refuse();
                            continue;
                        }
                    };
                    let preauth_guard = auth::PreauthGuard::new(preauth_permit);
                    let connection_tls = Arc::clone(&tls_lifecycle);
                    let config = args.config.clone();
                    let disclaimer = args.disclaimer.clone();
                    let profile = agent_lease.current_profile();
                    let session_agent_lease = Arc::clone(&agent_lease);
                    let cp_coordinator = Arc::clone(&cp_coordinator);
                    let timezone_controller = Arc::clone(&timezone_controller);
                    let emitter = emitter.clone();
                    let resume_registry = Arc::clone(&resume_registry);
                    let session_shutdown = session_shutdown.subscribe();
                    sessions.spawn(async move {
                        let selected_host_identity = match connection_tls.host_identity() {
                            Ok(identity) => identity,
                            Err(error) => {
                                tracing::warn!(
                                    target: NET,
                                    %peer,
                                    reason_class = error.reason_class(),
                                    "stable TLS host identity unavailable before QUIC handshake"
                                );
                                return;
                            }
                        };
                        let connection = match tokio::time::timeout(
                            TCP_TLS_HANDSHAKE_TIMEOUT,
                            incoming,
                        )
                        .await
                        {
                            Ok(Ok(connection)) => connection,
                            Ok(Err(error)) => {
                                tracing::debug!(target: NET, %peer, %error, "QUIC TLS handshake failed");
                                return;
                            }
                            Err(_) => {
                                tracing::debug!(target: NET, %peer, "QUIC TLS handshake timed out");
                                return;
                            }
                        };
                        let host_identity = match connection_tls.host_identity() {
                            Ok(identity) if identity == selected_host_identity => identity,
                            Ok(_) => {
                                connection.close(
                                    0_u32.into(),
                                    b"TLS host identity changed during handshake",
                                );
                                tracing::warn!(
                                    target: NET,
                                    %peer,
                                    "TLS host identity changed during QUIC handshake"
                                );
                                return;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    target: NET,
                                    %peer,
                                    reason_class = error.reason_class(),
                                    "stable TLS host identity unavailable after QUIC handshake"
                                );
                                return;
                            }
                        };
                        let stream = match tokio::time::timeout(
                            WEBSOCKET_UPGRADE_TIMEOUT,
                            arcen_transport::quic::accept_direct(connection),
                        )
                        .await
                        {
                            Ok(Ok(stream)) => stream,
                            Ok(Err(error)) => {
                                tracing::debug!(target: NET, %peer, %error, "QUIC direct stream failed");
                                return;
                            }
                            Err(_) => {
                                tracing::debug!(target: NET, %peer, "QUIC direct stream timed out");
                                return;
                            }
                        };
                        let feedback = stream.feedback_snapshot();
                        tracing::debug!(
                            target: NET,
                            %peer,
                            rtt_us = u64::try_from(feedback.rtt.as_micros()).unwrap_or(u64::MAX),
                            current_mtu = feedback.current_mtu,
                            congestion_window_bytes = feedback.congestion_window,
                            "QUIC path established"
                        );
                        let mut ws_config = WebSocketConfig::default();
                        ws_config.max_message_size = Some(MAX_INBOUND_MESSAGE);
                        ws_config.max_frame_size = Some(MAX_INBOUND_MESSAGE);
                        let websocket = tokio_tungstenite::WebSocketStream::from_raw_socket(
                            stream,
                            Role::Server,
                            Some(ws_config),
                        )
                        .await;
                        tracing::info!(target: NET, %peer, "QUIC session stream ready");
                        session::run(
                            crate::resume::DirectSessionSocket::quic(websocket),
                            config,
                            disclaimer,
                            peer.to_string(),
                            preauth_guard,
                            session_agent_lease,
                            cp_coordinator,
                            timezone_controller,
                            profile,
                            emitter,
                            resume_registry,
                            host_identity,
                            session_shutdown,
                        )
                        .await;
                    });
                } else {
                    tracing::error!(
                        target: crate::logging::NET,
                        "mandatory QUIC endpoint closed"
                    );
                    break;
                }
                continue;
            }
            accepted = accept_compatibility(&compatibility_listener) => {
                #[cfg(feature = "wss-compat")]
                match accepted {
                    Ok((stream, peer)) => spawn_compatibility_session(
                        stream,
                        peer,
                        &args,
                        &tls_lifecycle,
                        &agent_lease,
                        &cp_coordinator,
                        &timezone_controller,
                        &emitter,
                        &resume_registry,
                        &session_shutdown,
                        &preauth_slots,
                        &mut sessions,
                    ),
                    Err(error) => {
                        tracing::warn!(
                            target: NET,
                            %error,
                            "dormant WSS compatibility accept failed"
                        );
                    }
                }
                #[cfg(not(feature = "wss-compat"))]
                let _ = accepted;
                continue;
            },
        }
    }

    quic_endpoint.close(0u32.into(), b"host shutdown");
    quic_endpoint.wait_idle().await;
    let _ = session_shutdown.send(true);
    if let Err(error) = resume_registry.shutdown() {
        tracing::error!(target: NET, ?error, "resume registry shutdown failed closed");
    }
    let shutdown_deadline = tokio::time::Instant::now() + SESSION_SHUTDOWN_TIMEOUT;
    while !sessions.is_empty() {
        match tokio::time::timeout_at(shutdown_deadline, sessions.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(error))) => {
                tracing::debug!(target: NET, %error, "session stopped during shutdown");
            }
            Ok(None) => break,
            Err(_) => {
                tracing::warn!(
                    target: NET,
                    "session cleanup exceeded shutdown deadline; cancelling remaining tasks"
                );
                sessions.abort_all();
                while let Some(result) = sessions.join_next().await {
                    if let Err(error) = result {
                        tracing::debug!(target: NET, %error, "session cancelled during shutdown");
                    }
                }
                break;
            }
        }
    }
    tracing::info!(target: NET, "all sessions stopped; display cleanup completed");
    if let Err(error) = log_controller.flush_log() {
        eprintln!("host observability flush failed: {error}");
    }
    Ok(())
}

fn profile_source_name(source: arcen_session::pier_config::LoggingProfileSource) -> &'static str {
    match source {
        arcen_session::pier_config::LoggingProfileSource::Level => "logging_level",
        arcen_session::pier_config::LoggingProfileSource::LegacyVerbosity => "legacy_verbosity",
        arcen_session::pier_config::LoggingProfileSource::ProductionDefault => "production_default",
    }
}

fn emit_effective_profile(
    emitter: &LifecycleEmitter,
    profile: arcen_telemetry::OperationalProfile,
    source: &'static str,
) {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "profile_level",
        arcen_telemetry::FieldValue::Integer(i64::from(u8::from(profile))),
    );
    let _ = fields.insert(
        "profile_name",
        arcen_telemetry::FieldValue::String(profile.as_str().to_owned()),
    );
    let _ = fields.insert(
        "profile_source",
        arcen_telemetry::FieldValue::String(source.to_owned()),
    );
    emit_lifecycle_event(
        emitter,
        arcen_telemetry::LifecycleEventKind::EffectiveProfile,
        random_correlation_id(),
        fields,
    );
}

fn emit_service_health_snapshot(emitter: &LifecycleEmitter) {
    let sid = random_correlation_id();
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "overall_state",
        arcen_telemetry::FieldValue::String("unavailable".to_owned()),
    );
    emit_lifecycle_event(
        emitter,
        arcen_telemetry::LifecycleEventKind::HealthSnapshot,
        sid.clone(),
        fields,
    );
    emitter.emit_drop_notices(arcen_observability::LifecycleContext {
        sid,
        user: None,
        host: std::env::var("COMPUTERNAME").ok(),
        peer_addr: None,
        health_state: None,
    });
}

fn random_correlation_id() -> arcen_telemetry::CorrelationId {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_ok() {
        arcen_telemetry::CorrelationId::from_uuid_v4_bytes(bytes)
    } else {
        arcen_telemetry::CorrelationId::parse_uuid("00000000-0000-4000-8000-000000000000")
            .expect("fixed fallback value is a canonical UUID")
    }
}

async fn initialize_tls_before_bind<T, U, Load, Ready, Bind, BindFuture>(
    load: Load,
    ready: Ready,
    bind: Bind,
) -> Result<(T, U), String>
where
    Load: FnOnce() -> Result<T, String>,
    Ready: FnOnce(&T),
    Bind: FnOnce() -> BindFuture,
    BindFuture: std::future::Future<Output = Result<U, String>>,
{
    let tls = load()?;
    ready(&tls);
    let listener = bind().await?;
    Ok((tls, listener))
}

async fn run_log_maintenance(
    policy: arcen_telemetry::RetentionPolicy,
    agent_lease: &Arc<session::BrokerAgentLease>,
) {
    #[cfg(windows)]
    {
        let agent_lease = Arc::clone(agent_lease);
        match tokio::task::spawn_blocking(move || log_maintenance::run(policy, &agent_lease)).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                target: NET,
                %error,
                "Windows log maintenance failed; active sessions continue"
            ),
            Err(error) => tracing::warn!(
                target: NET,
                %error,
                "Windows log maintenance task failed; active sessions continue"
            ),
        }
    }
    #[cfg(not(windows))]
    let _ = (policy, agent_lease);
}

#[cfg(windows)]
async fn run_session_agent(
    read_handle: isize,
    write_handle: isize,
    iddcx_control_handle: Option<isize>,
    session_log_id: arcen_telemetry::CorrelationId,
    log_controller: logging::LogController,
) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::protocol::Role;

    let emitter = eventlog::LifecycleEmitter::init_process_local(log_controller.handle());
    if let Some(handle) = iddcx_control_handle {
        iddcx::install_inherited_control_handle(handle)?;
    }
    let stream = ipc::PipeStream::from_inherited_handles(read_handle, write_handle)?;
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(64 * 1024 * 1024);
    config.max_frame_size = Some(64 * 1024 * 1024);
    let websocket = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    session::run_agent(websocket, session_log_id, log_controller, emitter).await
}

#[cfg(not(windows))]
async fn run_session_agent(
    _read_handle: isize,
    _write_handle: isize,
    _iddcx_control_handle: Option<isize>,
    _session_log_id: arcen_telemetry::CorrelationId,
    _log_controller: logging::LogController,
) -> Result<(), String> {
    Err("session-agent is only available on Windows".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_args_error(result: Result<Args, String>) -> String {
        match result {
            Ok(_) => panic!("argument parsing unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    #[test]
    fn host_arguments_can_be_parsed_after_the_service_subcommand() {
        let args = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                r"C:\ProgramData\Arcen\tls\host.crt",
                "--tls-key",
                r"C:\ProgramData\Arcen\tls\host.key",
                "--port",
                "18444",
                "--capenc-bin",
                r"C:\Program Files\Arcen\Pier\arcen-capenc.exe",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("service arguments");
        assert_eq!(args.port, 18444);
        assert_eq!(args.direct_quic_port(), 18444);
        assert_eq!(args.first_login_timeout, Duration::from_secs(5 * 60));
        assert_ne!(
            args.config.capenc_bin,
            r"C:\Program Files\Arcen\Pier\arcen-capenc.exe"
        );
        assert!(args.config_disabled);
        assert!(!args.config.timezone_redirection);
        assert_eq!(
            reload_configured_profile(&args),
            Ok((
                arcen_telemetry::OperationalProfile::Critical,
                arcen_session::pier_config::LoggingProfileSource::ProductionDefault,
                arcen_telemetry::QosTargets::default(),
            ))
        );
    }

    #[test]
    fn audio_compression_and_microphone_policy_are_explicit() {
        let parse = |extra: &[&str]| {
            let mut arguments = vec![
                "--no-config".to_string(),
                "--tls-cert".to_string(),
                "host.crt".to_string(),
                "--tls-key".to_string(),
                "host.key".to_string(),
            ];
            arguments.extend(extra.iter().map(|value| (*value).to_string()));
            parse_args_from(arguments.into_iter()).expect("microphone arguments")
        };
        let defaults = parse(&[]);
        assert_eq!(defaults.direct_quic_port(), 18_444);
        assert!(!defaults.config.audio_compressed);
        assert!(!defaults.config.microphone_input_enabled);
        let configured = parse(&["--audio-compressed", "--microphone-input"]);
        assert!(configured.config.audio_compressed);
        assert!(configured.config.microphone_input_enabled);
    }

    #[test]
    fn clipboard_policy_defaults_and_cli_overrides_are_strict() {
        let args = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--clipboard-direction",
                "host_to_client",
                "--clipboard-content",
                "text",
                "--clipboard-max-bytes",
                "1048576",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("clipboard overrides");
        assert_eq!(
            args.config.clipboard_policy.direction,
            arcen_media::clipboard::ClipboardDirection::HostToClient
        );
        assert_eq!(
            args.config.clipboard_policy.content,
            arcen_media::clipboard::ClipboardContent::Text
        );
        assert_eq!(args.config.clipboard_policy.max_bytes, 1024 * 1024);

        assert!(validated_clipboard_policy(
            arcen_media::clipboard::ClipboardDirection::Both,
            arcen_media::clipboard::ClipboardContent::All,
            1024 * 1024 - 1
        )
        .is_err());
        assert!(validated_clipboard_policy(
            arcen_media::clipboard::ClipboardDirection::Both,
            arcen_media::clipboard::ClipboardContent::All,
            arcen_media::clipboard::HARD_MAX_CLIPBOARD_BYTES + 1
        )
        .is_err());
    }

    #[test]
    fn preauth_clipboard_frame_memory_is_bounded_for_sixteen_connections() {
        assert_eq!(
            MAX_INBOUND_MESSAGE,
            arcen_protocol::CLIPBOARD_HEADER_SIZE + arcen_protocol::CHUNK_BYTES
        );
        assert_eq!(
            PREAUTH_CAPACITY * MAX_INBOUND_MESSAGE,
            16 * (arcen_protocol::CHUNK_BYTES + arcen_protocol::CLIPBOARD_HEADER_SIZE)
        );
    }

    #[test]
    fn direct_transport_rejects_zero_quic_port() {
        let result = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--port",
                "0",
            ]
            .into_iter()
            .map(str::to_string),
        );
        assert!(matches!(
            result,
            Err(error) if error == "direct QUIC requires a nonzero UDP port"
        ));
    }

    #[test]
    fn timezone_redirection_cli_uses_last_override_and_defaults_off() {
        let parse = |flags: &[&str]| {
            let mut arguments = vec![
                "--no-config".to_string(),
                "--tls-cert".to_string(),
                "host.crt".to_string(),
                "--tls-key".to_string(),
                "host.key".to_string(),
            ];
            arguments.extend(flags.iter().map(|value| (*value).to_string()));
            parse_args_from(arguments.into_iter()).expect("timezone arguments")
        };
        assert!(!parse(&[]).config.timezone_redirection);
        assert!(
            parse(&["--timezone-redirection"])
                .config
                .timezone_redirection
        );
        assert!(
            !parse(&["--timezone-redirection", "--no-timezone-redirection"])
                .config
                .timezone_redirection
        );
        assert!(
            parse(&["--no-timezone-redirection", "--timezone-redirection"])
                .config
                .timezone_redirection
        );
    }

    #[test]
    fn first_login_timeout_is_bounded() {
        let base = |seconds: &str| {
            parse_args_from(
                [
                    "--no-config",
                    "--tls-cert",
                    "host.crt",
                    "--tls-key",
                    "host.key",
                    "--first-login-timeout-secs",
                    seconds,
                ]
                .into_iter()
                .map(str::to_string),
            )
        };
        assert!(base("29").is_err());
        assert_eq!(
            base("600").expect("bounded timeout").first_login_timeout,
            Duration::from_secs(600)
        );
        assert!(base("1801").is_err());
    }

    #[test]
    fn json_settings_load_before_cli_overrides() {
        let root =
            std::env::temp_dir().join(format!("arcen-pier-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("config dir");
        let path = root.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "listen":{"port":18443,"quic_port":18444},
                "tls":{"cert":"host.crt","key":"host.key"},
                "capture":{"binary":"arcen-capenc.exe"},
                "video":{"codec":"h265","chroma":"yuv444","fps":60},
                "audio":{"enabled":false,"compressed":false},
                "microphone_input":{"enabled":false},
                "redirection":{"timezone":true},
                "platform":{"desktop":{"adapter":"NVIDIA GRID V100D-16Q","output":0}}
            }"#,
        )
        .expect("write config");

        let args = parse_args_from(
            [
                "--config",
                path.to_str().expect("config path"),
                "--adapter-output-index",
                "1",
                "--audio",
                "--audio-compressed",
                "--no-timezone-redirection",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("merged config");
        assert_eq!(args.port, 18_443);
        assert_eq!(args.quic_port, Some(18_444));
        assert_eq!(args.direct_quic_port(), 18_444);
        assert!(args.config.audio_enabled);
        assert!(args.config.audio_compressed);
        assert!(!args.config.timezone_redirection);
        assert_eq!(args.config.codec, VideoCodec::H265);
        assert_eq!(
            args.config.output_selector,
            display::OutputSelector::Adapter {
                name: "NVIDIA GRID V100D-16Q".to_string(),
                output_index: 1,
            }
        );
        assert_eq!(
            args.tls.certificate_path.to_string_lossy(),
            root.join("host.crt").to_string_lossy().as_ref()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The reserved-GPU invariant `allowed_adapters` documents was only ever
    /// honoured for the empty case, which inherits the desktop adapter. A
    /// non-empty list naming another GPU passed in silence — exactly how
    /// pier-windows.example.internal encoded a remote session on an RTX6000 documented as
    /// reserved for other work while `platform.desktop.adapter` said V100D.
    #[test]
    fn adapters_beyond_the_desktop_gpu_are_identified_for_the_operator_warning() {
        let v100 = "NVIDIA GRID V100D-16Q";

        assert_eq!(
            adapters_beyond_desktop(
                &[
                    "NVIDIA GRID RTX6000-8Q".to_string(),
                    "NVIDIA GRID V100D-16Q".to_string(),
                ],
                v100,
            ),
            vec!["NVIDIA GRID RTX6000-8Q".to_string()],
            "the borrowed GPU must be named so the decision is attributable",
        );

        assert!(
            adapters_beyond_desktop(&["NVIDIA GRID V100D-16Q".to_string()], v100).is_empty(),
            "a host pinned to its own desktop adapter must stay silent",
        );

        assert!(
            adapters_beyond_desktop(&["nvidia grid v100d-16q".to_string()], v100).is_empty(),
            "adapter descriptions match case-insensitively everywhere else in this host, \
             so case alone must not be reported as a different GPU",
        );
    }

    #[test]
    fn nvidia_headless_requires_one_explicit_streaming_adapter() {
        let root = std::env::temp_dir().join(format!(
            "arcen-pier-nvidia-headless-config-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("config dir");
        let path = root.join("pier.json");
        let write = |adapters: &str, iddcx: bool| {
            std::fs::write(
                &path,
                format!(
                    r#"{{
                        "tls":{{"cert":"host.crt","key":"host.key"}},
                        "audio":{{"enabled":true,"compressed":false}},
                        "microphone_input":{{"enabled":false}},
                        "platform":{{
                            "iddcx":{{"enabled":{iddcx},"render_adapter":{{"stable_id":"gpu"}}}},
                            "multi_monitor":{{
                                "advertise_enabled":true,
                                "allowed_adapters":{adapters},
                                "nvidia_headless_enabled":true
                            }}
                        }}
                    }}"#
                ),
            )
            .expect("write config");
            parse_args_from(
                ["--config", path.to_str().expect("config path")]
                    .into_iter()
                    .map(str::to_string),
            )
        };
        let args = write(r#"["NVIDIA GRID V100D-16Q"]"#, false).expect("one streaming adapter");
        assert!(args.config.multi_monitor.nvidia_headless_enabled);
        assert_eq!(
            args.config.multi_monitor.allowed_adapters,
            ["NVIDIA GRID V100D-16Q"]
        );
        assert!(write(
            r#"["NVIDIA GRID V100D-16Q","NVIDIA GRID RTX6000-8Q"]"#,
            false
        )
        .is_err());
        assert!(write(r#"["NVIDIA GRID V100D-16Q"]"#, true).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disclaimer_is_loaded_from_a_config_relative_directory() {
        let root =
            std::env::temp_dir().join(format!("arcen-pier-disclaimer-test-{}", std::process::id()));
        let disclaimer_dir = root.join("disclaimers");
        std::fs::create_dir_all(&disclaimer_dir).expect("disclaimer dir");
        std::fs::write(disclaimer_dir.join("en_US.txt"), b"Authorized use only.")
            .expect("write disclaimer");
        let path = root.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "tls":{"cert":"host.crt","key":"host.key"},
                "auth":{"disclaimer":{
                    "enabled":true,
                    "locale":"en_US",
                    "directory":"disclaimers"
                }},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{}
            }"#,
        )
        .expect("write config");

        let args = parse_args_from(
            ["--config", path.to_str().expect("config path")]
                .into_iter()
                .map(str::to_string),
        )
        .expect("configured disclaimer");
        let disclaimer = args.disclaimer.expect("prepared disclaimer");
        assert_eq!(disclaimer.text(), "Authorized use only.");
        assert_eq!(disclaimer.locale().as_str(), "en_US");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enabled_disclaimer_rejects_missing_or_invalid_content() {
        let root = std::env::temp_dir().join(format!(
            "arcen-pier-invalid-disclaimer-test-{}",
            std::process::id()
        ));
        let disclaimer_dir = root.join("disclaimers");
        std::fs::create_dir_all(&disclaimer_dir).expect("disclaimer dir");
        let path = root.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "tls":{"cert":"host.crt","key":"host.key"},
                "auth":{"disclaimer":{"enabled":true,"directory":"disclaimers"}},
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{}
            }"#,
        )
        .expect("write config");

        let parse = || {
            parse_args_from(
                ["--config", path.to_str().expect("config path")]
                    .into_iter()
                    .map(str::to_string),
            )
        };
        assert!(parse().is_err());
        std::fs::write(disclaimer_dir.join("en_US.txt"), []).expect("empty disclaimer");
        assert!(parse().is_err());
        std::fs::write(disclaimer_dir.join("en_US.txt"), [0xff]).expect("invalid disclaimer");
        assert!(parse().is_err());
        std::fs::write(
            disclaimer_dir.join("en_US.txt"),
            vec![b'x'; arcen_identity::MAX_DISCLAIMER_CONTENT_BYTES + 1],
        )
        .expect("oversized disclaimer");
        assert!(parse().is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn global_and_adapter_cli_selectors_are_mutually_exclusive() {
        let result = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--output-index",
                "2",
                "--adapter-name",
                "NVIDIA GRID V100D-16Q",
            ]
            .into_iter()
            .map(str::to_string),
        );
        assert!(result.is_err());
    }

    #[test]
    fn mf_software_encoder_requires_h264_yuv420() {
        let parse = |codec: &str, chroma: &str| {
            parse_args_from(
                [
                    "--no-config",
                    "--tls-cert",
                    "host.crt",
                    "--tls-key",
                    "host.key",
                    "--encoder",
                    "software-h264",
                    "--codec",
                    codec,
                    "--chroma",
                    chroma,
                ]
                .into_iter()
                .map(str::to_string),
            )
        };

        assert!(parse("h264", "yuv420").is_ok());
        assert!(parse("h265", "yuv420").is_err());
        assert!(parse("h265", "yuv444").is_err());
    }

    #[test]
    fn encoder_selection_is_limited_to_shipped_backends() {
        assert!(parse_encoder("mf").is_err());
        assert!(parse_encoder("media-foundation").is_err());
        assert_eq!(
            parse_encoder("auto").expect("auto"),
            crate::capenc::EncoderSelection::Auto
        );
        assert_eq!(
            parse_encoder("openh264").expect("OpenH264 alias"),
            crate::capenc::EncoderSelection::SoftwareH264
        );
    }

    #[test]
    fn color_keys_are_parsed_from_cli_flags() {
        let args = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--bit-depth",
                "10",
                "--color-range",
                "full",
                "--color-matrix",
                "bt2020ncl",
                "--color-policy",
                "always-on",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("colour flags");
        assert_eq!(args.config.bit_depth, BitDepth::Ten);
        assert_eq!(args.config.color_range, ColorRange::Full);
        assert_eq!(args.config.color_matrix, ColorMatrix::Bt2020Ncl);
        assert_eq!(args.config.color_policy, ColorPolicy::AlwaysOn);
    }

    #[test]
    fn color_defaults_do_not_change_existing_behaviour() {
        let args = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("defaults");
        assert_eq!(args.config.bit_depth, BitDepth::Eight);
        assert_eq!(args.config.color_range, ColorRange::Limited);
        assert_eq!(args.config.color_matrix, ColorMatrix::Bt709);
        assert_eq!(args.config.color_policy, ColorPolicy::DefaultOff);
    }

    #[test]
    fn bad_color_values_are_rejected_not_silently_defaulted() {
        let parse_with = |flag: &str, value: &str| {
            parse_args_from(
                [
                    "--no-config",
                    "--tls-cert",
                    "host.crt",
                    "--tls-key",
                    "host.key",
                    flag,
                    value,
                ]
                .into_iter()
                .map(str::to_string),
            )
        };
        let bit_depth_error = expect_args_error(parse_with("--bit-depth", "9"));
        assert!(
            bit_depth_error.contains('9'),
            "error must name the offending value: {bit_depth_error}"
        );
        let range_error = expect_args_error(parse_with("--color-range", "studio"));
        assert!(
            range_error.contains("studio"),
            "error must name the offending value: {range_error}"
        );
        let matrix_error = expect_args_error(parse_with("--color-matrix", "bt2020c"));
        assert!(
            matrix_error.contains("bt2020c"),
            "error must name the offending value: {matrix_error}"
        );
        let policy_error = expect_args_error(parse_with("--color-policy", "sometimes"));
        assert!(
            policy_error.contains("sometimes"),
            "error must name the offending value: {policy_error}"
        );
    }

    #[test]
    fn variant_overrides_individual_keys() {
        let args = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--codec",
                "h264",
                "--chroma",
                "yuv420",
                "--bit-depth",
                "8",
                "--variant",
                "hevc-444-10-full-bt709",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("variant overrides");
        assert_eq!(args.config.codec, VideoCodec::H265);
        assert_eq!(args.config.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(args.config.bit_depth, BitDepth::Ten);
        assert_eq!(args.config.color_range, ColorRange::Full);
        assert_eq!(args.config.color_matrix, ColorMatrix::Bt709);
        assert_eq!(args.config.color_policy, ColorPolicy::AlwaysOn);
        assert!(args.config.variant_pinned);
    }

    #[test]
    fn variant_rejects_an_unknown_id() {
        let error = expect_args_error(parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--variant",
                "hevc-444-10",
            ]
            .into_iter()
            .map(str::to_string),
        ));
        assert!(
            error.contains("hevc-444-10"),
            "error must name the offending variant id: {error}"
        );
    }

    #[test]
    fn variant_accepts_av1_420_but_rejects_unwired_chroma_without_partial_apply() {
        let parse_variant = |id: &str| {
            parse_args_from(
                [
                    "--no-config",
                    "--tls-cert",
                    "host.crt",
                    "--tls-key",
                    "host.key",
                    "--variant",
                    id,
                ]
                .into_iter()
                .map(str::to_string),
            )
        };
        let av1 = parse_variant("av1-420-8-full-bt709").expect("AV1 Main is wired");
        assert_eq!(av1.config.codec, VideoCodec::Av1);
        assert_eq!(av1.config.chroma, ChromaSubsampling::Yuv420);

        // AV1 4:4:4 is coherent in the software tier, but Windows NVENC
        // exposes Main-profile 4:2:0 only.
        let av1_error = expect_args_error(parse_variant("av1-444-10-full-bt709"));
        assert!(
            av1_error.contains("av1"),
            "error must name av1: {av1_error}"
        );

        // 4:2:2 is a probe-only row (Blackwell NVENC), not a shipped chroma.
        let chroma_error = expect_args_error(parse_variant("hevc-422-10-full-bt709"));
        assert!(
            chroma_error.contains("yuv422"),
            "error must name yuv422: {chroma_error}"
        );

        // A rejected variant must not half-apply: codec/chroma/bit_depth stay
        // exactly as configured before the attempt.
        let mut codec = VideoCodec::H264;
        let mut chroma = ChromaSubsampling::Yuv420;
        let mut bit_depth = BitDepth::Eight;
        let mut color_range = ColorRange::Limited;
        let mut color_matrix = ColorMatrix::Bt709;
        assert!(apply_variant(
            "hevc-422-10-full-bt709",
            &mut codec,
            &mut chroma,
            &mut bit_depth,
            &mut color_range,
            &mut color_matrix,
        )
        .is_err());
        assert_eq!(codec, VideoCodec::H264);
        assert_eq!(chroma, ChromaSubsampling::Yuv420);
        assert_eq!(bit_depth, BitDepth::Eight);
    }

    fn initial_video_request(
        selection: VideoSelectionIntent,
    ) -> arcen_protocol::messages::InitialVideoRequestMsg {
        arcen_protocol::messages::InitialVideoRequestMsg {
            quality: arcen_protocol::messages::QualitySettings {
                codec: "h264".to_string(),
                chroma: "yuv420".to_string(),
                bit_depth: "8".to_string(),
                color_range: "limited".to_string(),
                color_matrix: "bt709".to_string(),
                video_selection: selection,
                ..arcen_protocol::messages::QualitySettings::default()
            },
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: true,
                yuv444: true,
                main10: true,
                full_range: true,
                ..arcen_protocol::messages::ClientVideoCapabilitiesMsg::default()
            },
        }
    }

    #[test]
    fn auth_time_adaptive_video_prefers_av1_and_variant_pin_wins() {
        let mut config = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap()
        .config;
        let request = initial_video_request(VideoSelectionIntent::AdaptivePerformance);
        config.apply_initial_video_request(&request).unwrap();
        assert_eq!(config.codec, VideoCodec::Av1);
        assert_eq!(config.chroma, ChromaSubsampling::Yuv420);
        assert_eq!(
            config.video_selection,
            VideoSelectionIntent::AdaptivePerformance
        );
        assert!(config.auth_video_request.is_some());

        let mut pinned = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--variant",
                "hevc-444-10-full-bt709",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap()
        .config;
        pinned.apply_initial_video_request(&request).unwrap();
        assert_eq!(pinned.codec, VideoCodec::H265);
        assert_eq!(pinned.chroma, ChromaSubsampling::Yuv444);
        assert!(pinned.auth_video_request.is_some());
    }

    #[test]
    fn auth_time_quality_and_exact_codec_pin_reach_encoder_creation() {
        let mut request = initial_video_request(VideoSelectionIntent::AdaptivePerformance);
        request.quality.encode_intent = "quality".to_string();
        let mut pinned = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--codec",
                "h265",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap()
        .config;
        assert!(pinned.codec_pinned);
        pinned.apply_initial_video_request(&request).unwrap();
        assert_eq!(pinned.codec, VideoCodec::H265);
        assert_eq!(pinned.video_selection, VideoSelectionIntent::Exact);
        assert_eq!(
            pinned.requested_encode_intent(),
            arcen_media::EncodeIntent::Quality
        );

        let mut incompatible = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--codec",
                "av1",
                "--bit-depth",
                "10",
                "--color-range",
                "full",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap()
        .config;
        let mut grading = initial_video_request(VideoSelectionIntent::ColorFidelity);
        grading.quality.codec = "h265".to_string();
        grading.quality.chroma = "yuv444".to_string();
        grading.quality.bit_depth = "10".to_string();
        grading.quality.color_range = "full".to_string();
        assert!(incompatible.apply_initial_video_request(&grading).is_err());
    }

    #[test]
    fn explicit_openh264_backend_normalizes_unpinned_format_but_respects_exact_pins() {
        let mut software = parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--encoder",
                "software-h264",
                "--bit-depth",
                "10",
                "--fps",
                "60",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .unwrap()
        .config;
        software
            .apply_software_h264_backend(crate::capenc::EncoderSelection::SoftwareH264)
            .unwrap();
        assert_eq!(software.codec, VideoCodec::H264);
        assert_eq!(software.chroma, ChromaSubsampling::Yuv420);
        assert_eq!(software.bit_depth, BitDepth::Eight);
        assert_eq!(software.fps, 30);

        let mut pinned = software;
        pinned.codec = VideoCodec::H265;
        pinned.codec_pinned = true;
        assert!(pinned
            .apply_software_h264_backend(crate::capenc::EncoderSelection::SoftwareH264)
            .is_err());
    }

    #[test]
    fn exact_openh264_variant_rejects_incompatible_fps_during_config_parse() {
        let error = expect_args_error(parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--encoder",
                "software-h264",
                "--variant",
                "h264-420-8-limited-bt709",
                "--fps",
                "60",
            ]
            .into_iter()
            .map(str::to_string),
        ));
        assert!(error.contains("OpenH264"));
        assert!(error.contains("variant"));
    }

    #[test]
    fn twelve_bit_with_explicit_nvenc_is_rejected() {
        let error = expect_args_error(parse_args_from(
            [
                "--no-config",
                "--tls-cert",
                "host.crt",
                "--tls-key",
                "host.key",
                "--bit-depth",
                "12",
                "--encoder",
                "nvenc",
            ]
            .into_iter()
            .map(str::to_string),
        ));
        assert!(
            error.contains("12") && error.contains("nvenc"),
            "error must explain the NVENC/12-bit conflict: {error}"
        );
    }

    #[test]
    fn color_policy_always_on_forces_the_ceiling_regardless_of_client_request() {
        let ceiling = BitDepth::Ten;
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_bit_depth(ceiling, None),
            ceiling
        );
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_bit_depth(ceiling, Some(BitDepth::Eight)),
            ceiling,
            "a client asking for less must be ignored"
        );
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_bit_depth(ceiling, Some(BitDepth::Twelve)),
            ceiling,
            "a client asking for more must be capped at the configured ceiling"
        );
    }

    #[test]
    fn color_policy_always_off_forces_the_conservative_baseline_regardless_of_client_request() {
        let ceiling = BitDepth::Ten;
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_bit_depth(ceiling, None),
            BitDepth::Eight
        );
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_bit_depth(ceiling, Some(BitDepth::Twelve)),
            BitDepth::Eight,
            "a client asking for more must be ignored"
        );
    }

    #[test]
    fn color_policy_default_on_defaults_high_but_honours_an_explicit_client_request() {
        let ceiling = BitDepth::Ten;
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_bit_depth(ceiling, None),
            ceiling
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_bit_depth(ceiling, Some(BitDepth::Eight)),
            BitDepth::Eight
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_bit_depth(ceiling, Some(BitDepth::Twelve)),
            ceiling,
            "a request above the ceiling is capped, not honoured verbatim"
        );
    }

    #[test]
    fn color_policy_default_off_defaults_conservative_but_lets_a_client_negotiate_up() {
        let ceiling = BitDepth::Ten;
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_bit_depth(ceiling, None),
            BitDepth::Eight
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_bit_depth(ceiling, Some(BitDepth::Ten)),
            ceiling
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_bit_depth(ceiling, Some(BitDepth::Twelve)),
            ceiling,
            "a request above the ceiling is still capped"
        );
    }

    #[test]
    fn color_policy_resolves_color_range_with_the_same_ceiling_semantics() {
        let ceiling = ColorRange::Full;
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_color_range(ceiling, None),
            ColorRange::Full
        );
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_color_range(ceiling, Some(ColorRange::Full)),
            ColorRange::Limited
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_range(ceiling, Some(ColorRange::Full)),
            ColorRange::Full
        );
        let limited_ceiling = ColorRange::Limited;
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_range(limited_ceiling, Some(ColorRange::Full)),
            ColorRange::Limited,
            "a limited ceiling must never yield full range"
        );
    }

    #[test]
    fn color_policy_resolves_matrix_without_clamping_a_client_choice() {
        let ceiling = ColorMatrix::Bt2020Ncl;
        assert_eq!(
            ColorPolicy::AlwaysOn.resolve_color_matrix(ceiling, Some(ColorMatrix::Bt601)),
            ceiling
        );
        assert_eq!(
            ColorPolicy::AlwaysOff.resolve_color_matrix(ceiling, Some(ColorMatrix::Bt601)),
            ColorMatrix::Bt709
        );
        assert_eq!(
            ColorPolicy::DefaultOn.resolve_color_matrix(ceiling, Some(ColorMatrix::Bt601)),
            ColorMatrix::Bt601,
            "matrix is not a fidelity axis, so an explicit request is honoured verbatim"
        );
        assert_eq!(
            ColorPolicy::DefaultOff.resolve_color_matrix(ceiling, None),
            ColorMatrix::Bt709
        );
    }

    #[test]
    fn color_policy_tokens_round_trip() {
        for policy in ColorPolicy::ALL {
            assert_eq!(ColorPolicy::from_token(policy.token()), Some(*policy));
        }
        assert_eq!(ColorPolicy::from_token("sometimes"), None);
    }

    #[test]
    fn color_keys_and_variant_are_applied_from_json_config() {
        let root = std::env::temp_dir().join(format!(
            "arcen-pier-color-config-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("config dir");
        let path = root.join("pier.json");
        std::fs::write(
            &path,
            r#"{
                "tls":{"cert":"host.crt","key":"host.key"},
                "video":{
                    "bit_depth":"10",
                    "color_range":"full",
                    "color_matrix":"bt2020ncl",
                    "color_policy":"always-on"
                },
                "audio":{"enabled":true,"compressed":false},
                "microphone_input":{"enabled":false},
                "platform":{}
            }"#,
        )
        .expect("write config");

        let args = parse_args_from(
            ["--config", path.to_str().expect("config path")]
                .into_iter()
                .map(str::to_string),
        )
        .expect("json colour config");
        assert_eq!(args.config.bit_depth, BitDepth::Ten);
        assert_eq!(args.config.color_range, ColorRange::Full);
        assert_eq!(args.config.color_matrix, ColorMatrix::Bt2020Ncl);
        assert_eq!(args.config.color_policy, ColorPolicy::AlwaysOn);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn preauth_capacity_is_bounded_and_released() {
        let slots = Arc::new(Semaphore::new(2));
        let first = slots.clone().try_acquire_owned().unwrap();
        let second = slots.clone().try_acquire_owned().unwrap();
        assert!(slots.clone().try_acquire_owned().is_err());
        drop(first);
        assert!(slots.clone().try_acquire_owned().is_ok());
        drop(second);
    }

    #[tokio::test]
    async fn stalled_transport_deadline_releases_permit() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned().unwrap();
        let stalled = std::future::pending::<()>();
        assert!(tokio::time::timeout(Duration::from_millis(5), stalled)
            .await
            .is_err());
        drop(permit);
        assert!(slots.try_acquire_owned().is_ok());
    }
}
