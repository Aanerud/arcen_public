// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One `OpenH264` session for every software encode path.
//!
//! The Linux Xorg path (MIT-SHM capture), the Linux Wayland path (`PipeWire`
//! shared memory) and the Windows path (Windows Graphics Capture through a
//! staging texture) all turn eight-bit BGRA pictures into H.264 4:2:0 under
//! the shared Software contract. Everything between "here is a BGRA picture and
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

use arcen_keel::BgraFrame;

use super::pipeline::{PipelineId, pipeline_contract};
use super::{
    ColorTransform, ConversionError, EncodedAccessUnit, FrameLayoutError, I420Frame, I420FrameMut,
    KeyframePolicy, MAX_SOFTWARE_H264_FPS, MAX_SOFTWARE_H264_THREADS, SoftwareH264Config,
    SoftwareH264Content, SoftwareH264Encoder, SoftwareH264Error, SoftwareH264Threading,
    StageDamage, convert_bgra_to_i420_rows_parallel,
};
use crate::{
    BitDepth, ChromaSubsampling, DirtyRatio, RepresentativeFrameKind, VideoCodec,
    VideoConfiguration,
};

/// What a software session is opened for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareSessionConfig {
    /// Coded width (even, within `OpenH264`'s 3840x2160).
    pub width: u32,
    /// Coded height (even, within `OpenH264`'s 3840x2160).
    pub height: u32,
    /// Initial frame rate, 1 to [`MAX_SOFTWARE_H264_FPS`].
    pub fps: u32,
    /// The coded format: H.264 4:2:0 eight-bit, with the range, matrix,
    /// primaries and transfer the stream states and the conversion applies.
    pub video: VideoConfiguration,
    /// Encoder worker threads, 1 to 64; `OpenH264` encodes up to
    /// [`MAX_SOFTWARE_H264_THREADS`] fixed slices in parallel.
    pub threads: u16,
    /// What the pictures show (`OpenH264`'s usage mode).
    pub content: SoftwareH264Content,
    /// How live bitrate requests are held.
    pub rate: SoftwareRate,
}

/// How a session holds live bitrate requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SoftwareRate {
    /// A stream that owns the link: a quarter of the Software contract's
    /// start bitrate at least, its ceiling at most.
    #[default]
    Contract,
    /// A canvas surface sharing the link: the governor's grant as it is,
    /// from [`MIN_GRANTED_BPS`] up to the contract's ceiling.
    Granted,
}

/// The least bitrate a granted surface is held to: below it `OpenH264`'s
/// rate control cannot hold a picture of any size.
pub const MIN_GRANTED_BPS: u64 = 64_000;

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
            content: SoftwareH264Content::Screen,
            rate: SoftwareRate::Contract,
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
    rate: SoftwareRate,
    max_fps: u32,
    /// Threads BGRA to I420 conversion is spread over (the encoder's).
    convert_workers: usize,
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
    /// refuses (odd dimensions, beyond its 3840x2160 or 60 fps).
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
            content: config.content,
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
            rate: config.rate,
            max_fps: MAX_SOFTWARE_H264_FPS,
            convert_workers: usize::from(config.threads.max(1)),
        })
    }

    /// The worker threads a host offers `OpenH264`: its parallelism, held
    /// to the [`MAX_SOFTWARE_H264_THREADS`] slices `OpenH264` encodes in
    /// parallel.
    #[must_use]
    pub fn host_threads() -> u16 {
        std::thread::available_parallelism()
            .ok()
            .and_then(|count| u16::try_from(count.get()).ok())
            .map_or(1, |count| count.clamp(1, MAX_SOFTWARE_H264_THREADS))
    }

    /// The slices and worker threads `OpenH264` encodes with.
    #[must_use]
    pub const fn threading(&self) -> SoftwareH264Threading {
        self.encoder.threading()
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
        let (transform, workers) = (self.transform, self.convert_workers);
        let height = self.height as usize;
        let full = 0..height;
        let dirty: Vec<std::ops::Range<usize>>;
        let ranges = match damage {
            StageDamage::Clean => return Ok(()),
            StageDamage::Full => std::slice::from_ref(&full),
            StageDamage::Rows(map) => {
                dirty = map.dirty_block_rows().collect();
                &dirty
            }
        };
        let mut destination = self.planes_mut()?;
        convert_bgra_to_i420_rows_parallel(source, &mut destination, ranges, transform, workers)
            .map_err(SoftwareSessionError::Conversion)
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

    /// Fills the retained planes with black in the session's colour
    /// contract (Y 16 for limited range, 0 for full; Cb/Cr 128): the
    /// probe's starting picture, and what a host streams before its first
    /// capture arrives.
    pub fn stage_black(&mut self) {
        let coded = |code: i32| u8::try_from(code.clamp(0, 255)).unwrap_or(0);
        self.y.fill(coded(self.transform.luma(0, 0, 0)));
        self.u.fill(coded(self.transform.cb(0, 0, 0)));
        self.v.fill(coded(self.transform.cr(0, 0, 0)));
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

    /// A live frame-rate request (the host's governor or the link), held to
    /// `1..=`[`MAX_SOFTWARE_H264_FPS`]. `OpenH264` applies it without a recovery point, so no
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

    /// A live bitrate request, held as the session's [`SoftwareRate`]
    /// says.
    ///
    /// # Errors
    ///
    /// [`SoftwareSessionError::Encoder`] when `OpenH264` refuses it.
    pub fn reconfigure_bitrate(&mut self, bps: u64) -> Result<LiveChange, SoftwareSessionError> {
        let floor = match self.rate {
            SoftwareRate::Contract => u64::from(self.start_bps / 4),
            SoftwareRate::Granted => MIN_GRANTED_BPS,
        };
        let bps = bps.clamp(floor, u64::from(self.ceiling_bps).max(floor));
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
            content: SoftwareH264Content::Screen,
            rate: SoftwareRate::Contract,
        }
    }

    fn bgra(width: usize, height: usize, value: u8) -> Vec<u8> {
        vec![value; width * height * 4]
    }

    #[test]
    fn only_h264_420_eight_bit_within_the_contract_opens() {
        // No size of the contract's own: OpenH264's 3840x2160 bounds it.
        assert!(SoftwareH264Session::new(config(3840, 2160)).is_ok());
        assert!(matches!(
            SoftwareH264Session::new(config(3842, 2160)),
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
    fn a_granted_session_takes_the_grant_below_the_contracts_floor() {
        let mut session = SoftwareH264Session::new(SoftwareSessionConfig {
            rate: SoftwareRate::Granted,
            ..config(1280, 720)
        })
        .expect("session");
        assert!(u64::from(session.bitrate_bounds().0 / 4) > 300_000);
        assert_eq!(
            session.reconfigure_bitrate(300_000).expect("grant"),
            LiveChange::Applied(300_000)
        );
        assert_eq!(
            session.reconfigure_bitrate(1).expect("floor"),
            LiveChange::Applied(64_000)
        );
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
            session
                .reconfigure_framerate(120)
                .expect("held to OpenH264's 60"),
            LiveChange::Applied(60)
        );
        assert_eq!(
            session.reconfigure_framerate(0).expect("at least 1"),
            LiveChange::Applied(1)
        );
        assert!(SoftwareH264Session::host_threads() >= 1);
    }
}
