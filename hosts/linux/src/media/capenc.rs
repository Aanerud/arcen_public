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

use arcen_media::video::capture_handoff::{PipeWireCaptureHandoff, ScreenCastCursorMode};
pub use arcen_media::video::ResolvedMediaPlan;
use arcen_media::video::{
    adaptive_codec_ladder, parse_ready_v1, parse_unavailable_v1, pipeline_for_request,
    resolve_media_plan_degrading, BackendAvailability, BackendCandidate, BackendLimits,
    CaptureAdmissionNotice, EncoderBackend, EncoderRequest, MediaRequest, PipelineId,
    PlanDegradation, PresetAdmission, ReadyExpectation, StreamingPreset, VideoVariant,
    ADMISSION_PREFIX,
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

/// The capture hand-off a Wayland session put into the child's environment,
/// when the child captures a Wayland session (Xorg sessions carry none).
fn wayland_handoff(config: &CapencConfig) -> Option<PipeWireCaptureHandoff> {
    let environment = &config.execution.as_ref()?.environment;
    PipeWireCaptureHandoff::from_env(|name| environment.get(name).map(str::to_owned))
        .ok()
        .flatten()
}

/// A Wayland session decides cursor truth when it starts its stream: an
/// `embedded` cursor is in the pixels (host authority), `metadata` leaves it
/// to the Deck. capenc refuses a request that disagrees, so the request
/// follows the stream.
fn apply_wayland_cursor_truth(mut config: CapencConfig) -> CapencConfig {
    let Some(handoff) = wayland_handoff(&config) else {
        return config;
    };
    let authority = handoff.cursor_mode().cursor_authority();
    if config.cursor_mode != authority {
        tracing::info!(
            target: target::CAPENC,
            requested_cursor = ?config.cursor_mode,
            active_cursor = ?authority,
            stream_cursor_mode = handoff.cursor_mode().token(),
            "the Wayland session's stream decides cursor authority"
        );
        config.cursor_mode = authority;
    }
    config
}

fn apply_capture_cursor_constraint(config: CapencConfig) -> (CapencConfig, bool) {
    // A Wayland stream's cursor is whatever the session's stream decided:
    // an embedded cursor is composed by the compositor at any bit depth, so
    // the Xorg wide-capture constraint below does not apply.
    if wayland_handoff(&config).is_some() {
        return (apply_wayland_cursor_truth(config), false);
    }
    let mut config = config;
    let moved_to_local = matches!(config.cursor_mode, CursorMode::Host)
        && !arcen_capenc::linux_capture_supports_host_cursor(config.bit_depth);
    if moved_to_local {
        config.cursor_mode = CursorMode::Local;
    }
    (config, moved_to_local)
}

#[derive(Debug, Error)]
pub enum CapencStartError {
    #[error("invalid capenc configuration: {0}")]
    InvalidConfig(String),
    #[error("{0}")]
    PipelineRefused(&'static str),
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
    /// Resolved transfer characteristics to encode and signal.
    pub transfer: TransferCharacteristics,
    /// Resolved colour primaries to encode and signal.
    pub color_primaries: ColorPrimaries,
    /// What the captured desktop's code values mean. Sent whatever the
    /// output contract is, because a PQ desktop stays PQ when the session is
    /// SDR: capenc converts it (the shared desktop rule) instead of
    /// relabelling it.
    pub desktop_encoding: arcen_media::video::DesktopSignalEncoding,
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
    /// What later bitrate controllers should preserve first under pressure.
    pub motion_priority: arcen_media::video::MotionPriority,
    /// Damage-driven QP biasing to request. Roster-wide; see
    /// `docs/architecture/qp-maps.md`.
    pub qp_map: arcen_media::video::QpMapPolicy,
    /// Mirrors the Pier's effective profile so capenc's debug-only stderr
    /// diagnostics follow `pier.json` `logging.level`.
    pub debug_diagnostics: bool,
    /// Optional native encoder ceiling from the served pipeline contract.
    pub encoder_max_bitrate_bps: Option<u32>,
    /// Exact configured capture width expected in READY.
    pub width: u32,
    /// Exact configured capture height expected in READY.
    pub height: u32,
    /// Cursor authority fixed before the NvFBC session starts.
    pub cursor_mode: CursorMode,
    /// A sustained source rate this host's qualification measured for the
    /// stream's size (`capture-rate=`): Speed admission evidence for a
    /// Wayland session. Never sent to an Xorg capture.
    pub capture_rate: Option<u32>,
    /// What `capture_rate` rests on: measured on this output, or inferred
    /// from a larger qualified one (`capture-rate-evidence=inferred:WxH`,
    /// sent only then).
    pub capture_rate_evidence: arcen_media::video::CaptureRateEvidence,
    /// The host's Wayland Speed evidence per codec token (rate and what it
    /// rests on), used when `capture_rate` is unset: the codec a session
    /// finally encodes with is known only after the Deck's request, so the
    /// argv takes the entry for [`Self::codec`] at spawn time.
    pub capture_rates_by_codec: Vec<(String, u32, arcen_media::video::CaptureRateEvidence)>,
    /// The runtime guard found a Speed epoch of this attachment below
    /// Speed's floor (`speed-runtime=below-floor`): a Speed request is served
    /// as Auto with `speed_runtime_below_floor`. Wayland only.
    pub speed_runtime_shortfall: bool,
    /// This host's qualification verified capenc's `gpu-convert` DMA-BUF
    /// import against a host-copy reference (`gpu-import=verified`): only
    /// then does a Wayland capture offer DMA-BUF. Never sent to an Xorg
    /// capture.
    pub gpu_import_verified: bool,
    /// The host's wide-composition proof as capenc tokens
    /// (`fidelity-route=`, `wide-proven=`, `sdr-white=`), from its
    /// qualification record: sent only to a ten-bit Wayland capture, whose
    /// Grading or HDR composition admits nothing those facts do not prove.
    pub wayland_fidelity_args: Vec<String>,
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
            primaries: self.color_primaries,
            transfer: self.transfer,
        }
    }

    /// Carry the negotiated single-monitor contract into a later respawn,
    /// such as display resize, instead of rebuilding from stale host defaults.
    pub fn pinned_to_active_plan(
        mut self,
        plan: &ResolvedMediaPlan,
        encoder: EncoderRequest,
        intent: EncodeIntent,
        motion_priority: arcen_media::video::MotionPriority,
    ) -> Self {
        self.codec = plan.codec_token().to_string();
        self.yuv444 = matches!(plan.video.chroma, ChromaSubsampling::Yuv444);
        self.bit_depth = plan.video.bit_depth;
        self.color_range = plan.video.range;
        self.color_matrix = plan.video.matrix;
        self.transfer = plan.video.transfer;
        self.color_primaries = plan.video.primaries;
        self.fps = plan.fps;
        self.encoder = encoder;
        self.intent = intent;
        self.motion_priority = motion_priority;
        self
    }

    /// Whether this capture reports cursor images: a Wayland stream that
    /// records its cursor as metadata under Deck-drawn authority. Such a
    /// child writes framed-v2 stdout (`arcen_media::capenc_stream`).
    pub(crate) fn cursor_events(&self) -> bool {
        self.cursor_mode == CursorMode::Local
            && wayland_handoff(self)
                .is_some_and(|handoff| handoff.cursor_mode() == ScreenCastCursorMode::Metadata)
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
    pub(crate) fn argv(&self) -> Vec<String> {
        let mut v = vec![
            self.output_index.to_string(),
            self.codec.clone(),
            self.fps.to_string(),
        ];
        if self.yuv444 {
            v.push("yuv444".to_string());
        }
        v.push(if self.cursor_events() {
            arcen_media::capenc_stream::FRAMED_OUTPUT_V2.to_string()
        } else {
            FRAMED_OUTPUT_V1.to_string()
        });
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
            if let Some(max_bitrate_bps) = self.encoder_max_bitrate_bps {
                v.push(format!("max-bitrate={max_bitrate_bps}"));
            }
        }
        // Transfer and primaries ride as their own tokens because
        // `variant=<id>` names codec, chroma, depth, range and matrix and has
        // no room for them. Emitted only when they are not the BT.709 default,
        // so every session that never asked for HDR produces the argv it
        // always did -- and the argv assertions that pin those commands stay
        // valid.
        if self.transfer != TransferCharacteristics::Bt709 {
            v.push(format!("transfer={}", self.transfer.token()));
        }
        // The lab pipe serves only PQ sessions. Otherwise the desktop's own
        // encoding is sent whenever it is not SDR, so capenc can pass PQ
        // through or convert it; absent means SDR.
        let pipe = crate::cli::experimental_rgb10_pipe()
            .filter(|_| self.transfer == TransferCharacteristics::Pq);
        if let Some(pipe) = pipe {
            v.push(format!("rgb10-pipe={}", pipe.display()));
        } else if self.desktop_encoding != arcen_media::video::DesktopSignalEncoding::Sdr {
            v.push(format!(
                "desktop-encoding={}",
                self.desktop_encoding.token()
            ));
        }
        if self.color_primaries != ColorPrimaries::Bt709 {
            v.push(format!("primaries={}", self.color_primaries.token()));
        }
        // Only when it is not the default: an absent `intent=` already means
        // `Interactive` to `requested_intent`, so emitting it unconditionally
        // would change the argv of every session that never asked for a
        // different intent — and invalidate the argv assertions that pin
        // those unchanged commands — while telling capenc nothing new.
        if self.intent != EncodeIntent::default() {
            v.push(format!("intent={}", self.intent.token()));
        }
        if self.motion_priority != arcen_media::video::MotionPriority::default() {
            v.push(format!("priority={}", self.motion_priority.token()));
        }
        // Same conditional shape and same reason as `intent=`: absent already
        // means off, so a host that never opted into the experiment produces
        // the argv it always did.
        if self.qp_map != arcen_media::video::QpMapPolicy::default() {
            v.push(format!("qp-map={}", self.qp_map.token()));
        }
        // Only a Wayland capture admits Speed on measured rate evidence; the
        // Xorg paths have no such argument.
        let rate = self
            .capture_rate
            .map(|rate| (rate, self.capture_rate_evidence))
            .or_else(|| {
                self.capture_rates_by_codec
                    .iter()
                    .find(|(codec, ..)| *codec == self.codec)
                    .map(|(_, rate, evidence)| (*rate, *evidence))
            });
        if let (Some((rate, evidence)), Some(_)) = (rate, wayland_handoff(self)) {
            v.push(format!("capture-rate={rate}"));
            if evidence.is_inferred() {
                v.push(format!("capture-rate-evidence={}", evidence.token()));
            }
        }
        if self.speed_runtime_shortfall && wayland_handoff(self).is_some() {
            v.push("speed-runtime=below-floor".to_owned());
        }
        if self.gpu_import_verified && wayland_handoff(self).is_some() {
            v.push("gpu-import=verified".to_owned());
        }
        if self.bit_depth == BitDepth::Ten && wayland_handoff(self).is_some() {
            v.extend(self.wayland_fidelity_args.iter().cloned());
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

#[cfg(test)]
async fn wait_for_ready<R>(
    reader: &mut crate::bounded_io::BoundedLineReader<R>,
    config: &CapencConfig,
) -> Result<ResolvedMediaPlan, CapencStartError>
where
    R: AsyncBufRead + Unpin,
{
    wait_for_admitted_ready(reader, config)
        .await
        .map(|(plan, _)| plan)
}

const fn streaming_preset(pipeline: PipelineId) -> Option<StreamingPreset> {
    match pipeline {
        PipelineId::Auto => Some(StreamingPreset::Auto),
        PipelineId::Speed => Some(StreamingPreset::Speed),
        PipelineId::Grading => Some(StreamingPreset::Grading),
        PipelineId::Hdr => Some(StreamingPreset::Hdr),
        PipelineId::Software => None,
    }
}

/// Checks a capture admission against the request this config made and
/// returns the frame rate `READY` must then state.
///
/// A named preset request accepts the shared admission's permitted answers
/// (a visible Speed-to-Auto downgrade, a rate clamp); an exact request
/// accepts only exact admission.
fn admitted_fps(
    notice: &CaptureAdmissionNotice,
    config: &CapencConfig,
) -> Result<u32, CapencStartError> {
    let protocol = |detail: String| CapencStartError::ReadyProtocol(detail);
    let codec = config.codec_enum()?;
    let expected = pipeline_for_request(
        config.video_selection,
        config.video_configuration(codec),
        config.fps,
        config.motion_priority,
    )
    .and_then(streaming_preset);
    let served = notice
        .check_request(
            expected.unwrap_or(notice.requested),
            config.fps,
            codec,
            config.cursor_mode,
        )
        .map_err(|error| protocol(format!("capture admission: {error}")))?;
    if expected.is_none()
        && (!matches!(notice.admission, PresetAdmission::Admit(_))
            || served.fps_ceiling != config.fps)
    {
        return Err(protocol(
            "capture admission changed an exact request".to_string(),
        ));
    }
    Ok(served.fps_ceiling)
}

fn log_admission(notice: &CaptureAdmissionNotice, config: &CapencConfig) {
    let Some(served) = notice.served() else {
        tracing::warn!(
            target: target::CAPENC,
            sid = %config.session_log_id,
            requested = notice.requested.pipeline().token(),
            decision = notice.admission.decision_token(),
            capture = notice.capture.ready_token(),
            copy_path = notice.copy_path.token(),
            "capture source cannot serve the requested preset"
        );
        return;
    };
    let degradation = notice.degradation().unwrap_or_default();
    let reason = match notice.admission {
        PresetAdmission::Downgrade { reason, .. } => Some(reason.to_string()),
        _ => served.fps_limited_by.map(|reason| reason.to_string()),
    };
    if degradation.is_exact() {
        tracing::info!(
            target: target::CAPENC,
            sid = %config.session_log_id,
            requested = notice.requested.pipeline().token(),
            served = served.pipeline.token(),
            fps = served.fps_ceiling,
            capture = notice.capture.ready_token(),
            copy_path = notice.copy_path.token(),
            colour_assumed = served.colour_assumed,
            cursor = notice.cursor.token(),
            node = notice.frame.node_id,
            format = notice.frame.format.token(),
            memory = notice.frame.memory.token(),
            "capture admitted the requested preset"
        );
    } else {
        tracing::warn!(
            target: target::CAPENC,
            sid = %config.session_log_id,
            requested = notice.requested.pipeline().token(),
            served = served.pipeline.token(),
            decision = notice.admission.decision_token(),
            reason = reason.as_deref().unwrap_or("-"),
            fps = served.fps_ceiling,
            fps_clamped = degradation.fps_clamped,
            measured_fps = ?notice.measured_fps,
            capture = notice.capture.ready_token(),
            copy_path = notice.copy_path.token(),
            colour_assumed = served.colour_assumed,
            cursor = notice.cursor.token(),
            node = notice.frame.node_id,
            "capture serves a degraded plan for the requested preset"
        );
    }
}

/// What the child stated about admission so far.
enum AdmissionSeen {
    /// No `ADMISSION` line: `READY` must match the request exactly.
    None,
    /// A served admission and the rate `READY` must state.
    Served(Box<CaptureAdmissionNotice>, u32),
    /// The source cannot serve the request: only `UNAVAILABLE` (or the
    /// child's exit) may follow.
    Unavailable,
}

/// Waits for READY, accepting one `ADMISSION` line before it from a
/// capture source that decides admission itself (the Wayland provider).
async fn wait_for_admitted_ready<R>(
    reader: &mut crate::bounded_io::BoundedLineReader<R>,
    config: &CapencConfig,
) -> Result<(ResolvedMediaPlan, Option<CaptureAdmissionNotice>), CapencStartError>
where
    R: AsyncBufRead + Unpin,
{
    let mut last_error = None;
    let mut admission = AdmissionSeen::None;
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
                if line.starts_with(ADMISSION_PREFIX) {
                    let (notice, sid) = CaptureAdmissionNotice::parse(&line).map_err(|error| {
                        CapencStartError::ReadyProtocol(format!("ADMISSION: {error}"))
                    })?;
                    if sid.as_deref() != Some(config.session_log_id.as_str()) {
                        return Err(CapencStartError::ReadyProtocol(
                            "ADMISSION names another session".to_string(),
                        ));
                    }
                    if !matches!(admission, AdmissionSeen::None) {
                        return Err(CapencStartError::ReadyProtocol(
                            "duplicate ADMISSION".to_string(),
                        ));
                    }
                    log_admission(&notice, config);
                    admission = if notice.served().is_some() {
                        let fps = admitted_fps(&notice, config)?;
                        AdmissionSeen::Served(Box::new(notice), fps)
                    } else {
                        // Only UNAVAILABLE or the end of the child may follow.
                        AdmissionSeen::Unavailable
                    };
                    continue;
                }
                if line.starts_with("[capenc] READY ") {
                    let mut request = config.media_request()?;
                    match &admission {
                        AdmissionSeen::None => {}
                        AdmissionSeen::Served(_, fps) => request.fps = *fps,
                        AdmissionSeen::Unavailable => {
                            return Err(CapencStartError::ReadyProtocol(
                                "READY after an unavailable ADMISSION".to_string(),
                            ));
                        }
                    }
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
                    // Level 3. Records the path that *ran*, which is not
                    // knowable from configuration: the Windows DDA -> WGC
                    // fallback is silent, and on Linux the NvFBC and XShm
                    // engines are separate binaries' worth of behaviour behind
                    // one `backend=` token that names only the *encoder*.
                    //
                    // `None` means the capenc predates the field, and is
                    // reported as such rather than guessed.
                    match arcen_media::video::parse_ready_capture(&line) {
                        Some(capture) => tracing::debug!(
                            target: target::CAPENC,
                            sid = %config.session_log_id,
                            event = "capture_path_selected",
                            capture = capture.ready_token(),
                            zero_copy = capture.zero_copy(),
                            encoder = plan.backend.ready_token(),
                            bit_depth = plan.bit_depth_token(),
                            "capture path selected",
                        ),
                        None => tracing::debug!(
                            target: target::CAPENC,
                            sid = %config.session_log_id,
                            event = "capture_path_selected",
                            capture = "unreported",
                            encoder = plan.backend.ready_token(),
                            "capenc did not name its capture path",
                        ),
                    }
                    forward_capenc_line(&line);
                    let notice = match admission {
                        AdmissionSeen::Served(notice, _) => Some(*notice),
                        AdmissionSeen::None | AdmissionSeen::Unavailable => None,
                    };
                    return Ok((plan, notice));
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
    /// Apply a live bitrate target.
    Bitrate(u64),
    /// Apply a live frame-rate target.
    Framerate(u32),
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
    idr_tx: std::sync::Arc<std::sync::RwLock<mpsc::UnboundedSender<()>>>,
    control_tx: std::sync::Arc<std::sync::RwLock<mpsc::Sender<StdinCmd>>>,
}

#[derive(Clone)]
pub struct BitrateRequester {
    tx: std::sync::Arc<std::sync::RwLock<mpsc::Sender<StdinCmd>>>,
}

impl IdrRequester {
    fn new(idr_tx: mpsc::UnboundedSender<()>, control_tx: mpsc::Sender<StdinCmd>) -> Self {
        Self {
            idr_tx: std::sync::Arc::new(std::sync::RwLock::new(idr_tx)),
            control_tx: std::sync::Arc::new(std::sync::RwLock::new(control_tx)),
        }
    }

    /// Request one keyframe. Non-blocking and independent of bitrate/framerate
    /// control commands, so client recovery is never hidden behind a full
    /// control mailbox.
    pub fn request(&self) -> bool {
        let tx = self
            .idr_tx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match tx.send(()) {
            Ok(()) => {
                tracing::debug!(target: target::CAPENC, "IDR requested");
                true
            }
            Err(mpsc::error::SendError(_)) => {
                tracing::warn!(target: target::CAPENC, "IDR request dropped — capenc not running");
                false
            }
        }
    }

    /// Point this requester — and every clone sharing its slot — at a newly
    /// spawned capenc. Used by the mid-session stream resize after the old
    /// child was shut down.
    pub fn retarget(&self, session: &CapencSession) {
        let mut idr_tx = self
            .idr_tx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *idr_tx = session.idr_tx.clone();
        let mut control_tx = self
            .control_tx
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *control_tx = session.stdin_tx.clone();
    }
}

impl BitrateRequester {
    fn new(idr: &IdrRequester) -> Self {
        Self {
            tx: std::sync::Arc::clone(&idr.control_tx),
        }
    }

    pub fn request(&self, bps: u64) -> bool {
        self.send(StdinCmd::Bitrate(bps))
    }

    pub fn request_framerate(&self, fps: u32) -> bool {
        self.send(StdinCmd::Framerate(fps))
    }

    fn send(&self, command: StdinCmd) -> bool {
        let tx = self
            .tx
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match tx.try_send(command) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => false,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// What capenc stated about its capture: the admission before `READY` and
/// the latest pipeline statistics.
struct ObservedCapture {
    admission: Option<CaptureAdmissionNotice>,
    stats: tokio::sync::watch::Receiver<Option<PipelineStatsSample>>,
}

/// A running `capenc` session: take the access-unit stream with
/// [`take_frames`](Self::take_frames), request keyframes via [`idr`](Self::idr),
/// and call [`shutdown`](Self::shutdown) (or drop) to stop the child.
pub struct CapencSession {
    frames: Option<mpsc::Receiver<AccessUnit>>,
    /// Boxed together so the session stays small inside `CapencHandle`.
    observed: Box<ObservedCapture>,
    idr: IdrRequester,
    child: Child,
    idr_tx: mpsc::UnboundedSender<()>,
    stdin_tx: mpsc::Sender<StdinCmd>,
}

impl CapencSession {
    /// Clone the keyframe-request handle (for IDR-on-drop and `request_full_frame`).
    pub fn idr(&self) -> IdrRequester {
        self.idr.clone()
    }

    pub fn bitrate(&self) -> BitrateRequester {
        BitrateRequester::new(&self.idr)
    }

    /// The capture admission the child stated before READY, when its
    /// source decides admission itself (the Wayland provider): requested
    /// and served preset, copy path, colour and cursor truth, and the
    /// first real frame that proves the session's capture step.
    pub fn capture_admission(&self) -> Option<&CaptureAdmissionNotice> {
        self.observed.admission.as_ref()
    }

    /// Follows capenc's once-a-second pipeline statistics: the latest
    /// sample, `None` until the first.
    pub fn pipeline_stats(&self) -> tokio::sync::watch::Receiver<Option<PipelineStatsSample>> {
        self.observed.stats.clone()
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

/// Announce a fallback (another hardware codec, or software encode), and say
/// exactly what it cost.
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
    if resolved.encoder == EncoderRequest::NativeNvenc {
        tracing::warn!(
            target: target::CAPENC,
            backend = EncoderBackend::NativeNvenc.ready_token(),
            requested = %requested,
            served = %served,
            unavailable_reason = reason.unwrap_or("explicitly requested"),
            codec_changed = degradation.codec_changed,
            chroma_changed = degradation.chroma_changed,
            "requested codec unavailable on the hardware encoder; trying another hardware codec"
        );
        return;
    }
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

fn native_probe_attempt(config: &CapencConfig) -> CapencConfig {
    let (mut attempt, _) = apply_capture_cursor_constraint(config.clone());
    attempt.encoder = EncoderRequest::NativeNvenc;
    attempt
}

/// Whether a native encoder opens for `config` without capturing: `None`
/// when capenc's `probe-v1` reports it available, otherwise its
/// `UNAVAILABLE` line. The Wayland qualification records the answer so a
/// host without one sizes and serves its sessions in software.
pub(crate) async fn probe_native_encoder(
    config: &CapencConfig,
) -> Result<Option<String>, CapencStartError> {
    probe_native(config).await
}

async fn probe_native(config: &CapencConfig) -> Result<Option<String>, CapencStartError> {
    let attempt = native_probe_attempt(config);
    let mut args = attempt.argv();
    args.push("probe-v1".to_string());
    let mut command = crate::command_for_helper(&attempt.binary, "capenc");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_child_environment(&mut command, &attempt)?;
    let mut child = command.spawn().map_err(CapencStartError::Spawn)?;
    let stderr = child.stderr.take().ok_or_else(|| {
        CapencStartError::InvalidConfig("native probe has no stderr pipe".to_string())
    })?;
    let expected = format!(
        "[capenc] PROBE version=1 backend=native-nvenc available=true sid={}",
        attempt.session_log_id
    );
    let outcome = tokio::time::timeout(CAPENC_PROBE_TIMEOUT, async {
        let mut reader = crate::bounded_io::BoundedLineReader::new(BufReader::new(stderr));
        while let Some(bounded) = reader.next_bounded_line().await.map_err(|error| {
            CapencStartError::ExitedBeforeReady(format!(": probe stderr read failed: {error}"))
        })? {
            if bounded.truncated {
                tracing::warn!(
                    target: target::CAPENC,
                    sid = %attempt.session_log_id,
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
/// Why a Wayland session's software fallback is refused: its output was
/// created at its size before any encoder existed and cannot be fitted now.
pub(crate) const WAYLAND_SOFTWARE_GEOMETRY_REFUSAL: &str =
    "the Wayland session's output is larger than the software encoder's 1920x1200 and \
     no hardware encoder is available";

/// What the session's own stream says about the cursor, for a Wayland
/// capture: an embedded cursor is in the pixels whatever the encoder, so
/// OpenH264 carries `cursor=host` there (`BackendLimits::with_source_cursor`).
fn wayland_source_cursor(config: &CapencConfig) -> Option<arcen_media::video::CursorEvidence> {
    wayland_handoff(config).map(|handoff| handoff.cursor_mode().cursor_evidence())
}

fn software_fallback_config(
    config: CapencConfig,
) -> Result<(CapencConfig, PlanDegradation), CapencStartError> {
    // A Wayland stream decides cursor truth before anything is resolved.
    let mut config = apply_wayland_cursor_truth(config);
    let source_cursor = wayland_source_cursor(&config);
    let requested_codec = config.codec_enum()?;
    let requested_pipeline = arcen_media::video::pipeline_for_request(
        config.video_selection,
        config.video_configuration(requested_codec),
        config.fps,
        config.motion_priority,
    );
    let software_decision =
        arcen_media::video::software_fallback_decision(requested_pipeline, config.video_selection);
    if software_decision.is_refusal() {
        return Err(CapencStartError::PipelineRefused(
            software_decision.reason(),
        ));
    }
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

    let limits = source_cursor.map_or(OPENH264_LIMITS, |cursor| {
        OPENH264_LIMITS.with_source_cursor(cursor)
    });
    if source_cursor.is_some()
        && (config.width > limits.max_width || config.height > limits.max_height)
    {
        tracing::warn!(
            target: target::CAPENC,
            width = config.width,
            height = config.height,
            max_width = limits.max_width,
            max_height = limits.max_height,
            "software encode cannot serve this Wayland session: its output was created at a \
             size OpenH264 cannot encode, and a Wayland output is not resized for a fallback"
        );
        return Err(CapencStartError::PipelineRefused(
            WAYLAND_SOFTWARE_GEOMETRY_REFUSAL,
        ));
    }
    let candidates = [BackendCandidate {
        backend: EncoderBackend::OpenH264,
        availability: BackendAvailability::Available(limits),
    }];
    let (plan, mut degradation) =
        resolve_media_plan_degrading(request, &candidates).map_err(|error| {
            CapencStartError::InvalidConfig(format!(
                "OpenH264 fallback cannot serve the request: {error}"
            ))
        })?;

    config.encoder = EncoderRequest::SoftwareH264;
    // OpenH264 has no per-block QP-map API, and capenc's software paths
    // refuse a map rather than ignore it; an operator's map for the
    // requested pipeline therefore does not follow the stream into software.
    if config.qp_map.submits_map() {
        tracing::info!(
            target: target::CAPENC,
            requested_qp_map = config.qp_map.token(),
            "software encode has no QP map; serving with qp-map=off"
        );
        config.qp_map = arcen_media::video::QpMapPolicy::Off;
    }
    config.codec = plan.codec_token().to_string();
    config.yuv444 = matches!(plan.video.chroma, ChromaSubsampling::Yuv444);
    config.bit_depth = plan.video.bit_depth;
    config.color_range = plan.video.range;
    config.color_matrix = plan.video.matrix;
    config.transfer = plan.video.transfer;
    config.color_primaries = plan.video.primaries;
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
    let (config, cursor_moved_to_local) = apply_capture_cursor_constraint(config);
    if cursor_moved_to_local {
        tracing::warn!(
            target: target::CAPENC,
            bit_depth = config.bit_depth.token(),
            requested_cursor = "host",
            active_cursor = "local",
            cursor_moved_to_local = true,
            "Linux wide capture cannot composite the host cursor; using the Deck-local cursor"
        );
    }
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
    let (plan, admission) = match tokio::time::timeout(
        CAPENC_READY_TIMEOUT,
        wait_for_admitted_ready(&mut stderr_lines, &config),
    )
    .await
    {
        Ok(Ok(ready)) => ready,
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
    let (idr_tx, idr_rx) = mpsc::unbounded_channel();

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
    let cursor = config.cursor_events().then(|| {
        wayland_handoff(&config).map(|handoff| CursorTarget {
            hub: crate::media::wayland_cursor::hub(handoff.runtime_dir()),
            connector: handoff.output().as_str().to_owned(),
            generation: handoff.generation().get(),
        })
    });
    match cursor.flatten() {
        Some(cursor) => {
            tokio::spawn(
                read_framed_v2(stdout, nal_codec, frames_tx, cursor)
                    .instrument(helper_span.clone()),
            );
        }
        None => {
            tokio::spawn(read_stdout(stdout, nal_codec, frames_tx).instrument(helper_span.clone()));
        }
    }
    tokio::spawn(write_stdin(stdin, idr_rx, stdin_rx).instrument(helper_span.clone()));
    let (stats_tx, stats) = tokio::sync::watch::channel(None);
    tokio::spawn(read_stderr(stderr_lines, stats_tx).instrument(helper_span));

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
            observed: Box::new(ObservedCapture { admission, stats }),
            idr: IdrRequester::new(idr_tx.clone(), stdin_tx.clone()),
            child,
            idr_tx,
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
    // intentionally replaces the inherited environment. A Wayland session's
    // child captures the hand-off's PipeWire stream and gets no X display.
    if wayland_handoff(config).is_none() {
        if let Some(display) = &config.display {
            command.env("DISPLAY", display);
        }
        if let Some(xauthority) = &config.xauthority {
            command.env("XAUTHORITY", xauthority);
        }
    }
    command.env("ARCEN_SESSION_LOG_ID", config.session_log_id.as_str());
    // Defence in depth for anything that still opens CUDA by ordinal: count
    // GPUs in PCI order, not CUDA's default fastest-first, so ordinals mean
    // the same thing to capenc as to `lspci`. The dedicated session's own GPU
    // arrives by PCI address in the session environment
    // (`ARCEN_CAPTURE_GPU_PCI`), which `configure` applied above.
    command.env("CUDA_DEVICE_ORDER", "PCI_BUS_ID");
    if config.debug_diagnostics {
        command.env("ARCEN_CAPENC_DEBUG", "1");
    }
    Ok(())
}

/// capenc's Wayland capture harness (`wayland-capture-probe ARGS…`) in the
/// same identity and environment a capture of `config` runs with: the
/// session user and its capture hand-off. Qualification uses it to verify
/// the GPU import against a host copy before the route is recorded.
///
/// # Errors
///
/// [`CapencStartError::InvalidConfig`] without a Wayland hand-off, or when
/// the child identity cannot be applied.
pub(crate) fn wayland_capture_probe_command(
    config: &CapencConfig,
    args: &[&str],
) -> Result<Command, CapencStartError> {
    if wayland_handoff(config).is_none() {
        return Err(CapencStartError::InvalidConfig(
            "the capture harness needs a Wayland capture hand-off".to_owned(),
        ));
    }
    let mut command = crate::command_for_helper(&config.binary, "capenc");
    command.arg("wayland-capture-probe").args(args);
    configure_child_environment(&mut command, config)?;
    Ok(command)
}

/// capenc's qualification picture (`wayland-ramp-client pq|sdr`) in the
/// session of `config`'s Wayland hand-off, as the session user: a
/// full-screen ten-bit ramp over a white patch, PQ-tagged or untagged.
pub(crate) fn wayland_ramp_client_command(
    config: &CapencConfig,
    pq: bool,
    seconds: u32,
) -> Result<Command, CapencStartError> {
    if wayland_handoff(config).is_none() {
        return Err(CapencStartError::InvalidConfig(
            "the ramp client needs a Wayland capture hand-off".to_owned(),
        ));
    }
    let mut command = crate::command_for_helper(&config.binary, "capenc");
    command
        .arg("wayland-ramp-client")
        .arg(if pq { "pq" } else { "sdr" })
        .arg(format!("seconds={seconds}"));
    configure_child_environment(&mut command, config)?;
    Ok(command)
}

/// capenc measuring one frame of `config`'s ten-bit Wayland capture
/// (`fidelity-probe`): the drawn ramp's codes and the white patch, on one
/// `FIDELITY` line, without encoding.
pub(crate) fn wayland_fidelity_probe_command(
    config: &CapencConfig,
) -> Result<Command, CapencStartError> {
    if wayland_handoff(config).is_none() || config.bit_depth != BitDepth::Ten {
        return Err(CapencStartError::InvalidConfig(
            "the fidelity probe needs a ten-bit Wayland capture".to_owned(),
        ));
    }
    let mut command = crate::command_for_helper(&config.binary, "capenc");
    command.args(config.argv()).arg("fidelity-probe");
    configure_child_environment(&mut command, config)?;
    Ok(command)
}

pub(crate) fn admission_probe_command(
    config: &CapencConfig,
) -> Result<tokio::process::Command, CapencStartError> {
    let (config, _) = apply_capture_cursor_constraint(config.clone());
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
    configure_child_environment(&mut command, &config)?;
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

/// Where a capture's cursor events go: its desktop's hub, as the events of
/// one output stream.
struct CursorTarget {
    hub: std::sync::Arc<crate::media::wayland_cursor::WaylandCursorHub>,
    connector: String,
    generation: u64,
}

/// framed-v2 stdout → access units and cursor events. The child runs as
/// the desktop's user, so every record is bounded before it is read; a
/// malformed record ends the stream like a malformed framed-v1 one.
async fn read_framed_v2(
    mut stdout: tokio::process::ChildStdout,
    codec: NalCodec,
    frames_tx: mpsc::Sender<AccessUnit>,
    cursor: CursorTarget,
) {
    loop {
        match read_framed_v2_record(&mut stdout, codec).await {
            Ok(None) => {
                tracing::info!(target: target::CAPENC, "capenc stdout closed (EOF)");
                break;
            }
            Ok(Some(FramedV2Record::AccessUnit(au))) => {
                if frames_tx.send(au).await.is_err() {
                    return;
                }
            }
            Ok(Some(FramedV2Record::Cursor(event))) => {
                cursor
                    .hub
                    .apply(&cursor.connector, cursor.generation, &event);
            }
            Err(e) => {
                tracing::warn!(target: target::CAPENC, error = %e, "invalid framed-v2 capenc stdout");
                break;
            }
        }
    }
}

enum FramedV2Record {
    AccessUnit(AccessUnit),
    Cursor(arcen_media::capenc_stream::CapencCursorEvent),
}

async fn read_framed_v2_record<R>(
    reader: &mut R,
    codec: NalCodec,
) -> std::io::Result<Option<FramedV2Record>>
where
    R: AsyncRead + Unpin,
{
    use arcen_media::capenc_stream::{
        decode_cursor_event, record_body_length, record_kind, CapencRecordKind, RECORD_LENGTH_BYTES,
    };
    let invalid = |error: arcen_media::capenc_stream::CapencRecordError| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
    };
    let mut length = [0u8; RECORD_LENGTH_BYTES];
    let first = reader.read(&mut length[..1]).await?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length[1..]).await?;
    let body_length = record_body_length(length).map_err(invalid)?;
    let mut kind = [0u8; 1];
    reader.read_exact(&mut kind).await?;
    let kind = record_kind(kind[0], body_length).map_err(invalid)?;
    let mut body = vec![0u8; body_length];
    reader.read_exact(&mut body).await?;
    Ok(Some(match kind {
        CapencRecordKind::AccessUnit => {
            if body.len() > MAX_ACCESS_UNIT_SIZE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "framed-v2 access unit too long",
                ));
            }
            let is_keyframe = access_unit_is_keyframe(&body, codec);
            FramedV2Record::AccessUnit(AccessUnit {
                data: body,
                is_keyframe,
            })
        }
        CapencRecordKind::Cursor => {
            FramedV2Record::Cursor(decode_cursor_event(&body).map_err(invalid)?)
        }
    }))
}

/// stdin ← keyframe requests. `Stop` writes the graceful stop command and then
/// drops stdin so older capenc children also exit on EOF.
async fn write_stdin(
    mut stdin: tokio::process::ChildStdin,
    mut idr_rx: mpsc::UnboundedReceiver<()>,
    mut rx: mpsc::Receiver<StdinCmd>,
) {
    loop {
        tokio::select! {
            idr = idr_rx.recv() => {
                if idr.is_none() {
                    if rx.is_closed() {
                        break;
                    }
                    continue;
                }
                if let Err(e) = stdin.write_all(b"IDR\n").await.and(stdin.flush().await) {
                    tracing::warn!(target: target::CAPENC, error = %e, "capenc IDR write failed");
                    break;
                }
            }
            cmd = rx.recv() => {
                let Some(cmd) = cmd else {
                    if idr_rx.is_closed() {
                        break;
                    }
                    continue;
                };
                match cmd {
            StdinCmd::Bitrate(bps) => {
                let line = arcen_media::capenc_control::CapencControlCommand::Bitrate { bps }
                    .as_wire_line();
                if let Err(e) = stdin
                    .write_all(line.as_bytes())
                    .await
                    .and(stdin.flush().await)
                {
                    tracing::warn!(target: target::CAPENC, error = %e, "capenc bitrate write failed");
                    break;
                }
            }
            StdinCmd::Framerate(fps) => {
                let line = arcen_media::capenc_control::CapencControlCommand::Framerate { fps }
                    .as_wire_line();
                if let Err(e) = stdin
                    .write_all(line.as_bytes())
                    .await
                    .and(stdin.flush().await)
                {
                    tracing::warn!(target: target::CAPENC, error = %e, "capenc framerate write failed");
                    break;
                }
            }
            StdinCmd::Stop => {
                let _ = stdin.write_all(b"STOP\n").await.and(stdin.flush().await);
                break;
            }
                }
            }
        }
    }
    // Dropping `stdin` here closes the pipe → capenc sees EOF and exits.
    drop(stdin);
    tracing::debug!(target: target::CAPENC, "capenc stdin closed");
}

/// One of capenc's once-a-second pipeline statistics lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineStatsSample {
    /// Counts lines seen, so equal consecutive samples stay distinguishable.
    pub sequence: u64,
    /// Access units emitted in that second (`enc_fps`).
    pub encoded: u32,
    /// Frames the capture source delivered in that second (`source_fps`),
    /// when the capture path reports it (the Wayland compositions do).
    pub source: Option<u32>,
    /// Why that second's frame rate was what it was (`fps_limiter`), when the
    /// capture loop classifies it (the damage-gated Xorg wide and Wayland
    /// loops do).
    pub limiter: Option<arcen_media::video::FrameRateLimiter>,
    /// The loop's pacing target in that second (`target_fps`).
    pub target_fps: Option<u32>,
}

impl PipelineStatsSample {
    /// Parses a `[capenc] enc_fps=…` line.
    #[must_use]
    pub fn parse(line: &str, sequence: u64) -> Option<Self> {
        let field = |name: &str| {
            line.split_whitespace()
                .find_map(|token| token.strip_prefix(name)?.strip_prefix('='))
                .and_then(|value| value.parse::<u32>().ok())
        };
        is_pipeline_stats(line).then_some(())?;
        let limiter = line
            .split_whitespace()
            .find_map(|token| token.strip_prefix("fps_limiter="))
            .and_then(arcen_media::video::FrameRateLimiter::from_token);
        Some(Self {
            sequence,
            encoded: field("enc_fps")?,
            source: field("source_fps"),
            limiter,
            target_fps: field("target_fps"),
        })
    }
}

/// stderr → logs. `capenc` prints `[capenc] …` diagnostics + ~1 Hz stats; the
/// stats lines carry `avg_encode_ms=…` (harvested later for health telemetry)
/// and are also published as [`PipelineStatsSample`]s.
async fn read_stderr<R>(
    mut reader: crate::bounded_io::BoundedLineReader<R>,
    stats: tokio::sync::watch::Sender<Option<PipelineStatsSample>>,
) where
    R: AsyncBufRead + Unpin,
{
    let mut sequence = 0_u64;
    let mut limiter = None;
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
                if let Some(sample) = PipelineStatsSample::parse(&bounded.text, sequence + 1) {
                    sequence += 1;
                    if sample.limiter.is_some() && sample.limiter != limiter {
                        tracing::info!(
                            target: target::CAPENC,
                            previous = limiter.map(arcen_media::video::FrameRateLimiter::token),
                            fps_limiter = sample.limiter.map(arcen_media::video::FrameRateLimiter::token),
                            enc_fps = sample.encoded,
                            target_fps = sample.target_fps,
                            "capenc frame-rate limiter changed"
                        );
                        limiter = sample.limiter;
                    }
                    stats.send_replace(Some(sample));
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
    use super::{BitrateRequester, IdrRequester, StdinCmd};
    use tokio::sync::mpsc;

    /// Build an [`IdrRequester`] backed by an observable channel. The returned
    /// receiver yields one [`StdinCmd`] per `request()` / shutdown so tests can
    /// assert IDR-on-drop throttling.
    pub(crate) fn fake_idr() -> (IdrRequester, mpsc::UnboundedReceiver<()>) {
        let (idr_tx, idr_rx) = mpsc::unbounded_channel();
        let (control_tx, _control_rx) = mpsc::channel::<StdinCmd>(8);
        (IdrRequester::new(idr_tx, control_tx), idr_rx)
    }

    pub(crate) fn fake_bitrate(idr: &IdrRequester) -> BitrateRequester {
        BitrateRequester::new(idr)
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
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Sdr,
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            intent: EncodeIntent::default(),
            motion_priority: arcen_media::video::MotionPriority::Detail,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            debug_diagnostics: false,
            encoder_max_bitrate_bps: None,
            width: 3840,
            height: 2160,
            cursor_mode: CursorMode::Host,
            capture_rate: None,
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec: Vec::new(),
            speed_runtime_shortfall: false,
            gpu_import_verified: false,
            wayland_fidelity_args: Vec::new(),
            display: None,
            xauthority: None,
            execution: None,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    #[test]
    fn capture_children_count_cuda_devices_in_pci_order() {
        let mut command = tokio::process::Command::new("true");
        configure_child_environment(&mut command, &native_config()).expect("configure");
        let order = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("CUDA_DEVICE_ORDER"))
            .and_then(|(_, value)| value);
        assert_eq!(order, Some(std::ffi::OsStr::new("PCI_BUS_ID")));
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
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            desktop_encoding: arcen_media::video::DesktopSignalEncoding::Sdr,
            video_selection: VideoSelectionIntent::Exact,
            codec_pinned: false,
            variant_pinned: false,
            intent: EncodeIntent::default(),
            motion_priority: arcen_media::video::MotionPriority::Detail,
            qp_map: arcen_media::video::QpMapPolicy::default(),
            debug_diagnostics: false,
            encoder_max_bitrate_bps: None,
            width: 1920,
            height: 1080,
            cursor_mode: CursorMode::Local,
            capture_rate: None,
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec: Vec::new(),
            speed_runtime_shortfall: false,
            gpu_import_verified: false,
            wayland_fidelity_args: Vec::new(),
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

    #[test]
    fn wide_capture_moves_host_cursor_to_local_before_spawn() {
        let mut cfg = native_config();
        cfg.bit_depth = BitDepth::Ten;
        let (resolved, moved) = apply_capture_cursor_constraint(cfg);

        assert!(moved);
        assert_eq!(resolved.cursor_mode, CursorMode::Local);
        assert!(
            resolved
                .argv()
                .iter()
                .any(|argument| argument == "cursor=local"),
            "the child must receive the degraded cursor mode"
        );
    }

    #[test]
    fn eight_bit_nvfbc_keeps_the_requested_host_cursor() {
        let cfg = native_config();
        let (resolved, moved) = apply_capture_cursor_constraint(cfg);

        assert!(!moved);
        assert_eq!(resolved.cursor_mode, CursorMode::Host);
    }

    #[test]
    fn native_probe_applies_the_same_wide_cursor_constraint_as_live_spawn() {
        let mut wide = native_config();
        wide.bit_depth = BitDepth::Ten;
        assert!(native_probe_attempt(&wide)
            .argv()
            .iter()
            .any(|argument| argument == "cursor=local"));

        let eight_bit = native_config();
        assert!(native_probe_attempt(&eight_bit)
            .argv()
            .iter()
            .any(|argument| argument == "cursor=host"));
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
        assert!(cfg.argv().iter().any(|arg| arg == "intent=quality"));
        cfg.motion_priority = arcen_media::video::MotionPriority::Motion;
        assert!(cfg.argv().iter().any(|arg| arg == "priority=motion"));
    }

    #[test]
    fn argv_carries_pipeline_encoder_ceiling_only_when_host_resolved_one() {
        let mut cfg = native_config();
        cfg.codec = "h265".to_string();
        cfg.yuv444 = true;
        cfg.bit_depth = BitDepth::Ten;
        cfg.color_range = ColorRange::Full;
        cfg.color_matrix = ColorMatrix::Bt709;
        cfg.transfer = TransferCharacteristics::Bt709;
        cfg.color_primaries = ColorPrimaries::Bt709;

        assert!(
            !cfg.argv().iter().any(|arg| arg.starts_with("max-bitrate=")),
            "a Custom colour match must not infer Grading's ceiling"
        );

        cfg.encoder_max_bitrate_bps =
            arcen_media::video::pipeline_contract(arcen_media::video::PipelineId::Grading)
                .encoder_ceiling_bps();
        assert!(cfg.argv().iter().any(|arg| arg == "max-bitrate=250000000"));

        cfg.color_matrix = ColorMatrix::Bt2020Ncl;
        cfg.encoder_max_bitrate_bps = Some(500_000_000);
        assert!(
            cfg.argv().iter().any(|arg| arg == "max-bitrate=500000000"),
            "any future pipeline ceiling, including HDR, must flow through the generic field"
        );

        let mut auto = native_config();
        auto.encoder_max_bitrate_bps = None;
        assert!(!auto
            .argv()
            .iter()
            .any(|arg| arg.starts_with("max-bitrate=")));
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

    /// capenc writes framed-v2 in every Wayland epoch (NVENC eight-bit, wide
    /// Grading/HDR and OpenH264) exactly when its argv asks for it, so the
    /// reader the Pier picks must follow the same argv for every encoder,
    /// contract and runtime software fallback.
    #[test]
    fn every_wayland_epoch_reads_the_framing_its_argv_asks_for() {
        use arcen_media::capenc_stream::FRAMED_OUTPUT_V2;
        for encoder in [
            EncoderRequest::Auto,
            EncoderRequest::NativeNvenc,
            EncoderRequest::SoftwareH264,
        ] {
            for (bit_depth, transfer) in [
                (BitDepth::Eight, TransferCharacteristics::Bt709),
                (BitDepth::Ten, TransferCharacteristics::Bt709),
                (BitDepth::Ten, TransferCharacteristics::Pq),
            ] {
                for cursor in [
                    ScreenCastCursorMode::Metadata,
                    ScreenCastCursorMode::Embedded,
                ] {
                    let mut config = wayland_config(30, false, cursor);
                    config.encoder = encoder;
                    config.bit_depth = bit_depth;
                    config.transfer = transfer;
                    let mut spawned = vec![apply_wayland_cursor_truth(config.clone())];
                    if let Ok((software, _)) = software_fallback_config(config) {
                        spawned.push(software);
                    }
                    for config in spawned {
                        let argv = config.argv();
                        let v2 = argv.iter().any(|arg| arg == FRAMED_OUTPUT_V2);
                        let v1 = argv.iter().any(|arg| arg == FRAMED_OUTPUT_V1);
                        let case = format!("{encoder:?} {bit_depth:?} {transfer:?} {cursor:?}");
                        assert!(v1 != v2, "{case}: one framing, {argv:?}");
                        assert_eq!(config.cursor_events(), v2, "{case}");
                        assert_eq!(v2, cursor == ScreenCastCursorMode::Metadata, "{case}");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn the_framed_v2_reader_takes_what_capenc_writes_and_refuses_framed_v1() {
        use arcen_media::capenc_stream::{
            encode_cursor_event, encode_record, CapencCursorEvent, CapencCursorSprite,
            CapencRecordKind,
        };
        let access_unit = [0u8, 0, 0, 1, 0x65, 0x88, 0x84];
        let mut stdout = encode_record(CapencRecordKind::AccessUnit, &access_unit).unwrap();
        let event = CapencCursorEvent {
            on_output: false,
            sprite: CapencCursorSprite::Unchanged,
        };
        stdout
            .extend(encode_record(CapencRecordKind::Cursor, &encode_cursor_event(&event)).unwrap());
        let mut reader = stdout.as_slice();
        assert!(matches!(
            read_framed_v2_record(&mut reader, NalCodec::H264).await.unwrap(),
            Some(FramedV2Record::AccessUnit(AccessUnit { ref data, is_keyframe: true })) if data == &access_unit
        ));
        assert!(matches!(
            read_framed_v2_record(&mut reader, NalCodec::H264).await.unwrap(),
            Some(FramedV2Record::Cursor(ref read)) if *read == event
        ));
        assert!(read_framed_v2_record(&mut reader, NalCodec::H264)
            .await
            .unwrap()
            .is_none());
        // framed-v1 (`u32 length || access unit`) is not framed-v2: its
        // first access-unit byte is read as an unknown kind.
        let mut framed_v1 = 7u32.to_be_bytes().to_vec();
        framed_v1.extend_from_slice(&access_unit);
        let mut reader = framed_v1.as_slice();
        assert!(read_framed_v2_record(&mut reader, NalCodec::H264)
            .await
            .is_err());
    }

    #[test]
    fn wayland_software_fallback_keeps_the_streams_cursor_and_size() {
        // The compositor drew the cursor: OpenH264 carries it, and nothing
        // is reported as moved to the Deck.
        let mut embedded = wayland_config(30, false, ScreenCastCursorMode::Embedded);
        embedded.cursor_mode = CursorMode::Host;
        let (resolved, degradation) =
            software_fallback_config(embedded).expect("software serves the stream");
        assert_eq!(resolved.encoder, EncoderRequest::SoftwareH264);
        assert_eq!(resolved.cursor_mode, CursorMode::Host);
        assert!(!degradation.cursor_moved_to_local);
        assert_eq!((resolved.width, resolved.height), (1920, 1080));
        // An operator's QP map does not follow the stream into OpenH264.
        let mut mapped = wayland_config(30, false, ScreenCastCursorMode::Embedded);
        mapped.qp_map = arcen_media::video::QpMapPolicy::On;
        let (resolved, _) = software_fallback_config(mapped).expect("qp map");
        assert_eq!(resolved.qp_map, arcen_media::video::QpMapPolicy::Off);
        assert!(!resolved
            .argv()
            .iter()
            .any(|arg| arg.starts_with("qp-map=on")));
        // A metadata stream keeps the Deck's cursor.
        let (resolved, _) =
            software_fallback_config(wayland_config(30, false, ScreenCastCursorMode::Metadata))
                .expect("metadata");
        assert_eq!(resolved.cursor_mode, CursorMode::Local);
        // Speed is visibly degraded to 30 fps, never refused.
        let (resolved, degradation) =
            software_fallback_config(wayland_config(60, true, ScreenCastCursorMode::Embedded))
                .expect("speed");
        assert_eq!(resolved.fps, 30);
        assert!(degradation.fps_clamped);
        // An output larger than OpenH264's cannot be fitted after the fact.
        let mut large = wayland_config(30, false, ScreenCastCursorMode::Embedded);
        large.width = 2560;
        large.height = 1600;
        assert!(matches!(
            software_fallback_config(large),
            Err(CapencStartError::PipelineRefused(reason))
                if reason == WAYLAND_SOFTWARE_GEOMETRY_REFUSAL
        ));
        // Xorg keeps its rule: the session display was fitted before the
        // modeset, and MIT-SHM has no cursor in its pixels.
        let mut xorg = native_config();
        xorg.codec = "h264".to_string();
        xorg.yuv444 = false;
        xorg.fps = 30;
        xorg.width = 1280;
        xorg.height = 800;
        xorg.cursor_mode = CursorMode::Host;
        let (resolved, degradation) = software_fallback_config(xorg).expect("xorg");
        assert_eq!(resolved.cursor_mode, CursorMode::Local);
        assert!(degradation.cursor_moved_to_local);
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
        let resized = cfg.pinned_to_active_plan(
            &plan,
            EncoderRequest::NativeNvenc,
            EncodeIntent::Quality,
            arcen_media::video::MotionPriority::Detail,
        );
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

    /// What the Pier asks of a Wayland session: an Auto/Speed SDR8 request
    /// with the session's capture hand-off in the child's environment.
    fn wayland_config(fps: u32, motion: bool, cursor: ScreenCastCursorMode) -> CapencConfig {
        let identity = crate::session::identity::UserIdentity {
            username: "artist".into(),
            uid: 1001,
            gid: 100,
            supplementary_groups: vec![100],
            home: "/home/artist".into(),
            shell: "/bin/bash".into(),
        };
        let runtime_dir = crate::session::wayland::private_runtime_dir(1001, "c9");
        let ready = crate::session::wayland::WaylandAgentReady {
            compositor: "gnome-shell".into(),
            input_socket: runtime_dir.join(crate::session::wayland::INPUT_SOCKET_NAME),
            runtime_dir: runtime_dir.clone(),
            wayland_display: "wayland-0".into(),
            outputs: vec![],
            streams: vec![],
            input_device_types: 7,
            generation: 1,
            compositor_render_node: None,
            compositor_gpu_pci: None,
            compositor_gpu_vendor: None,
            output_mode: arcen_session::wayland_outputs::VirtualOutputMode::StartupMonitors,
            output_socket: None,
            x11: None,
            cursor_mode: Default::default(),
            clipboard: None,
        };
        let handoff = PipeWireCaptureHandoff::new(
            36,
            "Meta-0",
            1,
            (1920, 1080),
            (0, 0),
            cursor,
            &runtime_dir,
        )
        .unwrap();
        let environment = crate::session::identity::SessionEnvironment::build_wayland(
            &identity,
            "gnome",
            None,
            [("XDG_SESSION_ID".into(), "c9".into())],
        )
        .with_wayland_session(&ready, &handoff);
        CapencConfig {
            codec: "h264".to_string(),
            fps,
            yuv444: false,
            video_selection: VideoSelectionIntent::AdaptivePerformance,
            motion_priority: if motion {
                arcen_media::video::MotionPriority::Motion
            } else {
                arcen_media::video::MotionPriority::Detail
            },
            width: 1920,
            height: 1080,
            cursor_mode: CursorMode::Local,
            capture_rate: None,
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec: Vec::new(),
            speed_runtime_shortfall: false,
            gpu_import_verified: false,
            wayland_fidelity_args: Vec::new(),
            display: Some(":0".into()),
            xauthority: Some("/home/artist/.Xauthority".into()),
            execution: Some(UserExecution::new(identity, environment)),
            ..native_config()
        }
    }

    fn sid() -> String {
        native_config().session_log_id.to_string()
    }

    fn wayland_ready(fps: u32) -> String {
        format!(
            "[capenc] READY version=2 backend=native-nvenc codec=h264 chroma=yuv420 bit_depth=8 \
             range=limited matrix=bt709 primaries=bt709 transfer=bt709 width=1920 height=1080 \
             fps={fps} supports_h264=true supports_h265=true supports_yuv444=true \
             supports_main10=true supports_full_range=true cursor=host capture=pipewire \
             capture_zero_copy=false conversion_backend=gpu sid={}",
            sid()
        )
    }

    const SPEED_AS_AUTO: &str =
        "[capenc] ADMISSION version=1 requested=speed requested_fps=60 decision=downgrade \
        reason=copy_path_not_rate_qualified served=auto codec=h264 fps=30 transform=sdr8-rgb \
        colour=assumed cursor_mode=host capture=pipewire copy_path=host-copy cursor=composited \
        measured_fps=unmeasured node=36 format=bgrx8 width=1920 height=1080 memory=host-shared";

    async fn admitted(
        config: &CapencConfig,
        lines: &[String],
    ) -> Result<(ResolvedMediaPlan, Option<CaptureAdmissionNotice>), CapencStartError> {
        let (reader, mut writer) = tokio::io::duplex(8192);
        let text = lines.join("\n") + "\n";
        tokio::spawn(async move {
            writer.write_all(text.as_bytes()).await.unwrap();
        });
        let mut lines = crate::bounded_io::BoundedLineReader::new(BufReader::new(reader));
        wait_for_admitted_ready(&mut lines, config).await
    }

    #[test]
    fn a_qualified_capture_rate_reaches_only_a_wayland_capture() {
        let rated = CapencConfig {
            capture_rate: Some(58),
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec: Vec::new(),
            speed_runtime_shortfall: false,
            gpu_import_verified: true,
            wayland_fidelity_args: Vec::new(),
            ..wayland_config(60, true, ScreenCastCursorMode::Embedded)
        };
        assert!(rated.argv().contains(&"capture-rate=58".to_owned()));
        let unrated = wayland_config(60, true, ScreenCastCursorMode::Embedded);
        assert!(!unrated
            .argv()
            .iter()
            .any(|argument| argument.starts_with("capture-rate=")));
        let xorg = CapencConfig {
            capture_rate: Some(58),
            capture_rate_evidence: arcen_media::video::CaptureRateEvidence::Measured,
            capture_rates_by_codec: Vec::new(),
            speed_runtime_shortfall: false,
            gpu_import_verified: true,
            wayland_fidelity_args: Vec::new(),
            ..native_config()
        };
        assert!(!xorg
            .argv()
            .iter()
            .any(|argument| argument.starts_with("capture-rate=")));
        // GPU import verification follows the same rule.
        assert!(rated.argv().contains(&"gpu-import=verified".to_owned()));
        assert!(!unrated
            .argv()
            .iter()
            .any(|argument| argument.starts_with("gpu-import=")));
        assert!(!xorg
            .argv()
            .iter()
            .any(|argument| argument.starts_with("gpu-import=")));
        // Per-codec evidence: the entry for the codec actually spawned, its
        // inference stated; none for a codec without evidence; and a
        // runtime shortfall is passed on.
        let inferred = arcen_media::video::CaptureRateEvidence::Inferred {
            width: 2560,
            height: 1600,
        };
        let by_codec = CapencConfig {
            codec: "h265".to_owned(),
            capture_rates_by_codec: vec![("h265".to_owned(), 60, inferred)],
            speed_runtime_shortfall: true,
            ..wayland_config(60, true, ScreenCastCursorMode::Embedded)
        };
        let argv = by_codec.argv();
        assert!(argv.contains(&"capture-rate=60".to_owned()), "{argv:?}");
        assert!(argv.contains(&"capture-rate-evidence=inferred:2560x1600".to_owned()));
        assert!(argv.contains(&"speed-runtime=below-floor".to_owned()));
        let other_codec = CapencConfig {
            codec: "h264".to_owned(),
            ..by_codec
        };
        assert!(!other_codec
            .argv()
            .iter()
            .any(|argument| argument.starts_with("capture-rate")));
    }

    #[test]
    fn fidelity_arguments_reach_only_a_ten_bit_wayland_capture() {
        let fidelity = vec![
            "fidelity-route=force-hdr".to_owned(),
            "wide-proven=dmabuf".to_owned(),
            "sdr-white=203".to_owned(),
        ];
        let ten_bit = CapencConfig {
            bit_depth: BitDepth::Ten,
            wayland_fidelity_args: fidelity.clone(),
            ..wayland_config(60, false, ScreenCastCursorMode::Embedded)
        };
        let argv = ten_bit.argv();
        assert!(fidelity.iter().all(|argument| argv.contains(argument)));
        let eight_bit = CapencConfig {
            wayland_fidelity_args: fidelity.clone(),
            ..wayland_config(60, false, ScreenCastCursorMode::Embedded)
        };
        let xorg = CapencConfig {
            bit_depth: BitDepth::Ten,
            wayland_fidelity_args: fidelity,
            ..native_config()
        };
        for config in [eight_bit, xorg] {
            assert!(!config.argv().iter().any(|argument| {
                argument.starts_with("fidelity-route=")
                    || argument.starts_with("wide-proven=")
                    || argument.starts_with("sdr-white=")
            }));
        }
    }

    #[test]
    fn pipeline_stats_lines_publish_encoded_and_source_rates() {
        let wayland = "[capenc] enc_fps=30 encode_submitted=30 source_fps=57 source_superseded=27 \
                       avg_upload_ms=0.84 capture_backend=pipewire sid=7a2d";
        assert_eq!(
            PipelineStatsSample::parse(wayland, 3),
            Some(PipelineStatsSample {
                sequence: 3,
                encoded: 30,
                source: Some(57),
                limiter: None,
                target_fps: None,
            })
        );
        let xorg_wide = "[capenc] enc_fps=11 encode_submitted=21 avg_capture_ms=3.80 \
                         capture_backend=xshm avg_convert_ms=3.10 avg_upload_ms=2.40 \
                         avg_upload_kib=21600 fps_limiter=no_damage rate_capped=true \
                         contract_fps=30 target_fps=8 ticks_due=8 ticks_no_change=4 \
                         fresh_frames=4 busy_ms=150 sid=7a2d";
        let sample = PipelineStatsSample::parse(xorg_wide, 4).expect("wide stats line");
        assert_eq!(sample.encoded, 11);
        assert_eq!(
            sample.limiter,
            Some(arcen_media::video::FrameRateLimiter::NoDamage)
        );
        assert_eq!(sample.target_fps, Some(8));
        let xorg = "[capenc] enc_fps=60 avg_encode_ms=2.1 kbps=900";
        assert_eq!(
            PipelineStatsSample::parse(xorg, 1).map(|sample| (sample.encoded, sample.source)),
            Some((60, None))
        );
        assert_eq!(
            PipelineStatsSample::parse("[capenc] READY version=2", 1),
            None
        );
        assert_eq!(PipelineStatsSample::parse("[capenc] enc_fps=x", 1), None);
    }

    #[test]
    fn a_wayland_child_gets_no_x_display_and_follows_the_streams_cursor() {
        let config = wayland_config(30, false, ScreenCastCursorMode::Embedded);
        let mut command = tokio::process::Command::new("true");
        configure_child_environment(&mut command, &config).expect("configure");
        let environment: std::collections::BTreeMap<_, _> = command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned()))
            })
            .collect();
        assert!(!environment.contains_key("DISPLAY"));
        assert!(!environment.contains_key("XAUTHORITY"));
        assert_eq!(
            environment
                .get("ARCEN_CAPTURE_PROVIDER")
                .map(String::as_str),
            Some("wayland")
        );
        let (embedded, _) = apply_capture_cursor_constraint(config);
        assert_eq!(embedded.cursor_mode, CursorMode::Host);
        assert!(embedded.argv().contains(&"cursor=host".to_string()));
        let (metadata, _) = apply_capture_cursor_constraint(wayland_config(
            30,
            false,
            ScreenCastCursorMode::Metadata,
        ));
        assert_eq!(metadata.cursor_mode, CursorMode::Local);
        // Xorg sessions are untouched.
        let (xorg, _) = apply_capture_cursor_constraint(native_config());
        assert_eq!(xorg.cursor_mode, CursorMode::Host);
        // A ten-bit Wayland stream (Grading/HDR) with an embedded cursor
        // keeps host authority: the Xorg wide-capture rule does not apply.
        let mut wide = wayland_config(30, false, ScreenCastCursorMode::Embedded);
        wide.bit_depth = BitDepth::Ten;
        let (wide, moved) = apply_capture_cursor_constraint(wide);
        assert!(!moved);
        assert_eq!(wide.cursor_mode, CursorMode::Host);
    }

    #[tokio::test]
    async fn a_speed_request_served_as_auto_is_accepted_only_through_its_admission() {
        let (config, _) = apply_capture_cursor_constraint(wayland_config(
            60,
            true,
            ScreenCastCursorMode::Embedded,
        ));
        let admission = format!("{SPEED_AS_AUTO} sid={}", sid());
        let (plan, notice) = admitted(&config, &[admission.clone(), wayland_ready(30)])
            .await
            .expect("visible downgrade accepted");
        assert_eq!(plan.fps, 30);
        assert_eq!(plan.cursor_mode, CursorMode::Host);
        let notice = notice.expect("admission kept");
        assert_eq!(notice.served().unwrap().pipeline, PipelineId::Auto);
        assert!(notice.degradation().unwrap().fps_clamped);
        // Without the admission the same READY is a request conflict.
        assert!(matches!(
            admitted(&config, &[wayland_ready(30)]).await,
            Err(CapencStartError::ReadyProtocol(_))
        ));
        // READY must state the admitted rate, not the requested one.
        assert!(matches!(
            admitted(&config, &[admission.clone(), wayland_ready(60)]).await,
            Err(CapencStartError::ReadyProtocol(_))
        ));
        // Another session's admission, or two of them, are refused.
        assert!(matches!(
            admitted(&config, &[SPEED_AS_AUTO.to_string(), wayland_ready(30)]).await,
            Err(CapencStartError::ReadyProtocol(_))
        ));
        assert!(matches!(
            admitted(&config, &[admission.clone(), admission, wayland_ready(30)]).await,
            Err(CapencStartError::ReadyProtocol(_))
        ));
        // An Auto request cannot be answered with a Speed admission.
        let (auto, _) = apply_capture_cursor_constraint(wayland_config(
            30,
            false,
            ScreenCastCursorMode::Embedded,
        ));
        assert!(matches!(
            admitted(
                &auto,
                &[format!("{SPEED_AS_AUTO} sid={}", sid()), wayland_ready(30)]
            )
            .await,
            Err(CapencStartError::ReadyProtocol(_))
        ));
    }

    #[tokio::test]
    async fn an_unavailable_admission_ends_in_the_backend_refusal() {
        let (config, _) = apply_capture_cursor_constraint(wayland_config(
            30,
            false,
            ScreenCastCursorMode::Embedded,
        ));
        let refused = format!(
            "[capenc] ADMISSION version=1 requested=auto requested_fps=30 decision=unavailable \
             reason=gpu_identity_unproven capture=pipewire copy_path=gpu-convert cursor=composited \
             measured_fps=unmeasured node=36 format=bgrx8 width=1920 height=1080 \
             memory=dmabuf modifier=0x0300000000606010 sid={}",
            sid()
        );
        let unavailable =
            "[capenc] UNAVAILABLE version=1 backend=native-nvenc code=unsupported_configuration"
                .to_string();
        assert!(matches!(
            admitted(&config, &[refused.clone(), unavailable]).await,
            Err(CapencStartError::BackendUnavailable(_))
        ));
        // After an unavailable admission, READY is a protocol error even if
        // it matches the request exactly, and so is a second ADMISSION.
        assert!(matches!(
            admitted(&config, &[refused.clone(), wayland_ready(30)]).await,
            Err(CapencStartError::ReadyProtocol(detail)) if detail.contains("unavailable")
        ));
        let served = format!(
            "[capenc] ADMISSION version=1 requested=auto requested_fps=30 decision=admit \
             served=auto codec=h264 fps=30 transform=sdr8-rgb colour=assumed cursor_mode=host \
             capture=pipewire copy_path=host-copy cursor=composited measured_fps=unmeasured \
             node=36 format=bgrx8 width=1920 height=1080 memory=host-shared sid={}",
            sid()
        );
        assert!(matches!(
            admitted(&config, &[refused, served, wayland_ready(30)]).await,
            Err(CapencStartError::ReadyProtocol(detail)) if detail.contains("duplicate")
        ));
    }

    #[tokio::test]
    async fn an_auto_request_below_the_contract_rate_is_served_at_that_rate() {
        let (config, _) = apply_capture_cursor_constraint(wayland_config(
            24,
            false,
            ScreenCastCursorMode::Embedded,
        ));
        let admission = format!(
            "[capenc] ADMISSION version=1 requested=auto requested_fps=24 decision=admit \
             served=auto codec=h264 fps=24 transform=sdr8-rgb colour=assumed cursor_mode=host \
             capture=pipewire copy_path=host-copy cursor=composited measured_fps=unmeasured \
             node=36 format=bgrx8 width=1920 height=1080 memory=host-shared sid={}",
            sid()
        );
        let (plan, notice) = admitted(&config, &[admission.clone(), wayland_ready(24)])
            .await
            .expect("24 fps Auto accepted");
        assert_eq!(plan.fps, 24);
        assert!(notice.unwrap().degradation().unwrap().is_exact());
        // A notice answering a 30 fps request does not answer this one.
        assert!(matches!(
            admitted(
                &config,
                &[
                    admission.replace("requested_fps=24", "requested_fps=30"),
                    wayland_ready(24)
                ]
            )
            .await,
            Err(CapencStartError::ReadyProtocol(_))
        ));
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
        let (idr_tx, idr_rx) = mpsc::unbounded_channel();
        drop(idr_tx);
        tokio::spawn(write_stdin(stdin, idr_rx, rx));
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
        control.send(StdinCmd::Bitrate(1)).await.unwrap();

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
