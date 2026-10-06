#![allow(unsafe_code)]

//! `VideoToolbox` encoding for the macOS Pier.
//!
//! This is the native adapter only. It turns captured surfaces into an Annex B
//! elementary stream the existing Deck decoder already understands. Which
//! codec and profile a session is entitled to, and how a stream degrades, stay
//! in the shared crates.
//!
//! `VideoToolbox` emits length-prefixed AVCC/HVCC samples with parameter sets
//! carried out-of-band in the format description. The Deck expects Annex B, so
//! this module converts, and re-emits parameter sets ahead of every keyframe so
//! a client that joins or reconnects mid-stream can start decoding.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use apple_cf::cf::{AsCFType, CFArray, CFDictionary, CFString, CFType};
use apple_cf::cm::CMSampleBuffer;
use apple_cf::cv::CVPixelBuffer;
use apple_cf::iosurface::IOSurface;
use serde::Serialize;
use videotoolbox::Codec;

use crate::capture::CapturedFrame;

const ENCODE_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetFormatDescription(sample_buffer: *mut c_void) -> *mut c_void;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        format_description: *mut c_void,
        index: usize,
        parameter_set_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> i32;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        format_description: *mut c_void,
        index: usize,
        parameter_set_out: *mut *const u8,
        parameter_set_size_out: *mut usize,
        parameter_set_count_out: *mut usize,
        nal_unit_header_length_out: *mut i32,
    ) -> i32;
}

/// Which codec the Pier encodes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderCodec {
    /// H.264. The compatibility path.
    H264,
    /// HEVC. Required for everything above eight bits.
    Hevc,
}

impl EncoderCodec {
    const fn videotoolbox_codec(self) -> Codec {
        match self {
            Self::H264 => Codec::H264,
            Self::Hevc => Codec::HEVC,
        }
    }
}

/// The HEVC profile a stream must be encoded with.
///
/// Stated, because `VideoToolbox` does not infer it from the input. Handed a
/// 10-bit 4:4:4 surface and no profile, it encodes Main: 8-bit 4:2:0. Measured
/// on the lab — every Grading session this host served before this existed
/// was Main, whatever the plan and the capture said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EncodeProfile {
    /// Whatever `VideoToolbox` chooses for the codec: H.264 High, HEVC Main.
    /// The 8-bit 4:2:0 fast path, unchanged.
    CodecDefault,
    /// HEVC Main 10: 10-bit 4:2:0.
    Main10,
    /// HEVC Main 4:4:4 10 (range extensions): 10-bit 4:4:4.
    Main44410,
}

impl EncodeProfile {
    /// The profile a stream of this shape needs.
    #[must_use]
    pub const fn for_shape(
        codec: EncoderCodec,
        chroma: arcen_media::ChromaSubsampling,
        depth: arcen_media::BitDepth,
    ) -> Self {
        match (codec, chroma, depth) {
            (
                EncoderCodec::Hevc,
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::BitDepth::Ten,
            ) => Self::Main44410,
            (
                EncoderCodec::Hevc,
                arcen_media::ChromaSubsampling::Yuv420,
                arcen_media::BitDepth::Ten,
            ) => Self::Main10,
            _ => Self::CodecDefault,
        }
    }

    /// The `VideoToolbox` profile-level constant's name.
    const fn symbol(self) -> Option<&'static std::ffi::CStr> {
        match self {
            Self::CodecDefault => None,
            Self::Main10 => Some(c"kVTProfileLevel_HEVC_Main10_AutoLevel"),
            // Exported by VideoToolbox but not declared in the SDK headers, so
            // it is looked up at run time rather than linked: a macOS without
            // it gets an honest refusal instead of a process that cannot load.
            Self::Main44410 => Some(c"kVTProfileLevel_HEVC_Main44410_AutoLevel"),
        }
    }
}

/// The colour description a stream is tagged with.
///
/// `VideoToolbox` writes these into the VUI. Left unset it writes none, and a
/// decoder then guesses — which the Deck does by overriding with BT.709,
/// measured. Guessing is harmless for the BT.709 fast path, which is why that
/// path is left untagged, and wrong for everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct EncodeColour {
    /// `CoreVideo` colour-primaries name, such as `ITU_R_709_2`.
    pub primaries: &'static str,
    /// `CoreVideo` transfer-function name, such as `SMPTE_ST_2084_PQ`.
    pub transfer: &'static str,
    /// `CoreVideo` matrix name, such as `ITU_R_2020`.
    pub matrix: &'static str,
}

impl EncodeColour {
    /// Maps the shared plan's colour tokens onto `CoreVideo` names.
    ///
    /// Returns `None` for a token this host has no name for, so an unknown
    /// value is refused rather than tagged as something else.
    #[must_use]
    pub fn from_plan_tokens(primaries: &str, transfer: &str, matrix: &str) -> Option<Self> {
        Some(Self {
            primaries: match primaries {
                "bt709" => "ITU_R_709_2",
                "bt2020" => "ITU_R_2020",
                "display_p3" | "p3" => "P3_D65",
                _ => return None,
            },
            transfer: match transfer {
                "bt709" => "ITU_R_709_2",
                "srgb" => "IEC_sRGB",
                "pq" => "SMPTE_ST_2084_PQ",
                "hlg" => "ITU_R_2100_HLG",
                _ => return None,
            },
            matrix: match matrix {
                "bt709" => "ITU_R_709_2",
                "bt2020" | "bt2020ncl" => "ITU_R_2020",
                "bt601" => "ITU_R_601_4",
                _ => return None,
            },
        })
    }
}

/// The colour description a capture plan's frames are encoded with.
///
/// The capture fixes the colour space — `ScreenCaptureKit` is told which one
/// to deliver — so the tags follow it rather than being chosen separately
/// and drifting. The 8-bit fast path stays untagged, exactly as it was.
#[must_use]
pub const fn colour_for_capture(capture: &crate::capture::CaptureConfig) -> Option<EncodeColour> {
    use crate::capture::{CaptureDynamicRange, CapturePixelFormat};
    match (capture.pixel_format, capture.dynamic_range) {
        (
            CapturePixelFormat::Bgra8 | CapturePixelFormat::Nv12VideoRange,
            CaptureDynamicRange::Sdr,
        ) => None,
        (_, CaptureDynamicRange::Sdr) => Some(EncodeColour {
            primaries: "ITU_R_709_2",
            transfer: "ITU_R_709_2",
            matrix: "ITU_R_709_2",
        }),
        (_, CaptureDynamicRange::HdrLocalDisplay | CaptureDynamicRange::HdrCanonicalDisplay) => {
            Some(EncodeColour {
                primaries: "ITU_R_2020",
                transfer: "SMPTE_ST_2084_PQ",
                matrix: "ITU_R_2020",
            })
        }
    }
}

/// A resolved encode plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    /// Encoded width in pixels.
    pub width: i32,
    /// Encoded height in pixels.
    pub height: i32,
    /// Codec to use.
    pub codec: EncoderCodec,
    /// Target average bitrate in bits per second.
    pub bitrate_bps: i32,
    /// Encoder-side ceiling in bits per second.
    pub max_bitrate_bps: i32,
    /// Expected source frame rate.
    pub fps: u32,
    /// Shared pipeline keyframe cadence and recovery policy.
    pub keyframe: arcen_media::video::KeyframePolicy,
    /// What shared policy says to preserve under pressure.
    pub motion_priority: arcen_media::video::MotionPriority,
    /// The profile the stream is encoded with.
    pub profile: EncodeProfile,
    /// The colour description written into the stream, when there is one.
    pub colour: Option<EncodeColour>,
}

impl EncoderConfig {
    /// Creates a low-latency plan for a live desktop at the given size.
    #[must_use]
    pub fn realtime(width: i32, height: i32, codec: EncoderCodec, fps: u32) -> Self {
        Self::realtime_for(
            width,
            height,
            codec,
            fps,
            arcen_media::ChromaSubsampling::Yuv420,
            arcen_media::BitDepth::Eight,
        )
    }

    /// A real-time plan for a stream of a stated shape.
    ///
    /// The shape is an argument because it changes what the stream is worth:
    /// Grading carries 4:4:4 ten-bit, which needs two and a half times the
    /// budget of the eight-bit 4:2:0 the Auto path sends. Sizing both the same
    /// way starves one of them.
    #[must_use]
    pub fn realtime_for(
        width: i32,
        height: i32,
        codec: EncoderCodec,
        fps: u32,
        chroma: arcen_media::ChromaSubsampling,
        depth: arcen_media::BitDepth,
    ) -> Self {
        Self {
            width,
            height,
            codec,
            // Sized from what is actually being encoded rather than fixed. A
            // flat figure is wrong in both directions at once: it wasted a
            // link at 720p and smeared anything larger, and it did not move
            // when the picture or the frame rate did. The arithmetic is the
            // one the Linux Pier has always sized NVENC with.
            //
            // Capped at what that same arithmetic spends on 1080p of the same
            // cadence, because VideoToolbox spends its configured average
            // whatever the picture is doing. Measured on the lab: bytes per
            // frame tracked the target exactly and did not vary between a
            // desktop with animation running and one with nothing moving at
            // all. So the target is not a budget the encoder draws on when it
            // needs to — it is the bandwidth bill, every frame, forever.
            //
            // At 2560x1440 the uncapped figure is 8.29 Mbps, which this link
            // could not carry: the writer queue stood at 52 ms and the session
            // delivered 22 fps of a requested 30. Capped, the same session ran
            // at 29 fps with a 0.07 ms queue. Sessions at or below 1080p are
            // unchanged, so this does not move Linux parity at the size Linux
            // is measured at.
            bitrate_bps: i32::try_from(capped_bitrate_bps(
                width.unsigned_abs(),
                height.unsigned_abs(),
                fps,
                chroma,
                depth,
            ))
            .unwrap_or(i32::MAX),
            max_bitrate_bps: i32::try_from(capped_bitrate_bps(
                width.unsigned_abs(),
                height.unsigned_abs(),
                fps,
                chroma,
                depth,
            ))
            .unwrap_or(i32::MAX),
            fps,
            keyframe: arcen_media::video::KeyframePolicy::ON_DEMAND_ONLY,
            motion_priority: arcen_media::video::MotionPriority::Detail,
            profile: EncodeProfile::for_shape(codec, chroma, depth),
            colour: None,
        }
    }

    /// This plan with its average bitrate replaced by
    /// `ARCEN_VIDEO_BITRATE_BPS`, when that is set to a positive number.
    ///
    /// A measurement lever, not a setting: it is how a link's knee is found
    /// without a rebuild per point, and nothing ships depending on it.
    #[must_use]
    pub fn with_diagnostic_bitrate_override(mut self) -> Self {
        if let Some(bps) = std::env::var("ARCEN_VIDEO_BITRATE_BPS")
            .ok()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|&bps| bps > 0)
        {
            tracing::info!(
                target: "arcen::media",
                configured = self.bitrate_bps,
                overridden = bps,
                "average bitrate overridden for measurement"
            );
            self.bitrate_bps = bps;
        }
        self
    }

    /// This plan with its average bitrate set by the served pipeline contract.
    #[must_use]
    pub const fn with_bitrate_bps(mut self, bitrate_bps: i32) -> Self {
        self.bitrate_bps = bitrate_bps;
        self.max_bitrate_bps = bitrate_bps;
        self
    }

    /// This plan with its average and encoder ceiling set by the served
    /// pipeline contract.
    #[must_use]
    pub const fn with_bitrate_bounds(mut self, bitrate_bps: i32, max_bitrate_bps: i32) -> Self {
        self.bitrate_bps = bitrate_bps;
        self.max_bitrate_bps = if max_bitrate_bps > bitrate_bps {
            max_bitrate_bps
        } else {
            bitrate_bps
        };
        self
    }

    /// This plan, tagged with a colour description.
    #[must_use]
    pub const fn with_colour(mut self, colour: Option<EncodeColour>) -> Self {
        self.colour = colour;
        self
    }

    /// This plan with the shared motion/detail preference recorded.
    #[must_use]
    pub const fn with_motion_priority(
        mut self,
        priority: arcen_media::video::MotionPriority,
    ) -> Self {
        self.motion_priority = priority;
        self
    }

    /// This plan with the shared served-pipeline keyframe policy recorded.
    #[must_use]
    pub const fn with_keyframe_policy(
        mut self,
        keyframe: arcen_media::video::KeyframePolicy,
    ) -> Self {
        self.keyframe = keyframe;
        self
    }
}

/// Why encoding could not start or continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum EncodeError {
    /// `VideoToolbox` refused to create the session.
    SessionCreate(String),
    /// `VideoToolbox` refused a frame.
    Encode(String),
    /// `VideoToolbox` produced a sample buffer the host could not read.
    SampleData,
    /// The encoder produced a frame with no parameter sets, so no decoder
    /// could start from it.
    MissingParameterSets,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionCreate(detail) => {
                write!(formatter, "encoder session creation failed: {detail}")
            }
            Self::Encode(detail) => write!(formatter, "encode failed: {detail}"),
            Self::SampleData => formatter.write_str("encoder produced unreadable sample data"),
            Self::MissingParameterSets => formatter.write_str("encoder produced no parameter sets"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// One encoded access unit in Annex B form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    /// Annex B bytes, parameter sets included when this is a keyframe.
    pub bytes: Vec<u8>,
    /// Whether a decoder can start from this access unit.
    pub keyframe: bool,
    /// Presentation timestamp numerator.
    pub pts: i64,
    /// Presentation timestamp timescale.
    pub timescale: i32,
}

/// A running `VideoToolbox` compression session.
pub struct Encoder {
    session: Option<VtCompressionSession>,
    config: EncoderConfig,
    frame_index: i64,
    encode_split: EncodeSplit,
    /// Frames VideoToolbox dropped in real-time mode.
    dropped_frames: u64,
    /// What the last keyframe's SPS says the stream is.
    stream_truth: Option<arcen_media::hevc_sps::HevcStreamTruth>,
    /// The `CoreVideo` layout of the last surface handed to `VideoToolbox`.
    input_pixel_format: u32,
}

/// Minimal read-back seam for classifying a session encoder.
pub trait EncoderAcceleration {
    fn uses_hardware_acceleration(&self) -> Option<bool>;
}

/// Where the time inside one encode call actually goes.
///
/// `mean_encode_ms` wraps three different things, and 12.8 ms is far too slow
/// for 1440p on an Apple media engine — which is a reason to check what else
/// is inside the measurement before concluding anything about the encoder.
#[derive(Debug, Default, Clone, Copy)]
struct EncodeSplit {
    wrap_ns: u64,
    wait_ns: u64,
    frames: u64,
}

impl EncodeSplit {
    fn observe(&mut self, wrap: Duration, wait: Duration) {
        // Nanosecond totals overflow after centuries of uptime.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.wrap_ns = self.wrap_ns.saturating_add(wrap.as_nanos() as u64);
            self.wait_ns = self.wait_ns.saturating_add(wait.as_nanos() as u64);
        }
        self.frames = self.frames.saturating_add(1);
    }

    /// Every 150 frames, about five seconds at the rates this host reaches.
    const fn should_report(self) -> bool {
        self.frames % 150 == 0
    }

    /// Mean wrap and wait in milliseconds, with the frame count behind them.
    fn snapshot(self) -> (f64, f64, u64) {
        if self.frames == 0 {
            return (0.0, 0.0, 0);
        }
        // Both totals are small multiples of the frame count; `f64` holds
        // every integer well past what a session accumulates.
        #[allow(clippy::cast_precision_loss)]
        let divisor = self.frames as f64;
        #[allow(clippy::cast_precision_loss)]
        let means = (
            self.wrap_ns as f64 / divisor / 1_000_000.0,
            self.wait_ns as f64 / divisor / 1_000_000.0,
        );
        (means.0, means.1, self.frames)
    }
}

impl std::fmt::Debug for Encoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Encoder")
            .field("config", &self.config)
            .field("frame_index", &self.frame_index)
            .finish_non_exhaustive()
    }
}

/// A small `VTCompressionSession` wrapper that can pass an encoder
/// specification at creation time.
///
/// The crate wrapper is still the model for ownership and callbacks, but the
/// low-latency rate-control key is only accepted in `VTCompressionSessionCreate`
///'s `encoderSpecification`, and `videotoolbox` 0.18.1 does not expose that
/// argument. This wrapper keeps the raw pointer private and exposes only the
/// operations this adapter already used.
struct VtCompressionSession {
    session: videotoolbox::ffi::VTCompressionSessionRef,
    /// Created with `RequireHardwareAcceleratedVideoEncoder`, so its existence
    /// proves a hardware encoder even when the session does not report
    /// `UsingHardwareAcceleratedVideoEncoder` (the low-latency encoder does not).
    hardware_required: bool,
}

// SAFETY: matches the upstream `videotoolbox` wrapper. VideoToolbox owns the
// encoder queue and documents the compression session API as thread-safe for
// frame submission; this type adds no Rust aliasable state beyond the raw
// session handle.
unsafe impl Send for VtCompressionSession {}
// SAFETY: as above. Shared references only call VideoToolbox functions that
// synchronize internally.
unsafe impl Sync for VtCompressionSession {}

impl VtCompressionSession {
    fn new(config: EncoderConfig) -> Result<Self, videotoolbox::VTError> {
        if uses_low_latency_rate_control(config) {
            match Self::create(config, true) {
                Ok(session) => return Ok(session),
                Err(error) => tracing::warn!(
                    target: "arcen::media",
                    %error,
                    "no hardware low-latency VideoToolbox encoder; retrying without requiring one",
                ),
            }
        }
        Self::create(config, false)
    }

    fn create(
        config: EncoderConfig,
        require_hardware: bool,
    ) -> Result<Self, videotoolbox::VTError> {
        let specification = encoder_specification(config, require_hardware);
        let mut session = std::ptr::null_mut();
        // SAFETY: all pointers either name live CoreFoundation objects for the
        // duration of the call or are null by API contract. `session` is a live
        // out-parameter. The callback is a C ABI function below.
        let status = unsafe {
            videotoolbox::ffi::VTCompressionSessionCreate(
                videotoolbox::ffi::kCFAllocatorDefault,
                config.width,
                config.height,
                config.codec.videotoolbox_codec().as_cm_codec_type(),
                specification
                    .as_ref()
                    .map_or(std::ptr::null(), |spec| spec.as_ptr().cast_const().cast()),
                std::ptr::null(),
                videotoolbox::ffi::kCFAllocatorDefault,
                Some(low_latency_encode_callback),
                std::ptr::null_mut(),
                &mut session,
            )
        };
        if status != 0 || session.is_null() {
            return Err(videotoolbox::VTError::SessionCreateFailed(status));
        }
        let session = Self {
            session,
            hardware_required: require_hardware,
        };
        session.apply_realtime_properties(config)?;
        // SAFETY: the session was created successfully and is live.
        let status = unsafe {
            videotoolbox::ffi::VTCompressionSessionPrepareToEncodeFrames(session.session)
        };
        if status != 0 {
            return Err(videotoolbox::VTError::PrepareFailed(status));
        }
        Ok(session)
    }

    fn apply_realtime_properties(
        &self,
        config: EncoderConfig,
    ) -> Result<(), videotoolbox::VTError> {
        let mut pairs = Vec::<(CFString, CFType)>::new();
        // SAFETY: all keys passed here are process-lifetime VideoToolbox
        // constants from the SDK.
        unsafe {
            push_bool_property(
                &mut pairs,
                videotoolbox::ffi::kVTCompressionPropertyKey_RealTime,
                true,
            )?;
            push_bool_property(
                &mut pairs,
                videotoolbox::ffi::kVTCompressionPropertyKey_AllowFrameReordering,
                false,
            )?;
            push_number_property(
                &mut pairs,
                videotoolbox::ffi::kVTCompressionPropertyKey_AverageBitRate,
                i64::from(config.bitrate_bps),
            )?;
            if config.max_bitrate_bps > config.bitrate_bps {
                let (key, value) =
                    data_rate_limits_property(config.max_bitrate_bps).ok_or_else(|| {
                        videotoolbox::VTError::InvalidArgument(
                            "VideoToolbox did not export DataRateLimits".to_owned(),
                        )
                    })?;
                pairs.push((key, value));
            }
            push_number_property(
                &mut pairs,
                videotoolbox::ffi::kVTCompressionPropertyKey_ExpectedFrameRate,
                i64::from(config.fps.max(1)),
            )?;
            push_number_property(
                &mut pairs,
                videotoolbox::ffi::kVTCompressionPropertyKey_MaxKeyFrameInterval,
                videotoolbox_max_keyframe_interval(config.keyframe, config.fps),
            )?;
            push_named_number_property(
                &mut pairs,
                "MaxKeyFrameIntervalDuration",
                config
                    .keyframe
                    .safety_refresh_interval()
                    .map_or(0, |interval| {
                        i64::try_from(interval.as_secs()).unwrap_or(i64::MAX)
                    }),
            );
            push_number_property(
                &mut pairs,
                videotoolbox::ffi::kVTCompressionPropertyKey_MaxFrameDelayCount,
                0,
            )?;
            if prioritizes_encode_speed(config) {
                push_bool_property(
                    &mut pairs,
                    videotoolbox::ffi::kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                    true,
                )?;
            }
        }
        self.set_property_pairs(&pairs)
    }

    fn set_properties(&self, properties: &CFDictionary) -> Result<(), videotoolbox::VTError> {
        // SAFETY: the session is live and `properties` is a valid
        // CoreFoundation dictionary for this call.
        let status = unsafe {
            videotoolbox::ffi::VTSessionSetProperties(
                self.session.cast(),
                properties.as_ptr().cast_const().cast(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(videotoolbox::VTError::ApiFailed {
                api: "VTSessionSetProperties",
                status,
            })
        }
    }

    fn set_property_pairs(
        &self,
        pairs: &[(CFString, CFType)],
    ) -> Result<(), videotoolbox::VTError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let borrowed: Vec<(&dyn AsCFType, &dyn AsCFType)> = pairs
            .iter()
            .map(|(key, value)| (key as &dyn AsCFType, value as &dyn AsCFType))
            .collect();
        self.set_properties(&CFDictionary::from_pairs(&borrowed))
    }

    unsafe fn copy_property(
        &self,
        key: videotoolbox::ffi::CFStringRef,
    ) -> Result<Option<CFType>, videotoolbox::VTError> {
        let mut out: *mut c_void = std::ptr::null_mut();
        // SAFETY: caller supplies a valid VideoToolbox property key; `out` is a
        // live out-parameter and CoreFoundation returns a +1 object on success.
        let status = unsafe {
            videotoolbox::ffi::VTSessionCopyProperty(
                self.session.cast(),
                key,
                videotoolbox::ffi::kCFAllocatorDefault,
                (&mut out as *mut *mut c_void).cast(),
            )
        };
        if status != 0 {
            return Err(videotoolbox::VTError::ApiFailed {
                api: "VTSessionCopyProperty",
                status,
            });
        }
        Ok(CFType::from_raw(out))
    }

    fn encode_frame(
        &self,
        image_buffer: CVPixelBuffer,
        presentation_timestamp: apple_cf::cm::CMTime,
        duration: apple_cf::cm::CMTime,
        frame_properties: Option<CFDictionary>,
        timeout: Duration,
    ) -> Result<Result<CMSampleBuffer, videotoolbox::VTError>, ()> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let context = Box::into_raw(Box::new(EncodeCallbackContext { tx }));
        let frame_properties_ref = frame_properties.as_ref();
        // SAFETY: the session and pixel buffer are live. `context` is consumed
        // by `low_latency_encode_callback`; if VideoToolbox refuses submission
        // synchronously it is reclaimed below.
        let status = unsafe {
            videotoolbox::ffi::VTCompressionSessionEncodeFrame(
                self.session,
                image_buffer.as_ptr().cast(),
                presentation_timestamp,
                duration,
                frame_properties_ref
                    .map_or(std::ptr::null(), |dict| dict.as_ptr().cast_const().cast()),
                context.cast::<c_void>(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            // SAFETY: VideoToolbox did not take ownership when submission
            // failed synchronously, so reclaim the box.
            unsafe { drop(Box::from_raw(context)) };
            return Ok(Err(videotoolbox::VTError::EncodeFailed(status)));
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => Ok(result),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                std::mem::forget(image_buffer);
                if let Some(properties) = frame_properties {
                    std::mem::forget(properties);
                }
                Err(())
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Ok(Err(videotoolbox::VTError::EncoderCallback(-1)))
            }
        }
    }
}

impl Drop for VtCompressionSession {
    fn drop(&mut self) {
        if !self.session.is_null() {
            // SAFETY: this wrapper owns the +1 session returned by
            // `VTCompressionSessionCreate`, and invalidation is the documented
            // teardown operation before releasing it.
            unsafe {
                videotoolbox::ffi::VTCompressionSessionInvalidate(self.session);
                videotoolbox::ffi::CFRelease(self.session.cast());
            }
        }
    }
}

struct EncodeCallbackContext {
    tx: std::sync::mpsc::SyncSender<Result<CMSampleBuffer, videotoolbox::VTError>>,
}

unsafe extern "C" fn low_latency_encode_callback(
    _output_callback_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: videotoolbox::ffi::OSStatus,
    _info_flags: videotoolbox::ffi::VTEncodeInfoFlags,
    sample_buffer: videotoolbox::ffi::CMSampleBufferRef,
) {
    if source_frame_ref_con.is_null() {
        return;
    }
    // SAFETY: `encode_frame` allocated this exact box for the frame and
    // VideoToolbox calls the callback at most once for that frame.
    let context = unsafe { Box::from_raw(source_frame_ref_con.cast::<EncodeCallbackContext>()) };
    let result = if status != 0 {
        Err(videotoolbox::VTError::EncoderCallback(status))
    } else if sample_buffer.is_null() {
        Err(videotoolbox::VTError::EncoderCallback(-1))
    } else {
        // SAFETY: `sample_buffer` is valid for the callback; retaining it gives
        // Rust an owned object after returning to VideoToolbox.
        unsafe { CMSampleBuffer::from_raw_retained(sample_buffer.cast()) }
            .ok_or(videotoolbox::VTError::EncoderCallback(-1))
    };
    let _ = context.tx.send(result);
}

impl Encoder {
    /// Creates a low-latency encoder for `config`.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::SessionCreate`] when `VideoToolbox` refuses the
    /// requested size, codec, or profile.
    pub fn new(config: EncoderConfig) -> Result<Self, EncodeError> {
        let session = VtCompressionSession::new(config)
            .map_err(|error| EncodeError::SessionCreate(error.to_string()))?;
        apply_profile_and_colour(&session, config)?;
        Ok(Self {
            session: Some(session),
            config,
            frame_index: 0,
            encode_split: EncodeSplit::default(),
            dropped_frames: 0,
            stream_truth: None,
            input_pixel_format: 0,
        })
    }

    /// What the stream's own SPS says it carries, once a keyframe has been
    /// produced. HEVC only; `None` for H.264 and before the first keyframe.
    #[must_use]
    pub const fn stream_truth(&self) -> Option<arcen_media::hevc_sps::HevcStreamTruth> {
        self.stream_truth
    }

    /// Changes the average bitrate for the frames that follow.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] when `VideoToolbox` refuses the value.
    pub fn set_average_bitrate(&mut self, bps: u64) -> Result<(), EncodeError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        // SAFETY: VideoToolbox exports this process-lifetime `CFStringRef`,
        // as for the keyframe option key below.
        let Some(key) = (unsafe {
            CFString::from_raw_retained(
                videotoolbox::ffi::kVTCompressionPropertyKey_AverageBitRate
                    .cast_mut()
                    .cast(),
            )
        }) else {
            return Err(EncodeError::Encode(
                "VideoToolbox did not export AverageBitRate".to_owned(),
            ));
        };
        let bps = i32::try_from(bps).unwrap_or(i32::MAX);
        let value: CFType = apple_cf::cf::CFNumber::from_i64(i64::from(bps)).into();
        session
            .set_properties(&CFDictionary::from_pairs(&[(&key, &value)]))
            .map_err(|error| EncodeError::Encode(format!("AverageBitRate: {error}")))?;
        self.config.bitrate_bps = bps;
        Ok(())
    }

    /// Changes the average bitrate and native encoder ceiling for following frames.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] when `VideoToolbox` refuses either value.
    pub fn set_bitrate_bounds(&mut self, bps: u64, max_bps: u64) -> Result<(), EncodeError> {
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        // SAFETY: VideoToolbox exports this process-lifetime `CFStringRef`,
        // as for the keyframe option key below.
        let Some(average_key) = (unsafe {
            CFString::from_raw_retained(
                videotoolbox::ffi::kVTCompressionPropertyKey_AverageBitRate
                    .cast_mut()
                    .cast(),
            )
        }) else {
            return Err(EncodeError::Encode(
                "VideoToolbox did not export AverageBitRate".to_owned(),
            ));
        };
        let bounded_max = max_bps.max(bps).min(i32::MAX as u64);
        let average_bps = bps.min(i32::MAX as u64);
        let average_value: CFType =
            apple_cf::cf::CFNumber::from_i64(i64::try_from(average_bps).unwrap_or(i64::MAX)).into();
        if bounded_max > average_bps {
            let (limit_key, limit_value) =
                data_rate_limits_property(i32::try_from(bounded_max).unwrap_or(i32::MAX))
                    .ok_or_else(|| {
                        EncodeError::Encode("VideoToolbox did not export DataRateLimits".to_owned())
                    })?;
            session
                .set_properties(&CFDictionary::from_pairs(&[
                    (&average_key, &average_value),
                    (&limit_key, &limit_value),
                ]))
                .map_err(|error| EncodeError::Encode(format!("bitrate bounds: {error}")))?;
        } else {
            session
                .set_properties(&CFDictionary::from_pairs(&[(&average_key, &average_value)]))
                .map_err(|error| EncodeError::Encode(format!("AverageBitRate: {error}")))?;
        }
        self.config.bitrate_bps = i32::try_from(average_bps).unwrap_or(i32::MAX);
        self.config.max_bitrate_bps = i32::try_from(bounded_max).unwrap_or(i32::MAX);
        Ok(())
    }

    /// Whether `VideoToolbox` chose a hardware encoder for this session.
    ///
    /// `None` means the property could not be read at all, which is a
    /// different answer from "software" and is reported as such.
    ///
    /// This is read rather than assumed. Hardware encoding has been allowed by
    /// default since macOS 10.15, so not asking for it is not evidence of
    /// software — but neither is asking evidence of getting it. The header is
    /// explicit that a request can be refused because the machine has no such
    /// encoder, because the format is unsupported, or simply because the
    /// hardware encoders are busy. Only the read-back distinguishes those from
    /// success, and a host that cannot tell them apart reports a software
    /// session as though it were accelerated.
    #[must_use]
    pub fn uses_hardware_acceleration(&self) -> Option<bool> {
        let session = self.session.as_ref()?;
        let hardware_required = session.hardware_required;
        // SAFETY: `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder`
        // is a process-lifetime VideoToolbox constant, and the session is live
        // for this borrow.
        let property = unsafe {
            session.copy_property(
                videotoolbox::ffi::kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
            )
        }
        .ok()
        .flatten();
        // SAFETY: `kCFBooleanTrue` is a process-lifetime Core Foundation
        // singleton; comparing pointers reads nothing through them.
        let expected = unsafe { videotoolbox::ffi::kCFBooleanTrue }.cast::<std::ffi::c_void>();
        let readback = property
            .map(|property| property.as_ptr().cast_const().cast::<std::ffi::c_void>() == expected);
        classify_acceleration(readback, hardware_required)
    }

    /// Returns the plan this encoder was built with.
    #[must_use]
    pub const fn config(&self) -> EncoderConfig {
        self.config
    }

    /// Encodes one captured surface.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] when `VideoToolbox` rejects the frame.
    pub fn encode(
        &mut self,
        frame: &CapturedFrame,
    ) -> Result<Option<EncodedAccessUnit>, EncodeError> {
        self.encode_with_keyframe_request(frame, false)
    }

    /// Encodes one captured surface, forcing this access unit to be a recovery
    /// point when `force_keyframe` is true.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError::Encode`] when `VideoToolbox` rejects the frame.
    pub fn encode_with_keyframe_request(
        &mut self,
        frame: &CapturedFrame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedAccessUnit>, EncodeError> {
        let timescale = i32::try_from(self.config.fps.max(1)).unwrap_or(60);
        let pts = self.frame_index;
        self.frame_index += 1;
        if force_keyframe {
            return self.encode_forced_keyframe(frame, pts, timescale);
        }
        let Some(sample) = self.encode_frame(
            &frame.surface,
            Some(frame.clone()),
            pts,
            timescale,
            None,
            "encode",
        )?
        else {
            return Ok(None);
        };
        let Some(block) = sample.data_buffer() else {
            return Err(EncodeError::SampleData);
        };
        let Some(data) = block.copy_data_bytes(0, block.data_length()) else {
            return Err(EncodeError::SampleData);
        };
        self.finish_access_unit(&data, sample.as_ptr().cast::<c_void>(), pts, timescale)
    }

    fn encode_forced_keyframe(
        &mut self,
        frame: &CapturedFrame,
        pts: i64,
        timescale: i32,
    ) -> Result<Option<EncodedAccessUnit>, EncodeError> {
        self.encode_surface_as_keyframe(&frame.surface, Some(frame.clone()), pts, timescale)
    }

    /// Encodes a surface nobody captured — a probe's — as a keyframe, and
    /// returns what the stream's own SPS says it is.
    ///
    /// This is how the host proves a profile rather than assuming it: the
    /// answer comes from the bitstream `VideoToolbox` produced.
    ///
    /// # Errors
    ///
    /// Returns [`EncodeError`] when `VideoToolbox` refuses the surface.
    pub fn prove_with_surface(
        &mut self,
        surface: &IOSurface,
    ) -> Result<Option<arcen_media::hevc_sps::HevcStreamTruth>, EncodeError> {
        let timescale = i32::try_from(self.config.fps.max(1)).unwrap_or(60);
        let pts = self.frame_index;
        self.frame_index += 1;
        // `encode_frame` waits for the frame's completion, so the keyframe and
        // its parameter sets have been read by the time this returns.
        let unit = self.encode_surface_as_keyframe(surface, None, pts, timescale)?;
        Ok(unit.and(self.stream_truth))
    }

    fn encode_surface_as_keyframe(
        &mut self,
        surface: &IOSurface,
        lease: Option<CapturedFrame>,
        pts: i64,
        timescale: i32,
    ) -> Result<Option<EncodedAccessUnit>, EncodeError> {
        let options = force_keyframe_options()?;
        let Some(sample) = self.encode_frame(
            surface,
            lease,
            pts,
            timescale,
            Some(options),
            "forced keyframe",
        )?
        else {
            return Ok(None);
        };
        let Some(block) = sample.data_buffer() else {
            return Err(EncodeError::SampleData);
        };
        let Some(data) = block.copy_data_bytes(0, block.data_length()) else {
            return Err(EncodeError::SampleData);
        };
        self.finish_access_unit(&data, sample.as_ptr().cast::<c_void>(), pts, timescale)
    }

    fn encode_frame(
        &mut self,
        surface: &IOSurface,
        lease: Option<CapturedFrame>,
        pts: i64,
        timescale: i32,
        options: Option<CFDictionary>,
        label: &str,
    ) -> Result<Option<CMSampleBuffer>, EncodeError> {
        let wrap_started = Instant::now();
        self.input_pixel_format = surface.pixel_format();
        let pixel_buffer = CVPixelBuffer::create_with_io_surface(surface)
            .map_err(|status| EncodeError::Encode(format!("CVPixelBufferCreate: {status}")))?;
        let wrap = wrap_started.elapsed();
        let submit_started = Instant::now();
        let Some(session) = self.session.take() else {
            return Err(EncodeError::Encode(
                "encoder session was invalidated after a timed-out frame".to_owned(),
            ));
        };
        let retained_frame = lease;
        let result = match session.encode_frame(
            pixel_buffer,
            apple_cf::cm::CMTime::new(pts, timescale),
            apple_cf::cm::CMTime::INVALID,
            options,
            ENCODE_COMPLETION_TIMEOUT,
        ) {
            Ok(completed) => completed,
            Err(()) => {
                return Err(EncodeError::Encode(format!(
                    "{label} timed out after {ENCODE_COMPLETION_TIMEOUT:?}"
                )));
            }
        };
        drop(retained_frame);
        self.session = Some(session);
        // Three costs share the one number this host used to report. Wrapping
        // the IOSurface happens per frame and is ours; the wait is
        // VideoToolbox's; whatever follows is assembly. Only the split says
        // which of them is worth attacking, and the first two are not
        // obviously the same size.
        self.encode_split.observe(wrap, submit_started.elapsed());
        if self.encode_split.should_report() {
            let (wrap_ms, wait_ms, frames) = self.encode_split.snapshot();
            tracing::info!(
                target: "arcen::media",
                mean_surface_wrap_ms = wrap_ms,
                mean_vt_wait_ms = wait_ms,
                frames,
                "encode cost split",
            );
        }

        match result {
            Ok(sample) => Ok(Some(sample)),
            // A real-time session may drop a frame. VideoToolbox reports that
            // as success with no sample, which this binding surfaces as a
            // callback status of -1. It is not a failure: the next frame
            // encodes normally, and a keyframe that was dropped stays owed.
            // Treating it as an error ended the whole session on one frame
            // lost to media-engine contention.
            Err(videotoolbox::VTError::EncoderCallback(-1)) => {
                self.dropped_frames = self.dropped_frames.saturating_add(1);
                tracing::debug!(
                    target: "arcen::media",
                    dropped = self.dropped_frames,
                    "VideoToolbox dropped a frame"
                );
                Ok(None)
            }
            Err(error) => Err(EncodeError::Encode(error.to_string())),
        }
    }

    fn finish_access_unit(
        &mut self,
        data: &[u8],
        sample_ptr: *mut c_void,
        pts: i64,
        timescale: i32,
    ) -> Result<Option<EncodedAccessUnit>, EncodeError> {
        if data.is_empty() {
            return Ok(None);
        }

        let keyframe = is_keyframe(data, self.config.codec);
        let mut bytes = Vec::with_capacity(data.len() + 256);
        if keyframe {
            // SAFETY: the sample buffer that yielded `sample_ptr` is still
            // alive for this scope, and CoreMedia owns the returned parameter
            // set storage.
            let parameter_sets = unsafe { parameter_sets(sample_ptr, self.config.codec) };
            if parameter_sets.is_empty() {
                return Err(EncodeError::MissingParameterSets);
            }
            for set in &parameter_sets {
                bytes.extend_from_slice(&arcen_media::annexb::START_CODE);
                bytes.extend_from_slice(set);
            }
            if self.config.codec == EncoderCodec::Hevc {
                self.observe_stream_truth(&parameter_sets);
            }
        }
        append_annex_b(&mut bytes, data);
        Ok(Some(EncodedAccessUnit {
            bytes,
            keyframe,
            pts,
            timescale,
        }))
    }
}

impl Encoder {
    /// Reads the keyframe's SPS and reports it when it changes.
    ///
    /// The session's plan says what was asked for; this says what was
    /// encoded. They have disagreed before without anything noticing, so the
    /// bitstream is read rather than trusted.
    fn observe_stream_truth(&mut self, parameter_sets: &[Vec<u8>]) {
        let Some(truth) = parameter_sets
            .iter()
            .find_map(|set| arcen_media::hevc_sps::parse_sps(set).ok())
        else {
            return;
        };
        if self.stream_truth == Some(truth) {
            return;
        }
        self.stream_truth = Some(truth);
        let colour = truth.colour;
        tracing::info!(
            target: "arcen::media",
            summary = %truth.summary(),
            profile_idc = truth.profile_idc,
            chroma_format_idc = truth.chroma_format_idc,
            bit_depth = truth.bit_depth_luma,
            primaries = colour.map(|colour| colour.primaries),
            transfer = colour.map(|colour| colour.transfer),
            matrix = colour.map(|colour| colour.matrix),
            full_range = colour.map(|colour| colour.full_range),
            input_pixel_format = %fourcc(self.input_pixel_format),
            "encoded stream truth",
        );
    }
}

impl EncoderAcceleration for Encoder {
    fn uses_hardware_acceleration(&self) -> Option<bool> {
        Self::uses_hardware_acceleration(self)
    }
}

/// Converts VideoToolbox's read-back into the shared accelerator class.
#[must_use]
pub fn accelerator_class_from_encoder(
    encoder: &impl EncoderAcceleration,
) -> Option<arcen_media::video::AcceleratorClass> {
    encoder.uses_hardware_acceleration().map(|hardware| {
        if hardware {
            arcen_media::video::AcceleratorClass::Hardware
        } else {
            arcen_media::video::AcceleratorClass::Software
        }
    })
}

/// Sets the profile and colour description a plan asks for, before the first
/// frame.
///
/// A refused profile is a refused session, not a quieter one: encoding Main
/// for a plan that promised Main 4:4:4 10 is exactly the silent downgrade
/// this exists to end.
fn apply_profile_and_colour(
    session: &VtCompressionSession,
    config: EncoderConfig,
) -> Result<(), EncodeError> {
    let mut pairs: Vec<(CFString, CFType)> = Vec::new();
    if config.codec == EncoderCodec::H264 {
        // SAFETY: process-lifetime VideoToolbox constants.
        let key = unsafe {
            cf_string_constant(videotoolbox::ffi::kVTCompressionPropertyKey_ProfileLevel)
        }
        .ok_or_else(|| EncodeError::SessionCreate("no ProfileLevel key".to_owned()))?;
        let value =
            unsafe { cf_string_constant(videotoolbox::ffi::kVTProfileLevel_H264_High_AutoLevel) }
                .ok_or_else(|| EncodeError::SessionCreate("no H.264 High profile".to_owned()))?;
        pairs.push((key, value.into()));
    }
    if let Some(symbol) = config.profile.symbol() {
        let profile = exported_cf_string(symbol).ok_or_else(|| {
            EncodeError::SessionCreate(format!(
                "this macOS has no {} profile",
                symbol.to_string_lossy()
            ))
        })?;
        // SAFETY: a process-lifetime VideoToolbox constant.
        let key = unsafe {
            cf_string_constant(videotoolbox::ffi::kVTCompressionPropertyKey_ProfileLevel)
        }
        .ok_or_else(|| EncodeError::SessionCreate("no ProfileLevel key".to_owned()))?;
        pairs.push((key, profile.into()));
    }
    if let Some(colour) = config.colour {
        // SAFETY: process-lifetime VideoToolbox constants.
        let keys = unsafe {
            [
                (
                    videotoolbox::ffi::kVTCompressionPropertyKey_ColorPrimaries,
                    colour.primaries,
                ),
                (
                    videotoolbox::ffi::kVTCompressionPropertyKey_TransferFunction,
                    colour.transfer,
                ),
                (
                    videotoolbox::ffi::kVTCompressionPropertyKey_YCbCrMatrix,
                    colour.matrix,
                ),
            ]
        };
        for (key, value) in keys {
            // SAFETY: process-lifetime VideoToolbox constants.
            let key = unsafe { cf_string_constant(key) }
                .ok_or_else(|| EncodeError::SessionCreate("no colour key".to_owned()))?;
            pairs.push((key, CFString::new(value).into()));
        }
    }
    if pairs.is_empty() {
        return Ok(());
    }
    let borrowed: Vec<(&dyn apple_cf::cf::AsCFType, &dyn apple_cf::cf::AsCFType)> = pairs
        .iter()
        .map(|(key, value)| {
            (
                key as &dyn apple_cf::cf::AsCFType,
                value as &dyn apple_cf::cf::AsCFType,
            )
        })
        .collect();
    session
        .set_properties(&CFDictionary::from_pairs(&borrowed))
        .map_err(|error| {
            EncodeError::SessionCreate(format!(
                "VideoToolbox refused profile {:?} with colour {:?}: {error}",
                config.profile, config.colour
            ))
        })
}

fn encoder_specification(config: EncoderConfig, require_hardware: bool) -> Option<CFDictionary> {
    if !uses_low_latency_rate_control(config) {
        return None;
    }
    // SAFETY: process-lifetime VideoToolbox constants.
    let low_latency = unsafe {
        cf_string_constant(
            videotoolbox::ffi::kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
        )
    }?;
    let value = cf_bool(true);
    if !require_hardware {
        return Some(CFDictionary::from_pairs(&[(
            &low_latency as &dyn AsCFType,
            &value as &dyn AsCFType,
        )]));
    }
    // SAFETY: as above.
    let require = unsafe {
        cf_string_constant(
            videotoolbox::ffi::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
        )
    }?;
    Some(CFDictionary::from_pairs(&[
        (&low_latency as &dyn AsCFType, &value as &dyn AsCFType),
        (&require as &dyn AsCFType, &value as &dyn AsCFType),
    ]))
}

/// The encoder's acceleration class: its own read-back when it reports one,
/// otherwise hardware when the session could only have been created on a
/// hardware encoder, otherwise unknown.
const fn classify_acceleration(readback: Option<bool>, hardware_required: bool) -> Option<bool> {
    match readback {
        Some(value) => Some(value),
        None if hardware_required => Some(true),
        None => None,
    }
}

const fn uses_low_latency_rate_control(config: EncoderConfig) -> bool {
    prioritizes_encode_speed(config)
}

const fn prioritizes_encode_speed(config: EncoderConfig) -> bool {
    matches!(config.profile, EncodeProfile::CodecDefault)
}

fn videotoolbox_max_keyframe_interval(
    keyframe: arcen_media::video::KeyframePolicy,
    fps: u32,
) -> i64 {
    match keyframe.safety_refresh_interval() {
        Some(_) => i64::from(keyframe.scheduled_period_frames(fps)),
        None => i64::from(i32::MAX),
    }
}

fn data_rate_limits_property(max_bitrate_bps: i32) -> Option<(CFString, CFType)> {
    let key = exported_cf_string(c"kVTCompressionPropertyKey_DataRateLimits")?;
    let bytes_per_second = apple_cf::cf::CFNumber::from_i64(i64::from(max_bitrate_bps.max(1)) / 8);
    let one_second = apple_cf::cf::CFNumber::from_i64(1);
    let values = CFArray::from_values(&[&bytes_per_second, &one_second]);
    // SAFETY: `values` is a live CFArray object; retaining it as a generic
    // CFType gives the property dictionary its own owned reference.
    let value = unsafe { CFType::from_raw_retained(values.as_ptr().cast()) }?;
    Some((key, value))
}

fn cf_bool(value: bool) -> CFType {
    // SAFETY: CoreFoundation boolean singletons are process-lifetime CFType
    // objects; retaining one gives this wrapper ordinary owned lifetime.
    unsafe {
        CFType::from_raw_retained(
            (if value {
                videotoolbox::ffi::kCFBooleanTrue
            } else {
                videotoolbox::ffi::kCFBooleanFalse
            })
            .cast_mut()
            .cast(),
        )
    }
    .expect("CoreFoundation boolean constants are non-null")
}

unsafe fn push_bool_property(
    pairs: &mut Vec<(CFString, CFType)>,
    key: videotoolbox::ffi::CFStringRef,
    value: bool,
) -> Result<(), videotoolbox::VTError> {
    let key = unsafe { cf_string_constant(key) }.ok_or_else(|| {
        videotoolbox::VTError::InvalidArgument("VideoToolbox property key is null".to_owned())
    })?;
    pairs.push((key, cf_bool(value)));
    Ok(())
}

unsafe fn push_number_property(
    pairs: &mut Vec<(CFString, CFType)>,
    key: videotoolbox::ffi::CFStringRef,
    value: i64,
) -> Result<(), videotoolbox::VTError> {
    let key = unsafe { cf_string_constant(key) }.ok_or_else(|| {
        videotoolbox::VTError::InvalidArgument("VideoToolbox property key is null".to_owned())
    })?;
    pairs.push((key, apple_cf::cf::CFNumber::from_i64(value).into()));
    Ok(())
}

fn push_named_number_property(pairs: &mut Vec<(CFString, CFType)>, key: &str, value: i64) {
    pairs.push((
        CFString::new(key),
        apple_cf::cf::CFNumber::from_i64(value).into(),
    ));
}

/// Wraps a process-lifetime `CFStringRef` constant.
///
/// # Safety
///
/// `constant` must be a `CFStringRef` exported by a system framework.
unsafe fn cf_string_constant<T>(constant: *const T) -> Option<CFString> {
    // SAFETY: the caller guarantees a live constant; the wrapper retains it.
    unsafe { CFString::from_raw_retained(constant.cast_mut().cast()) }
}

/// Looks up a `CFStringRef` constant a system framework exports by name.
fn exported_cf_string(symbol: &std::ffi::CStr) -> Option<CFString> {
    unsafe extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
    }
    // `RTLD_DEFAULT` on Apple platforms.
    let default = -2_isize as *mut c_void;
    // SAFETY: `dlsym` with RTLD_DEFAULT and a NUL-terminated name.
    let address = unsafe { dlsym(default, symbol.as_ptr()) };
    if address.is_null() {
        return None;
    }
    // SAFETY: the symbol is a `const CFStringRef` variable, so its address
    // holds the string's pointer.
    let value = unsafe { *address.cast::<*const c_void>() };
    if value.is_null() {
        return None;
    }
    // SAFETY: a framework-exported CFString constant.
    unsafe { cf_string_constant(value) }
}

/// A bi-planar 10-bit surface for a probe encode: `xf44` (4:4:4) or `xf20`
/// (4:2:0), full range, holding a horizontal luma ramp over neutral chroma.
///
/// Content matters a little — an encoder handed a flat surface can take
/// shortcuts — and exactness does not: the probe reads the SPS, not pixels.
#[must_use]
pub fn ten_bit_probe_surface(width: usize, height: usize, chroma_444: bool) -> Option<IOSurface> {
    let format: u32 = if chroma_444 { 0x7866_3434 } else { 0x7866_3230 };
    let luma_row = (width * 2).next_multiple_of(64);
    let (chroma_width, chroma_height) = if chroma_444 {
        (width, height)
    } else {
        (width.div_ceil(2), height.div_ceil(2))
    };
    let chroma_row = (chroma_width * 4).next_multiple_of(64);
    let luma_size = luma_row * height;
    let chroma_size = chroma_row * chroma_height;
    let planes = [
        apple_cf::iosurface::PlaneProperties {
            width,
            height,
            bytes_per_row: luma_row,
            bytes_per_element: 2,
            offset: 0,
            size: luma_size,
        },
        apple_cf::iosurface::PlaneProperties {
            width: chroma_width,
            height: chroma_height,
            bytes_per_row: chroma_row,
            bytes_per_element: 4,
            offset: luma_size,
            size: chroma_size,
        },
    ];
    let surface = IOSurface::create_with_properties(
        width,
        height,
        format,
        2,
        luma_row,
        luma_size + chroma_size,
        Some(&planes),
    )?;
    {
        let mut guard = surface.lock_read_write().ok()?;
        let luma = guard.base_address_of_plane_mut(0)?;
        let chroma = guard.base_address_of_plane_mut(1)?;
        for y in 0..height {
            for x in 0..width {
                // Ten significant bits, left-aligned in sixteen.
                let code = u16::try_from(x * 1023 / width.max(1)).unwrap_or(1023) << 6;
                // SAFETY: inside the plane just described, locked for writing.
                unsafe {
                    luma.add(y * luma_row + x * 2)
                        .cast::<u16>()
                        .write_unaligned(code)
                };
            }
        }
        for y in 0..chroma_height {
            for x in 0..chroma_width * 2 {
                // SAFETY: inside the plane just described, locked for writing.
                unsafe {
                    chroma
                        .add(y * chroma_row + x * 2)
                        .cast::<u16>()
                        .write_unaligned(512 << 6);
                }
            }
        }
    }
    Some(surface)
}

/// Renders a `CoreVideo` `OSType` as its four characters.
fn fourcc(value: u32) -> String {
    value
        .to_be_bytes()
        .iter()
        .map(|&byte| {
            if byte.is_ascii_graphic() {
                char::from(byte)
            } else {
                '?'
            }
        })
        .collect()
}

fn force_keyframe_options() -> Result<CFDictionary, EncodeError> {
    // SAFETY: VideoToolbox exports this process-lifetime `CFStringRef` and the
    // dictionary only keeps a retained reference to it; ownership of the
    // framework constant itself stays with the framework.
    let Some(key) = (unsafe {
        CFString::from_raw_retained(
            videotoolbox::ffi::kVTEncodeFrameOptionKey_ForceKeyFrame
                .cast_mut()
                .cast(),
        )
    }) else {
        return Err(EncodeError::Encode(
            "VideoToolbox did not export ForceKeyFrame".to_owned(),
        ));
    };
    // SAFETY: `kCFBooleanTrue` is a process-lifetime Core Foundation singleton.
    // Retaining it gives the safe wrapper a normal owned reference to release.
    let Some(value) =
        (unsafe { CFType::from_raw_retained(videotoolbox::ffi::kCFBooleanTrue.cast_mut().cast()) })
    else {
        return Err(EncodeError::Encode(
            "CoreFoundation did not export kCFBooleanTrue".to_owned(),
        ));
    };
    Ok(CFDictionary::from_pairs(&[(&key, &value)]))
}

/// Reads the out-of-band parameter sets from an encoded sample buffer.
///
/// # Safety
///
/// `sample_ptr` must be a live `CMSampleBufferRef`.
unsafe fn parameter_sets(sample_ptr: *mut c_void, codec: EncoderCodec) -> Vec<Vec<u8>> {
    if sample_ptr.is_null() {
        return Vec::new();
    }
    // SAFETY: caller guarantees a live sample buffer.
    let format = unsafe { CMSampleBufferGetFormatDescription(sample_ptr) };
    if format.is_null() {
        return Vec::new();
    }
    let mut count = 0_usize;
    let mut first: *const u8 = std::ptr::null();
    let mut first_size = 0_usize;
    // SAFETY: out-parameters are valid for the duration of the call.
    let status = unsafe {
        get_parameter_set(
            codec,
            format,
            0,
            &raw mut first,
            &raw mut first_size,
            &raw mut count,
            std::ptr::null_mut(),
        )
    };
    if status != 0 || count == 0 {
        return Vec::new();
    }
    let mut sets = Vec::with_capacity(count);
    for index in 0..count {
        let mut pointer: *const u8 = std::ptr::null();
        let mut size = 0_usize;
        // SAFETY: out-parameters are valid for the duration of the call.
        let status = unsafe {
            get_parameter_set(
                codec,
                format,
                index,
                &raw mut pointer,
                &raw mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || pointer.is_null() || size == 0 {
            continue;
        }
        // SAFETY: CoreMedia guarantees `size` readable bytes at `pointer`.
        sets.push(unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec());
    }
    sets
}

/// # Safety
///
/// `format` must be a live `CMVideoFormatDescriptionRef` and the out-pointers
/// must be valid or null.
unsafe fn get_parameter_set(
    codec: EncoderCodec,
    format: *mut c_void,
    index: usize,
    pointer: *mut *const u8,
    size: *mut usize,
    count: *mut usize,
    header_length: *mut i32,
) -> i32 {
    match codec {
        // SAFETY: forwarded from the caller's contract.
        EncoderCodec::Hevc => unsafe {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                format,
                index,
                pointer,
                size,
                count,
                header_length,
            )
        },
        // SAFETY: forwarded from the caller's contract.
        EncoderCodec::H264 => unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format,
                index,
                pointer,
                size,
                count,
                header_length,
            )
        },
    }
}

/// Maps this encoder's codec onto the shared NAL vocabulary.
///
/// The mapping is explicit because the two layouts alias: an H.264 P-slice
/// header reads as an HEVC IRAP type, so classification has to follow the
/// session's real codec rather than the bytes.
const fn nal_codec(codec: EncoderCodec) -> arcen_media::annexb::NalCodec {
    match codec {
        EncoderCodec::H264 => arcen_media::annexb::NalCodec::H264,
        EncoderCodec::Hevc => arcen_media::annexb::NalCodec::H265,
    }
}

/// Rewrites a length-prefixed sample into Annex B, in place of the 4-byte
/// lengths `VideoToolbox` emits.
///
/// The conversion itself is byte arithmetic over a documented bitstream and
/// lives in `arcen_media`; Windows Media Foundation produces the same
/// length-prefixed form, so a copy here would be the second of three.
fn append_annex_b(out: &mut Vec<u8>, sample: &[u8]) {
    arcen_media::annexb::append_length_prefixed_as_annex_b(out, sample);
}

/// Reports whether a length-prefixed sample contains an IRAP/IDR unit.
fn is_keyframe(sample: &[u8], codec: EncoderCodec) -> bool {
    arcen_media::annexb::length_prefixed_is_keyframe(sample, nal_codec(codec))
}

fn capped_bitrate_bps(
    width: u32,
    height: u32,
    fps: u32,
    chroma: arcen_media::ChromaSubsampling,
    depth: arcen_media::BitDepth,
) -> u32 {
    arcen_media::video::link_capped_average_bitrate_bps(width, height, fps, chroma, depth)
}

#[cfg(test)]
mod test_ffi {
    #[link(name = "CoreVideo", kind = "framework")]
    unsafe extern "C" {
        pub(super) static kCVPixelBufferPixelFormatTypeKey: *const std::ffi::c_void;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{BitDepth, ChromaSubsampling};

    const SDR: (ChromaSubsampling, BitDepth) = (ChromaSubsampling::Yuv420, BitDepth::Eight);

    fn length_prefixed(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for unit in units {
            out.extend_from_slice(
                &u32::try_from(unit.len())
                    .expect("test unit fits")
                    .to_be_bytes(),
            );
            out.extend_from_slice(unit);
        }
        out
    }

    /// Runs against the real `VideoToolbox`: a 10-bit surface with the
    /// profile set must come back from the bitstream as what was asked for.
    #[test]
    fn ten_bit_profiles_are_proven_by_the_sps_they_produce() {
        for (chroma_444, chroma, profile_idc, chroma_idc) in [
            (true, arcen_media::ChromaSubsampling::Yuv444, Some(4), 3),
            (false, arcen_media::ChromaSubsampling::Yuv420, None, 1),
        ] {
            let config = EncoderConfig::realtime_for(
                256,
                256,
                EncoderCodec::Hevc,
                30,
                chroma,
                arcen_media::BitDepth::Ten,
            )
            .with_colour(EncodeColour::from_plan_tokens("bt709", "bt709", "bt709"));
            let mut encoder = Encoder::new(config).expect("the profile is accepted");
            let surface = ten_bit_probe_surface(256, 256, chroma_444).expect("probe surface");
            let truth = encoder
                .prove_with_surface(&surface)
                .expect("encodes")
                .expect("a keyframe with an SPS");
            // Measured: VideoToolbox's Main 10 SPS states general_profile_idc
            // 1 with only the Main 10 compatibility flag set, over genuinely
            // 10-bit samples. Decoders obey the depth and chroma fields, so
            // those are what is asserted; the 4:4:4 profile is exact.
            if let Some(profile_idc) = profile_idc {
                assert_eq!(truth.profile_idc, profile_idc, "{}", truth.summary());
            }
            assert_eq!(truth.chroma_format_idc, chroma_idc, "{}", truth.summary());
            assert_eq!(truth.bit_depth_luma, 10, "{}", truth.summary());
            let colour = truth.colour.expect("tagged");
            assert_eq!(
                (colour.primaries, colour.transfer, colour.matrix),
                (1, 1, 1)
            );
            assert!(
                colour.full_range,
                "an xf44/xf20 surface is full range: {}",
                truth.summary()
            );
        }
    }

    /// Encodes a Grading keyframe with the real `VideoToolbox`, decodes it
    /// with the real decoder, and reads the pixels back: the proof that the
    /// stream is ten bits and 4:4:4, not just labelled so.
    #[test]
    fn a_grading_round_trip_keeps_ten_bits_and_single_pixel_chroma() {
        const WIDTH: usize = 1024;
        const HEIGHT: usize = 64;
        let surface = ten_bit_probe_surface(WIDTH, HEIGHT, true).expect("surface");
        // Luma: one code per column, the whole ten-bit range. Chroma: Cb and
        // Cr alternating high and low on every single pixel.
        let chroma_code = |x: usize| -> u16 { if x % 2 == 0 { 320 } else { 704 } };
        {
            let mut guard = surface.lock_read_write().expect("lock");
            let luma_row = surface.bytes_per_row_of_plane(0);
            let chroma_row = surface.bytes_per_row_of_plane(1);
            let luma = guard.base_address_of_plane_mut(0).expect("luma");
            let chroma = guard.base_address_of_plane_mut(1).expect("chroma");
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let code = u16::try_from(x).expect("fits") << 6;
                    // SAFETY: inside both planes, locked for writing.
                    unsafe {
                        luma.add(y * luma_row + x * 2)
                            .cast::<u16>()
                            .write_unaligned(code);
                        let pair = chroma.add(y * chroma_row + x * 4).cast::<u16>();
                        pair.write_unaligned(chroma_code(x) << 6);
                        pair.add(1).write_unaligned(chroma_code(x) << 6);
                    }
                }
            }
        }
        let config = EncoderConfig::realtime_for(
            i32::try_from(WIDTH).expect("fits"),
            i32::try_from(HEIGHT).expect("fits"),
            EncoderCodec::Hevc,
            30,
            arcen_media::ChromaSubsampling::Yuv444,
            arcen_media::BitDepth::Ten,
        )
        .with_colour(EncodeColour::from_plan_tokens("bt709", "bt709", "bt709"));
        // Generous, so the one frame is not starved of bits.
        let config = EncoderConfig {
            bitrate_bps: 40_000_000,
            max_bitrate_bps: 40_000_000,
            ..config
        };
        let mut encoder = Encoder::new(config).expect("Grading session");
        let sample = encoder
            .encode_frame(
                &surface,
                None,
                0,
                30,
                Some(force_keyframe_options().expect("options")),
                "test",
            )
            .expect("encodes")
            .expect("a sample");
        let format = sample.format_description().expect("format description");

        // SAFETY: a process-lifetime Core Video key.
        let key =
            unsafe { cf_string_constant(test_ffi::kCVPixelBufferPixelFormatTypeKey) }.expect("key");
        let value: CFType = apple_cf::cf::CFNumber::from_i64(0x7866_3434).into();
        let attributes = CFDictionary::from_pairs(&[(
            &key as &dyn apple_cf::cf::AsCFType,
            &value as &dyn apple_cf::cf::AsCFType,
        )]);
        let (sender, frames) = std::sync::mpsc::channel();
        let decoder =
            videotoolbox::decompression::DecompressionSession::new_with_image_buffer_attributes(
                &format,
                Some(&attributes),
                move |frame| {
                    let _ = sender.send(frame.image_buffer);
                },
            )
            .expect("decoder");
        decoder.decode(&sample).expect("decodes");
        let _ = decoder.wait_for_async_frames();
        let decoded = frames
            .recv_timeout(Duration::from_secs(5))
            .expect("a frame")
            .expect("an image");
        assert_eq!(decoded.pixel_format(), 0x7866_3434, "decoded as xf44");

        let surface = decoded.io_surface().expect("IOSurface-backed");
        let guard = surface.lock_read_only().expect("lock");
        let luma = guard.base_address_of_plane(0).expect("luma");
        let chroma = guard.base_address_of_plane(1).expect("chroma");
        let (luma_row, chroma_row) = (
            surface.bytes_per_row_of_plane(0),
            surface.bytes_per_row_of_plane(1),
        );
        let y = HEIGHT / 2;
        let mut levels = std::collections::BTreeSet::new();
        let mut worst_luma = 0_u16;
        let mut worst_chroma = 0_u16;
        for x in 0..WIDTH {
            // SAFETY: inside both planes, locked for reading.
            let (luma_code, cb) = unsafe {
                (
                    luma.add(y * luma_row + x * 2)
                        .cast::<u16>()
                        .read_unaligned()
                        >> 6,
                    chroma
                        .add(y * chroma_row + x * 4)
                        .cast::<u16>()
                        .read_unaligned()
                        >> 6,
                )
            };
            levels.insert(luma_code);
            worst_luma = worst_luma.max(luma_code.abs_diff(u16::try_from(x).expect("fits")));
            worst_chroma = worst_chroma.max(cb.abs_diff(chroma_code(x)));
        }
        eprintln!(
            "grading round trip: {} distinct luma levels of 1024, worst luma error \
             {worst_luma}, worst single-pixel chroma error {worst_chroma}",
            levels.len()
        );
        // Eight bits cannot hold more than 256 levels.
        assert!(
            levels.len() > 700,
            "only {} distinct luma levels survived",
            levels.len()
        );
        assert!(worst_luma <= 8, "luma drifted by {worst_luma} codes");
        // 4:2:0 would average each pair to 512, an error of 192.
        assert!(
            worst_chroma <= 32,
            "single-pixel chroma drifted by {worst_chroma} codes"
        );
    }

    fn assert_no_periodic_keyframes_then_forced_idr(
        mut encoder: Encoder,
        surface: &IOSurface,
        frames: u32,
        timescale: i32,
        label: &str,
    ) {
        let mut keyframes = Vec::new();
        for pts in 0..frames {
            let sample = encoder
                .encode_frame(surface, None, i64::from(pts), timescale, None, label)
                .expect("encodes")
                .expect("sample");
            let Some(block) = sample.data_buffer() else {
                panic!("sample data");
            };
            let Some(data) = block.copy_data_bytes(0, block.data_length()) else {
                panic!("sample bytes");
            };
            if is_keyframe(&data, encoder.config().codec) {
                keyframes.push(pts);
            }
        }
        assert_eq!(keyframes, vec![0], "{label} inserted periodic keyframes");

        let forced = encoder
            .encode_frame(
                surface,
                None,
                i64::from(frames),
                timescale,
                Some(force_keyframe_options().expect("force options")),
                label,
            )
            .expect("forced encode")
            .expect("forced sample");
        let Some(block) = forced.data_buffer() else {
            panic!("forced sample data");
        };
        let Some(data) = block.copy_data_bytes(0, block.data_length()) else {
            panic!("forced sample bytes");
        };
        assert!(
            is_keyframe(&data, encoder.config().codec),
            "{label} did not honor forced IDR"
        );
    }

    /// Native regression for VideoToolbox's undocumented `0 means automatic`
    /// cadence: absent shared safety policy must become a huge finite
    /// MaxKeyFrameInterval, not 0, on both encoder modes.
    #[test]
    fn on_demand_policy_disables_videotoolbox_periodic_keyframes() {
        const FRAMES: u32 = 150;
        let grading_surface = ten_bit_probe_surface(64, 64, true).expect("grading surface");
        let grading = Encoder::new(
            EncoderConfig::realtime_for(
                64,
                64,
                EncoderCodec::Hevc,
                30,
                arcen_media::ChromaSubsampling::Yuv444,
                arcen_media::BitDepth::Ten,
            )
            .with_colour(EncodeColour::from_plan_tokens("bt709", "bt709", "bt709")),
        )
        .expect("grading encoder");
        assert_no_periodic_keyframes_then_forced_idr(
            grading,
            &grading_surface,
            FRAMES,
            30,
            "grading",
        );

        let fast = Encoder::new(EncoderConfig::realtime(64, 64, EncoderCodec::Hevc, 30))
            .expect("low-latency encoder");
        assert_no_periodic_keyframes_then_forced_idr(
            fast,
            &grading_surface,
            FRAMES,
            30,
            "low-latency",
        );
    }

    #[test]
    fn the_fast_path_stays_untagged_codec_default() {
        let config = EncoderConfig::realtime(1920, 1080, EncoderCodec::Hevc, 30);
        assert_eq!(config.profile, EncodeProfile::CodecDefault);
        assert_eq!(config.colour, None);
        assert_eq!(
            colour_for_capture(&crate::capture::CaptureConfig::sdr(1, 1920, 1080, 30)),
            None
        );
        let grading =
            colour_for_capture(&crate::capture::CaptureConfig::grading(1, 1920, 1080, 30))
                .expect("grading is tagged");
        assert_eq!(grading.transfer, "ITU_R_709_2");
    }

    #[test]
    fn length_prefixed_samples_become_annex_b() {
        let sample = length_prefixed(&[&[0xAA, 0xBB], &[0xCC]]);
        let mut out = Vec::new();
        append_annex_b(&mut out, &sample);
        assert_eq!(out, vec![0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    }

    #[test]
    fn truncated_samples_do_not_read_past_the_end() {
        // A length that claims more bytes than are present must stop cleanly
        // rather than panic or emit garbage.
        let sample = vec![0, 0, 0, 8, 0x41, 0x42];
        let mut out = Vec::new();
        append_annex_b(&mut out, &sample);
        assert!(out.is_empty());
        assert!(!is_keyframe(&sample, EncoderCodec::H264));
        assert!(!is_keyframe(&sample, EncoderCodec::Hevc));
    }

    #[test]
    fn detects_h264_idr_and_hevc_irap() {
        // H.264 nal_unit_type 5.
        assert!(is_keyframe(
            &length_prefixed(&[&[0x65, 0x00]]),
            EncoderCodec::H264
        ));
        // HEVC nal_unit_type 20 (IDR_N_LP) is (20 << 1) = 0x28, with
        // temporal_id_plus_one of 1 in the second header byte and a payload.
        assert!(is_keyframe(
            &length_prefixed(&[&[0x28, 0x01, 0x00]]),
            EncoderCodec::Hevc
        ));
    }

    #[test]
    fn non_keyframe_slices_are_not_reported_as_keyframes() {
        // H.264 non-IDR slice is type 1; HEVC TRAIL_R is type 1 => 0x02.
        assert!(!is_keyframe(
            &length_prefixed(&[&[0x41, 0x00]]),
            EncoderCodec::H264
        ));
        assert!(!is_keyframe(
            &length_prefixed(&[&[0x02, 0x00]]),
            EncoderCodec::Hevc
        ));
    }

    #[test]
    fn h264_p_slice_is_not_mistaken_for_an_hevc_irap() {
        // `0x21` is an H.264 P-slice (type 1) but reads as HEVC type 16, which
        // is in the IRAP range. Detection must follow the session's codec, or
        // every H.264 frame is reported as a keyframe.
        let p_slice = length_prefixed(&[&[0x21, 0x01, 0x00]]);
        assert!(!is_keyframe(&p_slice, EncoderCodec::H264));
        assert!(is_keyframe(&p_slice, EncoderCodec::Hevc));
    }

    #[test]
    fn zero_length_units_terminate_parsing() {
        let sample = vec![0, 0, 0, 0, 0x65];
        let mut out = Vec::new();
        append_annex_b(&mut out, &sample);
        assert!(out.is_empty());
    }

    #[test]
    fn a_1080p_session_is_not_capped_at_all() {
        // Parity with Linux is only known at this size, so this is the case
        // that must not move.
        assert_eq!(
            capped_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1),
            arcen_media::video::average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1),
        );
    }

    #[test]
    fn a_smaller_session_still_costs_less_than_the_cap() {
        let small = capped_bitrate_bps(1280, 720, 30, SDR.0, SDR.1);
        let cap = arcen_media::video::average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        assert!(
            small < cap,
            "720p asked for {small}, which is not below {cap}"
        );
    }

    #[test]
    fn a_1440p_session_is_held_to_the_1080p_figure() {
        // The measured case: 8.29 Mbps uncapped, which this link answered with
        // a 52 ms writer queue and 22 fps of a requested 30.
        let capped = capped_bitrate_bps(2560, 1440, 30, SDR.0, SDR.1);
        assert_eq!(
            capped,
            arcen_media::video::average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1),
        );
        assert!(capped < arcen_media::video::average_bitrate_bps(2560, 1440, 30, SDR.0, SDR.1));
    }

    #[test]
    fn grading_is_held_below_the_measured_knee() {
        // The measured case: 1800x1130, 30 fps, 4:4:4 10-bit. The shared
        // figure was 10.8 Mbps and the link answered with 441 ms of frame age.
        let grading = capped_bitrate_bps(1800, 1130, 30, ChromaSubsampling::Yuv444, BitDepth::Ten);
        assert_eq!(
            grading,
            arcen_media::video::average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1),
            "Grading is held to the fast path's own 1080p budget"
        );
        // Small sessions still pay only for what they are.
        let small = capped_bitrate_bps(640, 360, 30, ChromaSubsampling::Yuv444, BitDepth::Ten);
        assert!(small < grading);
        // And the fast path is untouched by any of this.
        assert_eq!(
            capped_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1),
            arcen_media::video::average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1),
        );
    }

    #[test]
    fn pipeline_bitrate_override_reaches_encoder_config() {
        let config = EncoderConfig::realtime_for(
            1800,
            1168,
            EncoderCodec::Hevc,
            30,
            ChromaSubsampling::Yuv444,
            BitDepth::Ten,
        )
        .with_bitrate_bps(12_345_678);
        assert_eq!(config.bitrate_bps, 12_345_678);
        assert_eq!(config.max_bitrate_bps, 12_345_678);
    }

    #[test]
    fn grading_pipeline_bounds_reach_videotoolbox_config() {
        let config = EncoderConfig::realtime_for(
            1800,
            1168,
            EncoderCodec::Hevc,
            30,
            ChromaSubsampling::Yuv444,
            BitDepth::Ten,
        )
        .with_bitrate_bounds(4_665_600, 250_000_000);
        assert_eq!(config.bitrate_bps, 4_665_600);
        assert_eq!(config.max_bitrate_bps, 250_000_000);
        let (_, value) = data_rate_limits_property(config.max_bitrate_bps)
            .expect("VideoToolbox exports DataRateLimits");
        assert!(
            value.as_ptr().cast_const() as usize != 0,
            "DataRateLimits must be a non-null CFArray property value"
        );
    }

    #[test]
    fn a_session_created_requiring_hardware_is_hardware_even_without_a_readback() {
        assert_eq!(classify_acceleration(Some(false), true), Some(false));
        assert_eq!(classify_acceleration(Some(true), false), Some(true));
        assert_eq!(classify_acceleration(None, true), Some(true));
        assert_eq!(classify_acceleration(None, false), None);
    }

    #[test]
    fn the_low_latency_realtime_encoder_reports_its_acceleration_class() {
        // The 8-bit Auto/Speed encoder uses low-latency rate control, whose
        // sessions do not report UsingHardwareAcceleratedVideoEncoder. Without
        // a class every macOS Auto/Speed session was served as `custom`.
        for codec in [EncoderCodec::H264, EncoderCodec::Hevc] {
            let config = EncoderConfig::realtime(1920, 1080, codec, 60);
            let encoder = Encoder::new(config).expect("realtime encoder");
            assert!(
                encoder.uses_hardware_acceleration().is_some(),
                "{codec:?} low-latency encoder must report hardware or software"
            );
        }
    }

    #[test]
    fn accelerator_class_comes_from_the_session_encoder_readback() {
        struct FakeEncoder(Option<bool>);
        impl EncoderAcceleration for FakeEncoder {
            fn uses_hardware_acceleration(&self) -> Option<bool> {
                self.0
            }
        }

        assert_eq!(
            accelerator_class_from_encoder(&FakeEncoder(Some(true))),
            Some(arcen_media::video::AcceleratorClass::Hardware)
        );
        assert_eq!(
            accelerator_class_from_encoder(&FakeEncoder(Some(false))),
            Some(arcen_media::video::AcceleratorClass::Software)
        );
        assert_eq!(accelerator_class_from_encoder(&FakeEncoder(None)), None);
    }

    #[test]
    fn speed_start_keeps_the_thirty_fps_safe_bill_and_ceiling_can_reach_sixty() {
        // Measured: a frame-rate-scaled 60 fps start put the lab link into a
        // 670 ms backlog, so Speed still starts at the same safe bill as Auto.
        let contract = arcen_media::video::pipeline_contract(arcen_media::video::PipelineId::Speed);
        let (start, ceiling) = contract.bitrate_bounds(
            1920,
            1080,
            60,
            contract.colour.chroma,
            contract.colour.bit_depth,
        );
        let slow = capped_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        assert_eq!(start, slow);
        assert_eq!(
            ceiling,
            arcen_media::video::average_bitrate_bps(1920, 1080, 60, SDR.0, SDR.1)
        );
        assert_eq!(ceiling, start * 2);
    }
}
