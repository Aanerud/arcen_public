use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io::{self, Write};

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, ColorPrimaries as Oh264ColorPrimaries, Encoder, EncoderConfig, FrameRate, FrameType,
    IntraFramePeriod, MatrixCoefficients as Oh264MatrixCoefficients, Profile, RateControlMode,
    TransferCharacteristics as Oh264TransferCharacteristics, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;

use crate::{ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};

use super::I420Frame;

/// Hard cap for one software H.264 Annex-B access unit.
pub const MAX_SOFTWARE_H264_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARAMETER_SET_BYTES: usize = 64 * 1024;
const MAX_SOFTWARE_WIDTH: u32 = 1920;
const MAX_SOFTWARE_HEIGHT: u32 = 1200;
const MAX_SOFTWARE_FPS: u32 = 30;
const OPENH264_I32_MAX: usize = 2_147_483_647;

/// Validated `OpenH264` stream configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareH264Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub num_threads: u16,
    /// Coded sample range this stream's VUI must state truthfully. Silently
    /// tagging full-range samples as limited (or vice versa) is the class of
    /// bug this colour workstream exists to remove.
    pub range: ColorRange,
    /// Matrix coefficients this stream's VUI must state truthfully.
    pub matrix: ColorMatrix,
    /// Colour primaries this stream's VUI must state truthfully.
    ///
    /// [`ColorPrimaries::DisplayP3`] has no equivalent in the `openh264`
    /// 0.9.7 `ColorPrimaries` enum (it defines values up to BT.2020 only, no
    /// SMPTE EG 432-1 / value 12 variant) and is rejected by
    /// [`SoftwareH264Encoder::new`] with
    /// [`SoftwareH264Error::UnsupportedColorPrimaries`] rather than
    /// approximated as something else.
    pub primaries: ColorPrimaries,
    /// Transfer characteristics this stream's VUI must state truthfully.
    pub transfer: TransferCharacteristics,
}

impl SoftwareH264Config {
    /// The BT.709 limited-range contract Arcen shipped before colour was
    /// negotiable, for the given geometry/bitrate/thread count.
    ///
    /// A drop-in bridge for callers migrating off the old hardcoded
    /// `VuiConfig::bt709()`: identical bitstream colour signalling, until the
    /// caller threads a real negotiated [`ColorRange`]/[`ColorMatrix`]/
    /// [`ColorPrimaries`]/[`TransferCharacteristics`] through instead.
    #[must_use]
    pub const fn legacy_bt709_limited(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        num_threads: u16,
    ) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_bps,
            num_threads,
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }
}

/// Privacy-safe software encoder failure.
#[derive(Debug)]
pub enum SoftwareH264Error {
    InvalidConfig,
    FrameGeometryMismatch,
    IncompatibleNativeFrameLayout,
    Native(openh264::Error),
    OutputTooLarge,
    OutputAllocation,
    InvalidFrameType,
    MissingParameterSets,
    /// The requested colour primaries have no `openh264` VUI equivalent —
    /// see [`SoftwareH264Config::primaries`].
    UnsupportedColorPrimaries,
}

impl Display for SoftwareH264Error {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConfig => "invalid software H.264 configuration",
            Self::FrameGeometryMismatch => "I420 frame geometry differs from encoder configuration",
            Self::IncompatibleNativeFrameLayout => {
                "I420 frame layout is incompatible with OpenH264"
            }
            Self::Native(_) => "OpenH264 reported an encoder failure",
            Self::OutputTooLarge => "software H.264 access unit exceeds the 16 MiB cap",
            Self::OutputAllocation => "software H.264 output allocation failed",
            Self::InvalidFrameType => "OpenH264 returned an invalid or mixed frame type",
            Self::MissingParameterSets => "recovery IDR does not contain cached SPS and PPS",
            Self::UnsupportedColorPrimaries => {
                "requested colour primaries have no OpenH264 VUI equivalent"
            }
        })
    }
}

impl Error for SoftwareH264Error {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Native(error) => Some(error),
            _ => None,
        }
    }
}

/// Encoded frame classification exposed without native types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedFrameKind {
    Idr,
    Intra,
    Predicted,
}

/// Borrowed encoded Annex-B access unit.
#[derive(Debug, Clone, Copy)]
pub struct EncodedAccessUnit<'a> {
    pub bytes: &'a [u8],
    pub kind: EncodedFrameKind,
    pub is_keyframe: bool,
}

/// Bounded Arcen-owned software encoder counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SoftwareH264Stats {
    pub encoded_frames: u64,
    pub skipped_frames: u64,
    pub emitted_bytes: u64,
    pub forced_idrs: u64,
    pub output_capacity_growths: u64,
    pub output_capacity: usize,
}

struct CappedWriter<'a> {
    output: &'a mut Vec<u8>,
}

impl Write for CappedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let new_len = self
            .output
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("access-unit length overflow"))?;
        if new_len > MAX_SOFTWARE_H264_ACCESS_UNIT_BYTES {
            return Err(io::Error::other("access-unit cap exceeded"));
        }
        self.output
            .try_reserve(bytes.len())
            .map_err(|_| io::Error::other("access-unit allocation failed"))?;
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Map the negotiated matrix to `openh264`'s `MatrixCoefficients`.
///
/// Every [`ColorMatrix`] value Arcen offers has a same-numbered ITU-T H.273
/// equivalent in this enum (including [`ColorMatrix::Identity`], which
/// `openh264` also represents as matrix coefficient 0), so this mapping
/// cannot fail.
const fn vui_matrix_coefficients(matrix: ColorMatrix) -> Oh264MatrixCoefficients {
    match matrix {
        ColorMatrix::Identity => Oh264MatrixCoefficients::Identity,
        ColorMatrix::Bt709 => Oh264MatrixCoefficients::Bt709,
        ColorMatrix::Bt601 => Oh264MatrixCoefficients::Smpte170M,
        ColorMatrix::Bt2020Ncl => Oh264MatrixCoefficients::Bt2020Ncl,
    }
}

/// Map the negotiated transfer characteristic to `openh264`'s
/// `TransferCharacteristics`. Every [`TransferCharacteristics`] value Arcen
/// offers, including the HDR ones, has a same-numbered equivalent here, so
/// this mapping cannot fail.
const fn vui_transfer_characteristics(
    transfer: TransferCharacteristics,
) -> Oh264TransferCharacteristics {
    match transfer {
        TransferCharacteristics::Bt709 => Oh264TransferCharacteristics::Bt709,
        TransferCharacteristics::Srgb => Oh264TransferCharacteristics::Srgb,
        TransferCharacteristics::Pq => Oh264TransferCharacteristics::Smpte2084,
        TransferCharacteristics::Hlg => Oh264TransferCharacteristics::Hlg,
    }
}

/// Map the negotiated primaries to `openh264`'s `ColorPrimaries`.
///
/// # Errors
///
/// [`ColorPrimaries::DisplayP3`] has no equivalent: `openh264` 0.9.7's
/// `ColorPrimaries` enum defines values up to BT.2020 (ITU-T H.273 value 9)
/// only and has no SMPTE EG 432-1 / value-12 variant, and the crate exposes
/// no raw/custom escape hatch to set an arbitrary VUI byte. Rather than
/// silently substituting BT.709 or BT.2020 — which would tag the bitstream
/// with primaries it was not actually encoded in — this is a hard error.
fn vui_color_primaries(
    primaries: ColorPrimaries,
) -> Result<Oh264ColorPrimaries, SoftwareH264Error> {
    match primaries {
        ColorPrimaries::Bt709 => Ok(Oh264ColorPrimaries::Bt709),
        ColorPrimaries::Bt2020 => Ok(Oh264ColorPrimaries::Bt2020),
        ColorPrimaries::DisplayP3 => Err(SoftwareH264Error::UnsupportedColorPrimaries),
    }
}

/// Build the `openh264` VUI block that makes this stream's SPS state the
/// negotiated colour contract truthfully, instead of the previous hardcoded
/// `VuiConfig::bt709()` limited-range default.
///
/// `openh264` 0.9.7's `VuiConfig` does expose a full-range flag
/// ([`VuiConfig::full_range`]) as well as typed setters for primaries,
/// transfer and matrix — read from the crate's own source rather than
/// assumed — so range is not a rejection case here, only
/// [`ColorPrimaries::DisplayP3`] is (see [`vui_color_primaries`]).
fn build_vui(
    range: ColorRange,
    matrix: ColorMatrix,
    primaries: ColorPrimaries,
    transfer: TransferCharacteristics,
) -> Result<VuiConfig, SoftwareH264Error> {
    Ok(VuiConfig::new()
        .color_primaries(vui_color_primaries(primaries)?)
        .transfer_characteristics(vui_transfer_characteristics(transfer))
        .matrix_coefficients(vui_matrix_coefficients(matrix))
        .full_range(matches!(range, ColorRange::Full)))
}

/// Attachment-scoped safe wrapper around source-built `OpenH264`.
///
/// This is the sole Arcen caller of `OpenH264`. The wrapper exposes no native
/// pointers, but the dependency underneath remains complex unsafe C/C++ and is
/// not made memory-safe by this API.
pub struct SoftwareH264Encoder {
    config: SoftwareH264Config,
    inner: Encoder,
    output: Vec<u8>,
    parameter_sets: Vec<u8>,
    force_pending: bool,
    stats: SoftwareH264Stats,
}

impl std::fmt::Debug for SoftwareH264Encoder {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SoftwareH264Encoder")
            .field("config", &self.config)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl SoftwareH264Encoder {
    /// Construct the Baseline, screen-content, bitrate-controlled encoder.
    ///
    /// # Errors
    ///
    /// Rejects geometry above 1920x1200p30, odd dimensions, zero bitrate, excessive
    /// thread counts, or native initialization failure. Also rejects
    /// [`ColorPrimaries::DisplayP3`] with
    /// [`SoftwareH264Error::UnsupportedColorPrimaries`], since `openh264`
    /// cannot signal it in the VUI — see [`build_vui`].
    #[allow(clippy::cast_precision_loss)]
    pub fn new(config: SoftwareH264Config) -> Result<Self, SoftwareH264Error> {
        if config.width == 0
            || config.height == 0
            || config.width % 2 != 0
            || config.height % 2 != 0
            || config.width > MAX_SOFTWARE_WIDTH
            || config.height > MAX_SOFTWARE_HEIGHT
            || config.fps == 0
            || config.fps > MAX_SOFTWARE_FPS
            || config.bitrate_bps == 0
            || config.num_threads == 0
            || config.num_threads > 64
        {
            return Err(SoftwareH264Error::InvalidConfig);
        }
        let vui = build_vui(
            config.range,
            config.matrix,
            config.primaries,
            config.transfer,
        )?;
        let native = EncoderConfig::new()
            .profile(Profile::Baseline)
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                config.fps.saturating_mul(2),
            ))
            .num_threads(config.num_threads)
            .adaptive_quantization(false)
            .background_detection(false)
            .vui(vui);
        let inner = Encoder::with_api_config(OpenH264API::from_source(), native)
            .map_err(SoftwareH264Error::Native)?;
        Ok(Self {
            config,
            inner,
            output: Vec::new(),
            parameter_sets: Vec::new(),
            force_pending: true,
            stats: SoftwareH264Stats::default(),
        })
    }

    /// Force a recovery IDR on the next submitted frame.
    pub fn force_idr(&mut self) {
        self.inner.force_intra_frame();
        self.force_pending = true;
    }

    #[must_use]
    pub const fn config(&self) -> SoftwareH264Config {
        self.config
    }

    #[must_use]
    pub const fn stats(&self) -> SoftwareH264Stats {
        self.stats
    }

    /// Encode one checked I420 frame into retained, bounded output storage.
    ///
    /// Skip is returned as `Ok(None)` rather than an empty frame. Invalid and
    /// mixed native output fail closed.
    ///
    /// # Errors
    ///
    /// Returns typed geometry, native, frame-type, cap, allocation, or recovery
    /// parameter-set failures.
    pub fn encode(
        &mut self,
        frame: I420Frame<'_>,
    ) -> Result<Option<EncodedAccessUnit<'_>>, SoftwareH264Error> {
        let strides = frame.strides();
        if strides.1 != strides.2
            || [
                frame.width(),
                frame.height(),
                strides.0,
                strides.1,
                strides.2,
            ]
            .into_iter()
            .any(|value| value > OPENH264_I32_MAX)
        {
            return Err(SoftwareH264Error::IncompatibleNativeFrameLayout);
        }
        if frame.width() != self.config.width as usize
            || frame.height() != self.config.height as usize
        {
            return Err(SoftwareH264Error::FrameGeometryMismatch);
        }
        let (y, u, v) = frame.planes();
        let yuv = YUVSlices::new((y, u, v), (frame.width(), frame.height()), strides);
        let bitstream = self.inner.encode(&yuv).map_err(SoftwareH264Error::Native)?;
        let kind = match bitstream.frame_type() {
            FrameType::IDR => EncodedFrameKind::Idr,
            FrameType::I => EncodedFrameKind::Intra,
            FrameType::P => EncodedFrameKind::Predicted,
            FrameType::Skip => {
                self.stats.skipped_frames = self.stats.skipped_frames.saturating_add(1);
                return Ok(None);
            }
            FrameType::Invalid | FrameType::IPMixed => {
                return Err(SoftwareH264Error::InvalidFrameType);
            }
        };
        self.output.clear();
        let old_capacity = self.output.capacity();
        bitstream
            .write(&mut CappedWriter {
                output: &mut self.output,
            })
            .map_err(|error| {
                if error.to_string().contains("cap") {
                    SoftwareH264Error::OutputTooLarge
                } else if error.to_string().contains("allocation") {
                    SoftwareH264Error::OutputAllocation
                } else {
                    SoftwareH264Error::Native(error)
                }
            })?;
        if self.output.capacity() > old_capacity {
            self.stats.output_capacity_growths =
                self.stats.output_capacity_growths.saturating_add(1);
        }
        if self.output.is_empty() || self.output.len() > MAX_SOFTWARE_H264_ACCESS_UNIT_BYTES {
            return Err(SoftwareH264Error::OutputTooLarge);
        }
        let is_idr = matches!(kind, EncodedFrameKind::Idr);
        self.capture_parameter_sets()?;
        if is_idr && !(contains_nal_type(&self.output, 7) && contains_nal_type(&self.output, 8)) {
            self.prepend_parameter_sets()?;
        }
        if self.force_pending && !is_idr {
            return Err(SoftwareH264Error::InvalidFrameType);
        }
        if self.force_pending {
            self.stats.forced_idrs = self.stats.forced_idrs.saturating_add(1);
            self.force_pending = false;
        }
        self.stats.encoded_frames = self.stats.encoded_frames.saturating_add(1);
        self.stats.emitted_bytes = self
            .stats
            .emitted_bytes
            .saturating_add(self.output.len() as u64);
        self.stats.output_capacity = self.output.capacity();
        Ok(Some(EncodedAccessUnit {
            bytes: &self.output,
            kind,
            is_keyframe: is_idr,
        }))
    }

    fn capture_parameter_sets(&mut self) -> Result<(), SoftwareH264Error> {
        if !contains_nal_type(&self.output, 7) || !contains_nal_type(&self.output, 8) {
            return Ok(());
        }
        self.parameter_sets.clear();
        for range in NalRanges::new(&self.output) {
            let header = nal_header(&self.output[range.clone()]);
            if matches!(header, Some(7 | 8)) {
                let bytes = &self.output[range];
                let new_len = self
                    .parameter_sets
                    .len()
                    .checked_add(bytes.len())
                    .ok_or(SoftwareH264Error::OutputTooLarge)?;
                if new_len > MAX_PARAMETER_SET_BYTES {
                    return Err(SoftwareH264Error::OutputTooLarge);
                }
                self.parameter_sets
                    .try_reserve(bytes.len())
                    .map_err(|_| SoftwareH264Error::OutputAllocation)?;
                self.parameter_sets.extend_from_slice(bytes);
            }
        }
        Ok(())
    }

    fn prepend_parameter_sets(&mut self) -> Result<(), SoftwareH264Error> {
        if !contains_nal_type(&self.parameter_sets, 7)
            || !contains_nal_type(&self.parameter_sets, 8)
        {
            return Err(SoftwareH264Error::MissingParameterSets);
        }
        let prefix_len = self.parameter_sets.len();
        let old_len = self.output.len();
        let new_len = old_len
            .checked_add(prefix_len)
            .ok_or(SoftwareH264Error::OutputTooLarge)?;
        if new_len > MAX_SOFTWARE_H264_ACCESS_UNIT_BYTES {
            return Err(SoftwareH264Error::OutputTooLarge);
        }
        self.output
            .try_reserve(prefix_len)
            .map_err(|_| SoftwareH264Error::OutputAllocation)?;
        self.output.resize(new_len, 0);
        self.output.copy_within(0..old_len, prefix_len);
        self.output[..prefix_len].copy_from_slice(&self.parameter_sets);
        Ok(())
    }
}

fn start_code_len(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index..index + 4) == Some(&[0, 0, 0, 1]) {
        Some(4)
    } else if bytes.get(index..index + 3) == Some(&[0, 0, 1]) {
        Some(3)
    } else {
        None
    }
}

fn find_start(bytes: &[u8], from: usize) -> Option<usize> {
    (from..bytes.len().saturating_sub(2)).find(|index| start_code_len(bytes, *index).is_some())
}

struct NalRanges<'a> {
    bytes: &'a [u8],
    next_start: Option<usize>,
}

impl<'a> NalRanges<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            next_start: find_start(bytes, 0),
        }
    }
}

impl Iterator for NalRanges<'_> {
    type Item = std::ops::Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        let start = self.next_start?;
        let search_from = start.saturating_add(start_code_len(self.bytes, start)?);
        let end = find_start(self.bytes, search_from).unwrap_or(self.bytes.len());
        self.next_start = (end < self.bytes.len()).then_some(end);
        Some(start..end)
    }
}

fn nal_header(bytes: &[u8]) -> Option<u8> {
    let prefix = start_code_len(bytes, 0)?;
    bytes.get(prefix).map(|value| value & 0x1f)
}

fn contains_nal_type(bytes: &[u8], expected: u8) -> bool {
    NalRanges::new(bytes).any(|range| nal_header(&bytes[range]) == Some(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn black_frame() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (vec![16; 64 * 64], vec![128; 32 * 32], vec![128; 32 * 32])
    }

    #[test]
    fn controlled_encode_is_annex_b_bounded_and_reuses_output() {
        let config = SoftwareH264Config::legacy_bt709_limited(64, 64, 30, 500_000, 1);
        let mut encoder = SoftwareH264Encoder::new(config).expect("encoder");
        let (y, u, v) = black_frame();
        let frame = I420Frame::new(64, 64, &y, 64, &u, 32, &v, 32).expect("frame");
        let first = encoder.encode(frame).expect("encode").expect("access unit");
        assert!(first.is_keyframe);
        assert!(contains_nal_type(first.bytes, 7));
        assert!(contains_nal_type(first.bytes, 8));
        assert!(contains_nal_type(first.bytes, 5));
        assert!(first.bytes.len() <= MAX_SOFTWARE_H264_ACCESS_UNIT_BYTES);
        let first_capacity = encoder.stats().output_capacity;

        let second = encoder.encode(frame).expect("encode").expect("access unit");
        assert!(matches!(
            second.kind,
            EncodedFrameKind::Predicted | EncodedFrameKind::Intra
        ));
        assert!(encoder.stats().output_capacity >= first_capacity);

        encoder.force_idr();
        let recovery = encoder
            .encode(frame)
            .expect("recovery encode")
            .expect("recovery access unit");
        assert!(recovery.is_keyframe);
        assert!(contains_nal_type(recovery.bytes, 7));
        assert!(contains_nal_type(recovery.bytes, 8));
        assert_eq!(encoder.stats().forced_idrs, 2);
    }

    #[test]
    fn invalid_config_and_frame_mismatch_fail_closed() {
        assert!(matches!(
            SoftwareH264Encoder::new(SoftwareH264Config::legacy_bt709_limited(
                1922, 1080, 30, 1, 1
            )),
            Err(SoftwareH264Error::InvalidConfig)
        ));
        let mut encoder = SoftwareH264Encoder::new(SoftwareH264Config::legacy_bt709_limited(
            64, 64, 30, 500_000, 1,
        ))
        .expect("encoder");
        let (y, u, v) = (vec![16; 32 * 32], vec![128; 16 * 16], vec![128; 16 * 16]);
        let frame = I420Frame::new(32, 32, &y, 32, &u, 16, &v, 16).expect("frame");
        assert!(matches!(
            encoder.encode(frame),
            Err(SoftwareH264Error::FrameGeometryMismatch)
        ));
    }

    #[test]
    fn incompatible_native_layout_fails_before_openh264() {
        let mut encoder = SoftwareH264Encoder::new(SoftwareH264Config::legacy_bt709_limited(
            64, 64, 30, 500_000, 1,
        ))
        .expect("encoder");
        let y = vec![16; 64 * 64];
        let u = vec![128; 32 * 32];
        let v = vec![128; 33 * 32];
        let unequal = I420Frame::new(64, 64, &y, 64, &u, 32, &v, 33).expect("generic I420");
        assert!(matches!(
            encoder.encode(unequal),
            Err(SoftwareH264Error::IncompatibleNativeFrameLayout)
        ));

        let oversized_stride = I420Frame {
            width: 64,
            height: 64,
            y_stride: OPENH264_I32_MAX + 1,
            u_stride: 32,
            v_stride: 32,
            y: &y,
            u: &u,
            v: &v,
        };
        assert!(matches!(
            encoder.encode(oversized_stride),
            Err(SoftwareH264Error::IncompatibleNativeFrameLayout)
        ));

        for oversized_geometry in [
            I420Frame {
                width: OPENH264_I32_MAX + 1,
                height: 64,
                y_stride: 64,
                u_stride: 32,
                v_stride: 32,
                y: &y,
                u: &u,
                v: &v,
            },
            I420Frame {
                width: 64,
                height: OPENH264_I32_MAX + 1,
                y_stride: 64,
                u_stride: 32,
                v_stride: 32,
                y: &y,
                u: &u,
                v: &v,
            },
        ] {
            assert!(matches!(
                encoder.encode(oversized_geometry),
                Err(SoftwareH264Error::IncompatibleNativeFrameLayout)
            ));
        }
    }

    #[test]
    fn vui_matrix_coefficients_matches_h273_for_every_matrix() {
        for matrix in [
            ColorMatrix::Identity,
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
        ] {
            assert_eq!(
                vui_matrix_coefficients(matrix).as_u8(),
                matrix.h273_value(),
                "matrix {matrix:?} must round-trip its H.273 value through openh264"
            );
        }
    }

    #[test]
    fn vui_transfer_characteristics_matches_h273_for_every_value() {
        for transfer in [
            TransferCharacteristics::Bt709,
            TransferCharacteristics::Srgb,
            TransferCharacteristics::Pq,
            TransferCharacteristics::Hlg,
        ] {
            assert_eq!(
                vui_transfer_characteristics(transfer).as_u8(),
                transfer.h273_value(),
                "transfer {transfer:?} must round-trip its H.273 value through openh264"
            );
        }
    }

    #[test]
    fn vui_color_primaries_maps_bt709_and_bt2020_and_rejects_display_p3() {
        assert_eq!(
            vui_color_primaries(ColorPrimaries::Bt709)
                .expect("bt709 is mappable")
                .as_u8(),
            ColorPrimaries::Bt709.h273_value()
        );
        assert_eq!(
            vui_color_primaries(ColorPrimaries::Bt2020)
                .expect("bt2020 is mappable")
                .as_u8(),
            ColorPrimaries::Bt2020.h273_value()
        );
        assert!(matches!(
            vui_color_primaries(ColorPrimaries::DisplayP3),
            Err(SoftwareH264Error::UnsupportedColorPrimaries)
        ));
    }

    #[test]
    fn build_vui_matches_the_bt709_limited_and_full_presets() {
        let limited = build_vui(
            ColorRange::Limited,
            ColorMatrix::Bt709,
            ColorPrimaries::Bt709,
            TransferCharacteristics::Bt709,
        )
        .expect("bt709 limited is mappable");
        assert_eq!(limited, VuiConfig::bt709());

        let full = build_vui(
            ColorRange::Full,
            ColorMatrix::Bt709,
            ColorPrimaries::Bt709,
            TransferCharacteristics::Bt709,
        )
        .expect("bt709 full is mappable");
        assert_eq!(full, VuiConfig::bt709_full());
        assert_ne!(
            limited, full,
            "range must actually change the emitted VUI, not just be ignored"
        );
    }

    #[test]
    fn build_vui_reflects_matrix_primaries_and_transfer_together() {
        let vui = build_vui(
            ColorRange::Limited,
            ColorMatrix::Bt601,
            ColorPrimaries::Bt2020,
            TransferCharacteristics::Hlg,
        )
        .expect("bt601/bt2020/hlg is a mappable, if unusual, combination");
        let expected = VuiConfig::new()
            .color_primaries(Oh264ColorPrimaries::Bt2020)
            .transfer_characteristics(Oh264TransferCharacteristics::Hlg)
            .matrix_coefficients(Oh264MatrixCoefficients::Smpte170M)
            .full_range(false);
        assert_eq!(vui, expected);
    }

    #[test]
    fn build_vui_rejects_display_p3_rather_than_substituting_another_gamut() {
        let error = build_vui(
            ColorRange::Full,
            ColorMatrix::Bt709,
            ColorPrimaries::DisplayP3,
            TransferCharacteristics::Bt709,
        )
        .expect_err("DisplayP3 has no openh264 VUI equivalent");
        assert!(matches!(
            error,
            SoftwareH264Error::UnsupportedColorPrimaries
        ));
    }

    #[test]
    fn encoder_new_rejects_display_p3_before_touching_native_openh264() {
        // build_vui runs before EncoderConfig/Encoder::with_api_config, so
        // this must fail even though DisplayP3 is otherwise a perfectly
        // constructible ColorPrimaries value.
        let config = SoftwareH264Config {
            primaries: ColorPrimaries::DisplayP3,
            ..SoftwareH264Config::legacy_bt709_limited(64, 64, 30, 500_000, 1)
        };
        assert!(matches!(
            SoftwareH264Encoder::new(config),
            Err(SoftwareH264Error::UnsupportedColorPrimaries)
        ));
    }

    #[test]
    fn legacy_bt709_limited_constructs_the_pre_negotiation_contract() {
        let config = SoftwareH264Config::legacy_bt709_limited(1920, 1080, 30, 6_000_000, 4);
        assert_eq!(config.width, 1920);
        assert_eq!(config.height, 1080);
        assert_eq!(config.fps, 30);
        assert_eq!(config.bitrate_bps, 6_000_000);
        assert_eq!(config.num_threads, 4);
        assert_eq!(config.range, ColorRange::Limited);
        assert_eq!(config.matrix, ColorMatrix::Bt709);
        assert_eq!(config.primaries, ColorPrimaries::Bt709);
        assert_eq!(config.transfer, TransferCharacteristics::Bt709);
    }
}
