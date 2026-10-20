//! One `OpenH264` session for every Linux software encode path.
//!
//! The Xorg path (MIT-SHM capture) and the Wayland path (`PipeWire` shared
//! memory) both turn eight-bit BGRA pictures into H.264 4:2:0 under the
//! shared Software contract. Everything between "here is a BGRA picture and
//! what changed" and "here is an access unit" lives here, once: the
//! contract's bitrate bounds, the retained I420 planes, damage-driven
//! selective conversion, keepalive and keyframe re-encodes of the retained
//! picture, and the live frame-rate and bitrate rules. The platform loops own
//! only capture, the activity scheduler and the wire.
//!
//! The retained planes are the encoder's reference picture: a picture that
//! did not change is never converted again, and a keepalive or a requested
//! keyframe re-encodes exactly what was last staged.

use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_keel::{BgraFrame, DamageMap};

use super::pipeline::{PipelineId, pipeline_contract};
use super::{
    ColorTransform, ConversionError, EncodedAccessUnit, FrameLayoutError, I420Frame, I420FrameMut,
    KeyframePolicy, SoftwareH264Config, SoftwareH264Encoder, SoftwareH264Error,
    convert_bgra_to_i420, convert_bgra_to_i420_rows,
};
use crate::{
    BitDepth, ChromaSubsampling, DirtyRatio, RepresentativeFrameKind, VideoCodec,
    VideoConfiguration,
};

/// What a software session is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareSessionConfig {
    /// Coded width (even, at most the Software contract's).
    pub width: u32,
    /// Coded height (even, at most the Software contract's).
    pub height: u32,
    /// Initial frame-rate ceiling, 1 to the Software contract's.
    pub fps: u32,
    /// The coded format: H.264 4:2:0 eight-bit, with the range, matrix,
    /// primaries and transfer the stream states and the conversion applies.
    pub video: VideoConfiguration,
    /// Encoder worker threads, 1 to 64.
    pub threads: u16,
}

impl SoftwareSessionConfig {
    /// A stream of `video` at `width`x`height` and `fps`, with the host's
    /// worker threads.
    #[must_use]
    pub fn for_stream(width: u32, height: u32, fps: u32, video: VideoConfiguration) -> Self {
        Self {
            width,
            height,
            fps,
            video,
            threads: SoftwareH264Session::host_threads(),
        }
    }
}

/// Why a software session could not open or stage a picture.
#[derive(Debug)]
pub enum SoftwareSessionError {
    /// The coded format is not H.264 4:2:0 eight-bit.
    UnsupportedFormat,
    /// The retained planes could not be allocated or described.
    Allocation(&'static str),
    /// The I420 layout was refused.
    Layout(FrameLayoutError),
    /// BGRA to I420 conversion failed.
    Conversion(ConversionError),
    /// `OpenH264` refused the configuration or failed.
    Encoder(SoftwareH264Error),
}

impl Display for SoftwareSessionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat => {
                formatter.write_str("software encode serves H.264 4:2:0 eight-bit only")
            }
            Self::Allocation(what) => write!(formatter, "allocate {what}"),
            Self::Layout(error) => write!(formatter, "I420 layout: {error}"),
            Self::Conversion(error) => write!(formatter, "convert BGRA to I420: {error}"),
            Self::Encoder(error) => write!(formatter, "OpenH264: {error}"),
        }
    }
}

impl Error for SoftwareSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Layout(error) => Some(error),
            Self::Conversion(error) => Some(error),
            Self::Encoder(error) => Some(error),
            Self::UnsupportedFormat | Self::Allocation(_) => None,
        }
    }
}

/// What changed in a staged picture.
#[derive(Debug, Clone, Copy)]
pub enum StageDamage<'a> {
    /// Rebuild every plane (first picture, keyframe, full damage, or no map).
    Full,
    /// Rebuild only the 16x16 block rows the map marks dirty.
    Rows(DamageMap<'a>),
    /// Nothing changed; the retained planes stay as they are.
    Clean,
}

/// The result of a live rate change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveChange {
    /// Applied; the new value.
    Applied(u32),
    /// Already in effect after clamping; nothing changed.
    Unchanged,
}

/// Retained I420 planes, encoder and the Software contract's rate rules.
#[derive(Debug)]
pub struct SoftwareH264Session {
    encoder: SoftwareH264Encoder,
    transform: ColorTransform,
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    start_bps: u32,
    ceiling_bps: u32,
    max_fps: u32,
}

/// The Software contract's start and ceiling bitrate for this stream.
#[must_use]
pub fn software_bitrate_bounds(
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
) -> (u32, u32) {
    pipeline_contract(PipelineId::Software).bitrate_bounds(width, height, fps, chroma, depth)
}

fn zeroed(len: usize, what: &'static str) -> Result<Vec<u8>, SoftwareSessionError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| SoftwareSessionError::Allocation(what))?;
    bytes.resize(len, 0);
    Ok(bytes)
}

impl SoftwareH264Session {
    /// Opens `OpenH264` at the contract's start bitrate and allocates the
    /// retained planes (black until the first picture is staged).
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError`] for a format other than H.264 4:2:0
    /// eight-bit, an allocation failure, or a configuration `OpenH264`
    /// refuses (geometry above 1920x1200, odd dimensions, more than 30 fps).
    pub fn new(config: SoftwareSessionConfig) -> Result<Self, SoftwareSessionError> {
        let video = config.video;
        if video.codec != VideoCodec::H264
            || video.chroma != ChromaSubsampling::Yuv420
            || video.bit_depth != BitDepth::Eight
        {
            return Err(SoftwareSessionError::UnsupportedFormat);
        }
        let (start_bps, ceiling_bps) = software_bitrate_bounds(
            config.width,
            config.height,
            config.fps,
            video.chroma,
            video.bit_depth,
        );
        let encoder = SoftwareH264Encoder::new(SoftwareH264Config {
            width: config.width,
            height: config.height,
            fps: config.fps,
            bitrate_bps: start_bps,
            num_threads: config.threads,
            range: video.range,
            matrix: video.matrix,
            primaries: video.primaries,
            transfer: video.transfer,
            keyframe: KeyframePolicy::SOFTWARE_FALLBACK,
        })
        .map_err(SoftwareSessionError::Encoder)?;
        let width = config.width as usize;
        let height = config.height as usize;
        let luma = width
            .checked_mul(height)
            .ok_or(SoftwareSessionError::Allocation("I420 luma geometry"))?;
        let chroma = luma / 4;
        Ok(Self {
            encoder,
            transform: ColorTransform::new(video.matrix, video.range, video.bit_depth),
            width: config.width,
            height: config.height,
            y: zeroed(luma, "I420 luma plane")?,
            u: zeroed(chroma, "I420 Cb plane")?,
            v: zeroed(chroma, "I420 Cr plane")?,
            start_bps,
            ceiling_bps,
            max_fps: pipeline_contract(PipelineId::Software).max_fps,
        })
    }

    /// The worker threads a host offers `OpenH264`: its parallelism, 1 to 64.
    #[must_use]
    pub fn host_threads() -> u16 {
        std::thread::available_parallelism()
            .map(|count| count.get().min(64))
            .ok()
            .and_then(|count| u16::try_from(count).ok())
            .unwrap_or(1)
    }

    /// Coded width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Coded height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The contract's start and ceiling bitrate for this session.
    #[must_use]
    pub const fn bitrate_bounds(&self) -> (u32, u32) {
        (self.start_bps, self.ceiling_bps)
    }

    /// The encoder's current configuration.
    #[must_use]
    pub const fn encoder_config(&self) -> SoftwareH264Config {
        self.encoder.config()
    }

    /// Forces a keyframe on the next encode.
    pub fn force_idr(&mut self) {
        self.encoder.force_idr();
    }

    /// Converts `source` into the retained planes, entirely or only where
    /// `damage` says it changed.
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError::Layout`] or [`SoftwareSessionError::Conversion`];
    /// the retained planes may then be partly written and must be staged
    /// in full before the next encode.
    pub fn stage(
        &mut self,
        source: BgraFrame<'_>,
        damage: StageDamage<'_>,
    ) -> Result<(), SoftwareSessionError> {
        let transform = self.transform;
        let mut destination = self.planes_mut()?;
        match damage {
            StageDamage::Clean => Ok(()),
            StageDamage::Full => convert_bgra_to_i420(source, &mut destination, transform)
                .map_err(SoftwareSessionError::Conversion),
            StageDamage::Rows(map) => {
                for rows in map.dirty_block_rows() {
                    convert_bgra_to_i420_rows(source, &mut destination, rows, transform)
                        .map_err(SoftwareSessionError::Conversion)?;
                }
                Ok(())
            }
        }
    }

    /// Writes a representative encoder-admission picture into the retained
    /// planes: `dirty_ratio` of the luma plane (at least one sample) set to
    /// `counter`, and every plane for full motion. No capture is involved, so every capture path probes
    /// its software encoder with the identical content.
    pub fn stage_probe_picture(
        &mut self,
        kind: RepresentativeFrameKind,
        dirty_ratio: DirtyRatio,
        counter: u8,
    ) {
        let full = kind == RepresentativeFrameKind::FullMotion;
        let changed_luma = if full {
            self.y.len()
        } else {
            self.y
                .len()
                .saturating_mul(usize::from(dirty_ratio.basis_points()))
                .div_ceil(10_000)
                .max(1)
        };
        self.y[..changed_luma].fill(counter);
        if full {
            self.u.fill(counter.wrapping_add(64));
            self.v.fill(counter.wrapping_add(128));
        }
    }

    /// Fills the retained planes with BT.709 limited-range black (Y 16,
    /// Cb/Cr 128), the probe's starting picture.
    pub fn stage_black(&mut self) {
        self.y.fill(16);
        self.u.fill(128);
        self.v.fill(128);
    }

    fn planes_mut(&mut self) -> Result<I420FrameMut<'_>, SoftwareSessionError> {
        let width = self.width as usize;
        I420FrameMut::new(
            self.width,
            self.height,
            &mut self.y,
            width,
            &mut self.u,
            width / 2,
            &mut self.v,
            width / 2,
        )
        .map_err(SoftwareSessionError::Layout)
    }

    /// Encodes the retained picture. `Ok(None)` is `OpenH264`'s skip.
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError::Layout`] or [`SoftwareSessionError::Encoder`].
    pub fn encode(&mut self) -> Result<Option<EncodedAccessUnit<'_>>, SoftwareSessionError> {
        let width = self.width as usize;
        let frame = I420Frame::new(
            self.width,
            self.height,
            &self.y,
            width,
            &self.u,
            width / 2,
            &self.v,
            width / 2,
        )
        .map_err(SoftwareSessionError::Layout)?;
        self.encoder
            .encode(frame)
            .map_err(SoftwareSessionError::Encoder)
    }

    /// A live frame-rate request, held to `1..=` the Software contract's
    /// ceiling. `OpenH264` applies it without a recovery point, so no
    /// keyframe is forced (a forced IDR here made cadence changes visible as
    /// keyframe pops).
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError::Encoder`] when `OpenH264` refuses it.
    pub fn reconfigure_framerate(&mut self, fps: u32) -> Result<LiveChange, SoftwareSessionError> {
        let fps = fps.clamp(1, self.max_fps);
        if fps == self.encoder.config().fps {
            return Ok(LiveChange::Unchanged);
        }
        self.encoder
            .reconfigure_framerate(fps)
            .map_err(SoftwareSessionError::Encoder)?;
        Ok(LiveChange::Applied(fps))
    }

    /// A live bitrate request, held to a quarter of the contract's start
    /// bitrate at least and its ceiling at most.
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError::Encoder`] when `OpenH264` refuses it.
    pub fn reconfigure_bitrate(&mut self, bps: u64) -> Result<LiveChange, SoftwareSessionError> {
        let bps = bps.clamp(u64::from(self.start_bps / 4), u64::from(self.ceiling_bps));
        let bps = u32::try_from(bps).unwrap_or(u32::MAX);
        if bps == self.encoder.config().bitrate_bps {
            return Ok(LiveChange::Unchanged);
        }
        self.encoder
            .reconfigure_bitrate(bps)
            .map_err(SoftwareSessionError::Encoder)?;
        Ok(LiveChange::Applied(bps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::EncodedFrameKind;
    use crate::{ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};

    const VIDEO: VideoConfiguration = VideoConfiguration {
        codec: VideoCodec::H264,
        chroma: ChromaSubsampling::Yuv420,
        bit_depth: BitDepth::Eight,
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
    };

    fn config(width: u32, height: u32) -> SoftwareSessionConfig {
        SoftwareSessionConfig {
            width,
            height,
            fps: 30,
            video: VIDEO,
            threads: 1,
        }
    }

    fn bgra(width: usize, height: usize, value: u8) -> Vec<u8> {
        vec![value; width * height * 4]
    }

    #[test]
    fn only_h264_420_eight_bit_within_the_contract_opens() {
        assert!(SoftwareH264Session::new(config(1920, 1200)).is_ok());
        assert!(matches!(
            SoftwareH264Session::new(config(1922, 1080)),
            Err(SoftwareSessionError::Encoder(_))
        ));
        for video in [
            VideoConfiguration {
                codec: VideoCodec::H265,
                ..VIDEO
            },
            VideoConfiguration {
                chroma: ChromaSubsampling::Yuv444,
                ..VIDEO
            },
            VideoConfiguration {
                bit_depth: BitDepth::Ten,
                ..VIDEO
            },
        ] {
            assert!(matches!(
                SoftwareH264Session::new(SoftwareSessionConfig {
                    video,
                    ..config(64, 64)
                }),
                Err(SoftwareSessionError::UnsupportedFormat)
            ));
        }
    }

    #[test]
    fn the_first_encode_is_a_keyframe_and_the_retained_picture_reencodes() {
        let mut session = SoftwareH264Session::new(config(64, 64)).expect("session");
        let pixels = bgra(64, 64, 0x80);
        let frame = BgraFrame::new(&pixels, 64, 64, 64 * 4).expect("frame");
        session.stage(frame, StageDamage::Full).expect("stage");
        let first = session.encode().expect("encode").expect("access unit");
        assert!(first.is_keyframe);
        // A keepalive re-encodes the retained planes without new pixels.
        let _ = session.encode().expect("keepalive");
        session.force_idr();
        let forced = session.encode().expect("forced").expect("access unit");
        assert_eq!(forced.kind, EncodedFrameKind::Idr);
    }

    #[test]
    fn row_damage_converts_only_dirty_rows_and_matches_a_full_conversion() {
        let (width, height) = (64usize, 64usize);
        let mut tracker =
            arcen_keel::DamageTracker::new(width, height, arcen_keel::KernelPreference::Auto)
                .expect("damage tracker");
        let before = bgra(width, height, 0x20);
        let mut after = before.clone();
        // Change one block row (rows 16..32).
        for row in 16..32 {
            let start = row * width * 4;
            after[start..start + width * 4].fill(0xC0);
        }
        let mut selective = SoftwareH264Session::new(config(64, 64)).expect("session");
        let mut full = SoftwareH264Session::new(config(64, 64)).expect("session");
        let first = BgraFrame::new(&before, width, height, width * 4).expect("frame");
        let _ = tracker.update(first).expect("baseline");
        selective.stage(first, StageDamage::Full).expect("stage");
        let second = BgraFrame::new(&after, width, height, width * 4).expect("frame");
        let summary = tracker.update(second).expect("update");
        assert!(!summary.is_clean() && !summary.is_full_damage());
        selective
            .stage(second, StageDamage::Rows(tracker.damage_map()))
            .expect("rows");
        full.stage(second, StageDamage::Full).expect("full");
        assert_eq!(selective.y, full.y);
        assert_eq!(selective.u, full.u);
        assert_eq!(selective.v, full.v);
        // Clean leaves the planes untouched.
        let retained = selective.y.clone();
        let black = bgra(width, height, 0);
        selective
            .stage(
                BgraFrame::new(&black, width, height, width * 4).expect("frame"),
                StageDamage::Clean,
            )
            .expect("clean");
        assert_eq!(selective.y, retained);
    }

    #[test]
    fn probe_pictures_change_the_requested_share_of_luma() {
        let mut session = SoftwareH264Session::new(config(64, 64)).expect("session");
        session.stage_black();
        assert!(session.y.iter().all(|value| *value == 16));
        session.stage_probe_picture(
            RepresentativeFrameKind::Sparse,
            DirtyRatio::from_basis_points(2_500).expect("ratio"),
            7,
        );
        assert_eq!(
            session.y.iter().filter(|value| **value == 7).count(),
            64 * 64 / 4
        );
        assert!(session.u.iter().all(|value| *value == 128));
        session.stage_probe_picture(RepresentativeFrameKind::FullMotion, DirtyRatio::FULL, 9);
        assert!(session.y.iter().all(|value| *value == 9));
        assert!(session.u.iter().all(|value| *value == 9 + 64));
        assert!(session.v.iter().all(|value| *value == 9 + 128));
        assert!(session.encode().expect("encode").is_some());
    }

    #[test]
    fn live_rates_follow_the_software_contract() {
        let mut session = SoftwareH264Session::new(config(1280, 720)).expect("session");
        let (start, ceiling) = session.bitrate_bounds();
        assert_eq!(
            (start, ceiling),
            software_bitrate_bounds(1280, 720, 30, ChromaSubsampling::Yuv420, BitDepth::Eight)
        );
        assert_eq!(session.encoder_config().bitrate_bps, start);
        assert_eq!(
            session.reconfigure_bitrate(u64::MAX).expect("ceiling"),
            LiveChange::Applied(ceiling)
        );
        assert_eq!(
            session.reconfigure_bitrate(u64::MAX).expect("same"),
            LiveChange::Unchanged
        );
        assert_eq!(
            session.reconfigure_bitrate(0).expect("floor"),
            LiveChange::Applied(start / 4)
        );
        assert_eq!(
            session.reconfigure_framerate(120).expect("held to 30"),
            LiveChange::Unchanged
        );
        assert_eq!(
            session.reconfigure_framerate(0).expect("at least 1"),
            LiveChange::Applied(1)
        );
        assert!(SoftwareH264Session::host_threads() >= 1);
    }
}
