//! The capenc `ADMISSION` line: what a capture source admitted, stated
//! before `READY`.
//!
//! A provider whose source facts are known only once it has negotiated a
//! format and received a real frame (a `PipeWire` screencast) decides
//! admission inside the capture process with [`super::admit_capture_source`].
//! The parent must learn that decision, and the first-frame evidence behind
//! it, before it accepts `READY`; otherwise a Speed request served as Auto
//! would either fail the strict `READY` match or, worse, be reported as Speed.
//!
//! The line is the serialisation of the existing [`PresetAdmission`]
//! vocabulary plus the epoch's first frame, nothing new:
//!
//! ```text
//! [capenc] ADMISSION version=1 requested=speed requested_fps=60
//!     decision=downgrade reason=copy_path_not_rate_qualified served=auto
//!     codec=h264 fps=30
//!     transform=sdr8-rgb colour=assumed cursor_mode=host capture=pipewire
//!     copy_path=host-copy cursor=composited measured_fps=unmeasured node=36
//!     format=bgrx8 width=1920 height=1080 memory=host-shared sid=…
//! ```
//!
//! [`CaptureAdmissionNotice::check_request`] is the parent's single gate:
//! the notice must answer the request the parent made, serve a permitted
//! downgrade at no more than the requested rate and codec, and state cursor
//! truth. Unknown, duplicate or malformed fields are refused, as for `READY`.

use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use arcen_protocol::messages::CursorMode;

use super::capture_admission::{
    AdmissionReason, AdmittedCapture, PresetAdmission, SourceTransform,
};
use super::capture_surface::{
    CaptureMemory, CapturePixelFormat, CopyPath, CursorEvidence, FormatModifier,
};
use super::pipeline::PipelineId;
use super::{CaptureBackend, CaptureRateEvidence, PlanDegradation, Rgb10Signal, StreamingPreset};
use crate::VideoCodec;

/// Prefix of the line on capenc's stderr.
pub const ADMISSION_PREFIX: &str = "[capenc] ADMISSION ";
const ADMISSION_VERSION: &str = "1";

/// The first real frame an epoch received, as the capture consumer saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstFrameEvidence {
    /// Producer node the frame came from.
    pub node_id: u32,
    /// Pixel format of the frame.
    pub format: CapturePixelFormat,
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// Memory the frame arrived in.
    pub memory: CaptureMemory,
}

/// One epoch's admission, with the evidence the parent needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureAdmissionNotice {
    /// Preset the request resolved to.
    pub requested: StreamingPreset,
    /// Frame-rate ceiling the request asked for; the served rate never
    /// exceeds it.
    pub requested_fps: u32,
    /// The shared admission outcome.
    pub admission: PresetAdmission,
    /// Capture API family.
    pub capture: CaptureBackend,
    /// Path pixels take to the encoder.
    pub copy_path: CopyPath,
    /// Cursor representation of the source.
    pub cursor: CursorEvidence,
    /// Sustained source rate used as admission evidence, when measured.
    pub measured_fps: Option<u32>,
    /// What that rate rests on: measured on this output, or inferred from a
    /// larger one (`rate_evidence=inferred:<W>x<H>`, written only then).
    pub rate_evidence: CaptureRateEvidence,
    /// The first real frame.
    pub frame: FirstFrameEvidence,
}

/// Why a notice line was refused, or does not answer the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionNoticeError {
    /// Not an `ADMISSION` line.
    MissingPrefix,
    /// A token is not `name=value`.
    MalformedToken(String),
    /// A field appears twice.
    DuplicateField(String),
    /// A required field is absent.
    MissingField(&'static str),
    /// A field's value is not in the vocabulary.
    InvalidValue(&'static str),
    /// Fields this version does not define.
    UnknownFields(String),
    /// Fields contradict each other or the decision.
    Inconsistent(&'static str),
    /// The notice answers a different request.
    RequestConflict(&'static str),
    /// The source cannot serve the requested preset at all.
    Unavailable(AdmissionReason),
}

impl Display for AdmissionNoticeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => {
                write!(formatter, "capture admission unavailable: {reason}")
            }
            other => write!(formatter, "{other:?}"),
        }
    }
}

impl std::error::Error for AdmissionNoticeError {}

const fn preset_token(preset: StreamingPreset) -> &'static str {
    preset.pipeline().token()
}

fn parse_preset(value: &str) -> Option<StreamingPreset> {
    [
        StreamingPreset::Auto,
        StreamingPreset::Speed,
        StreamingPreset::Grading,
        StreamingPreset::Hdr,
    ]
    .into_iter()
    .find(|preset| preset_token(*preset) == value)
}

fn parse_pipeline(value: &str) -> Option<PipelineId> {
    PipelineId::ALL
        .iter()
        .copied()
        .find(|pipeline| pipeline.token() == value)
}

fn parse_copy_path(value: &str) -> Option<CopyPath> {
    [
        CopyPath::ZeroCopyImport,
        CopyPath::GpuConvert,
        CopyPath::HostCopy,
        CopyPath::CrossGpu,
    ]
    .into_iter()
    .find(|path| path.token() == value)
}

fn parse_cursor(value: &str) -> Option<CursorEvidence> {
    [
        CursorEvidence::SeparateMetadata,
        CursorEvidence::Composited,
        CursorEvidence::Absent,
        CursorEvidence::Unknown,
    ]
    .into_iter()
    .find(|cursor| cursor.token() == value)
}

fn parse_transform(value: &str, sdr_white_nits: Option<u16>) -> Option<SourceTransform> {
    Some(match value {
        "sdr8-rgb" => SourceTransform::Sdr8Rgb,
        "sdr8-yuv" => SourceTransform::Sdr8YuvPassthrough,
        "rgb10-direct" => SourceTransform::PackedRgb10(Rgb10Signal::Direct),
        "rgb10-pq-to-sdr" => SourceTransform::PackedRgb10(Rgb10Signal::PqBt2020ToSdrBt709),
        "f16-pq-direct" => SourceTransform::HalfFloatPq(Rgb10Signal::Direct),
        "f16-pq-to-sdr" => SourceTransform::HalfFloatPq(Rgb10Signal::PqBt2020ToSdrBt709),
        "linear-to-sdr" => SourceTransform::LinearToSdr,
        "linear-to-pq" => SourceTransform::LinearToPq {
            sdr_white_nits: sdr_white_nits?,
        },
        _ => return None,
    })
}

const fn cursor_mode_token(mode: Option<CursorMode>) -> &'static str {
    match mode {
        Some(CursorMode::Local) => "local",
        Some(CursorMode::Host) => "host",
        None => "unreported",
    }
}

fn memory_tokens(memory: CaptureMemory) -> String {
    match memory {
        CaptureMemory::DmaBuf { modifier } => {
            format!("memory={} modifier={:#018x}", memory.token(), modifier.0)
        }
        other => format!("memory={}", other.token()),
    }
}

/// Whether `served` is a truthful answer to `requested` under `decision`.
///
/// Exact admission serves the requested contract; a software encoder serves
/// the shared Software contract for Auto (and, as a downgrade, for Speed).
/// The only other downgrades the shared admission produces are Speed to Auto
/// and HDR to a genuine Grading.
#[must_use]
pub const fn permitted_answer(requested: StreamingPreset, admission: &PresetAdmission) -> bool {
    let requested = requested.pipeline();
    match admission {
        PresetAdmission::Admit(served) => matches!(
            (requested, served.pipeline),
            (PipelineId::Auto, PipelineId::Auto | PipelineId::Software)
                | (PipelineId::Speed, PipelineId::Speed)
                | (PipelineId::Grading, PipelineId::Grading)
                | (PipelineId::Hdr, PipelineId::Hdr)
        ),
        PresetAdmission::Downgrade { served, .. } => matches!(
            (requested, served.pipeline),
            (PipelineId::Speed, PipelineId::Auto | PipelineId::Software)
                | (PipelineId::Hdr, PipelineId::Grading)
        ),
        PresetAdmission::Unavailable { .. } => true,
    }
}

impl CaptureAdmissionNotice {
    /// The plan to serve, when any.
    #[must_use]
    pub const fn served(&self) -> Option<&AdmittedCapture> {
        self.admission.served()
    }

    /// Operator-visible difference between the requested contract and what
    /// is served, in the existing degradation vocabulary.
    #[must_use]
    pub fn degradation(&self) -> Option<PlanDegradation> {
        self.admission
            .degradation(self.requested, self.requested_fps)
    }

    /// Checks that this notice answers a parent's request and returns the
    /// plan to expect in `READY`.
    ///
    /// # Errors
    ///
    /// [`AdmissionNoticeError::RequestConflict`] naming the first mismatch,
    /// or [`AdmissionNoticeError::Unavailable`] when nothing can be served.
    pub fn check_request(
        &self,
        requested: StreamingPreset,
        requested_fps: u32,
        requested_codec: VideoCodec,
        cursor_mode: CursorMode,
    ) -> Result<&AdmittedCapture, AdmissionNoticeError> {
        if self.requested != requested {
            return Err(AdmissionNoticeError::RequestConflict("requested"));
        }
        if self.requested_fps != requested_fps {
            return Err(AdmissionNoticeError::RequestConflict("requested_fps"));
        }
        if let PresetAdmission::Unavailable { reason, .. } = self.admission {
            return Err(AdmissionNoticeError::Unavailable(reason));
        }
        if !permitted_answer(self.requested, &self.admission) {
            return Err(AdmissionNoticeError::RequestConflict("served"));
        }
        let served = self
            .served()
            .ok_or(AdmissionNoticeError::Inconsistent("served"))?;
        if served.fps_ceiling == 0 || served.fps_ceiling > requested_fps {
            return Err(AdmissionNoticeError::RequestConflict("fps"));
        }
        if served.codec != requested_codec {
            return Err(AdmissionNoticeError::RequestConflict("codec"));
        }
        if served.cursor_mode != Some(cursor_mode) {
            return Err(AdmissionNoticeError::RequestConflict("cursor_mode"));
        }
        if served.copy_path != self.copy_path {
            return Err(AdmissionNoticeError::Inconsistent("copy_path"));
        }
        Ok(served)
    }

    /// The canonical line.
    #[must_use]
    pub fn format(&self, session_log_id: Option<&str>) -> String {
        let mut fields = vec![
            format!("version={ADMISSION_VERSION}"),
            format!("requested={}", preset_token(self.requested)),
            format!("requested_fps={}", self.requested_fps),
            format!("decision={}", self.admission.decision_token()),
        ];
        match self.admission {
            PresetAdmission::Admit(_) => {}
            PresetAdmission::Downgrade { reason, .. } => {
                fields.push(format!("reason={}", reason.token()));
            }
            PresetAdmission::Unavailable {
                reason, fallback, ..
            } => {
                fields.push(format!("reason={}", reason.token()));
                if let Some(fallback) = fallback {
                    fields.push(format!("fallback={}", fallback.token()));
                }
            }
        }
        if let Some(served) = self.served() {
            fields.push(format!("served={}", served.pipeline.token()));
            fields.push(format!("codec={}", served.codec.token()));
            fields.push(format!("fps={}", served.fps_ceiling));
            if let Some(limited) = served.fps_limited_by {
                fields.push(format!("fps_limited_by={}", limited.token()));
            }
            fields.push(format!("transform={}", served.transform.token()));
            if let SourceTransform::LinearToPq { sdr_white_nits } = served.transform {
                fields.push(format!("sdr_white_nits={sdr_white_nits}"));
            }
            fields.push(format!(
                "colour={}",
                if served.colour_assumed {
                    "assumed"
                } else {
                    "reported"
                }
            ));
            fields.push(format!(
                "cursor_mode={}",
                cursor_mode_token(served.cursor_mode)
            ));
        }
        fields.push(format!("capture={}", self.capture.ready_token()));
        fields.push(format!("copy_path={}", self.copy_path.token()));
        fields.push(format!("cursor={}", self.cursor.token()));
        fields.push(format!(
            "measured_fps={}",
            self.measured_fps
                .map_or_else(|| "unmeasured".to_owned(), |fps| fps.to_string())
        ));
        if self.rate_evidence.is_inferred() {
            fields.push(format!("rate_evidence={}", self.rate_evidence.token()));
        }
        fields.push(format!("node={}", self.frame.node_id));
        fields.push(format!("format={}", self.frame.format.token()));
        fields.push(format!("width={}", self.frame.width));
        fields.push(format!("height={}", self.frame.height));
        fields.push(memory_tokens(self.frame.memory));
        if let Some(sid) = session_log_id {
            fields.push(format!("sid={sid}"));
        }
        format!("{ADMISSION_PREFIX}{}", fields.join(" "))
    }

    /// Parses a line produced by [`Self::format`]. The `sid` field, when
    /// present, is returned separately.
    ///
    /// # Errors
    ///
    /// [`AdmissionNoticeError`] for anything malformed, unknown or
    /// contradictory.
    #[allow(clippy::too_many_lines)]
    pub fn parse(line: &str) -> Result<(Self, Option<String>), AdmissionNoticeError> {
        let payload = line
            .strip_prefix(ADMISSION_PREFIX)
            .ok_or(AdmissionNoticeError::MissingPrefix)?;
        let mut fields = BTreeMap::new();
        for token in payload.split_ascii_whitespace() {
            let (name, value) = token
                .split_once('=')
                .filter(|(name, value)| !name.is_empty() && !value.is_empty())
                .ok_or_else(|| AdmissionNoticeError::MalformedToken(token.to_owned()))?;
            if fields.insert(name, value).is_some() {
                return Err(AdmissionNoticeError::DuplicateField(name.to_owned()));
            }
        }
        let rate_evidence_token = fields.remove("rate_evidence");
        let mut take = |name: &'static str| {
            fields
                .remove(name)
                .ok_or(AdmissionNoticeError::MissingField(name))
        };
        if take("version")? != ADMISSION_VERSION {
            return Err(AdmissionNoticeError::InvalidValue("version"));
        }
        let requested = parse_preset(take("requested")?)
            .ok_or(AdmissionNoticeError::InvalidValue("requested"))?;
        let requested_fps = take("requested_fps")?
            .parse::<u32>()
            .ok()
            .filter(|fps| *fps > 0)
            .ok_or(AdmissionNoticeError::InvalidValue("requested_fps"))?;
        let decision = take("decision")?;
        let capture = CaptureBackend::from_token(take("capture")?)
            .ok_or(AdmissionNoticeError::InvalidValue("capture"))?;
        let copy_path = parse_copy_path(take("copy_path")?)
            .ok_or(AdmissionNoticeError::InvalidValue("copy_path"))?;
        let cursor =
            parse_cursor(take("cursor")?).ok_or(AdmissionNoticeError::InvalidValue("cursor"))?;
        let measured_fps = match take("measured_fps")? {
            "unmeasured" => None,
            value => Some(
                value
                    .parse()
                    .map_err(|_| AdmissionNoticeError::InvalidValue("measured_fps"))?,
            ),
        };
        let rate_evidence = match rate_evidence_token {
            None => CaptureRateEvidence::Measured,
            Some(value) => match CaptureRateEvidence::from_token(value) {
                Some(evidence @ CaptureRateEvidence::Inferred { .. }) if measured_fps.is_some() => {
                    evidence
                }
                Some(_) => return Err(AdmissionNoticeError::Inconsistent("rate_evidence")),
                None => return Err(AdmissionNoticeError::InvalidValue("rate_evidence")),
            },
        };
        let number = |value: &str, name: &'static str| {
            value
                .parse::<u32>()
                .map_err(|_| AdmissionNoticeError::InvalidValue(name))
        };
        let node_id = number(take("node")?, "node")?;
        let format = CapturePixelFormat::from_token(take("format")?)
            .ok_or(AdmissionNoticeError::InvalidValue("format"))?;
        let width = number(take("width")?, "width")?;
        let height = number(take("height")?, "height")?;
        let memory = match take("memory")? {
            "host-shared" => CaptureMemory::HostShared,
            "gpu-device" => CaptureMemory::GpuDevice,
            "dmabuf" => {
                let modifier = take("modifier")?;
                let hex = modifier
                    .strip_prefix("0x")
                    .ok_or(AdmissionNoticeError::InvalidValue("modifier"))?;
                CaptureMemory::DmaBuf {
                    modifier: FormatModifier(
                        u64::from_str_radix(hex, 16)
                            .map_err(|_| AdmissionNoticeError::InvalidValue("modifier"))?,
                    ),
                }
            }
            _ => return Err(AdmissionNoticeError::InvalidValue("memory")),
        };
        let pipeline = requested.pipeline();
        let reason = |value: &str| {
            AdmissionReason::from_token(value, pipeline)
                .ok_or(AdmissionNoticeError::InvalidValue("reason"))
        };
        let served = if decision == "unavailable" {
            None
        } else {
            let served_pipeline = parse_pipeline(take("served")?)
                .ok_or(AdmissionNoticeError::InvalidValue("served"))?;
            let codec = VideoCodec::from_token(take("codec")?)
                .ok_or(AdmissionNoticeError::InvalidValue("codec"))?;
            let fps_ceiling = number(take("fps")?, "fps")?;
            let fps_limited_by = fields
                .remove("fps_limited_by")
                .map(|value| {
                    AdmissionReason::from_token(value, pipeline)
                        .ok_or(AdmissionNoticeError::InvalidValue("fps_limited_by"))
                })
                .transpose()?;
            let sdr_white_nits = fields
                .remove("sdr_white_nits")
                .map(|value| {
                    value
                        .parse::<u16>()
                        .map_err(|_| AdmissionNoticeError::InvalidValue("sdr_white_nits"))
                })
                .transpose()?;
            let mut take = |name: &'static str| {
                fields
                    .remove(name)
                    .ok_or(AdmissionNoticeError::MissingField(name))
            };
            let transform = parse_transform(take("transform")?, sdr_white_nits)
                .ok_or(AdmissionNoticeError::InvalidValue("transform"))?;
            let colour_assumed = match take("colour")? {
                "assumed" => true,
                "reported" => false,
                _ => return Err(AdmissionNoticeError::InvalidValue("colour")),
            };
            let cursor_mode = match take("cursor_mode")? {
                "local" => Some(CursorMode::Local),
                "host" => Some(CursorMode::Host),
                "unreported" => None,
                _ => return Err(AdmissionNoticeError::InvalidValue("cursor_mode")),
            };
            Some(AdmittedCapture {
                pipeline: served_pipeline,
                codec,
                fps_ceiling,
                fps_limited_by,
                transform,
                copy_path,
                colour_assumed,
                cursor_mode,
            })
        };
        let admission = match (decision, served) {
            ("admit", Some(served)) => PresetAdmission::Admit(served),
            ("downgrade", Some(served)) => PresetAdmission::Downgrade {
                requested,
                served,
                reason: reason(
                    fields
                        .remove("reason")
                        .ok_or(AdmissionNoticeError::MissingField("reason"))?,
                )?,
            },
            ("unavailable", None) => PresetAdmission::Unavailable {
                requested,
                reason: reason(
                    fields
                        .remove("reason")
                        .ok_or(AdmissionNoticeError::MissingField("reason"))?,
                )?,
                fallback: fields.remove("fallback").map(reason).transpose()?,
            },
            _ => return Err(AdmissionNoticeError::InvalidValue("decision")),
        };
        let sid = fields.remove("sid").map(str::to_owned);
        if !fields.is_empty() {
            return Err(AdmissionNoticeError::UnknownFields(
                fields.keys().copied().collect::<Vec<_>>().join(","),
            ));
        }
        let notice = Self {
            requested,
            requested_fps,
            admission,
            capture,
            copy_path,
            cursor,
            measured_fps,
            rate_evidence,
            frame: FirstFrameEvidence {
                node_id,
                format,
                width,
                height,
                memory,
            },
        };
        if !permitted_answer(requested, &notice.admission) {
            return Err(AdmissionNoticeError::Inconsistent("served"));
        }
        if notice
            .served()
            .is_some_and(|served| served.fps_ceiling > requested_fps)
        {
            return Err(AdmissionNoticeError::Inconsistent("fps"));
        }
        Ok((notice, sid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{
        AdmissionRequest, CaptureSourceEvidence, EncoderBackend, EncoderCapability, GpuMatch,
        SourceColour, SourceFact, admit_capture_source,
    };

    fn evidence(measured: Option<u32>) -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            backend: CaptureBackend::PipeWire,
            format: CapturePixelFormat::Bgrx8,
            colour: SourceColour::UNKNOWN,
            sample_precision: SourceFact::Unknown,
            output_hdr_active: SourceFact::Unknown,
            copy_path: CopyPath::HostCopy,
            gpu: GpuMatch::Unknown,
            sustained_fps: measured.map_or(SourceFact::Unknown, SourceFact::Authoritative),
            cursor: CursorEvidence::Composited,
        }
    }

    fn notice(
        preset: StreamingPreset,
        measured: Option<u32>,
        backend: EncoderBackend,
    ) -> CaptureAdmissionNotice {
        notice_at(
            preset,
            preset.pipeline_contract().max_fps,
            measured,
            backend,
        )
    }

    fn notice_at(
        preset: StreamingPreset,
        requested_fps: u32,
        measured: Option<u32>,
        backend: EncoderBackend,
    ) -> CaptureAdmissionNotice {
        let source = evidence(measured);
        CaptureAdmissionNotice {
            requested: preset,
            requested_fps,
            admission: admit_capture_source(
                AdmissionRequest {
                    preset,
                    requested_codec: VideoCodec::H264,
                    width: 1920,
                    height: 1080,
                    max_fps: requested_fps,
                },
                &source,
                &EncoderCapability {
                    backend,
                    limits: backend.contract(),
                },
            ),
            capture: source.backend,
            copy_path: source.copy_path,
            cursor: source.cursor,
            measured_fps: measured,
            rate_evidence: CaptureRateEvidence::Measured,
            frame: FirstFrameEvidence {
                node_id: 36,
                format: CapturePixelFormat::Bgrx8,
                width: 1920,
                height: 1080,
                memory: CaptureMemory::HostShared,
            },
        }
    }

    #[test]
    fn every_outcome_round_trips() {
        let sid = "6f1c2b4e-7a0d-4c9e-9f31-2d8a5b7c0e14";
        for (preset, measured, backend) in [
            (StreamingPreset::Auto, None, EncoderBackend::NativeNvenc),
            (StreamingPreset::Speed, None, EncoderBackend::NativeNvenc),
            (
                StreamingPreset::Speed,
                Some(35),
                EncoderBackend::NativeNvenc,
            ),
            (StreamingPreset::Speed, None, EncoderBackend::OpenH264),
            (StreamingPreset::Auto, None, EncoderBackend::OpenH264),
            (StreamingPreset::Grading, None, EncoderBackend::NativeNvenc),
            (StreamingPreset::Hdr, None, EncoderBackend::NativeNvenc),
        ] {
            let notice = notice(preset, measured, backend);
            let line = notice.format(Some(sid));
            let (parsed, parsed_sid) =
                CaptureAdmissionNotice::parse(&line).unwrap_or_else(|error| {
                    panic!("{line}: {error:?}");
                });
            assert_eq!(parsed, notice, "{line}");
            assert_eq!(parsed_sid.as_deref(), Some(sid));
        }
    }

    #[test]
    fn dmabuf_frames_keep_their_modifier() {
        let mut notice = notice(StreamingPreset::Auto, None, EncoderBackend::NativeNvenc);
        notice.frame.memory = CaptureMemory::DmaBuf {
            modifier: FormatModifier(0x0300_0000_004f_e010),
        };
        let line = notice.format(None);
        assert!(
            line.contains("memory=dmabuf modifier=0x03000000004fe010"),
            "{line}"
        );
        assert_eq!(CaptureAdmissionNotice::parse(&line).unwrap().0, notice);
    }

    #[test]
    fn speed_on_an_unqualified_copy_is_visibly_served_as_auto() {
        let notice = notice(StreamingPreset::Speed, None, EncoderBackend::NativeNvenc);
        let line = notice.format(None);
        assert!(
            line.contains(
                "requested=speed requested_fps=60 decision=downgrade reason=copy_path_not_rate_qualified served=auto"
            ),
            "{line}"
        );
        let served = notice
            .check_request(
                StreamingPreset::Speed,
                60,
                VideoCodec::H264,
                CursorMode::Host,
            )
            .unwrap();
        assert_eq!(served.pipeline, PipelineId::Auto);
        assert_eq!(served.fps_ceiling, 30);
        let degradation = notice.degradation().unwrap();
        assert!(degradation.fps_clamped);
    }

    #[test]
    fn a_measured_rate_below_the_contract_clamps_speed_visibly() {
        let notice = notice(
            StreamingPreset::Speed,
            Some(35),
            EncoderBackend::NativeNvenc,
        );
        let served = notice
            .check_request(
                StreamingPreset::Speed,
                60,
                VideoCodec::H264,
                CursorMode::Host,
            )
            .unwrap();
        assert_eq!(served.pipeline, PipelineId::Speed);
        assert_eq!(served.fps_ceiling, 35);
        assert_eq!(
            served.fps_limited_by,
            Some(AdmissionReason::CaptureRateBelowContract)
        );
        assert!(notice.degradation().unwrap().fps_clamped);
    }

    #[test]
    fn an_inferred_rate_is_stated_on_the_line_and_needs_a_rate() {
        let mut notice = notice(
            StreamingPreset::Speed,
            Some(60),
            EncoderBackend::NativeNvenc,
        );
        let measured = notice.format(None);
        assert!(!measured.contains("rate_evidence"), "{measured}");
        notice.rate_evidence = CaptureRateEvidence::Inferred {
            width: 2560,
            height: 1600,
        };
        let line = notice.format(None);
        assert!(
            line.contains(" rate_evidence=inferred:2560x1600 "),
            "{line}"
        );
        assert_eq!(CaptureAdmissionNotice::parse(&line).unwrap().0, notice);
        // `measured` is never written, and an inference without a rate is
        // contradictory.
        let explicit = measured.replace(" node=", " rate_evidence=measured node=");
        assert!(CaptureAdmissionNotice::parse(&explicit).is_err());
        let unrated = notice_at(StreamingPreset::Auto, 30, None, EncoderBackend::NativeNvenc)
            .format(None)
            .replace(" node=", " rate_evidence=inferred:2560x1600 node=");
        assert_eq!(
            CaptureAdmissionNotice::parse(&unrated).map(|_| ()),
            Err(AdmissionNoticeError::Inconsistent("rate_evidence"))
        );
    }

    #[test]
    fn a_request_below_the_contract_is_served_at_its_own_rate() {
        let notice = notice_at(StreamingPreset::Auto, 24, None, EncoderBackend::NativeNvenc);
        let line = notice.format(None);
        assert!(
            line.contains("requested=auto requested_fps=24 decision=admit"),
            "{line}"
        );
        assert!(line.contains(" fps=24 "), "{line}");
        assert_eq!(CaptureAdmissionNotice::parse(&line).unwrap().0, notice);
        let served = notice
            .check_request(
                StreamingPreset::Auto,
                24,
                VideoCodec::H264,
                CursorMode::Host,
            )
            .unwrap();
        assert_eq!(served.fps_ceiling, 24);
        assert!(notice.degradation().unwrap().is_exact());
        // The notice answers the 24 fps request only.
        assert_eq!(
            notice
                .check_request(
                    StreamingPreset::Auto,
                    30,
                    VideoCodec::H264,
                    CursorMode::Host
                )
                .map(|_| ()),
            Err(AdmissionNoticeError::RequestConflict("requested_fps"))
        );
        // A served rate above the requested one is not a valid line.
        let raised = line.replace(" fps=24 ", " fps=30 ");
        assert_eq!(
            CaptureAdmissionNotice::parse(&raised).map(|_| ()),
            Err(AdmissionNoticeError::Inconsistent("fps"))
        );
    }

    #[test]
    fn the_parent_refuses_a_notice_for_another_request() {
        let notice = notice(StreamingPreset::Auto, None, EncoderBackend::NativeNvenc);
        let check = |preset, fps, codec, cursor| {
            notice.check_request(preset, fps, codec, cursor).map(|_| ())
        };
        assert_eq!(
            check(
                StreamingPreset::Speed,
                60,
                VideoCodec::H264,
                CursorMode::Host
            ),
            Err(AdmissionNoticeError::RequestConflict("requested"))
        );
        assert_eq!(
            check(
                StreamingPreset::Auto,
                24,
                VideoCodec::H264,
                CursorMode::Host
            ),
            Err(AdmissionNoticeError::RequestConflict("requested_fps"))
        );
        assert_eq!(
            check(
                StreamingPreset::Auto,
                30,
                VideoCodec::H265,
                CursorMode::Host
            ),
            Err(AdmissionNoticeError::RequestConflict("codec"))
        );
        assert_eq!(
            check(
                StreamingPreset::Auto,
                30,
                VideoCodec::H264,
                CursorMode::Local
            ),
            Err(AdmissionNoticeError::RequestConflict("cursor_mode"))
        );
        assert!(
            check(
                StreamingPreset::Auto,
                30,
                VideoCodec::H264,
                CursorMode::Host
            )
            .is_ok()
        );
    }

    #[test]
    fn unavailable_is_an_error_for_the_parent() {
        let notice = notice(StreamingPreset::Grading, None, EncoderBackend::NativeNvenc);
        assert_eq!(
            notice.check_request(
                StreamingPreset::Grading,
                30,
                VideoCodec::H265,
                CursorMode::Host
            ),
            Err(AdmissionNoticeError::Unavailable(
                AdmissionReason::EightBitSource
            ))
        );
    }

    #[test]
    fn unknown_cursor_truth_is_not_accepted() {
        let mut notice = notice(StreamingPreset::Auto, None, EncoderBackend::NativeNvenc);
        if let PresetAdmission::Admit(served) = &mut notice.admission {
            served.cursor_mode = None;
        }
        assert_eq!(
            notice
                .check_request(
                    StreamingPreset::Auto,
                    30,
                    VideoCodec::H264,
                    CursorMode::Host
                )
                .map(|_| ()),
            Err(AdmissionNoticeError::RequestConflict("cursor_mode"))
        );
    }

    #[test]
    fn malformed_unknown_and_impossible_lines_are_refused() {
        let line = notice(StreamingPreset::Speed, None, EncoderBackend::NativeNvenc).format(None);
        for (bad, expected) in [
            (
                line.replace("version=1", "version=2"),
                AdmissionNoticeError::InvalidValue("version"),
            ),
            (
                format!("{line} extra=1"),
                AdmissionNoticeError::UnknownFields("extra".into()),
            ),
            (
                format!("{line} node=4"),
                AdmissionNoticeError::DuplicateField("node".into()),
            ),
            (
                line.replace(" capture=pipewire", ""),
                AdmissionNoticeError::MissingField("capture"),
            ),
            (
                line.replace("served=auto", "served=grading"),
                AdmissionNoticeError::Inconsistent("served"),
            ),
            (
                line.replace("copy_path=host-copy", "copy_path=teleport"),
                AdmissionNoticeError::InvalidValue("copy_path"),
            ),
            (
                line.replace("decision=downgrade", "decision=maybe"),
                AdmissionNoticeError::InvalidValue("decision"),
            ),
        ] {
            assert_eq!(
                CaptureAdmissionNotice::parse(&bad).map(|_| ()),
                Err(expected),
                "{bad}"
            );
        }
        assert_eq!(
            CaptureAdmissionNotice::parse("[capenc] READY version=1").map(|_| ()),
            Err(AdmissionNoticeError::MissingPrefix)
        );
    }
}
