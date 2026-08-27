//! Translating Keel's damage grid into codec-specific QP delta maps.
//!
//! Keel produces a uniform 16x16 damage grid ([`arcen_keel::BLOCK_SIZE`]).
//! NVENC accepts a per-frame signed QP delta per coding block, but *its* block
//! is codec-specific: a 16x16 macroblock for H.264, a 32x32 CTB for HEVC, a
//! 64x64 superblock for AV1. This module is the geometry translation between
//! the two, and nothing else.
//!
//! It deliberately takes no dependency on `arcen-keel`. The input is a plain
//! "is this 16x16 block dirty" predicate plus the grid dimensions, which keeps
//! this pure and exhaustively testable, and leaves the bridge in the one crate
//! that already owns both halves (`arcen-capenc`).
//!
//! # What this can and cannot buy
//!
//! The intuitive story — "spend fewer bits on the static parts of a mostly
//! static VFX interface" — is only half right, and the wrong half is the
//! expensive one to learn late.
//!
//! An unchanged region is already nearly free. Inter prediction codes it as
//! skip with no residual, so raising its QP saves very little: there are
//! almost no bits there to save. Pushing QP up on clean blocks mostly buys
//! risk, because the occasional clean-looking block that *does* carry residual
//! gets coded badly and then persists as a reference for every frame after it.
//!
//! The real gain is the other direction: spending the bit budget *down* on the
//! blocks that genuinely changed, so moving text and UI edges get more bits
//! than a uniform-QP encode would give them. That is why [`QpBias::default`]
//! is strongly asymmetric — a firm negative delta for dirty blocks and only a
//! token positive one for clean.
//!
//! Whether this beats a plain uniform-QP encode on real desktop content is an
//! open, measurable question. It is not self-evidently a win, and the honest
//! benchmark is bandwidth at matched text sharpness, not bandwidth alone.

use serde::{Deserialize, Serialize};

use crate::VideoCodec;

/// What a session wants done with QP delta maps.
///
/// Three states rather than a bool because the interesting measurement needs
/// a control arm: [`Self::Neutral`] exercises the entire map path — build,
/// size check, submission — while asserting nothing about the picture, so a
/// benchmark against [`Self::Off`] isolates the cost of carrying a map from
/// the effect of its contents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QpMapPolicy {
    /// No map is built or submitted. The shipped behaviour.
    #[default]
    Off,
    /// Build and submit a map biased by damage.
    On,
    /// Build and submit an all-zero map. The control arm.
    Neutral,
}

impl QpMapPolicy {
    /// Every policy in the vocabulary.
    pub const ALL: &'static [Self] = &[Self::Off, Self::On, Self::Neutral];

    /// Stable wire and argv token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Neutral => "neutral",
        }
    }

    /// Parse a token; unknown values are `None`.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.token() == value)
    }

    /// Whether any map at all is submitted under this policy.
    #[must_use]
    pub const fn submits_map(self) -> bool {
        matches!(self, Self::On | Self::Neutral)
    }
}

/// The coding-block geometry NVENC expects a QP map to be expressed in.
///
/// These sizes are **NVENC's QP-map granularity**, which is not always the
/// codec's own block size — HEVC allows 16/32/64 CTBs but NVENC's delta map is
/// addressed in 32x32 units. Because a mismatch here silently misaligns every
/// entry rather than failing, the encoder must cross-check the entry count it
/// computes against the size NVENC reports before trusting a map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpMapGeometry {
    /// H.264 macroblock: 16x16, exactly one Keel block.
    Macroblock16,
    /// HEVC coding tree block as NVENC addresses it: 32x32, 2x2 Keel blocks.
    Ctb32,
    /// AV1 superblock: 64x64, 4x4 Keel blocks.
    Superblock64,
}

impl QpMapGeometry {
    /// The QP-map geometry for `codec`, or `None` where Arcen drives no
    /// QP-map-capable encoder.
    #[must_use]
    pub const fn for_codec(codec: VideoCodec) -> Option<Self> {
        match codec {
            VideoCodec::H264 => Some(Self::Macroblock16),
            VideoCodec::H265 => Some(Self::Ctb32),
            VideoCodec::Av1 => Some(Self::Superblock64),
            // VP9 and JPEG have no NVENC QP-map path in this codebase.
            VideoCodec::Vp9 | VideoCodec::Jpeg => None,
        }
    }

    /// Block edge length in pixels.
    #[must_use]
    pub const fn block_size(self) -> u32 {
        match self {
            Self::Macroblock16 => 16,
            Self::Ctb32 => 32,
            Self::Superblock64 => 64,
        }
    }

    /// How many Keel blocks span one coding block on each axis.
    ///
    /// Always exact: every geometry is a power-of-two multiple of Keel's 16.
    #[must_use]
    pub const fn keel_blocks_per_side(self) -> u32 {
        self.block_size() / KEEL_BLOCK_SIZE
    }

    /// Map dimensions in coding blocks for a frame of `width` x `height`.
    #[must_use]
    pub const fn dimensions(self, width: u32, height: u32) -> (u32, u32) {
        let size = self.block_size();
        (width.div_ceil(size), height.div_ceil(size))
    }

    /// Number of entries NVENC expects for a frame of `width` x `height`.
    #[must_use]
    pub const fn entry_count(self, width: u32, height: u32) -> usize {
        let (cols, rows) = self.dimensions(width, height);
        cols as usize * rows as usize
    }
}

/// Keel's damage block edge length, mirrored so this module stays free of a
/// dependency on `arcen-keel`. A mismatch is caught by
/// `keel_block_size_matches_arcen_keel` in the capenc bridge.
pub const KEEL_BLOCK_SIZE: u32 = 16;

/// How far to push QP for changed and unchanged regions.
///
/// Units are codec QP steps. Negative spends more bits and raises quality;
/// positive spends fewer and lowers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QpBias {
    /// Applied to coding blocks overlapping any changed Keel block.
    pub dirty: i8,
    /// Applied to coding blocks where nothing changed.
    pub clean: i8,
}

impl QpBias {
    /// Neutral: every block keeps the rate controller's chosen QP.
    ///
    /// The correct control for any measurement of this feature — it exercises
    /// the whole map path while asserting nothing about content.
    pub const NEUTRAL: Self = Self { dirty: 0, clean: 0 };

    /// Clamp both terms into a range that cannot destroy an image.
    ///
    /// A delta map is applied on top of whatever QP the rate controller
    /// picked, so a large magnitude here can drive a block to either extreme
    /// of the QP range regardless of bitrate. Bounded well inside what NVENC
    /// would accept, on purpose: this is a bias, not an override.
    #[must_use]
    pub const fn clamped(self) -> Self {
        Self {
            dirty: clamp_delta(self.dirty),
            clean: clamp_delta(self.clean),
        }
    }
}

impl Default for QpBias {
    /// Deliberately asymmetric: spend real bits on change, barely tax stillness.
    ///
    /// See the module docs — an unchanged block is already coded as skip with
    /// almost no residual, so a large positive `clean` would buy very little
    /// bitrate while risking persistent damage to the occasional clean block
    /// that does carry residual.
    fn default() -> Self {
        Self {
            dirty: -4,
            clean: 1,
        }
    }
}

/// Widest delta this module will emit in either direction.
pub const MAX_ABS_QP_DELTA: i8 = 10;

const fn clamp_delta(value: i8) -> i8 {
    if value > MAX_ABS_QP_DELTA {
        MAX_ABS_QP_DELTA
    } else if value < -MAX_ABS_QP_DELTA {
        -MAX_ABS_QP_DELTA
    } else {
        value
    }
}

/// Why a QP map could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QpMapError {
    /// The codec has no QP-map geometry in this codebase.
    UnsupportedCodec(VideoCodec),
    /// Frame geometry was degenerate.
    EmptyFrame { width: u32, height: u32 },
    /// The supplied Keel grid does not cover the frame it claims to describe.
    ///
    /// Carries both so the mismatch is diagnosable from the message alone;
    /// silently trusting either side would misalign every entry in the map.
    GridMismatch {
        expected_cols: u32,
        expected_rows: u32,
        actual_cols: u32,
        actual_rows: u32,
    },
}

impl core::fmt::Display for QpMapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedCodec(codec) => {
                write!(f, "{} has no QP-map geometry", codec.token())
            }
            Self::EmptyFrame { width, height } => {
                write!(f, "degenerate frame geometry {width}x{height}")
            }
            Self::GridMismatch {
                expected_cols,
                expected_rows,
                actual_cols,
                actual_rows,
            } => write!(
                f,
                "Keel grid is {actual_cols}x{actual_rows} blocks but this frame needs \
                 {expected_cols}x{expected_rows}"
            ),
        }
    }
}

impl std::error::Error for QpMapError {}

/// Builds per-frame QP delta maps, reusing one allocation across frames.
///
/// Fixed to one frame geometry: a resolution change means a new encoder
/// session in this codebase, so it means a new builder too.
#[derive(Debug, Clone)]
pub struct QpDeltaMapBuilder {
    geometry: QpMapGeometry,
    cols: u32,
    rows: u32,
    keel_cols: u32,
    keel_rows: u32,
    deltas: Vec<i8>,
}

impl QpDeltaMapBuilder {
    /// Prepare a builder for one codec and frame geometry.
    ///
    /// # Errors
    ///
    /// [`QpMapError::UnsupportedCodec`] for a codec with no QP-map geometry,
    /// and [`QpMapError::EmptyFrame`] for degenerate dimensions.
    pub fn new(codec: VideoCodec, width: u32, height: u32) -> Result<Self, QpMapError> {
        if width == 0 || height == 0 {
            return Err(QpMapError::EmptyFrame { width, height });
        }
        let geometry =
            QpMapGeometry::for_codec(codec).ok_or(QpMapError::UnsupportedCodec(codec))?;
        let (cols, rows) = geometry.dimensions(width, height);
        Ok(Self {
            geometry,
            cols,
            rows,
            keel_cols: width.div_ceil(KEEL_BLOCK_SIZE),
            keel_rows: height.div_ceil(KEEL_BLOCK_SIZE),
            deltas: vec![0; cols as usize * rows as usize],
        })
    }

    /// The geometry this builder emits.
    #[must_use]
    pub const fn geometry(&self) -> QpMapGeometry {
        self.geometry
    }

    /// Map dimensions in coding blocks.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.cols, self.rows)
    }

    /// Keel grid dimensions this builder expects to be fed.
    #[must_use]
    pub const fn keel_dimensions(&self) -> (u32, u32) {
        (self.keel_cols, self.keel_rows)
    }

    /// Number of entries, which must equal NVENC's `qpDeltaMapSize`.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.deltas.len()
    }

    /// Fill the map from a Keel damage predicate, in NVENC raster order.
    ///
    /// `is_dirty` is called with Keel block coordinates and must be cheap; it
    /// is consulted once per covered Keel block. A coding block counts as
    /// dirty when **any** Keel block it covers is dirty — conservative on
    /// purpose, because under-marking starves a genuinely changed region
    /// while over-marking merely spends bits that were budgeted anyway.
    ///
    /// # Errors
    ///
    /// [`QpMapError::GridMismatch`] when the caller's grid does not match the
    /// geometry this builder was constructed for.
    pub fn build(
        &mut self,
        keel_cols: u32,
        keel_rows: u32,
        bias: QpBias,
        is_dirty: impl Fn(u32, u32) -> bool,
    ) -> Result<&[i8], QpMapError> {
        if keel_cols != self.keel_cols || keel_rows != self.keel_rows {
            return Err(QpMapError::GridMismatch {
                expected_cols: self.keel_cols,
                expected_rows: self.keel_rows,
                actual_cols: keel_cols,
                actual_rows: keel_rows,
            });
        }
        let bias = bias.clamped();
        let span = self.geometry.keel_blocks_per_side();
        for row in 0..self.rows {
            for col in 0..self.cols {
                let dirty = self.covers_dirty(col, row, span, &is_dirty);
                let index = (row * self.cols + col) as usize;
                self.deltas[index] = if dirty { bias.dirty } else { bias.clean };
            }
        }
        Ok(&self.deltas)
    }

    /// Fill the map with zeroes, keeping the allocation.
    ///
    /// The right map for a keyframe. Every block of an IDR is coded intra, so
    /// "unchanged since the last frame" describes nothing the encoder can act
    /// on — and taxing blocks that merely look clean would bake that penalty
    /// into the reference every later frame is predicted from.
    pub fn build_neutral(&mut self) -> &[i8] {
        self.deltas.fill(0);
        &self.deltas
    }

    /// The most recently built map.
    #[must_use]
    pub fn entries(&self) -> &[i8] {
        &self.deltas
    }

    fn covers_dirty(
        &self,
        col: u32,
        row: u32,
        span: u32,
        is_dirty: &impl Fn(u32, u32) -> bool,
    ) -> bool {
        let first_x = col * span;
        let first_y = row * span;
        for offset_y in 0..span {
            let keel_y = first_y + offset_y;
            if keel_y >= self.keel_rows {
                break;
            }
            for offset_x in 0..span {
                let keel_x = first_x + offset_x;
                if keel_x >= self.keel_cols {
                    break;
                }
                if is_dirty(keel_x, keel_y) {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_matches_each_codec_coding_block() {
        assert_eq!(
            QpMapGeometry::for_codec(VideoCodec::H264),
            Some(QpMapGeometry::Macroblock16)
        );
        assert_eq!(
            QpMapGeometry::for_codec(VideoCodec::H265),
            Some(QpMapGeometry::Ctb32)
        );
        assert_eq!(
            QpMapGeometry::for_codec(VideoCodec::Av1),
            Some(QpMapGeometry::Superblock64)
        );
        assert_eq!(QpMapGeometry::for_codec(VideoCodec::Vp9), None);
        assert_eq!(QpMapGeometry::for_codec(VideoCodec::Jpeg), None);
    }

    /// Every geometry must tile Keel's grid exactly, or a coding block would
    /// straddle a fraction of a damage block and the mapping would be a guess.
    #[test]
    fn every_geometry_is_a_whole_multiple_of_the_keel_block() {
        for geometry in [
            QpMapGeometry::Macroblock16,
            QpMapGeometry::Ctb32,
            QpMapGeometry::Superblock64,
        ] {
            assert_eq!(geometry.block_size() % KEEL_BLOCK_SIZE, 0);
            assert!(geometry.keel_blocks_per_side() >= 1);
        }
    }

    /// Partial edge blocks must still get an entry: NVENC sizes the map by
    /// rounding up, and a short map would leave it reading past our buffer.
    #[test]
    fn dimensions_round_up_for_partial_edge_blocks() {
        assert_eq!(QpMapGeometry::Ctb32.dimensions(1920, 1080), (60, 34));
        assert_eq!(
            QpMapGeometry::Macroblock16.dimensions(1920, 1080),
            (120, 68)
        );
        assert_eq!(QpMapGeometry::Superblock64.dimensions(1920, 1080), (30, 17));
        // 1080 is not a multiple of any of them, which is exactly the case a
        // floor division would get wrong.
        assert_eq!(QpMapGeometry::Ctb32.entry_count(1920, 1080), 60 * 34);
    }

    #[test]
    fn a_single_dirty_keel_block_dirties_only_its_own_coding_block() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H265, 128, 64).unwrap();
        let (cols, rows) = builder.dimensions();
        assert_eq!((cols, rows), (4, 2));
        let (keel_cols, keel_rows) = builder.keel_dimensions();
        assert_eq!((keel_cols, keel_rows), (8, 4));

        // Keel block (2,0) sits inside CTB (1,0) for 32x32 geometry.
        let bias = QpBias {
            dirty: -5,
            clean: 2,
        };
        let map = builder
            .build(keel_cols, keel_rows, bias, |x, y| (x, y) == (2, 0))
            .unwrap();

        assert_eq!(map[1], -5, "CTB (1,0) covers Keel (2,0) and must be dirty");
        assert_eq!(map[0], 2);
        assert_eq!(map[2], 2);
        assert_eq!(map[3], 2);
        assert!(map[4..].iter().all(|d| *d == 2), "row 1 saw no damage");
    }

    /// Under-marking starves a region that genuinely changed, so partial
    /// coverage must round towards dirty.
    #[test]
    fn a_coding_block_is_dirty_when_any_covered_keel_block_is() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::Av1, 64, 64).unwrap();
        assert_eq!(builder.dimensions(), (1, 1));
        let (keel_cols, keel_rows) = builder.keel_dimensions();
        assert_eq!((keel_cols, keel_rows), (4, 4));

        // One of sixteen covered Keel blocks, in the far corner.
        let map = builder
            .build(keel_cols, keel_rows, QpBias::default(), |x, y| {
                (x, y) == (3, 3)
            })
            .unwrap();
        assert_eq!(map[0], QpBias::default().dirty);
    }

    #[test]
    fn a_clean_frame_biases_every_block_the_same_way() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H264, 64, 32).unwrap();
        let (keel_cols, keel_rows) = builder.keel_dimensions();
        let bias = QpBias {
            dirty: -6,
            clean: 3,
        };
        let map = builder
            .build(keel_cols, keel_rows, bias, |_, _| false)
            .unwrap();
        assert_eq!(map.len(), 4 * 2);
        assert!(map.iter().all(|delta| *delta == 3));
    }

    /// A keyframe codes every block intra, so damage describes nothing and a
    /// clean penalty would be baked into the reference for every later frame.
    #[test]
    fn the_keyframe_map_is_entirely_neutral() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H265, 1920, 1080).unwrap();
        let (keel_cols, keel_rows) = builder.keel_dimensions();
        builder
            .build(keel_cols, keel_rows, QpBias::default(), |_, _| true)
            .unwrap();
        assert!(builder.entries().iter().any(|d| *d != 0));

        let neutral = builder.build_neutral();
        assert!(neutral.iter().all(|delta| *delta == 0));
        assert_eq!(neutral.len(), QpMapGeometry::Ctb32.entry_count(1920, 1080));
    }

    /// The entry count is what NVENC's `qpDeltaMapSize` is checked against, so
    /// it has to be exact for real geometries, not merely close.
    #[test]
    fn entry_count_matches_the_built_map_for_real_resolutions() {
        for (width, height) in [(1920, 1080), (3840, 2160), (3008, 1692), (1366, 768)] {
            for codec in [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1] {
                let mut builder = QpDeltaMapBuilder::new(codec, width, height).unwrap();
                let (keel_cols, keel_rows) = builder.keel_dimensions();
                let expected = QpMapGeometry::for_codec(codec)
                    .unwrap()
                    .entry_count(width, height);
                let map = builder
                    .build(keel_cols, keel_rows, QpBias::default(), |_, _| false)
                    .unwrap();
                assert_eq!(map.len(), expected, "{codec:?} at {width}x{height}");
                assert_eq!(builder.entry_count(), expected);
            }
        }
    }

    /// A mismatched grid must be refused, not silently misaligned: every entry
    /// after the first divergence would describe the wrong part of the screen.
    #[test]
    fn a_grid_that_does_not_match_the_frame_is_refused() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H265, 1920, 1080).unwrap();
        let error = builder
            .build(60, 34, QpBias::default(), |_, _| false)
            .unwrap_err();
        assert!(matches!(error, QpMapError::GridMismatch { .. }));
        assert!(error.to_string().contains("120x68"));
    }

    #[test]
    fn unsupported_codecs_and_degenerate_frames_are_typed_errors() {
        assert_eq!(
            QpDeltaMapBuilder::new(VideoCodec::Vp9, 1920, 1080).unwrap_err(),
            QpMapError::UnsupportedCodec(VideoCodec::Vp9)
        );
        assert_eq!(
            QpDeltaMapBuilder::new(VideoCodec::H265, 0, 1080).unwrap_err(),
            QpMapError::EmptyFrame {
                width: 0,
                height: 1080
            }
        );
    }

    /// A delta map rides on top of the rate controller's QP, so an unbounded
    /// bias could drive a block to either extreme regardless of bitrate.
    #[test]
    fn bias_is_clamped_to_a_survivable_range() {
        let wild = QpBias {
            dirty: -120,
            clean: 100,
        }
        .clamped();
        assert_eq!(wild.dirty, -MAX_ABS_QP_DELTA);
        assert_eq!(wild.clean, MAX_ABS_QP_DELTA);

        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H264, 32, 16).unwrap();
        let (keel_cols, keel_rows) = builder.keel_dimensions();
        let map = builder
            .build(
                keel_cols,
                keel_rows,
                QpBias {
                    dirty: -120,
                    clean: 0,
                },
                |_, _| true,
            )
            .unwrap();
        assert!(map.iter().all(|delta| *delta == -MAX_ABS_QP_DELTA));
    }

    /// The neutral bias is the control arm of any benchmark: it must exercise
    /// the whole path while changing nothing about the encode.
    #[test]
    fn the_neutral_bias_produces_an_all_zero_map() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::Av1, 256, 128).unwrap();
        let (keel_cols, keel_rows) = builder.keel_dimensions();
        let map = builder
            .build(keel_cols, keel_rows, QpBias::NEUTRAL, |x, _| x % 2 == 0)
            .unwrap();
        assert!(map.iter().all(|delta| *delta == 0));
    }

    /// Raster order is NVENC's contract; a column-major fill would place every
    /// delta on the wrong part of the screen without changing the map size.
    #[test]
    fn entries_are_emitted_in_raster_order() {
        let mut builder = QpDeltaMapBuilder::new(VideoCodec::H264, 48, 32).unwrap();
        let (cols, rows) = builder.dimensions();
        assert_eq!((cols, rows), (3, 2));
        let (keel_cols, keel_rows) = builder.keel_dimensions();

        // Dirty the whole second row of macroblocks only.
        let map = builder
            .build(
                keel_cols,
                keel_rows,
                QpBias {
                    dirty: -7,
                    clean: 0,
                },
                |_, y| y == 1,
            )
            .unwrap();
        assert_eq!(map, &[0, 0, 0, -7, -7, -7]);
    }
}
