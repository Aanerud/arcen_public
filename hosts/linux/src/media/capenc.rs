//! Supervises the native `arcen-capenc` child (NvFBC → NVENC). Ports
//! `server/capenc_backend.py`: spawn the same helper in its optional framed-v1
//! output mode, read exact Annex-B access units, forward `IDR\n` keyframe
//! requests on stdin, and
//! surface `[capenc] …` stderr diagnostics. Codec + chroma are FIXED at spawn;
//! a change means respawn (a later stage), matching `encoder_lifecycle.py`.
//!
//! capenc child contract:
//!   argv:   `arcen-capenc <output_index> <codec> [fps] [yuv444] framed-v1`
//!           `encoder=auto|nvenc|software-h264` [`intent=interactive|quality`]
//!           (`output_index` is 0-based; the runtime's 1-based `monitor_index`
//!            maps via `output_index = monitor_index.saturating_sub(1)`)
//!   stdout: repeated `u32_be payload_len || Annex-B AU` records. The optional
//!           mode leaves capenc's default raw stdout unchanged for Python 8443.
//!   stdin:  line `IDR\n` forces a keyframe (cheap — no respawn)
//!   stderr: versioned `READY`/`UNAVAILABLE`, diagnostics, and ~1 Hz stats
//!   env:    `DISPLAY` / `XAUTHORITY` select the X session to capture

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

pub use arcen_media::video::ResolvedMediaPlan;
use arcen_media::video::{
    adaptive_codec_ladder, parse_ready_v1, parse_unavailable_v1, resolve_media_plan_degrading,
    BackendAvailability, BackendCandidate, BackendLimits, EncoderBackend, EncoderRequest,
    MediaRequest, PlanDegradation, ReadyExpectation, VideoVariant,
};
use arcen_media::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent,
    TransferCharacteristics, VideoCodec, VideoConfiguration,
};
use arcen_protocol::messages::{CursorMode, VideoSelectionIntent};
use arcen_telemetry::CorrelationId;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::logging::target;
use crate::session::identity::UserExecution;

use super::annexb::{access_unit_is_keyframe, AccessUnit, NalCodec};

const FRAMED_OUTPUT_V1: &str = "framed-v1";
const FRAME_LENGTH_SIZE: usize = 4;
const MAX_ACCESS_UNIT_SIZE: usize = 16 * 1024 * 1024;
const FRAME_CHANNEL_CAPACITY: usize = 4;
const CAPENC_READY_TIMEOUT: Duration = Duration::from_secs(10);
const CAPENC_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const UNAVAILABLE_PREFIX: &str = "[capenc] UNAVAILABLE ";

pub type EncoderSelection = EncoderRequest;

pub fn parse_encoder_selection(value: &str) -> Result<EncoderSelection, String> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(EncoderRequest::Auto),
        "nvenc" => Ok(EncoderRequest::NativeNvenc),
        "software-h264" => Ok(EncoderRequest::SoftwareH264),
        other => Err(format!(
            "unsupported encoder {other:?}; expected auto|nvenc|software-h264"
        )),
    }
}

#[derive(Debug, Error)]
pub enum CapencStartError {
    #[error("invalid capenc configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to spawn capenc: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("capenc backend unavailable before READY: {0}")]
    BackendUnavailable(String),
    #[error("capenc READY protocol error: {0}")]
    ReadyProtocol(String),
    #[error("capenc exited before READY{0}")]
    ExitedBeforeReady(String),
    #[error("capenc did not emit READY within {}ms", CAPENC_READY_TIMEOUT.as_millis())]
    ReadyTimeout,
}

/// Everything needed to launch one `capenc` child.
#[derive(Debug, Clone)]
pub struct CapencConfig {
    pub binary: PathBuf,
    /// 0-based NvFBC desktop-output index (already `monitor_index - 1`).
    pub output_index: u32,
    /// `"h264"` or `"h265"` (the only Annex-B codecs).
    pub codec: String,
    pub encoder: EncoderSelection,
    pub fps: u32,
    /// True ⇒ append the `yuv444` argv token (High 4:4:4).
    pub yuv444: bool,
    /// Resolved coded component depth to request.
    pub bit_depth: BitDepth,
    /// Resolved coded sample range to request.
    pub color_range: ColorRange,
    /// Resolved matrix coefficients to request.
    pub color_matrix: ColorMatrix,
    /// Whether codec fallback is host-ranked performance or an exact/fidelity
    /// request.
    pub video_selection: VideoSelectionIntent,
    /// An operator explicitly selected `video.codec`; codec substitution is
    /// therefore forbidden even when the encoder backend is automatic.
    pub codec_pinned: bool,
    /// An operator selected one complete `video.variant`; every format axis
    /// is exact and no fallback may rewrite it.
    pub variant_pinned: bool,
    /// Resolved encoder intent to request.
    pub intent: EncodeIntent,
    /// Damage-driven QP biasing to request. Roster-wide; see
    /// `docs/architecture/qp-maps.md`.
    pub qp_map: arcen_media::video::QpMapPolicy,
    /// Exact configured capture width expected in READY.
    pub width: u32,
    /// Exact configured capture height expected in READY.
    pub height: u32,
    /// Cursor authority fixed before the NvFBC session starts.
    pub cursor_mode: CursorMode,
    /// `DISPLAY` for the child (e.g. `":0"`). None ⇒ inherit.
    pub display: Option<String>,
    /// `XAUTHORITY` for the child. None ⇒ inherit.
    pub xauthority: Option<String>,
    /// Authenticated user identity/environment. `None` is development no-auth.
    pub execution: Option<UserExecution>,
    /// Attempt-scoped diagnostic correlation inherited by the helper.
    pub session_log_id: CorrelationId,
}

impl CapencConfig {
    /// Parse the product codec token into the typed vocabulary.
    fn codec_enum(&self) -> Result<VideoCodec, CapencStartError> {
        match self.codec.as_str() {
            "h264" => Ok(VideoCodec::H264),
            "h265" => Ok(VideoCodec::H265),
            "av1" => Ok(VideoCodec::Av1),
            _ => Err(CapencStartError::InvalidConfig(format!(
                "unsupported codec {:?}",
                self.codec
            ))),
        }
    }

    /// The full coded-format contract this config asks capenc for.
    ///
    /// Primaries and transfer are always BT.709: neither is yet an
    /// independently negotiated axis anywhere in this host (every row in
    /// `arcen_media::video::PROBE_MATRIX` shares them), so every resolved
    /// plan states the same values every other coherent row does.
    fn video_configuration(&self, codec: VideoCodec) -> VideoConfiguration {
        VideoConfiguration {
            codec,
            chroma: if self.yuv444 {
                ChromaSubsampling::Yuv444
            } else {
                ChromaSubsampling::Yuv420
            },
            bit_depth: self.bit_depth,
            range: self.color_range,
            matrix: self.color_matrix,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    /// Carry the negotiated single-monitor contract into a later respawn,
    /// such as display resize, instead of rebuilding from stale host defaults.
    pub fn pinned_to_active_plan(
        mut self,
        plan: &ResolvedMediaPlan,
        encoder: EncoderRequest,
        intent: EncodeIntent,
    ) -> Self {
        self.codec = plan.codec_token().to_string();
        self.yuv444 = matches!(plan.video.chroma, ChromaSubsampling::Yuv444);
        self.bit_depth = plan.video.bit_depth;
        self.color_range = plan.video.range;
        self.color_matrix = plan.video.matrix;
        self.fps = plan.fps;
        self.encoder = encoder;
        self.intent = intent;
        self
    }

    /// Build the argv exactly as `capenc_backend.start()` does.
    ///
    /// A `variant=<id>` token is appended whenever this config's full colour
    /// contract is a coherent, offered format: it is the one token that
    /// selects codec, chroma, depth, range and matrix together (see
    /// `linux_policy::requested_variant` in the capenc crate) and so is
    /// authoritative over the legacy positional `codec`/`yuv444` tokens kept
    /// alongside it for back-compat and log readability. An unrecognised
    /// codec string or an incoherent combination omits the token rather than
    /// handing capenc something it would reject outright; `media_request`
    /// still fails this config before capenc would otherwise be asked to
    /// serve it.
    fn argv(&self) -> Vec<String> {
        let mut v = vec![
            self.output_index.to_string(),
            self.codec.clone(),
            self.fps.to_string(),
        ];
        if self.yuv444 {
            v.push("yuv444".to_string());
        }
        v.push(FRAMED_OUTPUT_V1.to_string());
        v.push(format!("encoder={}", self.encoder.as_arg()));
        v.push(format!(
            "cursor={}",
            match self.cursor_mode {
                CursorMode::Local => "local",
                CursorMode::Host => "host",
            }
        ));
        if let Ok(codec) = self.codec_enum() {
            let variant = VideoVariant::new(self.video_configuration(codec));
            if variant.is_coherent() {
                v.push(format!("variant={}", variant.id()));
            }
        }
        // Only when it is not the default: an absent `intent=` already means
        // `Interactive` to `requested_intent`, so emitting it unconditionally
        // would change the argv of every session that never asked for a
        // different intent — and invalidate the argv assertions that pin
        // those unchanged commands — while telling capenc nothing new.
        if self.intent != EncodeIntent::default() {
            v.push(format!("intent={}", self.intent.token()));
        }
        // Same conditional shape and same reason as `intent=`: absent already
        // means off, so a host that never opted into the experiment produces
        // the argv it always did.
        if self.qp_map != arcen_media::video::QpMapPolicy::default() {
            v.push(format!("qp-map={}", self.qp_map.token()));
        }
        v
    }
    fn media_request(&self) -> Result<MediaRequest, CapencStartError> {
        let codec = self.codec_enum()?;
        Ok(MediaRequest {
            encoder: self.encoder,
            video: self.video_configuration(codec),
            width: self.width,
            height: self.height,
            fps: self.fps,
            cursor_mode: self.cursor_mode,
        })
    }
}

fn forward_capenc_line(line: &str) {
    let line = crate::eventlog::bounded_diagnostic_line(line);
    let line = line.as_ref();
    if is_pipeline_stats(line) {
        tracing::info!(target: target::CAPENC, "{line}");
    } else if is_capenc_error(line) {
        tracing::error!(target: target::CAPENC, "{line}");
    } else if is_capenc_warning(line) || is_capenc_unavailable(line) {
        tracing::warn!(target: target::CAPENC, "{line}");
    } else {
        tracing::debug!(target: target::CAPENC, "{line}");
    }
}

async fn wait_for_ready<R>(
    reader: &mut crate::bounded_io::BoundedLineReader<R>,
    config: &CapencConfig,
) -> Result<ResolvedMediaPlan, CapencStartError>
where
    R: AsyncBufRead + Unpin,
{
    let mut last_error = None;
    loop {
        match reader.next_bounded_line().await {
            Ok(Some(bounded)) => {
                if bounded.truncated {
                    tracing::warn!(
                        target: target::CAPENC,
                        sid = %config.session_log_id,
                        "capenc stderr line exceeded the bounded read limit; excess bytes discarded"
                    );
                }
                let line = bounded.text;
                if line.starts_with("[capenc] READY ") {
                    let request = config.media_request()?;
                    let allowed_backends: &'static [EncoderBackend] = match config.encoder {
                        EncoderRequest::NativeNvenc => &[EncoderBackend::NativeNvenc],
                        EncoderRequest::SoftwareH264 => &[EncoderBackend::OpenH264],
                        EncoderRequest::Auto
                        | EncoderRequest::WindowsMediaFoundation
                        | EncoderRequest::SoftwareAv1 => &[],
                    };
                    let plan = parse_ready_v1(
                        &line,
                        ReadyExpectation {
                            request,
                            allowed_backends,
                            session_log_id: Some(config.session_log_id.as_str()),
                        },
                    )
                    .map_err(|error| CapencStartError::ReadyProtocol(error.to_string()))?;
                    forward_capenc_line(&line);
                    return Ok(plan);
                }
                if is_capenc_unavailable(&line) {
                    forward_capenc_line(&line);
                    let notice = parse_unavailable_v1(&line)
                        .map_err(|error| CapencStartError::ReadyProtocol(error.to_string()))?;
                    let expected = match config.encoder {
                        EncoderRequest::NativeNvenc => EncoderBackend::NativeNvenc,
                        EncoderRequest::SoftwareH264 => EncoderBackend::OpenH264,
                        EncoderRequest::Auto
                        | EncoderRequest::WindowsMediaFoundation
                        | EncoderRequest::SoftwareAv1 => {
                            return Err(CapencStartError::ReadyProtocol(
                                "capenc attempt used an unsupported or non-concrete Linux encoder"
                                    .to_string(),
                            ));
                        }
                    };
                    if notice.backend != expected {
                        return Err(CapencStartError::ReadyProtocol(
                            "UNAVAILABLE backend differs from concrete attempt".to_string(),
                        ));
                    }
                    return Err(CapencStartError::BackendUnavailable(line));
                }
                if is_capenc_error(&line) {
                    last_error = Some(line.clone());
                }
                forward_capenc_line(&line);
            }
            Ok(None) => {
                return Err(CapencStartError::ExitedBeforeReady(
                    last_error.map_or_else(String::new, |error| format!(": {error}")),
                ));
            }
            Err(error) => {
                return Err(CapencStartError::ExitedBeforeReady(format!(
                    ": stderr read failed: {error}"
                )));
            }
        }
    }
}

/// Command sent to the stdin-writer task.
pub(crate) enum StdinCmd {
    /// Force a keyframe (`IDR\n`).
    Idr,
    /// Request graceful encoder teardown, then close stdin.
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildShutdown {
    Graceful,
    Forced,
}

/// Cloneable handle to request keyframes on a running `capenc`. Throttling is
/// the caller's responsibility (see `FrameQueue` IDR-on-drop + the
/// `request_full_frame` 0.5 s guard) — this just forwards to stdin.
///
/// All clones share one inner sender slot so a mid-session capenc respawn
/// (stream resize) can [`retarget`](Self::retarget) every outstanding clone —
/// including the one held by the backpressure queue — at the new child.
#[derive(Clone)]
pub struct IdrRequester {
    tx: std::sync::Arc<std::sync::RwLock<mpsc::Sender<StdinCmd>>>,
}

impl IdrRequester {
    fn new(tx: mpsc::Sender<StdinCmd>) -> Self {
        Self {
            tx: std::sync::Arc::new(std::sync::RwLock::new(tx)),
        }
    }

    /// Request one keyframe. Non-blocking; drops silently if the child is gone
    /// or the mailbox is momentarily full (a keyframe is already in flight).
    pub fn request(&self) -> bool {
        let tx = self
            .tx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match tx.try_send(StdinCmd::Idr) {
            Ok(()) => {
                tracing::debug!(target: target::CAPENC, "IDR requested");
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(target: target::CAPENC, "IDR request coalesced (mailbox full)");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(target: target::CAPENC, "IDR request dropped — capenc not running");
                false
            }
        }
    }

    /// Point this requester — and every clone sharing its slot — at a newly
    /// spawned capenc. Used by the mid-session stream resize after the old
    /// child was shut down.
    pub fn retarget(&self, session: &CapencSession) {
        let mut tx = self
            .tx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *tx = session.stdin_tx.clone();
    }
}

/// A running `capenc` session: take the access-unit stream with
/// [`take_frames`](Self::take_frames), request keyframes via [`idr`](Self::idr),
/// and call [`shutdown`](Self::shutdown) (or drop) to stop the child.
pub struct CapencSession {
    frames: Option<mpsc::Receiver<AccessUnit>>,
    idr: IdrRequester,
    child: Child,
    stdin_tx: mpsc::Sender<StdinCmd>,
}

impl CapencSession {
    /// Clone the keyframe-request handle (for IDR-on-drop and `request_full_frame`).
    pub fn idr(&self) -> IdrRequester {
        self.idr.clone()
    }

    /// Take the access-unit stream. Returns `None` if already taken.
    pub fn take_frames(&mut self) -> Option<mpsc::Receiver<AccessUnit>> {
        self.frames.take()
    }

    /// Graceful stop: send `STOP`, close stdin, wait briefly, then SIGKILL as a
    /// fallback. Idempotent-safe.
    pub async fn shutdown(mut self) {
        match shutdown_child(&mut self.child, &self.stdin_tx, Duration::from_secs(2)).await {
            ChildShutdown::Graceful => {
                tracing::info!(target: target::CAPENC, "capenc exited cleanly")
            }
            ChildShutdown::Forced => {
                tracing::warn!(target: target::CAPENC, "capenc ignored graceful stop — killing");
            }
        }
    }
}

async fn shutdown_child(
    child: &mut Child,
    control: &mpsc::Sender<StdinCmd>,
    timeout: Duration,
) -> ChildShutdown {
    let graceful = async {
        let _ = control.send(StdinCmd::Stop).await;
        child.wait().await
    };
    match tokio::time::timeout(timeout, graceful).await {
        Ok(Ok(_)) => ChildShutdown::Graceful,
        Ok(Err(_)) | Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            ChildShutdown::Forced
        }
    }
}

impl Drop for CapencSession {
    fn drop(&mut self) {
        // Best-effort: never leak a capenc child if shutdown() wasn't called.
        let _ = self.child.start_kill();
    }
}

/// Spawn `capenc`, wait for authoritative capture+encoder readiness, then wire
/// up framed output and control. No server hello may be sent before this returns.
pub async fn spawn(
    config: CapencConfig,
) -> Result<(CapencSession, ResolvedMediaPlan), CapencStartError> {
    let adaptive_native = config.encoder == EncoderRequest::NativeNvenc
        && config.video_selection == VideoSelectionIntent::AdaptivePerformance;
    if config.encoder == EncoderRequest::Auto || adaptive_native {
        let requested = describe_request(&config);
        let mut native = config.clone();
        native.encoder = EncoderRequest::NativeNvenc;
        match spawn_one(native).await {
            Ok(session) => return Ok(session),
            Err(CapencStartError::BackendUnavailable(reason)) => {
                let mut last_reason = reason;
                for (hardware, degradation) in hardware_codec_fallback_configs(&config) {
                    report_fallback(
                        &requested,
                        &hardware,
                        degradation,
                        Some(last_reason.as_str()),
                    );
                    match spawn_one(hardware).await {
                        Ok(session) => return Ok(session),
                        Err(CapencStartError::BackendUnavailable(reason)) => {
                            last_reason = reason;
                        }
                        Err(error) => return Err(error),
                    }
                }
                if adaptive_native {
                    return Err(CapencStartError::BackendUnavailable(last_reason));
                }
                if !software_fallback_preserves_exact_pins(&config) {
                    return Err(CapencStartError::BackendUnavailable(last_reason));
                }
                let (software, degradation) = software_fallback_config(config)?;
                report_fallback(
                    &requested,
                    &software,
                    degradation,
                    Some(last_reason.as_str()),
                );
                return spawn_one(software).await;
            }
            Err(error) => return Err(error),
        }
    }
    if config.encoder == EncoderRequest::SoftwareH264 {
        let requested = describe_request(&config);
        let (software, degradation) = software_fallback_config(config)?;
        report_fallback(&requested, &software, degradation, None);
        return spawn_one(software).await;
    }
    spawn_one(config).await
}

/// Ordered same-GPU codec fallbacks before spending CPU on software H.264.
///
/// Every Auto AV1 request retains the established HEVC retry. An authenticated
/// adaptive-performance request additionally permits hardware H.264 as the
/// final NVENC tier; fidelity/exact requests never silently become ordinary
/// H.264 merely because it is available.
fn hardware_codec_fallback_configs(config: &CapencConfig) -> Vec<(CapencConfig, PlanDegradation)> {
    if config.codec_pinned
        || config.variant_pinned
        || (config.encoder != EncoderRequest::Auto
            && !(config.encoder == EncoderRequest::NativeNvenc
                && config.video_selection == VideoSelectionIntent::AdaptivePerformance))
    {
        return Vec::new();
    }
    let Some(preferred) = VideoCodec::from_token(&config.codec) else {
        return Vec::new();
    };
    let adaptive = config.video_selection == VideoSelectionIntent::AdaptivePerformance;
    let mut fallbacks = Vec::with_capacity(2);
    for codec in adaptive_codec_ladder(preferred).iter().copied().skip(1) {
        if codec == VideoCodec::H264 && !adaptive {
            continue;
        }
        let mut fallback = config.clone();
        fallback.encoder = EncoderRequest::NativeNvenc;
        fallback.codec = codec.token().to_string();
        if codec == VideoCodec::H264 {
            fallback.yuv444 = false;
            fallback.bit_depth = BitDepth::Eight;
        }
        fallbacks.push((
            fallback,
            PlanDegradation {
                codec_changed: true,
                bit_depth_reduced: codec == VideoCodec::H264 && config.bit_depth > BitDepth::Eight,
                chroma_changed: codec == VideoCodec::H264 && config.yuv444,
                ..PlanDegradation::default()
            },
        ));
    }
    fallbacks
}

fn describe_request(config: &CapencConfig) -> String {
    format!(
        "{} {} {}x{}@{}",
        config.codec,
        if config.yuv444 { "yuv444" } else { "yuv420" },
        config.width,
        config.height,
        config.fps
    )
}

/// Announce a software fallback, and say exactly what it cost.
///
/// A degraded session must never be silent. An operator seeing a worse picture
/// needs the requested plan, the served plan, and why the hardware path was not
/// taken, all in one line, without having to reproduce the session.
fn report_fallback(
    requested: &str,
    resolved: &CapencConfig,
    degradation: PlanDegradation,
    reason: Option<&str>,
) {
    let served = describe_request(resolved);
    if degradation.is_exact() {
        tracing::info!(
            target: target::CAPENC,
            backend = EncoderBackend::OpenH264.ready_token(),
            served = %served,
            unavailable_reason = reason.unwrap_or("explicitly requested"),
            "software encode selected; the requested plan was served unchanged"
        );
        return;
    }
    tracing::warn!(
        target: target::CAPENC,
        backend = EncoderBackend::OpenH264.ready_token(),
        requested = %requested,
        served = %served,
        unavailable_reason = reason.unwrap_or("explicitly requested"),
        codec_changed = degradation.codec_changed,
        chroma_changed = degradation.chroma_changed,
        fps_clamped = degradation.fps_clamped,
        geometry_clamped = degradation.geometry_clamped,
        cursor_moved_to_local = degradation.cursor_moved_to_local,
        "software encode selected and the plan was degraded to fit it"
    );
}

/// Resolve backend limits before display mutation without starting capture.
///
/// Explicit software requests are validated locally and never execute capenc,
/// so this path cannot load NVIDIA libraries. Native and auto requests use
/// capenc's transient no-frame native probe.
pub async fn preflight(config: CapencConfig) -> Result<(), CapencStartError> {
    config.media_request()?;
    if config.encoder == EncoderRequest::SoftwareH264 {
        software_fallback_config(config)?;
        return Ok(());
    }
    if !matches!(
        config.encoder,
        EncoderRequest::Auto | EncoderRequest::NativeNvenc
    ) {
        return Err(CapencStartError::InvalidConfig(
            "unsupported Linux preflight encoder".to_string(),
        ));
    }
    match probe_native(&config).await? {
        None => Ok(()),
        Some(line)
            if config.encoder == EncoderRequest::Auto
                || (config.encoder == EncoderRequest::NativeNvenc
                    && config.video_selection == VideoSelectionIntent::AdaptivePerformance) =>
        {
            let mut last_line = line;
            for (hardware, _) in hardware_codec_fallback_configs(&config) {
                match probe_native(&hardware).await? {
                    None => {
                        tracing::info!(
                            target: target::CAPENC,
                            unavailable = %last_line,
                            resolved_codec = %hardware.codec,
                            "hardware codec fallback selected before display mutation"
                        );
                        return Ok(());
                    }
                    Some(line) => last_line = line,
                }
            }
            if config.encoder == EncoderRequest::NativeNvenc {
                return Err(CapencStartError::BackendUnavailable(last_line));
            }
            if !software_fallback_preserves_exact_pins(&config) {
                return Err(CapencStartError::BackendUnavailable(last_line));
            }
            software_fallback_config(config)?;
            tracing::info!(
                target: target::CAPENC,
                unavailable = %last_line,
                "software limits selected before display mutation"
            );
            Ok(())
        }
        Some(line) => Err(CapencStartError::BackendUnavailable(line)),
    }
}

async fn probe_native(config: &CapencConfig) -> Result<Option<String>, CapencStartError> {
    let mut attempt = config.clone();
    attempt.encoder = EncoderRequest::NativeNvenc;
    let mut args = attempt.argv();
    args.push("probe-v1".to_string());
    let mut command = crate::command_for_helper(&config.binary, "capenc");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_child_environment(&mut command, config)?;
    let mut child = command.spawn().map_err(CapencStartError::Spawn)?;
    let stderr = child.stderr.take().ok_or_else(|| {
        CapencStartError::InvalidConfig("native probe has no stderr pipe".to_string())
    })?;
    let expected = format!(
        "[capenc] PROBE version=1 backend=native-nvenc available=true sid={}",
        config.session_log_id
    );
    let outcome = tokio::time::timeout(CAPENC_PROBE_TIMEOUT, async {
        let mut reader = crate::bounded_io::BoundedLineReader::new(BufReader::new(stderr));
        while let Some(bounded) = reader.next_bounded_line().await.map_err(|error| {
            CapencStartError::ExitedBeforeReady(format!(": probe stderr read failed: {error}"))
        })? {
            if bounded.truncated {
                tracing::warn!(
                    target: target::CAPENC,
                    sid = %config.session_log_id,
                    "capenc native-probe stderr line exceeded the bounded read limit; \
                     excess bytes discarded"
                );
            }
            let line = bounded.text;
            if line == expected {
                return Ok(None);
            }
            if is_capenc_unavailable(&line) {
                let notice = parse_unavailable_v1(&line)
                    .map_err(|error| CapencStartError::ReadyProtocol(error.to_string()))?;
                if notice.backend != EncoderBackend::NativeNvenc {
                    return Err(CapencStartError::ReadyProtocol(
                        "native probe reported a different backend".to_string(),
                    ));
                }
                return Ok(Some(line));
            }
            if line.starts_with("[capenc] PROBE ") {
                return Err(CapencStartError::ReadyProtocol(
                    "malformed native probe response".to_string(),
                ));
            }
            forward_capenc_line(&line);
        }
        Err(CapencStartError::ExitedBeforeReady(String::new()))
    })
    .await;
    let result = outcome.unwrap_or(Err(CapencStartError::ReadyTimeout));
    let status = match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Err(CapencStartError::ExitedBeforeReady(format!(
                ": wait for native probe: {error}"
            )));
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(CapencStartError::ReadyTimeout);
        }
    };
    if matches!(result, Ok(None)) && !status.success() {
        return Err(CapencStartError::ExitedBeforeReady(
            ": native probe failed after its availability response".to_string(),
        ));
    }
    result
}

/// The portable OpenH264 contract, read from the shared capability table
/// rather than restated here.
///
/// It used to be a local copy of the limits. That is exactly how two hosts
/// drift: the copy is correct on the day it is written and silently stale
/// afterwards. `EncoderBackend::contract` is now the single statement of what
/// a backend can do, and both hosts read it.
const OPENH264_LIMITS: BackendLimits = EncoderBackend::OpenH264.contract();

/// Fit a requested session geometry to the encoder that will actually serve it.
///
/// The Linux session display is mutated exactly once, before any encoder
/// exists, and the spec forbids a hidden second modeset. So when the encode
/// path is already known to be software, the geometry has to be fitted here
/// rather than after capenc has been handed a rectangle it must reject.
///
/// `Auto` is left alone: NVENC is tried first and can serve the full request,
/// and clamping every session to the software limits on the chance that NVENC
/// might be unavailable would penalise every healthy host.
#[must_use]
pub fn fit_to_encoder_limits(encoder: EncoderRequest, width: u32, height: u32) -> (u32, u32) {
    if encoder != EncoderRequest::SoftwareH264 {
        return (width, height);
    }
    let request = MediaRequest {
        encoder: EncoderRequest::SoftwareH264,
        video: VideoConfiguration {
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        },
        width,
        height,
        fps: OPENH264_LIMITS.max_fps,
        cursor_mode: CursorMode::Local,
    };
    let candidates = [BackendCandidate {
        backend: EncoderBackend::OpenH264,
        availability: BackendAvailability::Available(OPENH264_LIMITS),
    }];
    resolve_media_plan_degrading(request, &candidates)
        .map_or((width, height), |(plan, _)| (plan.width, plan.height))
}

/// Rewrite a request into the best plan OpenH264 can actually encode.
///
/// This used to refuse anything that was not already h264/yuv420 inside the
/// software limits. That made the fallback unreachable in practice, because the
/// shipped Linux default is h265/yuv444 at 60 fps: a host whose NVENC was
/// unavailable would fail the request instead of degrading, and serve nothing.
///
/// Resolution is delegated to `arcen-media` so every backend, including future
/// hardware vendors, degrades by the same rules rather than by a predicate
/// maintained per host. The returned [`PlanDegradation`] is reported by the
/// caller; a session that looks worse than configured must be able to say why.
fn software_fallback_config(
    mut config: CapencConfig,
) -> Result<(CapencConfig, PlanDegradation), CapencStartError> {
    if !software_fallback_preserves_exact_pins(&config) {
        return Err(CapencStartError::InvalidConfig(
            "OpenH264 fallback would violate an exact video.codec or video.variant pin".to_string(),
        ));
    }
    // Geometry is only unspecified when the child self-discovers the display,
    // in which case there is nothing here to fit and capenc enforces the
    // contract itself.
    let geometry_unspecified = config.width == 0 && config.height == 0;
    let mut request = config.media_request()?;
    request.encoder = EncoderRequest::SoftwareH264;
    if geometry_unspecified {
        // Stand in a representable geometry purely so codec, chroma and fps can
        // still be resolved; the resolved dimensions are discarded below.
        request.width = OPENH264_LIMITS.max_width;
        request.height = OPENH264_LIMITS.max_height;
    }

    let candidates = [BackendCandidate {
        backend: EncoderBackend::OpenH264,
        availability: BackendAvailability::Available(OPENH264_LIMITS),
    }];
    let (plan, mut degradation) =
        resolve_media_plan_degrading(request, &candidates).map_err(|error| {
            CapencStartError::InvalidConfig(format!(
                "OpenH264 fallback cannot serve the request: {error}"
            ))
        })?;

    config.encoder = EncoderRequest::SoftwareH264;
    config.codec = plan.codec_token().to_string();
    config.yuv444 = matches!(plan.video.chroma, ChromaSubsampling::Yuv444);
    config.bit_depth = plan.video.bit_depth;
    config.color_range = plan.video.range;
    config.color_matrix = plan.video.matrix;
    config.fps = plan.fps;
    config.cursor_mode = plan.cursor_mode;
    if geometry_unspecified {
        degradation.geometry_clamped = false;
    } else {
        config.width = plan.width;
        config.height = plan.height;
    }
    Ok((config, degradation))
}

fn software_fallback_preserves_exact_pins(config: &CapencConfig) -> bool {
    if config.codec_pinned && config.codec != "h264" {
        return false;
    }
    if !config.variant_pinned {
        return true;
    }
    let within_geometry = (config.width == 0 && config.height == 0)
        || (config.width <= OPENH264_LIMITS.max_width
            && config.height <= OPENH264_LIMITS.max_height);
    config.codec == "h264"
        && !config.yuv444
        && config.bit_depth == BitDepth::Eight
        && !config.color_matrix.is_identity()
        && config.fps <= OPENH264_LIMITS.max_fps
        && within_geometry
}

async fn spawn_one(
    config: CapencConfig,
) -> Result<(CapencSession, ResolvedMediaPlan), CapencStartError> {
    config.media_request()?;
    if matches!(
        config.encoder,
        EncoderRequest::Auto | EncoderRequest::WindowsMediaFoundation
    ) {
        return Err(CapencStartError::InvalidConfig(
            "Linux capenc attempts must use a concrete encoder".to_string(),
        ));
    }
    let argv = config.argv();
    let mut cmd = crate::command_for_helper(&config.binary, "capenc");
    cmd.args(&argv)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_child_environment(&mut cmd, &config)?;

    let mut child = cmd.spawn().map_err(|error| {
        tracing::error!(
            target: target::CAPENC,
            binary = %config.binary.display(),
            %error,
            "failed to spawn capenc"
        );
        CapencStartError::Spawn(error)
    })?;

    let stdout = child.stdout.take().ok_or_else(|| {
        CapencStartError::InvalidConfig("spawned capenc has no stdout pipe".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        CapencStartError::InvalidConfig("spawned capenc has no stderr pipe".to_string())
    })?;
    let stdin = child.stdin.take().ok_or_else(|| {
        CapencStartError::InvalidConfig("spawned capenc has no stdin pipe".to_string())
    })?;
    let mut stderr_lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(stderr));
    let plan = match tokio::time::timeout(
        CAPENC_READY_TIMEOUT,
        wait_for_ready(&mut stderr_lines, &config),
    )
    .await
    {
        Ok(Ok(plan)) => plan,
        Ok(Err(error)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(error);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(CapencStartError::ReadyTimeout);
        }
    };

    let (frames_tx, frames_rx) = mpsc::channel::<AccessUnit>(FRAME_CHANNEL_CAPACITY);
    // Small mailbox: coalesces bursts of keyframe requests (a full mailbox
    // means an IDR is already queued, so dropping extras is correct).
    let (stdin_tx, stdin_rx) = mpsc::channel::<StdinCmd>(4);

    let helper_span = tracing::info_span!(
        target: target::CAPENC,
        "capenc_helper",
        sid = %config.session_log_id
    );
    let nal_codec = match plan.video.codec {
        VideoCodec::H264 => NalCodec::H264,
        VideoCodec::H265 => NalCodec::H265,
        VideoCodec::Av1 => NalCodec::Av1,
        _ => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(CapencStartError::ReadyProtocol(
                "READY selected an unwired codec".to_string(),
            ));
        }
    };
    tokio::spawn(read_stdout(stdout, nal_codec, frames_tx).instrument(helper_span.clone()));
    tokio::spawn(write_stdin(stdin, stdin_rx).instrument(helper_span.clone()));
    tokio::spawn(read_stderr(stderr_lines).instrument(helper_span));

    tracing::info!(
        target: target::CAPENC,
        binary = %config.binary.display(),
        output = config.output_index,
        requested_encoder = config.encoder.as_arg(),
        backend = plan.backend.ready_token(),
        codec = plan.codec_token(),
        fps = plan.fps,
        chroma = plan.chroma_token(),
        width = plan.width,
        height = plan.height,
        display = config.display.as_deref().unwrap_or("<inherit>"),
        cursor = ?plan.cursor_mode,
        "capenc ready"
    );

    Ok((
        CapencSession {
            frames: Some(frames_rx),
            idr: IdrRequester::new(stdin_tx.clone()),
            child,
            stdin_tx,
        },
        plan,
    ))
}

fn configure_child_environment(
    command: &mut Command,
    config: &CapencConfig,
) -> Result<(), CapencStartError> {
    if let Some(execution) = &config.execution {
        execution.configure(command).map_err(|error| {
            CapencStartError::InvalidConfig(format!("configure child identity: {error}"))
        })?;
    }
    // Apply authenticated-session endpoints after `configure`, which
    // intentionally replaces the inherited environment.
    if let Some(display) = &config.display {
        command.env("DISPLAY", display);
    }
    if let Some(xauthority) = &config.xauthority {
        command.env("XAUTHORITY", xauthority);
    }
    command.env("ARCEN_SESSION_LOG_ID", config.session_log_id.as_str());
    Ok(())
}

pub(crate) fn admission_probe_command(
    config: &CapencConfig,
) -> Result<tokio::process::Command, CapencStartError> {
    config.media_request()?;
    if matches!(
        config.encoder,
        EncoderRequest::Auto | EncoderRequest::WindowsMediaFoundation
    ) {
        return Err(CapencStartError::InvalidConfig(
            "Linux admission probes require a concrete NVENC or OpenH264 backend".to_string(),
        ));
    }
    let mut command = crate::command_for_helper(&config.binary, "capenc");
    command.args(config.argv());
    configure_child_environment(&mut command, config)?;
    Ok(command)
}

/// stdout → exact framed-v1 encoded access units. Pipe read sizes are arbitrary and never
/// treated as child-write boundaries.
async fn read_stdout(
    mut stdout: tokio::process::ChildStdout,
    codec: NalCodec,
    frames_tx: mpsc::Sender<AccessUnit>,
) {
    loop {
        match read_framed_access_unit(&mut stdout, codec).await {
            Ok(None) => {
                tracing::info!(target: target::CAPENC, "capenc stdout closed (EOF)");
                break;
            }
            Ok(Some(au)) => {
                if frames_tx.send(au).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(target: target::CAPENC, error = %e, "invalid framed capenc stdout");
                break;
            }
        }
    }
}

async fn read_framed_access_unit<R>(
    reader: &mut R,
    codec: NalCodec,
) -> std::io::Result<Option<AccessUnit>>
where
    R: AsyncRead + Unpin,
{
    let mut length = [0u8; FRAME_LENGTH_SIZE];
    let first = reader.read(&mut length[..1]).await?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length[1..]).await.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("truncated framed-v1 length prefix: {error}"),
        )
    })?;

    let payload_len = u32::from_be_bytes(length) as usize;
    if payload_len == 0 || payload_len > MAX_ACCESS_UNIT_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "framed-v1 access-unit length {payload_len} outside 1..={MAX_ACCESS_UNIT_SIZE}"
            ),
        ));
    }

    let mut data = vec![0u8; payload_len];
    reader.read_exact(&mut data).await.map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("truncated framed-v1 access unit: expected {payload_len} bytes: {error}"),
        )
    })?;
    let is_keyframe = access_unit_is_keyframe(&data, codec);
    Ok(Some(AccessUnit { data, is_keyframe }))
}

/// stdin ← keyframe requests. `Stop` writes the graceful stop command and then
/// drops stdin so older capenc children also exit on EOF.
async fn write_stdin(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<StdinCmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            StdinCmd::Idr => {
                if let Err(e) = stdin.write_all(b"IDR\n").await.and(stdin.flush().await) {
                    tracing::warn!(target: target::CAPENC, error = %e, "capenc IDR write failed");
                    break;
                }
            }
            StdinCmd::Stop => {
                let _ = stdin.write_all(b"STOP\n").await.and(stdin.flush().await);
                break;
            }
        }
    }
    // Dropping `stdin` here closes the pipe → capenc sees EOF and exits.
    drop(stdin);
    tracing::debug!(target: target::CAPENC, "capenc stdin closed");
}

/// stderr → logs. `capenc` prints `[capenc] …` diagnostics + ~1 Hz stats; the
/// stats lines carry `avg_encode_ms=…` (harvested later for health telemetry).
async fn read_stderr<R>(mut reader: crate::bounded_io::BoundedLineReader<R>)
where
    R: AsyncBufRead + Unpin,
{
    loop {
        match reader.next_bounded_line().await {
            Ok(Some(bounded)) => {
                if bounded.truncated {
                    tracing::warn!(
                        target: target::CAPENC,
                        "capenc stderr line exceeded the bounded read limit; excess bytes \
                         discarded"
                    );
                }
                forward_capenc_line(&bounded.text);
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(target: target::CAPENC, error = %e, "capenc stderr read ended");
                break;
            }
        }
    }
}

fn is_pipeline_stats(line: &str) -> bool {
    line.starts_with("[capenc] enc_fps=")
}

fn is_capenc_warning(line: &str) -> bool {
    line.starts_with("[capenc] WARNING:")
}

fn is_capenc_error(line: &str) -> bool {
    line.starts_with("[capenc] ERROR:")
}

fn is_capenc_unavailable(line: &str) -> bool {
    line.starts_with(UNAVAILABLE_PREFIX)
}

/// Locate the `arcen-capenc` binary, mirroring
/// `capenc_backend.find_capenc_binary`: `ARCEN_CAPENC` env override, then
/// the repo release build dir, then alongside this binary. An explicit
/// `--capenc-bin` override (handled by the caller) takes precedence over all.
pub fn find_capenc_binary() -> Option<PathBuf> {
    let exe = "arcen-capenc";
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env) = std::env::var("ARCEN_CAPENC") {
        if !env.is_empty() {
            candidates.push(PathBuf::from(env));
        }
    }
    // server/capenc/target/release/arcen-capenc, relative to this crate.
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("server/capenc/target/release").join(exe));
        candidates.push(cwd.join("capenc/target/release").join(exe));
    }
    // Alongside the running binary (bundled install).
    if let Ok(mut here) = std::env::current_exe() {
        here.pop();
        candidates.push(here.join(exe));
    }
    candidates.into_iter().find(|c| c.is_file())
}

/// Test-only helpers for exercising the IDR path without a real child.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{IdrRequester, StdinCmd};
    use tokio::sync::mpsc;

    /// Build an [`IdrRequester`] backed by an observable channel. The returned
    /// receiver yields one [`StdinCmd`] per `request()` / shutdown so tests can
    /// assert IDR-on-drop throttling.
    pub(crate) fn fake_idr() -> (IdrRequester, mpsc::Receiver<StdinCmd>) {
        let (tx, rx) = mpsc::channel::<StdinCmd>(8);
        (IdrRequester::new(tx), rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NATIVE_READY: &str = "[capenc] READY version=2 backend=native-nvenc \
        codec=h265 chroma=yuv444 bit_depth=8 range=limited matrix=bt709 primaries=bt709 \
        transfer=bt709 width=3840 height=2160 fps=60 \
        supports_h264=true supports_h265=true supports_yuv444=true supports_main10=true \
        supports_full_range=true cursor=host \
        sid=00000000-0000-4000-8000-000000000000";

    fn native_config() -> CapencConfig {
        CapencConfig {
            binary: PathBuf::from("arcen-capenc"),
            output_index: 1,
            codec: "h265".to_string(),
            encoder: EncoderRequest::NativeNvenc,
            fps: 60,
            yuv444: true,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            intent: EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            width: 3840,
            height: 2160,
            cursor_mode: CursorMode::Host,
            display: None,
            xauthority: None,
            execution: None,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    #[test]
    fn argv_omits_yuv444_for_420() {
        let cfg = CapencConfig {
            binary: PathBuf::from("arcen-capenc"),
            output_index: 0,
            codec: "h264".to_string(),
            encoder: EncoderSelection::Auto,
            fps: 60,
            yuv444: false,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            intent: EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            width: 1920,
            height: 1080,
            cursor_mode: CursorMode::Local,
            display: None,
            xauthority: None,
            execution: None,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        };
        assert_eq!(
            cfg.argv(),
            vec![
                "0",
                "h264",
                "60",
                FRAMED_OUTPUT_V1,
                "encoder=auto",
                "cursor=local",
                "variant=h264-420-8-limited-bt709",
            ]
        );
    }

    #[test]
    fn argv_appends_yuv444_token() {
        let mut cfg = native_config();
        cfg.fps = 30;
        assert_eq!(
            cfg.argv(),
            vec![
                "1",
                "h265",
                "30",
                "yuv444",
                FRAMED_OUTPUT_V1,
                "encoder=nvenc",
                "cursor=host",
                "variant=hevc-444-8-limited-bt709",
            ]
        );
    }

    /// The intent token is conditional, and the reason matters: a session that
    /// never asked for a different intent must produce the exact argv it
    /// produced before intent existed, so that an operator diffing two spawn
    /// commands sees a token only where a real request caused one.
    #[test]
    fn argv_emits_the_intent_token_only_when_it_is_not_the_default() {
        let mut cfg = native_config();
        assert_eq!(cfg.intent, EncodeIntent::Interactive);
        assert!(
            !cfg.argv().iter().any(|arg| arg.starts_with("intent=")),
            "the default intent must leave the shipped argv untouched"
        );

        cfg.intent = EncodeIntent::Quality;
        assert_eq!(
            cfg.argv().last().map(String::as_str),
            Some("intent=quality")
        );
    }

    #[tokio::test]
    async fn explicit_software_preflight_never_executes_the_configured_binary() {
        let mut config = native_config();
        config.binary = PathBuf::from("definitely-absent-nvidia-probe");
        config.codec = "h264".to_string();
        config.encoder = EncoderRequest::SoftwareH264;
        config.fps = 60;
        config.yuv444 = false;
        config.width = 1920;
        config.height = 1080;
        config.cursor_mode = CursorMode::Local;
        preflight(config).await.expect("local software preflight");
    }

    #[test]
    fn software_limits_fit_unsupported_geometry_instead_of_rejecting_it() {
        // This test previously asserted that oversized geometry was rejected.
        // That was the defect: a 4K display on a host with no usable NVENC got
        // no session at all rather than a smaller one. Fitting is the contract
        // now, and the geometry is reported so the operator can see the cost.
        let mut config = native_config();
        config.codec = "h264".to_string();
        config.encoder = EncoderRequest::SoftwareH264;
        config.yuv444 = false;
        config.width = 3840;
        config.height = 2160;
        config.cursor_mode = CursorMode::Local;
        let (fitted, degradation) =
            software_fallback_config(config).expect("oversized geometry must be fitted");
        assert_eq!((fitted.width, fitted.height), (1920, 1080));
        assert!(degradation.geometry_clamped);
        assert_eq!(fitted.encoder, EncoderRequest::SoftwareH264);

        // Unspecified geometry means the child self-discovers the display, so
        // there is nothing to fit and nothing to report.
        let mut unspecified = native_config();
        unspecified.codec = "h264".to_string();
        unspecified.encoder = EncoderRequest::SoftwareH264;
        unspecified.yuv444 = false;
        unspecified.width = 0;
        unspecified.height = 0;
        unspecified.cursor_mode = CursorMode::Local;
        let (resolved, degradation) =
            software_fallback_config(unspecified).expect("unspecified geometry is allowed");
        assert_eq!((resolved.width, resolved.height), (0, 0));
        assert!(!degradation.geometry_clamped);
    }

    #[test]
    fn software_fallback_degrades_the_shipped_linux_default_instead_of_refusing() {
        // packaging/linux/arcen-pier.json ships h265 / yuv444 / 60fps. Before
        // this change the fallback refused exactly that, which made it
        // unreachable on the configuration we actually ship.
        let mut config = native_config();
        config.codec = "h265".to_string();
        config.yuv444 = true;
        config.bit_depth = BitDepth::Ten;
        config.color_range = ColorRange::Full;
        config.color_matrix = ColorMatrix::Identity;
        config.fps = 60;
        config.width = 2560;
        config.height = 1600;
        config.cursor_mode = CursorMode::Local;
        let (resolved, degradation) =
            software_fallback_config(config).expect("shipped default must degrade, not refuse");
        assert_eq!(resolved.codec, "h264");
        assert!(!resolved.yuv444);
        assert_eq!(resolved.bit_depth, BitDepth::Eight);
        assert_eq!(resolved.color_range, ColorRange::Full);
        assert_eq!(resolved.color_matrix, ColorMatrix::Bt709);
        assert_eq!(resolved.fps, 30);
        assert_eq!((resolved.width, resolved.height), (1920, 1200));
        assert!(degradation.codec_changed);
        assert!(degradation.chroma_changed);
        assert!(degradation.fps_clamped);
        assert!(degradation.geometry_clamped);
    }

    #[test]
    fn av1_auto_retries_hevc_hardware_before_software() {
        let mut config = native_config();
        config.encoder = EncoderRequest::Auto;
        config.codec = "av1".to_string();
        config.yuv444 = false;
        let fallbacks = hardware_codec_fallback_configs(&config);
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].0.encoder, EncoderRequest::NativeNvenc);
        assert_eq!(fallbacks[0].0.codec, "h265");
        assert!(!fallbacks[0].0.yuv444);
        assert!(fallbacks[0].1.codec_changed);
        assert!(!fallbacks[0].1.chroma_changed);

        config.video_selection = VideoSelectionIntent::AdaptivePerformance;
        let fallbacks = hardware_codec_fallback_configs(&config);
        assert_eq!(
            fallbacks
                .iter()
                .map(|(fallback, _)| fallback.codec.as_str())
                .collect::<Vec<_>>(),
            ["h265", "h264"]
        );

        config.codec = "h265".to_string();
        assert_eq!(hardware_codec_fallback_configs(&config)[0].0.codec, "h264");
        config.codec = "av1".to_string();
        config.encoder = EncoderRequest::NativeNvenc;
        assert_eq!(
            hardware_codec_fallback_configs(&config)
                .iter()
                .map(|(fallback, _)| fallback.codec.as_str())
                .collect::<Vec<_>>(),
            ["h265", "h264"]
        );
        config.video_selection = VideoSelectionIntent::Exact;
        assert!(hardware_codec_fallback_configs(&config).is_empty());
    }

    #[test]
    fn exact_codec_and_variant_pins_never_enter_codec_fallback_ladder() {
        let mut config = native_config();
        config.encoder = EncoderRequest::Auto;
        config.codec = "av1".to_string();
        config.video_selection = VideoSelectionIntent::AdaptivePerformance;

        config.codec_pinned = true;
        assert!(hardware_codec_fallback_configs(&config).is_empty());
        assert!(!software_fallback_preserves_exact_pins(&config));

        config.codec_pinned = false;
        config.variant_pinned = true;
        assert!(hardware_codec_fallback_configs(&config).is_empty());
        assert!(!software_fallback_preserves_exact_pins(&config));
    }

    #[test]
    fn h264_codec_pin_can_retain_its_codec_on_openh264() {
        let mut config = native_config();
        config.encoder = EncoderRequest::Auto;
        config.codec = "h264".to_string();
        config.yuv444 = false;
        config.bit_depth = BitDepth::Eight;
        config.codec_pinned = true;
        assert!(software_fallback_preserves_exact_pins(&config));
    }

    #[test]
    fn software_fallback_reports_no_degradation_when_nothing_had_to_change() {
        let mut config = native_config();
        config.codec = "h264".to_string();
        config.yuv444 = false;
        config.fps = 30;
        config.width = 1280;
        config.height = 800;
        config.cursor_mode = CursorMode::Local;
        let (resolved, degradation) =
            software_fallback_config(config).expect("already-admissible request");
        assert!(degradation.is_exact());
        assert_eq!((resolved.width, resolved.height), (1280, 800));
        assert_eq!(resolved.fps, 30);
    }

    #[test]
    fn ready_parser_builds_typed_native_plan() {
        let cfg = native_config();
        let plan = parse_ready_v1(
            NATIVE_READY,
            ReadyExpectation {
                request: cfg.media_request().expect("request"),
                allowed_backends: &[EncoderBackend::NativeNvenc],
                session_log_id: Some(cfg.session_log_id.as_str()),
            },
        )
        .expect("READY");
        assert_eq!(plan.backend, EncoderBackend::NativeNvenc);
        assert_eq!(plan.video.codec, VideoCodec::H265);
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv444);
        assert_eq!((plan.width, plan.height, plan.fps), (3840, 2160, 60));
        assert!(plan.supports_h264());
        assert!(plan.supports_h265());
        assert!(plan.supports_yuv444());
    }

    #[test]
    fn active_plan_pin_preserves_legacy_quality_across_resize() {
        let cfg = native_config();
        let plan = parse_ready_v1(
            NATIVE_READY,
            ReadyExpectation {
                request: cfg.media_request().expect("request"),
                allowed_backends: &[EncoderBackend::NativeNvenc],
                session_log_id: Some(cfg.session_log_id.as_str()),
            },
        )
        .expect("READY");
        let resized =
            cfg.pinned_to_active_plan(&plan, EncoderRequest::NativeNvenc, EncodeIntent::Quality);
        assert_eq!(resized.intent, EncodeIntent::Quality);
        assert_eq!(resized.codec, "h265");
        assert!(resized.yuv444);
        assert_eq!(resized.bit_depth, BitDepth::Eight);
    }

    #[test]
    fn ready_parser_rejects_mismatch_unknowns_and_invalid_active_caps() {
        let cfg = native_config();
        let expectation = ReadyExpectation {
            request: cfg.media_request().expect("request"),
            allowed_backends: &[EncoderBackend::NativeNvenc],
            session_log_id: Some(cfg.session_log_id.as_str()),
        };
        assert!(parse_ready_v1(&format!("{NATIVE_READY} surprise=true"), expectation).is_err());
        assert!(parse_ready_v1(
            &NATIVE_READY.replace("supports_h265=true", "supports_h265=false"),
            expectation
        )
        .is_err());
        assert!(
            parse_ready_v1(&NATIVE_READY.replace("width=3840", "width=0"), expectation).is_err()
        );
        assert!(parse_ready_v1(
            &NATIVE_READY.replace(
                "sid=00000000-0000-4000-8000-000000000000",
                "sid=10000000-0000-4000-8000-000000000000"
            ),
            expectation
        )
        .is_err());
    }

    #[tokio::test]
    async fn ready_wait_ignores_diagnostics_and_returns_authoritative_plan() {
        let (reader, mut writer) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            writer
                .write_all(format!("[capenc] CUDA ready: device=0/1\n{NATIVE_READY}\n").as_bytes())
                .await
                .unwrap();
        });
        let mut lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(reader));
        let plan = wait_for_ready(&mut lines, &native_config()).await.unwrap();
        assert_eq!(plan.backend, EncoderBackend::NativeNvenc);
    }

    #[tokio::test]
    async fn ready_wait_bounds_an_enormous_unterminated_diagnostic_line_and_still_resolves() {
        // A malformed/huge diagnostic before READY must not grow memory
        // with the input; the bounded reader discards the excess and
        // resynchronizes on the next newline, so the real READY line still
        // resolves the plan.
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let huge = vec![b'q'; 1024 * 1024];
            writer.write_all(&huge).await.unwrap();
            writer
                .write_all(format!("\n{NATIVE_READY}\n").as_bytes())
                .await
                .unwrap();
        });
        let mut lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(reader));
        let plan = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_ready(&mut lines, &native_config()),
        )
        .await
        .expect("must resolve promptly rather than hang on the enormous line")
        .unwrap();
        assert_eq!(plan.backend, EncoderBackend::NativeNvenc);
    }

    #[tokio::test]
    async fn unavailable_and_eof_are_distinct_startup_failures() {
        let (reader, mut writer) = tokio::io::duplex(1024);
        writer
            .write_all(
                b"[capenc] UNAVAILABLE version=1 backend=native-nvenc code=runtime_missing\n",
            )
            .await
            .unwrap();
        drop(writer);
        let mut lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(reader));
        assert!(matches!(
            wait_for_ready(&mut lines, &native_config()).await,
            Err(CapencStartError::BackendUnavailable(_))
        ));

        let (reader, writer) = tokio::io::duplex(64);
        drop(writer);
        let mut lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(reader));
        assert!(matches!(
            wait_for_ready(&mut lines, &native_config()).await,
            Err(CapencStartError::ExitedBeforeReady(_))
        ));
    }

    #[tokio::test]
    async fn ready_wait_can_be_bounded_by_the_caller() {
        let (reader, _writer) = tokio::io::duplex(64);
        let mut lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(reader));
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            wait_for_ready(&mut lines, &native_config())
        )
        .await
        .is_err());
    }

    #[test]
    fn stable_pipeline_stats_are_visible_at_info() {
        assert!(is_pipeline_stats(
            "[capenc] enc_fps=1 avg_encode_ms=2.1 emit_keepalive=1"
        ));
        assert!(!is_pipeline_stats(
            "[capenc] NVENC ready: 3840x2160 codec=h265"
        ));
    }

    #[test]
    fn explicit_capenc_warnings_remain_visible() {
        assert!(is_capenc_warning(
            "[capenc] WARNING: NvFBC ToCuda API 1.7 exposes no diff-map fields"
        ));
        assert!(!is_capenc_warning("[capenc] bound RandR output 0"));
    }

    #[test]
    fn backend_unavailability_remains_visible() {
        assert!(is_capenc_unavailable(
            "[capenc] UNAVAILABLE version=1 backend=native-nvenc code=cuda_init"
        ));
        assert!(is_capenc_unavailable(
            "[capenc] UNAVAILABLE version=1 backend=native-nvenc code=not_built"
        ));
    }

    #[test]
    fn fatal_capenc_errors_remain_visible() {
        assert!(is_capenc_error(
            "[capenc] ERROR: stage failed: CUDA context lost"
        ));
        assert!(is_capenc_error(
            "[capenc] ERROR: no backend for this platform/feature combination"
        ));
        assert!(!is_capenc_error("[capenc] CUDA ready: device=0/1"));
    }

    async fn fragmented_reader(bytes: Vec<u8>, chunk_sizes: &[usize]) -> tokio::io::DuplexStream {
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        let chunks = chunk_sizes.to_vec();
        tokio::spawn(async move {
            let mut offset = 0usize;
            let mut index = 0usize;
            while offset < bytes.len() {
                let size = chunks[index % chunks.len()].max(1);
                let end = (offset + size).min(bytes.len());
                writer.write_all(&bytes[offset..end]).await.unwrap();
                offset = end;
                index += 1;
            }
        });
        reader
    }

    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(FRAME_LENGTH_SIZE + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    #[cfg(windows)]
    fn shutdown_fixture_command(windows_script: &str, _unix_script: &str) -> Command {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-Command", windows_script]);
        command
    }

    #[cfg(unix)]
    fn shutdown_fixture_command(_windows_script: &str, unix_script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", unix_script]);
        command
    }

    async fn spawn_shutdown_fixture(
        windows_script: &str,
        unix_script: &str,
    ) -> (Child, mpsc::Sender<StdinCmd>) {
        let mut command = shutdown_fixture_command(windows_script, unix_script);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn shutdown fixture");
        let stdin = child.stdin.take().expect("fixture stdin");
        let (tx, rx) = mpsc::channel(1);
        tokio::spawn(write_stdin(stdin, rx));
        (child, tx)
    }

    #[tokio::test]
    async fn normal_and_resize_shutdown_reap_gracefully() {
        for _ in 0..2 {
            let (mut child, control) = spawn_shutdown_fixture(
                "$line = [Console]::In.ReadLine(); if ($line -eq 'STOP') { exit 0 } else { exit 7 }",
                "IFS= read -r line; test \"$line\" = STOP",
            )
            .await;
            assert_eq!(
                shutdown_child(&mut child, &control, Duration::from_secs(2)).await,
                ChildShutdown::Graceful
            );
        }
    }

    #[tokio::test]
    async fn shutdown_tolerates_a_broken_control_pipe() {
        let (mut child, control) = spawn_shutdown_fixture("exit 0", "exit 0").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            shutdown_child(&mut child, &control, Duration::from_secs(2)).await,
            ChildShutdown::Graceful
        );
    }

    #[tokio::test]
    async fn shutdown_force_kills_only_after_timeout() {
        let (mut child, control) =
            spawn_shutdown_fixture("Start-Sleep -Seconds 30", "sleep 30").await;
        assert_eq!(
            shutdown_child(&mut child, &control, Duration::from_millis(50)).await,
            ChildShutdown::Forced
        );
    }

    #[tokio::test]
    async fn shutdown_timeout_includes_a_blocked_control_send() {
        let mut command = shutdown_fixture_command("Start-Sleep -Seconds 30", "sleep 30");
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn shutdown fixture");
        let (control, _receiver) = mpsc::channel(1);
        control.send(StdinCmd::Idr).await.unwrap();

        assert_eq!(
            shutdown_child(&mut child, &control, Duration::from_millis(50)).await,
            ChildShutdown::Forced
        );
    }

    #[tokio::test]
    async fn framed_reader_reassembles_arbitrarily_fragmented_prefix_and_payload() {
        let payload = b"\0\0\0\x01\x67sps\0\0\0\x01\x65idr".to_vec();
        let mut reader = fragmented_reader(framed(&payload), &[1, 2, 1, 3]).await;
        let au = read_framed_access_unit(&mut reader, NalCodec::H264)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(au.data, payload);
        assert!(au.is_keyframe);
        assert!(read_framed_access_unit(&mut reader, NalCodec::H264)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn framed_reader_handles_large_4k_access_unit() {
        let mut payload = vec![0xAB; 4 * 1024 * 1024];
        payload[..5].copy_from_slice(b"\0\0\0\x01\x41");
        let mut reader = fragmented_reader(framed(&payload), &[1, 7, 31, 1024, 4093]).await;
        let au = read_framed_access_unit(&mut reader, NalCodec::H264)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(au.data, payload);
        assert!(!au.is_keyframe);
    }

    #[tokio::test]
    async fn framed_reader_rejects_truncated_prefix() {
        let mut reader = fragmented_reader(vec![0, 0, 1], &[1]).await;
        let error = read_framed_access_unit(&mut reader, NalCodec::H264)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn framed_reader_rejects_truncated_payload() {
        let mut bytes = 10u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        let mut reader = fragmented_reader(bytes, &[1, 2]).await;
        let error = read_framed_access_unit(&mut reader, NalCodec::H264)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn framed_reader_rejects_oversized_payload_before_allocating() {
        let bytes = ((MAX_ACCESS_UNIT_SIZE + 1) as u32).to_be_bytes().to_vec();
        let mut reader = fragmented_reader(bytes, &[1]).await;
        let error = read_framed_access_unit(&mut reader, NalCodec::H264)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
