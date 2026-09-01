//! Supervises the `arcen-capenc` engine as a Rust→Rust subprocess (Stage 1
//! — no Python anywhere). The engine captures the desktop (DXGI Desktop
//! Duplication, auto-falling-back to WGC on vGPU/RDP) and encodes with NVENC,
//! emitting **length-prefixed** access units on stdout: `[u32 BE len][AU]`
//! (the framing fix that fixed the black frame). We read those, detect keyframes
//! per-AU, and forward each AU to the session over a channel.
//!
//! IDR-on-demand is a single `IDR\n` line on the engine's stdin — no restart.
//!
//! Stage 2 folds this engine in-process (shared D3D11 device, no pipe); this
//! module is the seam that disappears then.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arcen_media::video::{
    parse_ready_v1, parse_unavailable_v1, BackendUnavailableNotice, EncoderBackend, EncoderRequest,
    MediaRequest, ReadyExpectation, ResolvedMediaPlan,
};
use arcen_media::{
    BitDepth, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent, TransferCharacteristics,
    VideoConfiguration,
};
use arcen_protocol::messages::CursorMode;
use arcen_protocol::wire::{
    BitDepth as WireBitDepth, ColorMatrix as WireColorMatrix, ColorRange as WireColorRange,
};
use arcen_protocol::{ChromaSubsampling, VideoCodec};
use arcen_telemetry::{CorrelationId, OperationalProfile};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::latest::{VideoPushResult, VideoQueue};
use crate::logging::CAPENC;
use crate::{chroma_name, codec_name};

const MAX_AU_BYTES: u32 = 16 * 1024 * 1024;
const FRAME_QUEUE_CAPACITY: usize = 4;
const CAPENC_READY_TIMEOUT: Duration = Duration::from_secs(10);
const IDR_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(1);
const CAPENC_WRITE_TIMEOUT: Duration = Duration::from_secs(1);
const CAPENC_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PRE_READY_DIAGNOSTICS: usize = 32;

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_ms: u32,
}

pub struct CapencConfig {
    pub binary: String,
    pub output_index: u32,
    pub adapter_name: Option<String>,
    pub adapter_output_index: Option<u32>,
    pub device_name: Option<String>,
    pub codec: VideoCodec,
    pub chroma: ChromaSubsampling,
    pub bit_depth: BitDepth,
    pub color_range: ColorRange,
    pub color_matrix: ColorMatrix,
    /// Resolved transfer characteristics to encode and signal.
    pub transfer: TransferCharacteristics,
    /// Resolved colour primaries to encode and signal.
    pub color_primaries: ColorPrimaries,
    /// Resolved encoder intent to request.
    pub intent: EncodeIntent,
    /// Damage-driven QP biasing to request. Roster-wide; see
    /// `docs/architecture/qp-maps.md`.
    pub qp_map: arcen_media::video::QpMapPolicy,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    /// Encoder backend selection passed to `arcen-capenc` as `encoder=<v>`.
    /// `None` preserves the engine's default (auto: NVENC if available, else
    /// source-built OpenH264).
    pub encoder: Option<EncoderSelection>,
    pub cursor_mode: CursorMode,
    pub session_log_id: CorrelationId,
}

/// Encoder backend the pier asks the capenc engine to use.
///
/// The shipped Pier accepts `encoder=auto|nvenc|software-h264`. Auto probes
/// the NVIDIA encode API. Typed pre-READY NVENC unavailability is returned to
/// the parent so it can retarget OpenH264 geometry before retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderSelection {
    Auto,
    Nvenc,
    SoftwareH264,
}

impl EncoderSelection {
    pub fn as_arg(self) -> &'static str {
        match self {
            EncoderSelection::Auto => "encoder=auto",
            EncoderSelection::Nvenc => "encoder=nvenc",
            EncoderSelection::SoftwareH264 => "encoder=software-h264",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            EncoderSelection::Auto => "auto",
            EncoderSelection::Nvenc => "nvenc",
            EncoderSelection::SoftwareH264 => "software-h264",
        }
    }

    /// Resolve `Auto` against the selected adapter: direct NVENC only for an
    /// NVIDIA-owned output with the encode runtime present, the OpenH264
    /// software path otherwise. Concrete selections pass through.
    ///
    /// The pier resolves `Auto` BEFORE spawning capenc so that display
    /// alignment, the advertised ServerHello capabilities, and the eventual
    /// concrete child all agree.
    pub fn resolve_auto(self, adapter_is_nvidia: bool, nvenc_runtime: bool) -> EncoderSelection {
        match self {
            EncoderSelection::Auto => {
                if adapter_is_nvidia && nvenc_runtime {
                    EncoderSelection::Nvenc
                } else {
                    EncoderSelection::SoftwareH264
                }
            }
            concrete => concrete,
        }
    }

    const fn request(self) -> EncoderRequest {
        match self {
            Self::Auto => EncoderRequest::Auto,
            Self::Nvenc => EncoderRequest::NativeNvenc,
            Self::SoftwareH264 => EncoderRequest::SoftwareH264,
        }
    }
}

/// A running capenc engine. Dropping it kills the child (which also stops when
/// our stdin closes — the engine treats a closed stdin as "parent gone").
pub struct Capenc {
    child: Child,
    idr: IdrRequester,
    control: mpsc::Sender<StdinCommand>,
    pipeline_telemetry: Arc<PipelineTelemetry>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PipelineTelemetrySnapshot {
    pub encoder_fps: f64,
    pub capture_fps: f64,
    pub average_stage_ms: f64,
    pub maximum_stage_ms: f64,
    pub average_readback_ms: f64,
    pub maximum_readback_ms: f64,
    pub average_conversion_ms: f64,
    pub maximum_conversion_ms: f64,
    pub average_encode_ms: f64,
    pub maximum_encode_ms: f64,
    pub slow_frames: u64,
    pub dropped_frames: u64,
}

#[derive(Debug, Default)]
struct PipelineTelemetry {
    encoder_fps: AtomicU64,
    capture_fps: AtomicU64,
    average_stage_ms: AtomicU64,
    maximum_stage_ms: AtomicU64,
    average_readback_ms: AtomicU64,
    maximum_readback_ms: AtomicU64,
    average_conversion_ms: AtomicU64,
    maximum_conversion_ms: AtomicU64,
    average_encode_ms: AtomicU64,
    maximum_encode_ms: AtomicU64,
    slow_frames: AtomicU64,
    dropped_frames: AtomicU64,
}

impl PipelineTelemetry {
    fn record(&self, stats: &PipelineStats) {
        self.encoder_fps.store(
            stats.encoder_fps.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.capture_fps.store(
            stats.capture_fps.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.average_stage_ms.store(
            stats.average_stage_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.maximum_stage_ms.store(
            stats.maximum_stage_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.average_readback_ms.store(
            stats.average_readback_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.maximum_readback_ms.store(
            stats.maximum_readback_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.average_conversion_ms.store(
            stats.average_conversion_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.maximum_conversion_ms.store(
            stats.maximum_conversion_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.average_encode_ms.store(
            stats.average_encode_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.maximum_encode_ms.store(
            stats.maximum_encode_ms.unwrap_or_default().to_bits(),
            Ordering::Relaxed,
        );
        self.slow_frames
            .store(stats.slow_frames.unwrap_or_default(), Ordering::Relaxed);
        self.dropped_frames
            .store(stats.dropped_frames.unwrap_or_default(), Ordering::Relaxed);
    }

    fn snapshot(&self) -> PipelineTelemetrySnapshot {
        PipelineTelemetrySnapshot {
            encoder_fps: f64::from_bits(self.encoder_fps.load(Ordering::Relaxed)),
            capture_fps: f64::from_bits(self.capture_fps.load(Ordering::Relaxed)),
            average_stage_ms: f64::from_bits(self.average_stage_ms.load(Ordering::Relaxed)),
            maximum_stage_ms: f64::from_bits(self.maximum_stage_ms.load(Ordering::Relaxed)),
            average_readback_ms: f64::from_bits(self.average_readback_ms.load(Ordering::Relaxed)),
            maximum_readback_ms: f64::from_bits(self.maximum_readback_ms.load(Ordering::Relaxed)),
            average_conversion_ms: f64::from_bits(
                self.average_conversion_ms.load(Ordering::Relaxed),
            ),
            maximum_conversion_ms: f64::from_bits(
                self.maximum_conversion_ms.load(Ordering::Relaxed),
            ),
            average_encode_ms: f64::from_bits(self.average_encode_ms.load(Ordering::Relaxed)),
            maximum_encode_ms: f64::from_bits(self.maximum_encode_ms.load(Ordering::Relaxed)),
            slow_frames: self.slow_frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub enum CapencStartError {
    StartFailed {
        binary: String,
        args: Vec<String>,
        source: String,
    },
    Unavailable(BackendUnavailableNotice),
    ReadyRejected {
        raw_line: String,
        expectation: String,
        source: String,
    },
    ExitedBeforeReady {
        status: Option<String>,
        diagnostics: Vec<String>,
    },
    ReadyTimeout {
        seconds: u64,
        diagnostics: Vec<String>,
        child_status: Option<String>,
    },
    Fatal(String),
}

impl CapencStartError {
    fn fatal(detail: impl Into<String>) -> Self {
        Self::Fatal(detail.into())
    }
}

impl std::fmt::Display for CapencStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StartFailed {
                binary,
                args,
                source,
            } => write!(
                formatter,
                "capenc child never started: spawn {binary:?} with args {:?}: {source}",
                args
            ),
            Self::Unavailable(notice) => write!(
                formatter,
                "capenc backend {:?} unavailable before READY: {:?}",
                notice.backend, notice.reason
            ),
            Self::ReadyRejected {
                raw_line,
                expectation,
                source,
            } => write!(
                formatter,
                "capenc emitted READY but parent rejected it: {source}; raw_ready={raw_line:?}; expectation={expectation}"
            ),
            Self::ExitedBeforeReady {
                status,
                diagnostics,
            } => write!(
                formatter,
                "capenc child started and exited before READY{}; diagnostics={}",
                status
                    .as_deref()
                    .map_or_else(String::new, |value| format!(" with status {value}")),
                format_diagnostics(diagnostics)
            ),
            Self::ReadyTimeout {
                seconds,
                diagnostics,
                child_status,
            } => write!(
                formatter,
                "capenc child started but did not emit READY within {seconds} seconds{}; diagnostics={}",
                child_status
                    .as_deref()
                    .map_or_else(String::new, |value| format!(
                        "; child status at timeout {value}"
                    )),
                format_diagnostics(diagnostics)
            ),
            Self::Fatal(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for CapencStartError {}

impl From<String> for CapencStartError {
    fn from(detail: String) -> Self {
        Self::Fatal(detail)
    }
}

fn push_diagnostic(diagnostics: &mut VecDeque<String>, line: String) {
    if diagnostics.len() == MAX_PRE_READY_DIAGNOSTICS {
        diagnostics.pop_front();
    }
    diagnostics.push_back(line);
}

fn format_diagnostics<'a>(diagnostics: impl IntoIterator<Item = &'a String>) -> String {
    let mut diagnostics = diagnostics.into_iter().peekable();
    if diagnostics.peek().is_none() {
        "<none>".to_string()
    } else {
        diagnostics.cloned().collect::<Vec<_>>().join(" | ")
    }
}

fn format_ready_expectation(expectation: &ReadyExpectation<'_>) -> String {
    format!("{expectation:?}")
}

#[derive(Debug)]
enum StdinCommand {
    Idr,
    Shutdown,
}

#[derive(Clone)]
pub struct IdrRequester {
    tx: mpsc::Sender<StdinCommand>,
}

impl IdrRequester {
    fn new(tx: mpsc::Sender<StdinCommand>) -> Self {
        Self { tx }
    }

    pub fn request(&self, reason: &'static str) -> bool {
        match self.tx.try_send(StdinCommand::Idr) {
            Ok(()) => {
                tracing::info!(target: CAPENC, reason, "IDR recovery queued");
                true
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::debug!(target: CAPENC, reason, "IDR recovery already pending");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(target: CAPENC, reason, "IDR request dropped: engine stdin closed");
                false
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildShutdown {
    Graceful,
    Forced,
}

impl Capenc {
    /// Spawn the engine and start the reader/stderr tasks. Encoded frames arrive
    /// on the returned receiver.
    pub async fn spawn(
        cfg: CapencConfig,
    ) -> Result<(Capenc, Arc<VideoQueue<EncodedFrame>>, ResolvedMediaPlan), CapencStartError> {
        // Multi-call dispatch: the Pier spawns itself with the `capenc`
        // subcommand rather than a separate executable. This is what makes the
        // installed footprint one binary, and it closes SEC-101/SEC-151 by
        // construction because there is no configurable helper path to hijack.
        let mut args = vec!["capenc".to_string()];
        args.extend(Self::build_args(&cfg));
        let expectation = Self::ready_expectation(&cfg)?;
        let binary = std::env::current_exe().map_err(|error| {
            CapencStartError::fatal(format!("resolve current executable: {error}"))
        })?;

        tracing::info!(
            target: CAPENC,
            binary = %binary.display(),
            configured_binary = %cfg.binary,
            output = cfg.output_index,
            adapter = cfg.adapter_name.as_deref().unwrap_or("<global-index>"),
            adapter_output = ?cfg.adapter_output_index,
            device = cfg.device_name.as_deref().unwrap_or("<resolved-by-engine>"),
            codec = codec_name(cfg.codec),
            chroma = chroma_name(cfg.chroma),
            fps = cfg.fps,
            encoder = cfg.encoder.map(EncoderSelection::name).unwrap_or("engine-default"),
            cursor = ?cfg.cursor_mode,
            "spawning capture+encode engine"
        );

        let mut child = Command::new(&binary)
            .args(&args)
            .env("ARCEN_SESSION_LOG_ID", cfg.session_log_id.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| CapencStartError::StartFailed {
                binary: binary.display().to_string(),
                args: args.clone(),
                source: error.to_string(),
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "capenc: no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "capenc: no stdout".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "capenc: no stderr".to_string())?;

        let mut stderr_lines = BufReader::new(stderr).lines();
        let plan = match Self::wait_for_ready(&mut stderr_lines, expectation, CAPENC_READY_TIMEOUT)
            .await
        {
            Ok(Ok(plan)) => plan,
            Ok(Err(mut error)) => {
                if let CapencStartError::ExitedBeforeReady { status, .. } = &mut error {
                    if status.is_none() {
                        *status = child.wait().await.ok().map(|value| value.to_string());
                    }
                } else {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
                return Err(error);
            }
            Err(mut error) => {
                let child_status = child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|value| value.to_string());
                if let CapencStartError::ReadyTimeout {
                    child_status: status,
                    ..
                } = &mut error
                {
                    *status = child_status;
                }
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(error);
            }
        };

        let frames = Arc::new(VideoQueue::new(FRAME_QUEUE_CAPACITY));
        let (stdin_tx, stdin_rx) = mpsc::channel(1);
        let idr = IdrRequester::new(stdin_tx.clone());
        let pipeline_telemetry = Arc::new(PipelineTelemetry::default());

        let codec = Self::protocol_codec(plan.video.codec);
        // Every record this helper emits carries its own output identity.
        // Two concurrent multi-monitor engines otherwise logged identical
        // "capture and encode aggregate" lines with nothing to tell them
        // apart, so a stuttering monitor could not be identified from the
        // host log at all.
        let helper_span = tracing::info_span!(
            target: CAPENC,
            "capenc_helper",
            sid = %cfg.session_log_id,
            output = cfg.output_index,
            device = cfg.device_name.as_deref().unwrap_or("<resolved-by-engine>"),
        );
        tokio::spawn(
            read_length_prefixed(stdout, codec, frames.clone(), idr.clone())
                .instrument(helper_span.clone()),
        );
        tokio::spawn(write_stdin(stdin, stdin_rx).instrument(helper_span.clone()));
        tokio::spawn(
            forward_stderr(stderr_lines, Arc::clone(&pipeline_telemetry)).instrument(helper_span),
        );

        Ok((
            Capenc {
                child,
                idr,
                control: stdin_tx,
                pipeline_telemetry,
            },
            frames,
            plan,
        ))
    }

    fn media_codec(codec: VideoCodec) -> arcen_media::VideoCodec {
        match codec {
            VideoCodec::Jpeg => arcen_media::VideoCodec::Jpeg,
            VideoCodec::H264 => arcen_media::VideoCodec::H264,
            VideoCodec::H265 => arcen_media::VideoCodec::H265,
            VideoCodec::Vp9 => arcen_media::VideoCodec::Vp9,
            VideoCodec::Av1 => arcen_media::VideoCodec::Av1,
        }
    }

    pub fn pipeline_telemetry(&self) -> PipelineTelemetrySnapshot {
        self.pipeline_telemetry.snapshot()
    }

    fn media_chroma(chroma: ChromaSubsampling) -> arcen_media::ChromaSubsampling {
        match chroma {
            ChromaSubsampling::Yuv420 => arcen_media::ChromaSubsampling::Yuv420,
            ChromaSubsampling::Yuv422 => arcen_media::ChromaSubsampling::Yuv422,
            ChromaSubsampling::Yuv444 => arcen_media::ChromaSubsampling::Yuv444,
        }
    }

    pub(crate) fn protocol_codec(codec: arcen_media::VideoCodec) -> VideoCodec {
        match codec {
            arcen_media::VideoCodec::Jpeg => VideoCodec::Jpeg,
            arcen_media::VideoCodec::H264 => VideoCodec::H264,
            arcen_media::VideoCodec::H265 => VideoCodec::H265,
            arcen_media::VideoCodec::Vp9 => VideoCodec::Vp9,
            arcen_media::VideoCodec::Av1 => VideoCodec::Av1,
        }
    }

    pub(crate) fn protocol_chroma(chroma: arcen_media::ChromaSubsampling) -> ChromaSubsampling {
        match chroma {
            arcen_media::ChromaSubsampling::Yuv420 => ChromaSubsampling::Yuv420,
            arcen_media::ChromaSubsampling::Yuv422 => ChromaSubsampling::Yuv422,
            arcen_media::ChromaSubsampling::Yuv444 => ChromaSubsampling::Yuv444,
        }
    }

    pub(crate) fn protocol_bit_depth(bit_depth: arcen_media::BitDepth) -> WireBitDepth {
        match bit_depth {
            arcen_media::BitDepth::Eight => WireBitDepth::Eight,
            arcen_media::BitDepth::Ten => WireBitDepth::Ten,
            arcen_media::BitDepth::Twelve => WireBitDepth::Twelve,
        }
    }

    pub(crate) fn protocol_color_range(range: arcen_media::ColorRange) -> WireColorRange {
        match range {
            arcen_media::ColorRange::Limited => WireColorRange::Limited,
            arcen_media::ColorRange::Full => WireColorRange::Full,
        }
    }

    pub(crate) fn protocol_color_matrix(matrix: arcen_media::ColorMatrix) -> WireColorMatrix {
        match matrix {
            arcen_media::ColorMatrix::Bt709 => WireColorMatrix::Bt709,
            arcen_media::ColorMatrix::Identity => WireColorMatrix::Identity,
            arcen_media::ColorMatrix::Bt601 => WireColorMatrix::Bt601,
            arcen_media::ColorMatrix::Bt2020Ncl => WireColorMatrix::Bt2020Ncl,
        }
    }

    fn ready_expectation(cfg: &CapencConfig) -> Result<ReadyExpectation<'_>, String> {
        if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 {
            return Err("capenc READY expectation has invalid geometry".to_string());
        }
        let encoder = cfg.encoder.unwrap_or(EncoderSelection::Auto).request();
        let allowed_backends: &'static [EncoderBackend] = match encoder {
            EncoderRequest::Auto => &[EncoderBackend::NativeNvenc, EncoderBackend::OpenH264],
            EncoderRequest::NativeNvenc => &[EncoderBackend::NativeNvenc],
            EncoderRequest::WindowsMediaFoundation => &[EncoderBackend::WindowsMediaFoundation],
            EncoderRequest::SoftwareH264 => &[EncoderBackend::OpenH264],
            EncoderRequest::SoftwareAv1 => &[EncoderBackend::Rav1e],
        };
        Ok(ReadyExpectation {
            request: MediaRequest {
                encoder,
                video: VideoConfiguration {
                    codec: Self::media_codec(cfg.codec),
                    chroma: Self::media_chroma(cfg.chroma),
                    bit_depth: cfg.bit_depth,
                    range: cfg.color_range,
                    matrix: cfg.color_matrix,
                    primaries: cfg.color_primaries,
                    transfer: cfg.transfer,
                },
                width: cfg.width,
                height: cfg.height,
                fps: cfg.fps,
                cursor_mode: cfg.cursor_mode,
            },
            allowed_backends,
            session_log_id: Some(cfg.session_log_id.as_str()),
        })
    }

    async fn wait_for_ready<R>(
        lines: &mut Lines<R>,
        expectation: ReadyExpectation<'_>,
        ready_timeout: Duration,
    ) -> Result<Result<ResolvedMediaPlan, CapencStartError>, CapencStartError>
    where
        R: AsyncBufRead + Unpin,
    {
        let mut helper_reported_failure = false;
        let mut diagnostics: VecDeque<String> = VecDeque::new();
        let deadline = tokio::time::Instant::now() + ready_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(CapencStartError::ReadyTimeout {
                    seconds: ready_timeout.as_secs(),
                    diagnostics: diagnostics.into(),
                    child_status: None,
                });
            }
            let line = match tokio::time::timeout(remaining, lines.next_line()).await {
                Ok(line) => line,
                Err(_) => {
                    return Err(CapencStartError::ReadyTimeout {
                        seconds: ready_timeout.as_secs(),
                        diagnostics: diagnostics.into(),
                        child_status: None,
                    });
                }
            };
            match line {
                Ok(Some(line)) if line.starts_with("[capenc] READY ") => {
                    let plan = match parse_ready_v1(&line, expectation) {
                        Ok(plan) => plan,
                        Err(error) => {
                            return Ok(Err(CapencStartError::ReadyRejected {
                                raw_line: line.clone(),
                                expectation: format_ready_expectation(&expectation),
                                source: error.to_string(),
                            }));
                        }
                    };
                    forward_capenc_line(&line);
                    return Ok(Ok(plan));
                }
                Ok(Some(line)) if line.starts_with("[capenc] UNAVAILABLE ") => {
                    forward_capenc_line(&line);
                    push_diagnostic(&mut diagnostics, line.clone());
                    let notice = match parse_unavailable_v1(&line) {
                        Ok(notice) => notice,
                        Err(error) => {
                            return Ok(Err(CapencStartError::fatal(format!(
                                "malformed capenc UNAVAILABLE: {error}; diagnostics={}",
                                format_diagnostics(&diagnostics)
                            ))));
                        }
                    };
                    if !expectation.allowed_backends.contains(&notice.backend) {
                        return Err(CapencStartError::fatal(format!(
                            "capenc UNAVAILABLE named disallowed backend {:?}",
                            notice.backend
                        )));
                    }
                    return Ok(Err(CapencStartError::Unavailable(notice)));
                }
                Ok(Some(line)) => {
                    if line.contains("ERROR") || line.contains("failed") {
                        helper_reported_failure = true;
                    }
                    forward_capenc_line(&line);
                    push_diagnostic(&mut diagnostics, line);
                }
                Ok(None) => {
                    if helper_reported_failure {
                        push_diagnostic(
                            &mut diagnostics,
                            "parent observed helper-reported pre-READY failure".to_string(),
                        );
                    }
                    return Ok(Err(CapencStartError::ExitedBeforeReady {
                        status: None,
                        diagnostics: diagnostics.into(),
                    }));
                }
                Err(_) => {
                    return Ok(Err(CapencStartError::fatal(format!(
                        "capenc stderr failed before READY; diagnostics={}",
                        format_diagnostics(&diagnostics)
                    ))));
                }
            }
        }
    }

    fn build_args(cfg: &CapencConfig) -> Vec<String> {
        let mut args = vec![
            cfg.output_index.to_string(),
            codec_name(cfg.codec).to_string(),
            cfg.fps.to_string(),
        ];
        if cfg.chroma == ChromaSubsampling::Yuv444 {
            args.push("yuv444".to_string());
        }
        let variant = arcen_media::video::VideoVariant::new(VideoConfiguration {
            codec: Self::media_codec(cfg.codec),
            chroma: Self::media_chroma(cfg.chroma),
            bit_depth: cfg.bit_depth,
            range: cfg.color_range,
            matrix: cfg.color_matrix,
            primaries: cfg.color_primaries,
            transfer: cfg.transfer,
        });
        if variant.is_coherent() {
            args.push(format!("variant={}", variant.id()));
        }
        // Their own tokens: `variant=<id>` names codec, chroma, depth,
        // range and matrix and has no room for these. Emitted only when
        // they differ from BT.709, so a session that never asked for HDR
        // produces the argv it always did.
        if cfg.transfer != TransferCharacteristics::Bt709 {
            args.push(format!("transfer={}", cfg.transfer.token()));
        }
        if cfg.color_primaries != ColorPrimaries::Bt709 {
            args.push(format!("primaries={}", cfg.color_primaries.token()));
        }
        // Only when it is not the default: an absent `intent=` already means
        // `Interactive` to the engine's own parser, so emitting it
        // unconditionally would change the argv of every session that never
        // asked for a different intent — and invalidate the argv assertions
        // that pin those unchanged commands — while telling capenc nothing new.
        if cfg.intent != EncodeIntent::default() {
            args.push(format!("intent={}", cfg.intent.token()));
        }
        // Same conditional shape and same reason.
        if cfg.qp_map != arcen_media::video::QpMapPolicy::default() {
            args.push(format!("qp-map={}", cfg.qp_map.token()));
        }
        args.push("framed-v1".to_string());
        if let Some(encoder) = cfg.encoder {
            args.push(encoder.as_arg().to_string());
        }
        if let Some(adapter_name) = cfg.adapter_name.as_deref() {
            args.push(format!("adapter={adapter_name}"));
        }
        if let Some(adapter_output_index) = cfg.adapter_output_index {
            args.push(format!("adapter-output={adapter_output_index}"));
        }
        if let Some(device_name) = cfg.device_name.as_deref() {
            args.push(format!("device={device_name}"));
        }
        args.push(format!(
            "cursor={}",
            match cfg.cursor_mode {
                CursorMode::Local => "local",
                CursorMode::Host => "host",
            }
        ));
        args
    }

    /// Real IDR-on-demand: one line on the engine's stdin, no restart.
    pub fn request_keyframe(&self, reason: &'static str) {
        self.idr.request(reason);
    }

    pub fn idr(&self) -> IdrRequester {
        self.idr.clone()
    }

    /// Stop the engine. Closing stdin signals the engine to exit; we then reap.
    pub async fn shutdown(&mut self) {
        match shutdown_child(&mut self.child, &self.control, CAPENC_SHUTDOWN_TIMEOUT).await {
            ChildShutdown::Graceful => {
                tracing::info!(target: CAPENC, "engine stopped gracefully");
            }
            ChildShutdown::Forced => {
                tracing::warn!(target: CAPENC, "engine ignored graceful stop and was force-killed");
            }
        }
    }
}

pub(crate) fn admission_probe_command(cfg: &CapencConfig) -> Result<std::process::Command, String> {
    Capenc::ready_expectation(cfg)?;
    let binary =
        std::env::current_exe().map_err(|error| format!("resolve current executable: {error}"))?;
    let mut command = std::process::Command::new(binary);
    command
        .arg("capenc")
        .args(Capenc::build_args(cfg))
        .env("ARCEN_SESSION_LOG_ID", cfg.session_log_id.as_str());
    Ok(command)
}

async fn shutdown_child(
    child: &mut Child,
    control: &mpsc::Sender<StdinCommand>,
    timeout: Duration,
) -> ChildShutdown {
    let graceful = async {
        let _ = control.send(StdinCommand::Shutdown).await;
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

pub(crate) fn protocol_codec(codec: arcen_media::VideoCodec) -> VideoCodec {
    Capenc::protocol_codec(codec)
}

pub(crate) fn media_codec(codec: VideoCodec) -> arcen_media::VideoCodec {
    Capenc::media_codec(codec)
}

pub(crate) fn media_chroma(chroma: ChromaSubsampling) -> arcen_media::ChromaSubsampling {
    Capenc::media_chroma(chroma)
}

pub(crate) fn protocol_chroma(chroma: arcen_media::ChromaSubsampling) -> ChromaSubsampling {
    Capenc::protocol_chroma(chroma)
}

pub(crate) fn protocol_bit_depth(bit_depth: arcen_media::BitDepth) -> WireBitDepth {
    Capenc::protocol_bit_depth(bit_depth)
}

pub(crate) fn protocol_color_range(range: arcen_media::ColorRange) -> WireColorRange {
    Capenc::protocol_color_range(range)
}

pub(crate) fn protocol_color_matrix(matrix: arcen_media::ColorMatrix) -> WireColorMatrix {
    Capenc::protocol_color_matrix(matrix)
}

/// Read framed-v1 `[u32 BE len][AU]*` from the engine's stdout. Each AU is one complete
/// access unit — exactly one WS message downstream.
async fn read_length_prefixed<R>(
    stdout: R,
    codec: VideoCodec,
    frames: Arc<VideoQueue<EncodedFrame>>,
    idr: IdrRequester,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    read_length_prefixed_inner(stdout, codec, frames.clone(), idr).await;
    frames.close();
}

async fn read_length_prefixed_inner<R>(
    stdout: R,
    codec: VideoCodec,
    frames: Arc<VideoQueue<EncodedFrame>>,
    idr: IdrRequester,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut r = BufReader::new(stdout);
    let mut len_buf = [0u8; 4];
    let mut frame_count: u64 = 0;
    loop {
        if r.read_exact(&mut len_buf).await.is_err() {
            tracing::info!(
                target: CAPENC,
                reason_class = "stdout_closed",
                frames = frame_count,
                "engine stdout closed"
            );
            break;
        }
        let len = u32::from_be_bytes(len_buf);
        if len == 0 || len > MAX_AU_BYTES {
            tracing::error!(target: CAPENC, len, "invalid AU length prefix — stopping reader");
            break;
        }
        let mut au = vec![0u8; len as usize];
        if r.read_exact(&mut au).await.is_err() {
            tracing::warn!(
                target: CAPENC,
                reason_class = "short_access_unit",
                expected_bytes = len,
                "short AU read — engine gone"
            );
            break;
        }
        let keyframe = is_keyframe(codec, &au);
        frame_count += 1;
        let frame = EncodedFrame {
            data: au,
            keyframe,
            timestamp_ms: now_ms(),
        };
        if !enqueue_encoded_frame(&frames, &idr, frame) {
            break;
        }
    }
    frames.close();
}

fn enqueue_encoded_frame(
    frames: &VideoQueue<EncodedFrame>,
    idr: &IdrRequester,
    frame: EncodedFrame,
) -> bool {
    let keyframe = frame.keyframe;
    match frames.push(frame, keyframe) {
        VideoPushResult::Enqueued { cleared } => {
            if cleared > 0 {
                tracing::debug!(
                    target: CAPENC,
                    cleared,
                    "capenc keyframe cleared superseded queued AUs"
                );
            }
            true
        }
        VideoPushResult::Dropped {
            count,
            recovery_started,
        } => {
            let reason = if recovery_started {
                "capenc_queue_drop"
            } else {
                "capenc_queue_awaiting_keyframe"
            };
            idr.request(reason);
            if recovery_started {
                tracing::warn!(
                    target: CAPENC,
                    dropped = count,
                    "capenc frame queue lost AU: cleared prediction chain, awaiting IDR"
                );
            } else {
                tracing::debug!(
                    target: CAPENC,
                    dropped = count,
                    "capenc frame queue suppressed non-keyframe while awaiting IDR"
                );
            }
            true
        }
        VideoPushResult::Closed(_) => {
            tracing::info!(target: CAPENC, "frame queue closed: stopping reader");
            false
        }
    }
}

async fn write_stdin(mut stdin: ChildStdin, mut commands: mpsc::Receiver<StdinCommand>) {
    while let Some(command) = commands.recv().await {
        match command {
            StdinCommand::Idr => {
                let write = async {
                    stdin.write_all(b"IDR\n").await?;
                    stdin.flush().await
                };
                match tokio::time::timeout(CAPENC_WRITE_TIMEOUT, write).await {
                    Ok(Ok(())) => {
                        tokio::time::sleep(IDR_REQUEST_MIN_INTERVAL).await;
                    }
                    Ok(Err(_)) => {
                        tracing::warn!(
                            target: CAPENC,
                            reason_class = "control_pipe_write_failed",
                            "capenc IDR write failed"
                        );
                        return;
                    }
                    Err(_) => {
                        tracing::warn!(target: CAPENC, "capenc IDR write timed out");
                        return;
                    }
                }
            }
            StdinCommand::Shutdown => {
                let stop = async {
                    stdin.write_all(b"STOP\n").await?;
                    stdin.flush().await
                };
                let _ = tokio::time::timeout(CAPENC_WRITE_TIMEOUT, stop).await;
                return;
            }
        }
    }
}

async fn forward_stderr<R>(mut lines: Lines<R>, telemetry: Arc<PipelineTelemetry>)
where
    R: AsyncBufRead + Unpin,
{
    while let Ok(Some(line)) = lines.next_line().await {
        forward_capenc_line_with_telemetry(&line, Some(&telemetry));
    }
}

fn forward_capenc_line(line: &str) {
    forward_capenc_line_with_telemetry(line, None);
}

fn forward_capenc_line_with_telemetry(line: &str, telemetry: Option<&PipelineTelemetry>) {
    if is_pipeline_stats(line) {
        let stats = parse_pipeline_stats(line);
        if let Some(telemetry) = telemetry {
            telemetry.record(&stats);
        }
        tracing::info!(
            target: CAPENC,
            event = "capture_encode_snapshot",
            encoder_fps = stats.encoder_fps,
            capture_fps = stats.capture_fps,
            average_stage_ms = stats.average_stage_ms,
            maximum_stage_ms = stats.maximum_stage_ms,
            average_readback_ms = stats.average_readback_ms,
            maximum_readback_ms = stats.maximum_readback_ms,
            average_conversion_ms = stats.average_conversion_ms,
            maximum_conversion_ms = stats.maximum_conversion_ms,
            average_encode_ms = stats.average_encode_ms,
            maximum_encode_ms = stats.maximum_encode_ms,
            slow_frames = stats.slow_frames,
            dropped_frames = stats.dropped_frames,
            "capture and encode aggregate"
        );
        // Level 3 only. Keep each event below StructuredFields' hard 16-field
        // limit. One oversized event silently loses every field after the
        // limit, which previously hid the restage outcomes and capture
        // counters this diagnostic exists to expose.
        tracing::debug!(
            target: CAPENC,
            event = "capture_encode_submission_detail",
            fresh_encode_count = stats.fresh_encode_count,
            average_fresh_encode_ms = stats.average_fresh_encode_ms,
            maximum_fresh_encode_ms = stats.maximum_fresh_encode_ms,
            restaged_encode_count = stats.restaged_encode_count,
            average_restaged_encode_ms = stats.average_restaged_encode_ms,
            maximum_restaged_encode_ms = stats.maximum_restaged_encode_ms,
            blank_encode_count = stats.blank_encode_count,
            average_blank_encode_ms = stats.average_blank_encode_ms,
            maximum_blank_encode_ms = stats.maximum_blank_encode_ms,
            "capture and encode submission detail"
        );
        tracing::debug!(
            target: CAPENC,
            event = "capture_encode_copy_detail",
            average_copy_ms = stats.average_copy_ms,
            maximum_copy_ms = stats.maximum_copy_ms,
            average_mirror_ms = stats.average_mirror_ms,
            maximum_mirror_ms = stats.maximum_mirror_ms,
            average_restage_ms = stats.average_restage_ms,
            maximum_restage_ms = stats.maximum_restage_ms,
            restage_copied = stats.restage_copied,
            restage_skipped = stats.restage_skipped,
            restage_unavailable = stats.restage_unavailable,
            "capture and encode copy detail"
        );
        tracing::debug!(
            target: CAPENC,
            event = "capture_encode_loop_detail",
            capture_new_frames = stats.capture_new_frames,
            capture_empty_polls = stats.capture_empty_polls,
            capture_timeouts = stats.capture_timeouts,
            capture_cursor_only = stats.capture_cursor_only,
            encode_submitted = stats.encode_submitted,
            encode_skipped_no_new = stats.encode_skipped_no_new,
            kilobits_per_second = stats.kilobits_per_second,
            "capture and encode loop detail"
        );
    } else if line.contains("ERROR") || line.contains("failed") {
        // Carry the line. Recording only that a failure happened, without what
        // the helper said about it, made Windows capenc failures diagnosable
        // solely from a truncated blob attached to the final error.
        tracing::warn!(
            target: CAPENC,
            reason_class = "helper_reported_failure",
            line = %helper_line(line),
            "capture and encode helper reported a failure"
        );
    } else {
        tracing::debug!(
            target: CAPENC,
            reason_class = "helper_diagnostic",
            line = %helper_line(line),
            "capture and encode helper diagnostic"
        );
    }
}

/// Bound a helper stderr line for structured logging.
///
/// The telemetry field contract caps string values, and the helper appends a
/// correlation suffix to every line that is already recorded on the event, so
/// it is dropped here rather than spending the budget twice.
fn helper_line(line: &str) -> String {
    const MAX: usize = 400;
    let trimmed = line
        .split(" sid=")
        .next()
        .unwrap_or(line)
        .trim_start_matches("[capenc] ")
        .trim();
    if trimmed.len() <= MAX {
        return trimmed.to_string();
    }
    let mut end = MAX;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

fn is_pipeline_stats(line: &str) -> bool {
    line.starts_with("[capenc] enc_fps=")
}

#[derive(Debug, Default, PartialEq)]
struct PipelineStats {
    encoder_fps: Option<f64>,
    capture_fps: Option<f64>,
    average_stage_ms: Option<f64>,
    maximum_stage_ms: Option<f64>,
    average_copy_ms: Option<f64>,
    maximum_copy_ms: Option<f64>,
    average_readback_ms: Option<f64>,
    maximum_readback_ms: Option<f64>,
    average_conversion_ms: Option<f64>,
    maximum_conversion_ms: Option<f64>,
    average_mirror_ms: Option<f64>,
    maximum_mirror_ms: Option<f64>,
    average_encode_ms: Option<f64>,
    maximum_encode_ms: Option<f64>,
    fresh_encode_count: Option<u64>,
    average_fresh_encode_ms: Option<f64>,
    maximum_fresh_encode_ms: Option<f64>,
    restaged_encode_count: Option<u64>,
    average_restaged_encode_ms: Option<f64>,
    maximum_restaged_encode_ms: Option<f64>,
    blank_encode_count: Option<u64>,
    average_blank_encode_ms: Option<f64>,
    maximum_blank_encode_ms: Option<f64>,
    average_restage_ms: Option<f64>,
    maximum_restage_ms: Option<f64>,
    restage_copied: Option<u64>,
    restage_skipped: Option<u64>,
    restage_unavailable: Option<u64>,
    capture_new_frames: Option<u64>,
    capture_empty_polls: Option<u64>,
    capture_timeouts: Option<u64>,
    capture_cursor_only: Option<u64>,
    encode_submitted: Option<u64>,
    encode_skipped_no_new: Option<u64>,
    kilobits_per_second: Option<u64>,
    slow_frames: Option<u64>,
    dropped_frames: Option<u64>,
}

fn parse_pipeline_stats(line: &str) -> PipelineStats {
    let mut stats = PipelineStats::default();
    for item in line.split_ascii_whitespace().skip(1) {
        let Some((key, value)) = item.split_once('=') else {
            continue;
        };
        match key {
            "enc_fps" => stats.encoder_fps = value.parse().ok(),
            "cap_fps" => stats.capture_fps = value.parse().ok(),
            "avg_stage_ms" => stats.average_stage_ms = value.parse().ok(),
            "max_stage_ms" => stats.maximum_stage_ms = value.parse().ok(),
            "avg_copy_ms" => stats.average_copy_ms = value.parse().ok(),
            "max_copy_ms" => stats.maximum_copy_ms = value.parse().ok(),
            "avg_readback_ms" => stats.average_readback_ms = value.parse().ok(),
            "max_readback_ms" => stats.maximum_readback_ms = value.parse().ok(),
            "avg_conversion_ms" => stats.average_conversion_ms = value.parse().ok(),
            "max_conversion_ms" => stats.maximum_conversion_ms = value.parse().ok(),
            "avg_mirror_ms" => stats.average_mirror_ms = value.parse().ok(),
            "max_mirror_ms" => stats.maximum_mirror_ms = value.parse().ok(),
            "avg_ms" | "avg_encode_ms" => stats.average_encode_ms = value.parse().ok(),
            "max_ms" | "max_encode_ms" => stats.maximum_encode_ms = value.parse().ok(),
            "fresh_encode_count" => stats.fresh_encode_count = value.parse().ok(),
            "avg_fresh_encode_ms" => stats.average_fresh_encode_ms = value.parse().ok(),
            "max_fresh_encode_ms" => stats.maximum_fresh_encode_ms = value.parse().ok(),
            "restaged_encode_count" => stats.restaged_encode_count = value.parse().ok(),
            "avg_restaged_encode_ms" => stats.average_restaged_encode_ms = value.parse().ok(),
            "max_restaged_encode_ms" => stats.maximum_restaged_encode_ms = value.parse().ok(),
            "blank_encode_count" => stats.blank_encode_count = value.parse().ok(),
            "avg_blank_encode_ms" => stats.average_blank_encode_ms = value.parse().ok(),
            "max_blank_encode_ms" => stats.maximum_blank_encode_ms = value.parse().ok(),
            "avg_restage_ms" => stats.average_restage_ms = value.parse().ok(),
            "max_restage_ms" => stats.maximum_restage_ms = value.parse().ok(),
            "restage_copied" => stats.restage_copied = value.parse().ok(),
            "restage_skipped" => stats.restage_skipped = value.parse().ok(),
            "restage_unavailable" => stats.restage_unavailable = value.parse().ok(),
            "capture_new" => stats.capture_new_frames = value.parse().ok(),
            "capture_empty" => stats.capture_empty_polls = value.parse().ok(),
            "timeout" => stats.capture_timeouts = value.parse().ok(),
            "cursor_only" => stats.capture_cursor_only = value.parse().ok(),
            "encode_submitted" => stats.encode_submitted = value.parse().ok(),
            "encode_skipped_no_new" => stats.encode_skipped_no_new = value.parse().ok(),
            "kbps" => stats.kilobits_per_second = value.parse().ok(),
            "slow" => stats.slow_frames = value.parse().ok(),
            "dropped" => stats.dropped_frames = value.parse().ok(),
            _ => {}
        }
    }
    stats
}

/// Minimum operator profile that retains the one-second capture/encode
/// aggregate.
///
/// Level 0 is a production profile that must stay quiet, so the aggregate is
/// deliberately not mandatory. Pinned as a function rather than left implicit
/// in a macro so the intended tier is testable.
const fn pipeline_snapshot_profile() -> OperationalProfile {
    OperationalProfile::Info
}

/// Minimum operator profile that retains the bounded capture/encode detail
/// needed to explain a latency regression.
///
/// This is the Level 3 optimization evidence: fresh versus restaged versus
/// blank submissions, the whole-frame mirror and republish copies, and the
/// capture outcomes that explain why a second produced the encodes it did.
const fn pipeline_detail_profile() -> OperationalProfile {
    OperationalProfile::Debug
}

// `event` counts as a structured field. These constants pin every Level 3
// pipeline event below the telemetry contract's hard field limit.
const PIPELINE_SUBMISSION_DETAIL_FIELDS: usize = 10;
const PIPELINE_COPY_DETAIL_FIELDS: usize = 10;
const PIPELINE_LOOP_DETAIL_FIELDS: usize = 8;

// The detail event must be strictly noisier than the operational aggregate,
// and neither may be mandatory at Level 0. Pinned at compile time so the two
// tiers cannot silently converge and start spamming a production host.
const _: () = assert!(pipeline_detail_profile().includes(pipeline_snapshot_profile()));
const _: () = assert!(!OperationalProfile::Critical.includes(pipeline_snapshot_profile()));
const _: () = assert!(!OperationalProfile::Info.includes(pipeline_detail_profile()));
const _: () = assert!(PIPELINE_SUBMISSION_DETAIL_FIELDS <= arcen_telemetry::MAX_STRUCTURED_FIELDS);
const _: () = assert!(PIPELINE_COPY_DETAIL_FIELDS <= arcen_telemetry::MAX_STRUCTURED_FIELDS);
const _: () = assert!(PIPELINE_LOOP_DETAIL_FIELDS <= arcen_telemetry::MAX_STRUCTURED_FIELDS);

fn now_ms() -> u32 {
    (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
        & 0xFFFF_FFFF) as u32
}

/// AU-aware keyframe detection — ported from `video_encoder.py`. Scans every
/// three- or four-byte Annex-B NAL (an AU may be `[VPS][SPS][PPS][SEI][IDR]`).
fn is_keyframe(codec: VideoCodec, au: &[u8]) -> bool {
    match codec {
        VideoCodec::H264 => scan_nals(au, |b0| (b0 & 0x1F) == 5),
        VideoCodec::H265 => scan_nals(au, |b0| matches!((b0 >> 1) & 0x3F, 19 | 20 | 21)),
        VideoCodec::Av1 => arcen_media::video::av1_low_overhead_has_sequence_header(au),
        VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    }
}

fn scan_nals(data: &[u8], is_idr: impl Fn(u8) -> bool) -> bool {
    let mut pos = 0usize;
    while let Some((idx, start_code_len)) = find_start_code(data, pos) {
        let hdr = idx + start_code_len;
        if hdr >= data.len() {
            return false;
        }
        if is_idr(data[hdr]) {
            return true;
        }
        pos = hdr;
    }
    false
}

fn find_start_code(data: &[u8], offset: usize) -> Option<(usize, usize)> {
    let mut index = offset;
    while index + 3 <= data.len() {
        if index + 4 <= data.len() && data[index..index + 4] == [0, 0, 0, 1] {
            return Some((index, 4));
        }
        if data[index..index + 3] == [0, 0, 1] {
            return Some((index, 3));
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::video::{
        format_ready_v1, format_unavailable_v1, BackendUnavailableNotice, BackendUnavailableReason,
        EncoderBackend,
    };

    fn test_config() -> CapencConfig {
        CapencConfig {
            binary: "arcen-capenc.exe".to_string(),
            output_index: 3,
            adapter_name: Some("VMware SVGA 3D".to_string()),
            adapter_output_index: Some(0),
            device_name: Some(r"\\.\DISPLAY1".to_string()),
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            intent: EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 30,
            width: 1920,
            height: 1080,
            encoder: Some(EncoderSelection::SoftwareH264),
            cursor_mode: CursorMode::Host,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    /// The intent token is conditional, and the reason matters: a session that
    /// never asked for a different intent must produce the exact argv it
    /// produced before intent existed, so that an operator diffing two spawn
    /// commands sees a token only where a real request caused one.
    #[test]
    fn build_args_emits_the_intent_token_only_when_it_is_not_the_default() {
        let mut cfg = test_config();
        assert_eq!(cfg.intent, EncodeIntent::Interactive);
        assert!(
            !Capenc::build_args(&cfg)
                .iter()
                .any(|arg| arg.starts_with("intent=")),
            "the default intent must leave the shipped argv untouched"
        );

        cfg.intent = EncodeIntent::Quality;
        assert!(
            Capenc::build_args(&cfg)
                .iter()
                .any(|arg| arg == "intent=quality"),
            "a requested intent must reach the engine verbatim"
        );
    }

    /// The QP-map token follows the same conditional shape as `intent=`: a
    /// host that never opted into the experiment must produce the argv it
    /// always did, and one that did must have it reach the engine verbatim.
    #[test]
    fn qp_map_token_is_emitted_only_when_it_is_not_the_default() {
        let mut cfg = test_config();
        assert_eq!(cfg.qp_map, arcen_media::video::QpMapPolicy::Off);
        assert!(
            !Capenc::build_args(&cfg)
                .iter()
                .any(|arg| arg.starts_with("qp-map=")),
            "the default policy must leave the shipped argv untouched"
        );

        cfg.qp_map = arcen_media::video::QpMapPolicy::Neutral;
        assert!(
            Capenc::build_args(&cfg)
                .iter()
                .any(|arg| arg == "qp-map=neutral"),
            "the control arm must be selectable, or the benchmark has none"
        );
    }

    #[test]
    fn embedded_capenc_keeps_windows_encoder_backends() {
        let features = arcen_capenc::compiled_backend_features();
        assert!(
            features.nvenc,
            "single-binary Pier must embed capenc with NVENC enabled"
        );
        assert!(
            features.software_h264,
            "single-binary Pier must embed source-built OpenH264 fallback"
        );
        assert!(
            !features.mf,
            "shipped Pier must not depend on the inbox MF encoder"
        );
    }

    fn test_plan() -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend: EncoderBackend::OpenH264,
            video: VideoConfiguration::legacy_h264(),
            width: 1920,
            height: 1080,
            fps: 30,
            cursor_mode: CursorMode::Host,
            cursor_in_video: true,
            codecs: arcen_media::CodecSet::from_slice(&[arcen_media::VideoCodec::H264]),
            chroma: arcen_media::ChromaSet::from_slice(&[arcen_media::ChromaSubsampling::Yuv420]),
            bit_depths: arcen_media::BitDepthSet::from_slice(&[arcen_media::BitDepth::Eight]),
            ranges: arcen_media::ColorRangeSet::from_slice(&[arcen_media::ColorRange::Limited]),
        }
    }

    #[test]
    fn auto_resolves_to_nvenc_only_for_nvidia_with_runtime() {
        use EncoderSelection::*;
        assert_eq!(Auto.resolve_auto(true, true), Nvenc);
        assert_eq!(Auto.resolve_auto(true, false), SoftwareH264);
        assert_eq!(Auto.resolve_auto(false, true), SoftwareH264);
        assert_eq!(Auto.resolve_auto(false, false), SoftwareH264);
    }

    #[test]
    fn concrete_encoder_selections_pass_through_resolution() {
        use EncoderSelection::*;
        assert_eq!(Nvenc.resolve_auto(false, false), Nvenc);
        assert_eq!(SoftwareH264.resolve_auto(true, true), SoftwareH264);
    }

    #[test]
    fn capenc_args_pin_the_resolved_adapter_output_and_device() {
        let args = Capenc::build_args(&test_config());

        assert_eq!(
            args,
            [
                "3",
                "h264",
                "30",
                "variant=h264-420-8-limited-bt709",
                "framed-v1",
                "encoder=software-h264",
                "adapter=VMware SVGA 3D",
                "adapter-output=0",
                r"device=\\.\DISPLAY1",
                "cursor=host",
            ]
        );
    }

    #[tokio::test]
    async fn auto_expectation_accepts_openh264_after_parent_geometry_transaction() {
        let mut config = test_config();
        config.encoder = Some(EncoderSelection::Auto);
        let args = Capenc::build_args(&config);
        assert!(args.iter().any(|argument| argument == "encoder=auto"));

        let line = format!(
            "{}\n",
            format_ready_v1(test_plan(), Some(config.session_log_id.as_str()))
        );
        let mut lines = BufReader::new(std::io::Cursor::new(line)).lines();
        let plan = Capenc::wait_for_ready(
            &mut lines,
            Capenc::ready_expectation(&config).expect("valid auto expectation"),
            Duration::from_secs(1),
        )
        .await
        .expect("READY wait must not time out")
        .expect("auto accepts the concrete OpenH264 READY");
        assert_eq!(plan.backend, EncoderBackend::OpenH264);
    }

    #[tokio::test]
    async fn canonical_ready_resolves_authoritative_media_plan() {
        let config = test_config();
        let line = format!(
            "{}\n",
            format_ready_v1(test_plan(), Some(config.session_log_id.as_str()))
        );
        let mut lines = BufReader::new(std::io::Cursor::new(line)).lines();
        let plan = Capenc::wait_for_ready(
            &mut lines,
            Capenc::ready_expectation(&config).expect("valid expectation"),
            Duration::from_secs(1),
        )
        .await
        .expect("READY wait must not time out")
        .expect("canonical READY");
        assert_eq!(plan, test_plan());
    }

    #[tokio::test]
    async fn malformed_or_mismatched_ready_is_terminal() {
        for line in [
            format!(
                "{} codec=h264\n",
                format_ready_v1(test_plan(), Some("00000000-0000-4000-8000-000000000000"))
            ),
            format!(
                "{}\n",
                format_ready_v1(
                    ResolvedMediaPlan {
                        width: 1280,
                        ..test_plan()
                    },
                    Some("00000000-0000-4000-8000-000000000000")
                )
            ),
        ] {
            let config = test_config();
            let mut lines = BufReader::new(std::io::Cursor::new(line)).lines();
            let error = Capenc::wait_for_ready(
                &mut lines,
                Capenc::ready_expectation(&config).expect("valid expectation"),
                Duration::from_secs(1),
            )
            .await
            .expect("READY wait must not time out")
            .expect_err("invalid READY must fail");
            assert!(matches!(error, CapencStartError::ReadyRejected { .. }));
            assert!(error.to_string().contains("raw_ready="));
            assert!(error.to_string().contains("expectation="));
        }
    }

    #[tokio::test]
    async fn typed_native_unavailable_is_preserved_for_parent_fallback() {
        let mut config = test_config();
        config.encoder = Some(EncoderSelection::Auto);
        let notice = BackendUnavailableNotice {
            backend: EncoderBackend::NativeNvenc,
            reason: BackendUnavailableReason::SessionLimit,
        };
        let input = format!("{}\n", format_unavailable_v1(notice));
        let mut lines = BufReader::new(std::io::Cursor::new(input)).lines();
        let error = Capenc::wait_for_ready(
            &mut lines,
            Capenc::ready_expectation(&config).expect("valid expectation"),
            Duration::from_secs(1),
        )
        .await
        .expect("READY wait must not time out")
        .expect_err("UNAVAILABLE precedes any READY");
        assert!(matches!(error, CapencStartError::Unavailable(actual) if actual == notice));
    }

    #[tokio::test]
    async fn disallowed_unavailable_and_eof_before_ready_are_fatal() {
        for input in [
            "[capenc] UNAVAILABLE version=1 backend=openh264 code=not_built\n",
            "",
        ] {
            let config = test_config();
            let mut lines = BufReader::new(std::io::Cursor::new(input)).lines();
            let error = Capenc::wait_for_ready(
                &mut lines,
                Capenc::ready_expectation(&config).expect("valid expectation"),
                Duration::from_secs(1),
            )
            .await
            .expect("READY wait must not time out")
            .expect_err("startup failure must be terminal");
            assert!(matches!(
                error,
                CapencStartError::Fatal(_) | CapencStartError::ExitedBeforeReady { .. }
            ));
        }
    }

    #[tokio::test]
    async fn ready_wait_can_be_bounded_by_timeout() {
        let config = test_config();
        let (_writer, reader) = tokio::io::duplex(16);
        let mut lines = BufReader::new(reader).lines();
        let error = Capenc::wait_for_ready(
            &mut lines,
            Capenc::ready_expectation(&config).expect("valid expectation"),
            Duration::from_millis(1),
        )
        .await
        .expect_err("bounded READY wait must time out");
        assert!(matches!(error, CapencStartError::ReadyTimeout { .. }));
    }

    #[test]
    fn h264_idr_detected_behind_sps_pps() {
        // [SPS type7][PPS type8][IDR type5]
        let au = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS
            0, 0, 0, 1, 0x68, 0xBB, // PPS
            0, 0, 0, 1, 0x65, 0xCC, // IDR (0x65 & 0x1F == 5)
        ];
        assert!(is_keyframe(VideoCodec::H264, &au));
    }

    #[test]
    fn h264_pframe_not_keyframe() {
        let au = [0, 0, 0, 1, 0x61, 0xCC]; // non-IDR slice
        assert!(!is_keyframe(VideoCodec::H264, &au));
    }

    #[test]
    fn h264_mixed_start_codes_detect_idr() {
        let au = [
            0, 0, 0, 1, 0x67, 0xAA, // four-byte SPS
            0, 0, 1, 0x68, 0xBB, // three-byte PPS
            0, 0, 1, 0x65, 0xCC, // three-byte IDR
        ];
        assert!(is_keyframe(VideoCodec::H264, &au));
    }

    #[test]
    fn hevc_idr_w_radl_detected() {
        // nal type 19 (IDR_W_RADL): (b0 >> 1) & 0x3F == 19 → b0 = 19<<1 = 0x26
        let au = [0, 0, 0, 1, 0x40, 0x01, 0, 0, 0, 1, 0x26, 0x01];
        assert!(is_keyframe(VideoCodec::H265, &au));
    }

    #[test]
    fn hevc_mixed_start_codes_detect_idr() {
        let au = [
            0, 0, 1, 0x40, 0x01, // three-byte VPS
            0, 0, 0, 1, 0x42, 0x01, // four-byte SPS
            0, 0, 1, 0x26, 0x01, // three-byte IDR_W_RADL
        ];
        assert!(is_keyframe(VideoCodec::H265, &au));
    }

    #[test]
    fn hevc_pframe_not_keyframe() {
        let au = [0, 0, 0, 1, 0x02, 0x01]; // nal type 1 (TRAIL_R)
        assert!(!is_keyframe(VideoCodec::H265, &au));
    }

    #[test]
    fn av1_repeated_sequence_header_is_a_recovery_keyframe() {
        let obu = |obu_type: u8, payload: &[u8]| {
            let mut output = vec![(obu_type << 3) | 0x02, payload.len() as u8];
            output.extend_from_slice(payload);
            output
        };
        let mut temporal_unit = obu(2, &[]);
        temporal_unit.extend(obu(1, &[0x10, 0x20]));
        temporal_unit.extend(obu(6, &[0x30]));
        assert!(is_keyframe(VideoCodec::Av1, &temporal_unit));
        assert!(!is_keyframe(VideoCodec::Av1, &obu(6, &[0x30])));
    }

    #[test]
    fn framed_v1_length_is_big_endian() {
        assert_eq!(u32::from_be_bytes([0, 0, 0x10, 0]), 4096);
    }

    #[test]
    fn stable_pipeline_stats_are_visible_at_light_log_tier() {
        assert!(is_pipeline_stats(
            "[capenc] enc_fps=30 avg_encode_ms=4.2 avg_hash_ms=1.4"
        ));
        assert!(!is_pipeline_stats(
            "[capenc] MF: adapter 0 'VMware SVGA 3D'"
        ));
    }

    #[test]
    fn pipeline_stats_are_parsed_into_aggregate_fields() {
        assert_eq!(
            parse_pipeline_stats(
                "[capenc] enc_fps=30 cap_fps=29.5 avg_stage_ms=7.5 max_stage_ms=9.0 \
                 avg_readback_ms=2.0 max_readback_ms=3.0 avg_conversion_ms=5.5 \
                 max_conversion_ms=6.0 avg_encode_ms=2.1 max_encode_ms=4.2 slow=1 dropped=2"
            ),
            PipelineStats {
                encoder_fps: Some(30.0),
                capture_fps: Some(29.5),
                average_stage_ms: Some(7.5),
                maximum_stage_ms: Some(9.0),
                average_readback_ms: Some(2.0),
                maximum_readback_ms: Some(3.0),
                average_conversion_ms: Some(5.5),
                maximum_conversion_ms: Some(6.0),
                average_encode_ms: Some(2.1),
                maximum_encode_ms: Some(4.2),
                slow_frames: Some(1),
                dropped_frames: Some(2),
                ..PipelineStats::default()
            }
        );
    }

    /// The exact line `hosts/capenc/src/win.rs::run_encode` emits once per
    /// second, so a field rename on either side fails here rather than in a
    /// lab log nobody can re-run.
    const FULL_PIPELINE_LINE: &str = "[capenc] enc_fps=27 cap_fps=9 avg_stage_ms=13.40 \
         max_stage_ms=18.10 avg_copy_ms=1.20 max_copy_ms=2.30 \
         avg_readback_ms=2.10 max_readback_ms=3.40 \
         avg_conversion_ms=10.80 max_conversion_ms=12.90 avg_mirror_ms=2.60 \
         max_mirror_ms=4.10 avg_encode_ms=6.20 max_encode_ms=11.30 \
         fresh_encode_count=9 avg_fresh_encode_ms=9.80 max_fresh_encode_ms=14.20 \
         restaged_encode_count=18 avg_restaged_encode_ms=4.40 \
         max_restaged_encode_ms=7.10 blank_encode_count=0 avg_blank_encode_ms=0.00 \
         max_blank_encode_ms=0.00 avg_restage_ms=3.10 max_restage_ms=5.20 \
         restage_copied=3 restage_skipped=15 restage_unavailable=0 kbps=41000 \
         capture_new=9 capture_empty=181 encode_submitted=27 encode_skipped_no_new=0 \
         timeout=180 cursor_only=1 want_idr=false";

    #[test]
    fn pipeline_stats_split_fresh_from_restaged_encode_cost() {
        let stats = parse_pipeline_stats(FULL_PIPELINE_LINE);
        assert_eq!(stats.fresh_encode_count, Some(9));
        assert_eq!(stats.average_fresh_encode_ms, Some(9.80));
        assert_eq!(stats.maximum_fresh_encode_ms, Some(14.20));
        assert_eq!(stats.restaged_encode_count, Some(18));
        assert_eq!(stats.average_restaged_encode_ms, Some(4.40));
        assert_eq!(stats.maximum_restaged_encode_ms, Some(7.10));
        assert_eq!(stats.blank_encode_count, Some(0));
        assert_eq!(stats.average_blank_encode_ms, Some(0.0));
        assert_eq!(stats.maximum_blank_encode_ms, Some(0.0));
        // The legacy aggregate must keep meaning exactly what it meant, or a
        // hardware comparison against the pre-split runs stops being valid.
        assert_eq!(stats.average_encode_ms, Some(6.20));
        assert_eq!(stats.maximum_encode_ms, Some(11.30));
        // Fresh must be dearer than restaged here, and every submission must
        // be accounted for by exactly one bucket.
        assert!(stats.average_fresh_encode_ms > stats.average_restaged_encode_ms);
        assert_eq!(
            stats.fresh_encode_count.unwrap()
                + stats.restaged_encode_count.unwrap()
                + stats.blank_encode_count.unwrap(),
            stats.encode_submitted.unwrap()
        );
    }

    #[test]
    fn pipeline_stats_carry_whole_frame_copy_and_capture_outcomes() {
        let stats = parse_pipeline_stats(FULL_PIPELINE_LINE);
        assert_eq!(stats.average_copy_ms, Some(1.20));
        assert_eq!(stats.maximum_copy_ms, Some(2.30));
        // The DXGI-held copy is a small fraction of the whole readback, which
        // is exactly the evidence that the release-early split is working.
        assert!(stats.average_copy_ms < stats.average_readback_ms);
        assert!(stats.average_copy_ms < stats.average_stage_ms);
        assert_eq!(stats.average_mirror_ms, Some(2.60));
        assert_eq!(stats.maximum_mirror_ms, Some(4.10));
        assert_eq!(stats.average_restage_ms, Some(3.10));
        assert_eq!(stats.maximum_restage_ms, Some(5.20));
        // Every republish is accounted for: copied, skipped because the slot
        // already held the newest generation, or unavailable.
        assert_eq!(stats.restage_copied, Some(3));
        assert_eq!(stats.restage_skipped, Some(15));
        assert_eq!(stats.restage_unavailable, Some(0));
        assert_eq!(
            stats.restage_copied.unwrap()
                + stats.restage_skipped.unwrap()
                + stats.restage_unavailable.unwrap(),
            stats.restaged_encode_count.unwrap()
        );
        assert_eq!(stats.capture_new_frames, Some(9));
        assert_eq!(stats.capture_empty_polls, Some(181));
        assert_eq!(stats.capture_timeouts, Some(180));
        assert_eq!(stats.capture_cursor_only, Some(1));
        assert_eq!(stats.encode_submitted, Some(27));
        assert_eq!(stats.encode_skipped_no_new, Some(0));
        assert_eq!(stats.kilobits_per_second, Some(41_000));
    }

    #[test]
    fn pipeline_stats_detail_is_level_three_and_absent_from_production_level_zero() {
        let production = OperationalProfile::Critical;
        let debug = OperationalProfile::Debug;
        assert!(!production.includes(pipeline_snapshot_profile()));
        assert!(!production.includes(pipeline_detail_profile()));
        assert!(debug.includes(pipeline_snapshot_profile()));
        assert!(debug.includes(pipeline_detail_profile()));
        // Detail is strictly noisier than the operational aggregate, so an
        // operator on Level 2 keeps the summary without the optimization spam.
        assert!(!OperationalProfile::Info.includes(pipeline_detail_profile()));
        assert!(OperationalProfile::Info.includes(pipeline_snapshot_profile()));
        assert!(PIPELINE_SUBMISSION_DETAIL_FIELDS <= arcen_telemetry::MAX_STRUCTURED_FIELDS);
        assert!(PIPELINE_COPY_DETAIL_FIELDS <= arcen_telemetry::MAX_STRUCTURED_FIELDS);
        assert!(PIPELINE_LOOP_DETAIL_FIELDS <= arcen_telemetry::MAX_STRUCTURED_FIELDS);
    }

    #[test]
    fn pipeline_stats_reject_unparseable_metrics_instead_of_defaulting_to_zero() {
        let stats = parse_pipeline_stats(
            "[capenc] enc_fps=30 fresh_encode_count=nan avg_fresh_encode_ms= \
             restaged_encode_count=18",
        );
        assert_eq!(stats.encoder_fps, Some(30.0));
        assert_eq!(stats.fresh_encode_count, None);
        assert_eq!(stats.average_fresh_encode_ms, None);
        assert_eq!(stats.restaged_encode_count, Some(18));
    }

    #[test]
    fn pipeline_stats_update_the_live_snapshot() {
        let telemetry = PipelineTelemetry::default();
        telemetry.record(&parse_pipeline_stats(
            "[capenc] enc_fps=8 cap_fps=9 avg_stage_ms=108.5 max_stage_ms=140 \
             avg_readback_ms=18.5 max_readback_ms=24 avg_conversion_ms=90 \
             max_conversion_ms=116 avg_encode_ms=2.5 max_encode_ms=4 slow=3 dropped=2",
        ));
        assert_eq!(
            telemetry.snapshot(),
            PipelineTelemetrySnapshot {
                encoder_fps: 8.0,
                capture_fps: 9.0,
                average_stage_ms: 108.5,
                maximum_stage_ms: 140.0,
                average_readback_ms: 18.5,
                maximum_readback_ms: 24.0,
                average_conversion_ms: 90.0,
                maximum_conversion_ms: 116.0,
                average_encode_ms: 2.5,
                maximum_encode_ms: 4.0,
                slow_frames: 3,
                dropped_frames: 2,
            }
        );
    }

    #[tokio::test]
    async fn lost_encoded_au_suppresses_chain_until_idr_without_storming() {
        let (tx, mut rx) = mpsc::channel(1);
        let idr = IdrRequester::new(tx);
        let queue = VideoQueue::new(1);
        let frame = |value, keyframe| EncodedFrame {
            data: vec![value],
            keyframe,
            timestamp_ms: 0,
        };
        assert!(enqueue_encoded_frame(&queue, &idr, frame(1, false)));
        assert!(enqueue_encoded_frame(&queue, &idr, frame(2, false)));
        assert!(queue.awaiting_keyframe());
        assert_eq!(queue.len(), 0);
        assert!(enqueue_encoded_frame(&queue, &idr, frame(3, false)));
        assert!(matches!(rx.try_recv(), Ok(StdinCommand::Idr)));
        assert!(
            rx.try_recv().is_err(),
            "repeated losses must coalesce behind one pending IDR"
        );
        assert_eq!(queue.len(), 0);

        assert!(enqueue_encoded_frame(&queue, &idr, frame(9, true)));
        assert!(!queue.awaiting_keyframe());
        assert_eq!(queue.pop().await.unwrap().data, vec![9]);
    }

    #[tokio::test]
    async fn capenc_eof_closes_frame_queue() {
        let (reader, writer) = tokio::io::duplex(16);
        drop(writer);
        let frames = Arc::new(VideoQueue::new(1));
        let (tx, _rx) = mpsc::channel(1);
        read_length_prefixed(
            reader,
            VideoCodec::H264,
            frames.clone(),
            IdrRequester::new(tx),
        )
        .await;
        assert!(frames.pop().await.is_none());
    }

    #[tokio::test]
    async fn oversized_or_truncated_access_unit_is_terminal() {
        for bytes in [
            (MAX_AU_BYTES + 1).to_be_bytes().to_vec(),
            [4u32.to_be_bytes().as_slice(), &[0, 0]].concat(),
        ] {
            let (mut writer, reader) = tokio::io::duplex(16);
            let frames = Arc::new(VideoQueue::new(1));
            let (tx, _rx) = mpsc::channel(1);
            let task = tokio::spawn(read_length_prefixed(
                reader,
                VideoCodec::H264,
                frames.clone(),
                IdrRequester::new(tx),
            ));
            writer.write_all(&bytes).await.expect("write test bytes");
            drop(writer);
            task.await.expect("reader task");
            assert!(frames.pop().await.is_none());
        }
    }

    #[test]
    fn repeated_losses_coalesce_and_retry_when_throttle_reopens() {
        let (tx, mut rx) = mpsc::channel(1);
        let idr = IdrRequester::new(tx);
        assert!(idr.request("first_loss"));
        assert!(!idr.request("second_loss"));
        assert!(matches!(rx.try_recv(), Ok(StdinCommand::Idr)));
        assert!(idr.request("retry_after_guard"));
        assert!(matches!(rx.try_recv(), Ok(StdinCommand::Idr)));
    }

    #[tokio::test]
    async fn stdout_eof_closes_frame_queue_after_draining() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let frames = Arc::new(VideoQueue::new(1));
        let (tx, _rx) = mpsc::channel(1);
        let task = tokio::spawn(read_length_prefixed(
            reader,
            VideoCodec::H264,
            frames.clone(),
            IdrRequester::new(tx),
        ));
        let au = [0, 0, 0, 1, 0x65];
        writer
            .write_all(&(au.len() as u32).to_be_bytes())
            .await
            .unwrap();
        writer.write_all(&au).await.unwrap();
        drop(writer);
        task.await.unwrap();

        assert_eq!(frames.pop().await.unwrap().data, au);
        assert!(frames.pop().await.is_none());
    }

    async fn spawn_shutdown_fixture(script: &str) -> (Child, mpsc::Sender<StdinCommand>) {
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", script])
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
        let (mut child, control) = spawn_shutdown_fixture("exit 0").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            shutdown_child(&mut child, &control, Duration::from_secs(2)).await,
            ChildShutdown::Graceful
        );
    }

    #[tokio::test]
    async fn shutdown_force_kills_only_after_timeout() {
        let (mut child, control) = spawn_shutdown_fixture("Start-Sleep -Seconds 30").await;
        assert_eq!(
            shutdown_child(&mut child, &control, Duration::from_millis(50)).await,
            ChildShutdown::Forced
        );
    }

    #[tokio::test]
    async fn shutdown_timeout_includes_a_blocked_control_send() {
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn shutdown fixture");
        let (control, _receiver) = mpsc::channel(1);
        control.send(StdinCommand::Idr).await.unwrap();

        assert_eq!(
            shutdown_child(&mut child, &control, Duration::from_millis(50)).await,
            ChildShutdown::Forced
        );
    }
}
