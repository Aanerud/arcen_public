//! The Wayland compositions: Auto and Speed (eight-bit SDR) and Grading and
//! HDR (wide).
//!
//! The shared contracts own every number (`arcen_media::video::pipeline`
//! `auto`/`speed`, VBV from `preset.rs`); these modules only choose,
//! once per epoch, which contract a request resolves to and hand the
//! source evidence to that contract's own shared admission:
//!
//! | Path | Module | Admission | Offer | Served cadence |
//! | --- | --- | --- | --- | --- |
//! | `linux-wayland-auto` | [`auto`] | `admit_auto` | SDR8 BGRx/BGRA, up to the Auto ceiling | Auto contract |
//! | `linux-wayland-speed` | [`speed`] | `admit_speed`, with a qualified source rate when one exists | SDR8 BGRx/BGRA, up to the Speed ceiling | Speed contract, or the contract it was visibly downgraded to |
//! | `linux-wayland-grading` | [`grading`] | `admit_grading` | half-float, then ten-bit, BT.2020/PQ from an HDR composition | Grading contract |
//! | `linux-wayland-hdr` | [`hdr`] | `admit_hdr` (Grading only if genuinely admissible) | ten-bit, then half-float, BT.2020/PQ | HDR contract, or Grading |
//! | `linux-wayland-software` | [`software`] | the requested contract's admission with `OpenH264` (the shared software rule) | SDR8 BGRx/BGRA in shared memory, up to the Software ceiling | Software contract (Speed visibly downgraded to it) |
//!
//! Selection happens once, before the source starts ([`WaylandPipeline`]).
//! After admission the epoch runs the *served* contract
//! (`arcen_media::video::AdmittedEpoch`): a
//! Speed request served as Auto runs Auto's cadence, priority and VBV, and
//! says so in the `ADMISSION` line before `READY`. No preset flag reaches
//! the `PipeWire` callback, the import or the encode loop.
//!
//! Auto and Speed share one SDR8 NVENC mechanism (`runner`, Linux only):
//! bounded `PipeWire` leases, the `host-copy` or (on a host that verified
//! it) `gpu-convert` import into a retained CUDA frame, the existing
//! CUDA/NVENC encoder, Keel's `SubmissionGate` and `PendingCaptureDamage`.
//! A request for the `software-h264` encoder is served by the separate
//! software composition (`software`, `software_runner`): the same leases
//! over shared memory only, the shared `OpenH264` session, Keel's hash and
//! idle cadence, and no CUDA at all. Grading and HDR never take either
//! path: they run their own wide mechanism (`wide`, Linux only), which reads
//! a wide DMA-BUF back into host memory and converts it with the shared wide
//! conversion, as Xorg Grading does with `XShm`.

#![cfg_attr(not(all(feature = "wayland", target_os = "linux")), allow(dead_code))]

pub mod auto;
pub mod grading;
pub mod hdr;
pub mod software;
pub mod speed;

#[cfg(all(feature = "wayland", target_os = "linux"))]
mod output;
#[cfg(all(feature = "wayland", target_os = "linux"))]
mod runner;
#[cfg(all(feature = "wayland", target_os = "linux"))]
pub use runner::run;
#[cfg(all(feature = "wayland", feature = "software-h264", target_os = "linux"))]
mod software_runner;
#[cfg(all(feature = "wayland", target_os = "linux"))]
mod wide;

use arcen_media::video::compositor_fidelity::FidelityRoute;
use arcen_media::video::{
    pipeline_contract, pipeline_for_request, AdmissionRequest, CaptureMemory, CapturePixelFormat,
    CaptureRateEvidence, CaptureSourceEvidence, EncoderCapability, MotionPriority, PipelineId,
    PresetAdmission, StreamingPreset,
};
use arcen_media::{EncodeIntent, VideoCodec, VideoConfiguration};
use arcen_protocol::messages::{CursorMode, VideoSelectionIntent};

use super::format::FormatOffer;
use crate::linux_policy::RequestedEncoder;

/// What the parent asked capenc for, read once from argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochRequest {
    /// Requested codec; the shared ladder starts from it.
    pub codec: VideoCodec,
    /// Requested coded format.
    pub video: VideoConfiguration,
    /// Requested frame-rate ceiling.
    pub fps: u32,
    /// Requested priority.
    pub priority: MotionPriority,
    /// Requested encoder intent.
    pub intent: EncodeIntent,
    /// Cursor authority the parent expects.
    pub cursor: CursorMode,
    /// Concrete or automatic encoder choice.
    pub encoder: RequestedEncoder,
    /// Optional native encoder bitrate ceiling.
    pub max_bitrate_bps: Option<u32>,
    /// Requested QP-map experiment arm.
    pub qp_map: crate::qp_map::QpMapPolicy,
    /// A sustained source rate qualified on this host (`capture-rate=`),
    /// used only as Speed admission evidence.
    pub qualified_rate: Option<u32>,
    /// What that rate rests on (`capture-rate-evidence=measured|
    /// inferred:<W>x<H>`, default measured); stated in the admission.
    pub rate_evidence: CaptureRateEvidence,
    /// The Pier's runtime guard found an earlier Speed epoch of this
    /// attachment below Speed's floor (`speed-runtime=below-floor`): Speed
    /// is served as Auto, saying so.
    pub speed_runtime_shortfall: bool,
    /// The host's qualification verified the `gpu-convert` DMA-BUF route
    /// against a host-copy reference (`gpu-import=verified`); without it
    /// DMA-BUF is never offered.
    pub gpu_import_verified: bool,
    /// Length-prefixed output.
    pub framed: bool,
    /// Tagged framed-v2 output: access units and cursor events
    /// (`arcen_media::capenc_stream`). Implies `framed`.
    pub framed_v2: bool,
    /// Native availability probe only (`probe-v1`).
    pub probe: bool,
    /// What the host's qualification proved about wide composition, for the
    /// Grading and HDR compositions (ignored by Auto and Speed).
    pub fidelity: FidelityArgs,
}

/// The wide-composition facts the parent passes from the host's
/// qualification record. Each is absent unless proven; an absent fact makes
/// the shared admission refuse visibly, never assume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FidelityArgs {
    /// How the session reaches a wide composition (`fidelity-route=`).
    pub route: Option<FidelityRoute>,
    /// The format and memory pairs whose ramp and channel proof passed on
    /// this host (`wide-proven=xbgr2101010@dmabuf,rgba-f16@dmabuf`). A
    /// format proves nothing in another memory, and a memory nothing for
    /// another format.
    pub proven: Vec<ProvenWide>,
    /// The composition's measured SDR white, in nits (`sdr-white=`).
    pub sdr_white_nits: Option<u16>,
    /// Measure a drawn ramp and white instead of serving (`fidelity-probe`).
    pub probe: bool,
}

const FIDELITY_ROUTE_ARG: &str = "fidelity-route=";
const WIDE_PROVEN_ARG: &str = "wide-proven=";
const SDR_WHITE_ARG: &str = "sdr-white=";

impl FidelityArgs {
    fn from_args(args: &[String]) -> Result<Self, String> {
        let single = |prefix: &str| -> Result<Option<&str>, String> {
            let mut values = args.iter().filter_map(|arg| arg.strip_prefix(prefix));
            let first = values.next();
            if values.next().is_some() {
                return Err(format!("{prefix} may be specified only once"));
            }
            Ok(first)
        };
        let route = single(FIDELITY_ROUTE_ARG)?
            .map(|value| {
                FidelityRoute::from_token(value)
                    .ok_or_else(|| format!("invalid fidelity-route {value:?}"))
            })
            .transpose()?;
        let mut proven = Vec::new();
        if let Some(value) = single(WIDE_PROVEN_ARG)? {
            for pair in value.split(',') {
                let parsed = ProvenWide::from_token(pair)
                    .ok_or_else(|| format!("invalid wide-proven pair {pair:?}"))?;
                if proven.contains(&parsed) {
                    return Err(format!("wide-proven pair {pair:?} repeated"));
                }
                proven.push(parsed);
            }
        }
        let sdr_white_nits = single(SDR_WHITE_ARG)?
            .map(|value| {
                value
                    .parse::<u16>()
                    .ok()
                    .filter(|nits| (1..=10_000).contains(nits))
                    .ok_or_else(|| format!("invalid sdr-white {value:?}"))
            })
            .transpose()?;
        Ok(Self {
            route,
            proven,
            sdr_white_nits,
            probe: args.iter().any(|argument| argument == "fidelity-probe"),
        })
    }

    /// Whether `format` in `memory` was proven to carry wide content in
    /// channel order.
    #[must_use]
    pub fn proves(&self, format: CapturePixelFormat, memory: CaptureMemory) -> bool {
        self.proven
            .iter()
            .any(|proven| proven.format == format && proven.memory == memory.token())
    }

    /// The `wide-proven=` value, for the log.
    #[must_use]
    pub fn proven_token(&self) -> String {
        if self.proven.is_empty() {
            return "none".to_owned();
        }
        self.proven
            .iter()
            .map(ProvenWide::token)
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// One proven format in one memory kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvenWide {
    /// The wide pixel format.
    pub format: CapturePixelFormat,
    /// [`CaptureMemory::token`]: `dmabuf` or `host-shared`.
    pub memory: &'static str,
}

impl ProvenWide {
    /// Parses `<format>@<memory>`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let (format, memory) = value.split_once('@')?;
        let format = CapturePixelFormat::from_token(format).filter(|format| {
            matches!(
                format,
                CapturePixelFormat::Xrgb2101010
                    | CapturePixelFormat::Xbgr2101010
                    | CapturePixelFormat::RgbaF16
            )
        })?;
        let memory = ["dmabuf", "host-shared"]
            .into_iter()
            .find(|token| *token == memory)?;
        Some(Self { format, memory })
    }

    /// `<format>@<memory>`.
    #[must_use]
    pub fn token(&self) -> String {
        format!("{}@{}", self.format.token(), self.memory)
    }
}

const CAPTURE_RATE_ARG: &str = "capture-rate=";
const CAPTURE_RATE_EVIDENCE_ARG: &str = "capture-rate-evidence=";
const SPEED_RUNTIME_ARG: &str = "speed-runtime=";

fn framed_v2_from_args(args: &[String]) -> bool {
    args.iter()
        .any(|argument| argument == arcen_media::capenc_stream::FRAMED_OUTPUT_V2)
}
const GPU_IMPORT_ARG: &str = "gpu-import=";

impl EpochRequest {
    /// Parses the same argv the Xorg paths read.
    ///
    /// # Errors
    ///
    /// The first malformed, repeated or unsupported argument.
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        let codec_token = args.get(2).map_or("h264", String::as_str);
        let codec = VideoCodec::from_token(codec_token)
            .ok_or_else(|| format!("unrecognized codec {codec_token:?}"))?;
        let fps = args
            .get(3)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(60)
            .clamp(1, 240);
        let yuv444 = args.iter().any(|value| value == "yuv444");
        let color = crate::requested_color(args, yuv444)?;
        let cursor = match crate::cursor_mode_from_args(args)? {
            crate::CursorCaptureMode::Local => CursorMode::Local,
            crate::CursorCaptureMode::Host => CursorMode::Host,
        };
        let mut qualified_rate = None;
        for value in args
            .iter()
            .filter_map(|argument| argument.strip_prefix(CAPTURE_RATE_ARG))
        {
            if qualified_rate.is_some() {
                return Err("capture-rate may be specified only once".to_owned());
            }
            qualified_rate = Some(
                value
                    .parse::<u32>()
                    .ok()
                    .filter(|fps| (1..=1000).contains(fps))
                    .ok_or_else(|| format!("invalid capture-rate {value:?}"))?,
            );
        }
        let mut rate_evidence = None;
        for value in args
            .iter()
            .filter_map(|argument| argument.strip_prefix(CAPTURE_RATE_EVIDENCE_ARG))
        {
            if rate_evidence.is_some() {
                return Err("capture-rate-evidence may be specified only once".to_owned());
            }
            rate_evidence = Some(
                CaptureRateEvidence::from_token(value)
                    .ok_or_else(|| format!("invalid capture-rate-evidence {value:?}"))?,
            );
        }
        if rate_evidence.is_some() && qualified_rate.is_none() {
            return Err("capture-rate-evidence needs a capture-rate".to_owned());
        }
        let mut speed_runtime_shortfall = None;
        for value in args
            .iter()
            .filter_map(|argument| argument.strip_prefix(SPEED_RUNTIME_ARG))
        {
            if speed_runtime_shortfall.is_some() {
                return Err("speed-runtime may be specified only once".to_owned());
            }
            speed_runtime_shortfall = Some(match value {
                "below-floor" => true,
                other => return Err(format!("invalid speed-runtime {other:?}")),
            });
        }
        let mut gpu_import_verified = None;
        for value in args
            .iter()
            .filter_map(|argument| argument.strip_prefix(GPU_IMPORT_ARG))
        {
            if gpu_import_verified.is_some() {
                return Err("gpu-import may be specified only once".to_owned());
            }
            gpu_import_verified = Some(match value {
                "verified" => true,
                other => return Err(format!("invalid gpu-import {other:?}")),
            });
        }
        Ok(Self {
            codec,
            video: VideoConfiguration {
                codec,
                chroma: color.chroma,
                bit_depth: color.bit_depth,
                range: color.range,
                matrix: color.matrix,
                primaries: color.primaries,
                transfer: color.transfer,
            },
            fps,
            priority: crate::requested_motion_priority(args)?,
            intent: crate::requested_intent(args)?,
            cursor,
            encoder: RequestedEncoder::from_args(args)?,
            max_bitrate_bps: crate::requested_encoder_max_bitrate_bps(args)?,
            qp_map: crate::requested_qp_map(args)?,
            qualified_rate,
            rate_evidence: rate_evidence.unwrap_or_default(),
            speed_runtime_shortfall: speed_runtime_shortfall.unwrap_or(false),
            gpu_import_verified: gpu_import_verified.unwrap_or(false),
            framed: crate::framed_output_from_args(args) || framed_v2_from_args(args),
            framed_v2: framed_v2_from_args(args),
            probe: args.iter().any(|argument| argument == "probe-v1"),
            fidelity: FidelityArgs::from_args(args)?,
        })
    }
}

/// Why a request has no Wayland composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionRefusal {
    /// The request resolves to a contract this provider has no composition
    /// for, or to no named contract at all (exact formats).
    NoComposition(Option<PipelineId>),
    /// The request names a contract with another encoder intent than the
    /// contract's.
    IntentMismatch,
}

impl std::fmt::Display for SelectionRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoComposition(Some(pipeline)) => write!(
                formatter,
                "the Wayland provider has no {} composition; Auto, Speed, Grading and HDR are \
                 served",
                pipeline.token()
            ),
            Self::NoComposition(None) => write!(
                formatter,
                "an exact format request has no Wayland composition; only the named contracts are \
                 served"
            ),
            Self::IntentMismatch => write!(
                formatter,
                "the requested encoder intent is not the requested contract's"
            ),
        }
    }
}

/// The coded format both contracts serve, for `codec`: the shared Auto
/// contract's colour (Speed's is the same).
#[must_use]
pub const fn sdr8_video(codec: VideoCodec) -> VideoConfiguration {
    VideoConfiguration {
        codec,
        ..pipeline_contract(PipelineId::Auto).colour
    }
}

/// The composition serving an epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandPipeline {
    /// `linux-wayland-auto`.
    Auto(auto::AutoPipeline),
    /// `linux-wayland-speed`.
    Speed(speed::SpeedPipeline),
    /// `linux-wayland-grading`.
    Grading(grading::GradingPipeline),
    /// `linux-wayland-hdr`.
    Hdr(hdr::HdrPipeline),
}

impl WaylandPipeline {
    /// Resolves the request with the shared preset rule, once.
    ///
    /// # Errors
    ///
    /// [`SelectionRefusal`] when no Wayland composition serves it.
    pub fn select(request: &EpochRequest) -> Result<Self, SelectionRefusal> {
        let selection = if request.video == sdr8_video(request.codec) {
            VideoSelectionIntent::AdaptivePerformance
        } else {
            VideoSelectionIntent::ColorFidelity
        };
        let pipeline =
            match pipeline_for_request(selection, request.video, request.fps, request.priority) {
                Some(PipelineId::Auto) => Self::Auto(auto::AutoPipeline),
                Some(PipelineId::Speed) => Self::Speed(
                    speed::SpeedPipeline::new(request.qualified_rate)
                        .with_rate_evidence(request.rate_evidence)
                        .after_runtime_shortfall(request.speed_runtime_shortfall),
                ),
                Some(PipelineId::Grading) => Self::Grading(grading::GradingPipeline),
                Some(PipelineId::Hdr) => Self::Hdr(hdr::HdrPipeline),
                other => return Err(SelectionRefusal::NoComposition(other)),
            };
        if request.intent != pipeline_contract(pipeline.id()).intent {
            return Err(SelectionRefusal::IntentMismatch);
        }
        Ok(pipeline)
    }

    /// The requested contract.
    #[must_use]
    pub const fn id(self) -> PipelineId {
        match self {
            Self::Auto(_) => auto::AutoPipeline::ID,
            Self::Speed(_) => speed::SpeedPipeline::ID,
            Self::Grading(_) => grading::GradingPipeline::ID,
            Self::Hdr(_) => hdr::HdrPipeline::ID,
        }
    }

    /// The preset the request resolved to.
    #[must_use]
    pub const fn preset(self) -> StreamingPreset {
        match self {
            Self::Auto(_) => StreamingPreset::Auto,
            Self::Speed(_) => StreamingPreset::Speed,
            Self::Grading(_) => StreamingPreset::Grading,
            Self::Hdr(_) => StreamingPreset::Hdr,
        }
    }

    /// Whether this composition captures a wide format (Grading, HDR) and
    /// runs the wide read-back, never the eight-bit SDR runner.
    #[must_use]
    pub const fn is_wide(self) -> bool {
        matches!(self, Self::Grading(_) | Self::Hdr(_))
    }

    /// What to ask the producer for, at no more than `requested_fps`.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        match self {
            Self::Auto(pipeline) => pipeline.offer(requested_fps),
            Self::Speed(pipeline) => pipeline.offer(requested_fps),
            Self::Grading(pipeline) => pipeline.offer(requested_fps),
            Self::Hdr(pipeline) => pipeline.offer(requested_fps),
        }
    }

    /// The sustained-rate evidence this composition contributes.
    #[must_use]
    pub const fn rate_evidence(self) -> Option<u32> {
        match self {
            Self::Auto(pipeline) => pipeline.rate_evidence(),
            Self::Speed(pipeline) => pipeline.rate_evidence(),
            Self::Grading(_) | Self::Hdr(_) => None,
        }
    }

    /// What the rate evidence rests on.
    #[must_use]
    pub const fn rate_source(self) -> CaptureRateEvidence {
        match self {
            // Grading and HDR are admitted at their contract rate, never on
            // an inferred one.
            Self::Auto(_) | Self::Grading(_) | Self::Hdr(_) => CaptureRateEvidence::Measured,
            Self::Speed(pipeline) => pipeline.rate_source(),
        }
    }

    /// Runs this composition's shared admission.
    #[must_use]
    pub fn admit(
        self,
        request: AdmissionRequest,
        source: &CaptureSourceEvidence,
        encoder: &EncoderCapability,
    ) -> PresetAdmission {
        match self {
            Self::Auto(pipeline) => pipeline.admit(request, source, encoder),
            Self::Speed(pipeline) => pipeline.admit(request, source, encoder),
            Self::Grading(pipeline) => pipeline.admit(request, source, encoder),
            Self::Hdr(pipeline) => pipeline.admit(request, source, encoder),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qp_map::QpMapPolicy;

    pub(super) fn args(tokens: &[&str]) -> Vec<String> {
        std::iter::once("capenc")
            .chain(tokens.iter().copied())
            .map(str::to_owned)
            .collect()
    }

    /// The argv the Linux Pier builds for an Auto/Speed request.
    pub(super) fn pier_argv(fps: &str, priority: Option<&str>) -> Vec<String> {
        let mut tokens = vec![
            "0",
            "h264",
            fps,
            "framed-v1",
            "encoder=nvenc",
            "cursor=host",
            "variant=h264-420-8-limited-bt709",
        ];
        if let Some(priority) = priority {
            tokens.push(priority);
        }
        args(&tokens)
    }

    #[test]
    fn framed_v2_is_parsed_and_implies_framed_output() {
        let v1 = EpochRequest::from_args(&pier_argv("60", None)).unwrap();
        assert!(v1.framed && !v1.framed_v2);
        let v2 = EpochRequest::from_args(&args(&["0", "h264", "60", "framed-v2", "cursor=local"]))
            .unwrap();
        assert!(v2.framed && v2.framed_v2);
        assert_eq!(v2.cursor, CursorMode::Local);
    }

    #[test]
    fn the_pier_argv_parses_into_one_request() {
        let request = EpochRequest::from_args(&pier_argv("60", Some("priority=motion"))).unwrap();
        assert_eq!(request.video, sdr8_video(VideoCodec::H264));
        assert_eq!(request.fps, 60);
        assert_eq!(request.priority, MotionPriority::Motion);
        assert_eq!(request.cursor, CursorMode::Host);
        assert_eq!(request.encoder, RequestedEncoder::Nvenc);
        assert_eq!(request.qp_map, QpMapPolicy::Off);
        assert_eq!(request.qualified_rate, None);
        assert!(request.framed);
        assert!(!request.probe);
        let mut rated = pier_argv("60", Some("priority=motion"));
        rated.push("capture-rate=35".into());
        assert_eq!(
            EpochRequest::from_args(&rated).unwrap().qualified_rate,
            Some(35)
        );
        rated.push("capture-rate=36".into());
        assert!(EpochRequest::from_args(&rated).is_err());
        let mut zero = pier_argv("60", None);
        zero.push("capture-rate=0".into());
        assert!(EpochRequest::from_args(&zero).is_err());
        assert!(!request.gpu_import_verified);
        let mut verified = pier_argv("60", None);
        verified.push("gpu-import=verified".into());
        assert!(
            EpochRequest::from_args(&verified)
                .unwrap()
                .gpu_import_verified
        );
        verified.push("gpu-import=verified".into());
        assert!(EpochRequest::from_args(&verified).is_err());
        let mut claimed = pier_argv("60", None);
        claimed.push("gpu-import=yes".into());
        assert!(EpochRequest::from_args(&claimed).is_err());
    }

    #[test]
    fn selection_uses_the_shared_preset_rule_once() {
        let auto = EpochRequest::from_args(&pier_argv("30", None)).unwrap();
        assert_eq!(
            WaylandPipeline::select(&auto).unwrap().id(),
            PipelineId::Auto
        );
        let speed = EpochRequest::from_args(&pier_argv("60", Some("priority=motion"))).unwrap();
        assert_eq!(
            WaylandPipeline::select(&speed).unwrap().id(),
            PipelineId::Speed
        );
        // The shared rule: more than 30 fps is Speed even without motion.
        let fast = EpochRequest::from_args(&pier_argv("60", None)).unwrap();
        assert_eq!(
            WaylandPipeline::select(&fast).unwrap().id(),
            PipelineId::Speed
        );
    }

    #[test]
    fn grading_and_hdr_select_their_own_compositions() {
        let grading_argv = |extra: &[&str]| {
            let mut tokens = vec![
                "0",
                "h265",
                "30",
                "yuv444",
                "variant=hevc-444-10-full-bt709",
            ];
            tokens.extend_from_slice(extra);
            EpochRequest::from_args(&args(&tokens)).unwrap()
        };
        let grading = grading_argv(&["intent=quality"]);
        let selected = WaylandPipeline::select(&grading).unwrap();
        assert_eq!(selected.id(), PipelineId::Grading);
        assert!(selected.is_wide());
        let hdr = grading_argv(&["intent=quality", "transfer=pq", "primaries=bt2020"]);
        let selected = WaylandPipeline::select(&hdr).unwrap();
        assert_eq!(selected.id(), PipelineId::Hdr);
        assert!(selected.is_wide());
        assert!(!WaylandPipeline::select(
            &EpochRequest::from_args(&pier_argv("30", None)).unwrap()
        )
        .unwrap()
        .is_wide());
        // A fidelity contract with the interactive encoder is not that contract.
        assert_eq!(
            WaylandPipeline::select(&grading_argv(&[])),
            Err(SelectionRefusal::IntentMismatch)
        );
        let mut quality = pier_argv("30", None);
        quality.push("intent=quality".into());
        assert_eq!(
            WaylandPipeline::select(&EpochRequest::from_args(&quality).unwrap()),
            Err(SelectionRefusal::IntentMismatch)
        );
    }

    #[test]
    fn fidelity_facts_parse_strictly_and_default_to_unproven() {
        let plain = EpochRequest::from_args(&pier_argv("30", None)).unwrap();
        assert_eq!(plain.fidelity, FidelityArgs::default());
        let mut argv = pier_argv("30", None);
        argv.extend(
            [
                "fidelity-route=force-hdr",
                "wide-proven=xbgr2101010@dmabuf,rgba-f16@dmabuf",
                "sdr-white=203",
            ]
            .map(String::from),
        );
        let facts = EpochRequest::from_args(&argv).unwrap().fidelity;
        assert_eq!(facts.route, Some(FidelityRoute::ForcedHdr));
        let dmabuf = CaptureMemory::DmaBuf {
            modifier: arcen_media::video::FormatModifier::LINEAR,
        };
        assert!(facts.proves(CapturePixelFormat::Xbgr2101010, dmabuf));
        assert!(facts.proves(CapturePixelFormat::RgbaF16, dmabuf));
        // Per format and memory: neither an unmeasured format nor another
        // memory is vouched for.
        assert!(!facts.proves(CapturePixelFormat::Xrgb2101010, dmabuf));
        assert!(!facts.proves(CapturePixelFormat::Xbgr2101010, CaptureMemory::HostShared));
        assert_eq!(facts.proven_token(), "xbgr2101010@dmabuf,rgba-f16@dmabuf");
        assert_eq!(facts.sdr_white_nits, Some(203));
        for bad in [
            "fidelity-route=always",
            "wide-proven=dmabuf",
            "wide-proven=bgrx8@dmabuf",
            "wide-proven=xbgr2101010@gpu-device",
            "wide-proven=xbgr2101010@dmabuf,xbgr2101010@dmabuf",
            "sdr-white=0",
        ] {
            let mut argv = pier_argv("30", None);
            argv.push(bad.into());
            assert!(EpochRequest::from_args(&argv).is_err(), "{bad}");
        }
        let mut repeated = pier_argv("30", None);
        repeated.extend(["sdr-white=203", "sdr-white=203"].map(String::from));
        assert!(EpochRequest::from_args(&repeated).is_err());
    }
}
