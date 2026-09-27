#![allow(unsafe_code)]

//! End-to-end capture and encode proof for the macOS Pier.
//!
//! This exists so a capture or encode claim can be checked on real hardware
//! instead of inferred from a successful API call. It reports what the machine
//! actually produced: surface format, frame count, keyframe placement, and
//! parameter-set presence. A run that starts a stream but never produces a
//! decodable keyframe is reported as a failure, not a success.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::capture::{
    CaptureConfig, CaptureDynamicRange, CaptureError, CapturePixelFormat, CaptureSession,
};
use crate::encode::{EncodeError, Encoder, EncoderCodec, EncoderConfig};

/// How long a single frame may take before the probe gives up.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// Options for one probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOptions {
    /// Display to capture. `None` selects the main display.
    pub display_id: Option<u32>,
    /// How many frames to capture and encode.
    pub frames: u32,
    /// Codec to encode with.
    pub codec: EncoderCodec,
    /// Pixel layout to request.
    pub pixel_format: CapturePixelFormat,
    /// Dynamic range to request.
    pub dynamic_range: CaptureDynamicRange,
    /// Move the pointer while capturing, to measure a screen that is
    /// changing rather than an idle one.
    ///
    /// `ScreenCaptureKit` only delivers a frame when something changes, so a
    /// static desktop measures the desktop, not the pipeline.
    pub motion: bool,
    /// Where to write the encoded stream, when the caller wants one.
    ///
    /// Each access unit is written as the shared video header followed by its
    /// Annex B bytes, so a decoder can replay exactly what a client would have
    /// received.
    pub out: Option<PathBuf>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            display_id: None,
            frames: 30,
            codec: EncoderCodec::Hevc,
            pixel_format: CapturePixelFormat::Nv12VideoRange,
            dynamic_range: CaptureDynamicRange::Sdr,
            motion: false,
            out: None,
        }
    }
}

/// What the probe asked the system for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RequestedPlan {
    /// Display captured.
    pub display_id: u32,
    /// Requested width in pixels.
    pub width: usize,
    /// Requested height in pixels.
    pub height: usize,
    /// Requested pixel layout.
    pub pixel_format: CapturePixelFormat,
    /// Requested dynamic range.
    pub dynamic_range: CaptureDynamicRange,
    /// Codec requested from `VideoToolbox`.
    pub codec: EncoderCodec,
    /// Number of frames requested.
    pub frames: u32,
}

/// What the system actually produced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ObservedResult {
    /// Frames delivered by `ScreenCaptureKit`.
    pub frames_captured: u32,
    /// Frames the encoder emitted bytes for.
    pub frames_encoded: u32,
    /// Access units a decoder could start from.
    pub keyframes: u32,
    /// Total Annex B bytes produced.
    pub encoded_bytes: usize,
    /// Actual surface width.
    pub surface_width: usize,
    /// Actual surface height.
    pub surface_height: usize,
    /// Actual surface pixel format, as a `CoreVideo` `OSType`.
    pub surface_os_type: u32,
    /// The surface format named, when the Pier recognises it.
    pub surface_pixel_format: Option<CapturePixelFormat>,
    /// Size of the first keyframe, which carries the parameter sets.
    pub first_keyframe_bytes: Option<usize>,
    /// Wall-clock duration of the capture loop.
    pub elapsed_ms: u128,
    /// Frames per second delivered by capture over the whole run.
    pub capture_fps: f64,
    /// Frames per second the encoder sustained.
    pub encode_fps: f64,
    /// Mean time spent inside `VTCompressionSession::encode`, in milliseconds.
    pub mean_encode_ms: f64,
    /// Worst single encode, in milliseconds. A high maximum with a low mean is
    /// what a dropped frame looks like to a person watching.
    pub max_encode_ms: f64,
    /// Resulting bitrate in megabits per second.
    pub bitrate_mbps: f64,
}

/// The full probe result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MediaProbeReport {
    /// What was asked for.
    pub requested: RequestedPlan,
    /// What happened.
    pub observed: ObservedResult,
    /// Whether this run proves a usable capture-and-encode path.
    pub usable: bool,
    /// Why the run is not usable, when it is not.
    pub refusal: Option<String>,
}

/// Why a probe could not run at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "stage", content = "detail")]
pub enum ProbeError {
    /// No display could be selected.
    NoDisplay,
    /// The display inventory could not be read.
    Inventory(String),
    /// Capture could not start or failed mid-run.
    Capture(CaptureError),
    /// Encoding could not start or failed mid-run.
    Encode(EncodeError),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDisplay => formatter.write_str("no capturable display"),
            Self::Inventory(detail) => write!(formatter, "display inventory failed: {detail}"),
            Self::Capture(error) => write!(formatter, "{error}"),
            Self::Encode(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Runs one capture-and-encode proof.
///
/// # Errors
///
/// Returns [`ProbeError`] when no display is available, capture cannot start,
/// or the encoder refuses the stream.
#[allow(clippy::too_many_lines)]
pub fn run(options: &ProbeOptions) -> Result<MediaProbeReport, ProbeError> {
    let displays =
        crate::displays::probe().map_err(|error| ProbeError::Inventory(format!("{error:?}")))?;
    let display = match options.display_id {
        Some(id) => displays
            .iter()
            .find(|display| display.display_id == id)
            .copied(),
        None => displays.first().copied(),
    }
    .ok_or(ProbeError::NoDisplay)?;

    let capture_config = CaptureConfig {
        display_id: display.display_id,
        width: display.pixel_width,
        height: display.pixel_height,
        fps: 60,
        shows_cursor: false,
        pixel_format: options.pixel_format,
        dynamic_range: options.dynamic_range,
    };
    let requested = RequestedPlan {
        display_id: display.display_id,
        width: capture_config.width,
        height: capture_config.height,
        pixel_format: options.pixel_format,
        dynamic_range: options.dynamic_range,
        codec: options.codec,
        frames: options.frames,
    };

    let session = CaptureSession::start(capture_config).map_err(ProbeError::Capture)?;

    // Generate real damage so the measurement reflects the pipeline rather
    // than how still the desktop happened to be.
    let motion = options.motion.then(|| {
        let width = capture_config.width;
        let height = capture_config.height;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            // Display dimensions are far below f64's exact integer range.
            #[allow(clippy::cast_precision_loss)]
            let bounds = crate::input::DesktopBounds::new(0.0, 0.0, width as f64, height as f64);
            let Ok(mut controller) = crate::input::InputController::new(bounds) else {
                return;
            };
            let mut tick = 0_u32;
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let angle = f64::from(tick) * 0.2;
                let motion = arcen_input::PointerMotion {
                    x: 0.5 + 0.3 * angle.cos(),
                    y: 0.5 + 0.3 * angle.sin(),
                    server_x: None,
                    server_y: None,
                    metadata: arcen_input::LowLatencyMetadata::default(),
                };
                let _ = controller.pointer_motion(&motion);
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        (stop, handle)
    });
    let mut encoder = Encoder::new(EncoderConfig::realtime(
        i32::try_from(capture_config.width).unwrap_or(1920),
        i32::try_from(capture_config.height).unwrap_or(1080),
        options.codec,
        capture_config.fps,
    ))
    .map_err(ProbeError::Encode)?;

    let mut sink = match options.out.as_ref() {
        Some(path) => Some(std::fs::File::create(path).map_err(|error| {
            ProbeError::Inventory(format!("create {}: {error}", path.display()))
        })?),
        None => None,
    };

    let started = Instant::now();
    let mut observed = ObservedResult {
        frames_captured: 0,
        frames_encoded: 0,
        keyframes: 0,
        encoded_bytes: 0,
        surface_width: 0,
        surface_height: 0,
        surface_os_type: 0,
        surface_pixel_format: None,
        first_keyframe_bytes: None,
        elapsed_ms: 0,
        capture_fps: 0.0,
        encode_fps: 0.0,
        mean_encode_ms: 0.0,
        max_encode_ms: 0.0,
        bitrate_mbps: 0.0,
    };
    let mut total_encode = Duration::ZERO;
    let mut worst_encode = Duration::ZERO;

    for _ in 0..options.frames {
        let frame = match session.next_frame(FRAME_TIMEOUT) {
            Ok(frame) => frame,
            // A display with nothing moving on it legitimately stops producing
            // frames. That is not an encode failure, so stop and report.
            Err(CaptureError::FrameTimeout) => break,
            Err(error) => return Err(ProbeError::Capture(error)),
        };
        observed.frames_captured += 1;
        observed.surface_width = frame.width;
        observed.surface_height = frame.height;
        observed.surface_os_type = frame.pixel_format;
        observed.surface_pixel_format = CapturePixelFormat::from_os_type(frame.pixel_format);

        let encode_started = Instant::now();
        let unit = encoder.encode(&frame).map_err(ProbeError::Encode)?;
        let encode_took = encode_started.elapsed();
        total_encode += encode_took;
        worst_encode = worst_encode.max(encode_took);
        if let Some(unit) = unit {
            observed.frames_encoded += 1;
            observed.encoded_bytes += unit.bytes.len();
            if let Some(file) = sink.as_mut() {
                use std::io::Write as _;
                let header = crate::stream::header_for(
                    &unit,
                    options.codec,
                    options.pixel_format,
                    observed.frames_encoded,
                );
                // Length-prefixed so a reader can split the stream back into
                // the access units a client would have received.
                let total = u32::try_from(header.len() + unit.bytes.len()).unwrap_or(0);
                file.write_all(&total.to_be_bytes())
                    .and_then(|()| file.write_all(&header))
                    .and_then(|()| file.write_all(&unit.bytes))
                    .map_err(|error| ProbeError::Inventory(format!("write stream: {error}")))?;
            }
            if unit.keyframe {
                observed.keyframes += 1;
                if observed.first_keyframe_bytes.is_none() {
                    observed.first_keyframe_bytes = Some(unit.bytes.len());
                }
            }
        }
    }
    if let Some((stop, handle)) = motion {
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = handle.join();
    }
    let elapsed = started.elapsed();
    observed.elapsed_ms = elapsed.as_millis();
    // Rates are reported over the whole run rather than a best case, because a
    // desktop that stalls once a second is not a smooth desktop.
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 {
        observed.capture_fps = f64::from(observed.frames_captured) / seconds;
        observed.encode_fps = f64::from(observed.frames_encoded) / seconds;
        // Byte counts for a desktop stream are far below f64's exact range.
        #[allow(clippy::cast_precision_loss)]
        let bits = (observed.encoded_bytes as f64) * 8.0;
        observed.bitrate_mbps = bits / seconds / 1_000_000.0;
    }
    if observed.frames_encoded > 0 {
        observed.mean_encode_ms =
            total_encode.as_secs_f64() * 1000.0 / f64::from(observed.frames_encoded);
    }
    observed.max_encode_ms = worst_encode.as_secs_f64() * 1000.0;
    session.stop();

    // A stream that never produced a decodable keyframe is not a usable path,
    // however cleanly the API calls returned.
    let refusal = if observed.frames_captured == 0 {
        Some("no frames were delivered".to_owned())
    } else if observed.frames_encoded == 0 {
        Some("frames were captured but none encoded".to_owned())
    } else if observed.keyframes == 0 {
        Some("no keyframe was produced, so no decoder could start".to_owned())
    } else {
        None
    };

    Ok(MediaProbeReport {
        requested,
        observed,
        usable: refusal.is_none(),
        refusal,
    })
}

/// Parses `probe-media` arguments.
///
/// # Errors
///
/// Returns a usage message for unknown or malformed arguments.
pub fn parse_options(arguments: &[String]) -> Result<ProbeOptions, String> {
    let mut options = ProbeOptions::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--frames" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--frames requires a count".to_owned())?;
                options.frames = value
                    .parse()
                    .map_err(|_| format!("--frames expects a number, got '{value}'"))?;
            }
            "--display" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--display requires an id".to_owned())?;
                options.display_id = Some(
                    value
                        .parse()
                        .map_err(|_| format!("--display expects a number, got '{value}'"))?,
                );
            }
            "--codec" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--codec requires a value".to_owned())?;
                options.codec = match value.as_str() {
                    "hevc" => EncoderCodec::Hevc,
                    "h264" => EncoderCodec::H264,
                    other => return Err(format!("--codec expects hevc or h264, got '{other}'")),
                };
            }
            "--format" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--format requires a value".to_owned())?;
                options.pixel_format = match value.as_str() {
                    "bgra8" => CapturePixelFormat::Bgra8,
                    "nv12" => CapturePixelFormat::Nv12VideoRange,
                    "nv12-10" => CapturePixelFormat::Nv12TenBitVideoRange,
                    "444-10" => CapturePixelFormat::FourFourFourTenBit,
                    other => {
                        return Err(format!(
                            "--format expects bgra8, nv12, nv12-10 or 444-10, got '{other}'"
                        ));
                    }
                };
            }
            "--motion" => options.motion = true,
            "--out" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--out requires a path".to_owned())?;
                options.out = Some(PathBuf::from(value));
            }
            "--dynamic-range" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| "--dynamic-range requires a value".to_owned())?;
                options.dynamic_range = match value.as_str() {
                    "sdr" => CaptureDynamicRange::Sdr,
                    "hdr-local" => CaptureDynamicRange::HdrLocalDisplay,
                    "hdr-canonical" => CaptureDynamicRange::HdrCanonicalDisplay,
                    other => {
                        return Err(format!(
                            "--dynamic-range expects sdr, hdr-local or hdr-canonical, got '{other}'"
                        ));
                    }
                };
            }
            other => return Err(format!("unknown probe-media argument: {other}")),
        }
        index += 1;
    }
    if options.frames == 0 {
        return Err("--frames must be at least 1".to_owned());
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_request_sdr_hevc() {
        let options = parse_options(&[]).expect("empty arguments are valid");
        assert_eq!(options.codec, EncoderCodec::Hevc);
        assert_eq!(options.dynamic_range, CaptureDynamicRange::Sdr);
        assert_eq!(options.pixel_format, CapturePixelFormat::Nv12VideoRange);
    }

    #[test]
    fn parses_every_supported_selector() {
        let arguments = [
            "--frames",
            "5",
            "--display",
            "7",
            "--codec",
            "h264",
            "--format",
            "444-10",
            "--dynamic-range",
            "hdr-local",
        ]
        .map(str::to_owned);
        let options = parse_options(&arguments).expect("valid arguments");
        assert_eq!(options.frames, 5);
        assert_eq!(options.display_id, Some(7));
        assert_eq!(options.codec, EncoderCodec::H264);
        assert_eq!(options.pixel_format, CapturePixelFormat::FourFourFourTenBit);
        assert_eq!(options.dynamic_range, CaptureDynamicRange::HdrLocalDisplay);
    }

    #[test]
    fn rejects_zero_frames_and_unknown_values() {
        assert!(parse_options(&["--frames".to_owned(), "0".to_owned()]).is_err());
        assert!(parse_options(&["--codec".to_owned(), "av1".to_owned()]).is_err());
        assert!(parse_options(&["--frames".to_owned()]).is_err());
        assert!(parse_options(&["--nope".to_owned()]).is_err());
    }
}
