// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Golden colour test patterns and round-trip error measurement.
//!
//! The colour work makes a specific, falsifiable claim: that 8-bit RGB desktop
//! content survives a 10-bit 4:4:4 full-range round trip unchanged. A claim
//! like that is only worth making if it is measured, so this module provides
//! the instrument — deterministic patterns chosen to expose the errors that
//! actually matter to a colourist, and a comparison that reports the worst
//! per-channel deviation rather than an average that would hide a single bad
//! pixel.
//!
//! Patterns are generated rather than stored as image files so they exist at
//! any resolution, cost nothing in the repository, and cannot drift from the
//! code that interprets them.

use crate::ChromaSubsampling;
use crate::video::ColorTransform;

/// One deterministic colour test pattern.
///
/// Each pattern targets a distinct failure mode. Together they cover the four
/// things that go wrong in a remote-desktop colour pipeline: range handling,
/// quantisation, chroma subsampling, and matrix error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestPattern {
    /// Horizontal neutral ramp across the full code range.
    ///
    /// Exposes banding and any luma scaling error. A grader reads this with an
    /// eyedropper, so an off-by-one is a real defect here.
    GreyRamp,
    /// Near-black and near-white wedges, one code apart.
    ///
    /// This is where limited range destroys information: codes below 16 and
    /// above 235 cannot be represented distinctly at all, so a limited-range
    /// round trip collapses these wedges into flat blocks.
    ShadowHighlightWedge,
    /// Fully saturated primaries and secondaries.
    ///
    /// Exposes matrix errors and chroma clipping, which show up as hue shifts
    /// on saturated colour long before they are visible on neutrals.
    SaturatedPrimaries,
    /// Alternating single-pixel colour columns.
    ///
    /// The 4:2:0 killer. Subsampling averages adjacent chroma samples, so this
    /// pattern smears into flat colour at 4:2:0 and survives intact at 4:4:4.
    /// It stands in for UI text, node graphs and thin mattes.
    ChromaDetail,
    /// Pseudo-random full-gamut noise.
    ///
    /// Covers combinations the structured patterns miss, without needing the
    /// whole 16-million-entry RGB cube.
    FullGamutNoise,
}

impl TestPattern {
    /// Every pattern, in reporting order.
    pub const ALL: &'static [Self] = &[
        Self::GreyRamp,
        Self::ShadowHighlightWedge,
        Self::SaturatedPrimaries,
        Self::ChromaDetail,
        Self::FullGamutNoise,
    ];

    /// Stable token used in logs and the findings file.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::GreyRamp => "grey_ramp",
            Self::ShadowHighlightWedge => "shadow_highlight_wedge",
            Self::SaturatedPrimaries => "saturated_primaries",
            Self::ChromaDetail => "chroma_detail",
            Self::FullGamutNoise => "full_gamut_noise",
        }
    }

    /// Parse a stable pattern token back into a [`TestPattern`].
    ///
    /// The inverse of [`TestPattern::token`]. Needed wherever a pattern
    /// choice has to cross a process boundary as text rather than as a Rust
    /// value -- the host picks a pattern from argv, encodes it, and records
    /// the token in a findings file or a sidecar; the client (on a different
    /// machine entirely) reads that same token back and must regenerate the
    /// *identical* deterministic pattern to compare against, so an unknown
    /// token is a typed `None` rather than a silent default that would make
    /// two machines compare against different references without either
    /// side knowing.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|pattern| pattern.token() == value)
    }

    /// What this pattern is designed to expose.
    #[must_use]
    pub const fn exposes(self) -> &'static str {
        match self {
            Self::GreyRamp => "banding and luma scaling error",
            Self::ShadowHighlightWedge => "range clipping below 16 and above 235",
            Self::SaturatedPrimaries => "matrix error and chroma clipping",
            Self::ChromaDetail => "chroma subsampling loss",
            Self::FullGamutNoise => "combinations the structured patterns miss",
        }
    }

    /// The RGB value of one pixel of this pattern.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub const fn pixel(
        self,
        column: usize,
        row: usize,
        width: usize,
        height: usize,
    ) -> (u8, u8, u8) {
        match self {
            Self::GreyRamp => {
                let level = if width <= 1 {
                    0
                } else {
                    (column * 255 / (width - 1)) as u8
                };
                (level, level, level)
            }
            Self::ShadowHighlightWedge => {
                // Top half walks up from absolute black, bottom half walks
                // down from absolute white, one code at a time.
                let span = if width == 0 { 1 } else { width };
                let step = (column * 32 / span) as u8;
                if row * 2 < height {
                    (step, step, step)
                } else {
                    (255 - step, 255 - step, 255 - step)
                }
            }
            Self::SaturatedPrimaries => {
                let span = if width == 0 { 1 } else { width };
                match (column * 6 / span) % 6 {
                    0 => (255, 0, 0),
                    1 => (0, 255, 0),
                    2 => (0, 0, 255),
                    3 => (0, 255, 255),
                    4 => (255, 0, 255),
                    _ => (255, 255, 0),
                }
            }
            Self::ChromaDetail => {
                // Single-pixel alternation: the finest chroma detail that can
                // exist, and precisely what 4:2:0 cannot carry.
                if column % 2 == 0 {
                    (255, 0, 0)
                } else {
                    (0, 0, 255)
                }
            }
            Self::FullGamutNoise => {
                // A small integer hash rather than an RNG so the pattern is
                // identical on every machine and in every language that might
                // reimplement the harness.
                let seed = (column.wrapping_mul(2_654_435_761)) ^ (row.wrapping_mul(2_246_822_519));
                let red = (seed >> 3) as u8;
                let green = (seed >> 11) as u8;
                let blue = (seed >> 19) as u8;
                (red, green, blue)
            }
        }
    }

    /// Render this pattern into a BGRA buffer, as capture would produce it.
    #[must_use]
    pub fn render_bgra(self, width: usize, height: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = self.pixel(x, y, width, height);
                out.extend_from_slice(&[b, g, r, 0xff]);
            }
        }
        out
    }

    /// This pattern as an eight-bit RGB PNG with no colour profile, for
    /// showing on a host at one image pixel per screen pixel. `None` for an
    /// empty or oversized picture.
    #[must_use]
    pub fn render_png(self, width: usize, height: usize) -> Option<Vec<u8>> {
        if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
            return None;
        }
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = self.pixel(x, y, width, height);
                rgb.extend_from_slice(&[r, g, b]);
            }
        }
        let mut output = Vec::new();
        {
            let mut encoder = png::Encoder::new(
                &mut output,
                u32::try_from(width).ok()?,
                u32::try_from(height).ok()?,
            );
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().ok()?;
            writer.write_image_data(&rgb).ok()?;
            writer.finish().ok()?;
        }
        Some(output)
    }
}

/// Worst-case and mean per-channel error over a comparison.
///
/// `max_error` is reported alongside the mean because a mean hides exactly the
/// failure a colourist notices: one wrong pixel in a matte edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorAccuracy {
    /// Largest absolute per-channel deviation, on the 0..=255 scale.
    pub max_error: u16,
    /// Mean absolute per-channel deviation.
    pub mean_error: f64,
    /// Pixels compared.
    pub pixels: usize,
    /// Position of the worst pixel, for diagnosis.
    pub worst_at: (usize, usize),
    /// Pixels with any channel more than [`OUTLIER_CODES`] off.
    pub outliers: usize,
    /// The smallest rectangle holding every outlier, as inclusive
    /// `[left, top, right, bottom]`; `None` when there is none.
    pub outlier_bounds: Option<[usize; 4]>,
}

/// An error, in eight-bit codes, above which a pixel counts as an outlier: a
/// lossy stream's quantisation stays within it.
pub const OUTLIER_CODES: u16 = 2;

impl ColorAccuracy {
    /// Whether the comparison was exact.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        self.max_error == 0
    }
}

/// Shared aggregation core: compare an `actual` pixel source against
/// `pattern`'s reference pixel at every position, tracking the worst-case and
/// mean absolute per-channel error.
///
/// Both [`measure_transform_roundtrip`] (whose actual pixel comes from a pure
/// [`ColorTransform`] round trip, entirely in-process) and
/// [`measure_rgb_roundtrip`] (whose actual pixel comes from a real,
/// already-decoded frame buffer, possibly produced on a different machine
/// entirely) go through this one accumulation loop, so the two measurements
/// can never disagree about *how* error is aggregated -- only about *where*
/// the compared pixel came from. `actual` returns `(r, g, b)`, matching
/// [`TestPattern::pixel`]'s own return order.
fn compare_to_pattern(
    pattern: TestPattern,
    width: usize,
    height: usize,
    actual: impl FnMut(usize, usize) -> (u8, u8, u8),
) -> ColorAccuracy {
    compare_to_signal(pattern, PatternSignal::Desktop, width, height, actual)
}

/// [`compare_to_pattern`] against the pattern as `signal` turns it into
/// stream values.
fn compare_to_signal(
    pattern: TestPattern,
    signal: PatternSignal,
    width: usize,
    height: usize,
    mut actual: impl FnMut(usize, usize) -> (u8, u8, u8),
) -> ColorAccuracy {
    let mut max_error = 0u16;
    let mut total = 0u64;
    let mut worst_at = (0, 0);
    let mut outliers = 0usize;
    let mut outlier_bounds: Option<[usize; 4]> = None;
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = signal.map(pattern.pixel(x, y, width, height));
            let (out_r, out_g, out_b) = actual(x, y);
            let mut pixel_error = 0u16;
            for (expected, got) in [(r, out_r), (g, out_g), (b, out_b)] {
                let error = u16::from(expected.abs_diff(got));
                total += u64::from(error);
                pixel_error = pixel_error.max(error);
                if error > max_error {
                    max_error = error;
                    worst_at = (x, y);
                }
            }
            if pixel_error > OUTLIER_CODES {
                outliers += 1;
                outlier_bounds = Some(match outlier_bounds {
                    None => [x, y, x, y],
                    Some([left, top, right, bottom]) => {
                        [left.min(x), top.min(y), right.max(x), bottom.max(y)]
                    }
                });
            }
        }
    }
    let pixels = width * height;
    ColorAccuracy {
        outliers,
        outlier_bounds,
        max_error,
        #[allow(clippy::cast_precision_loss)]
        mean_error: if pixels == 0 {
            0.0
        } else {
            total as f64 / (pixels * 3) as f64
        },
        pixels,
        worst_at,
    }
}

/// Measure a pattern's round trip through one colour transform.
///
/// This converts each pixel to coded samples and straight back, which isolates
/// the **colour** error from any codec loss. It is the measurement that decides
/// whether a given depth and range can carry desktop content faithfully, and it
/// deliberately does not model chroma subsampling: that is a separate,
/// much larger error measured by comparing 4:4:4 against 4:2:0 end to end.
///
/// This is a *pure colour-maths* figure with no codec involved at all --
/// contrast [`measure_rgb_roundtrip`], which measures a real encode/decode
/// and therefore also carries whatever quantisation loss the codec added.
/// Conflating the two would let "our colour maths is exact" (provable, and
/// proven by this function's own tests) stand in for "the codec is
/// lossless" (false for any lossy encode) -- see this module's doc.
#[must_use]
pub fn measure_transform_roundtrip(
    pattern: TestPattern,
    transform: ColorTransform,
    width: usize,
    height: usize,
) -> ColorAccuracy {
    compare_to_pattern(pattern, width, height, |x, y| {
        let (r, g, b) = pattern.pixel(x, y, width, height);
        let (out_b, out_g, out_r) = transform.to_bgr8(
            transform.luma(b, g, r),
            transform.cb(b, g, r),
            transform.cr(b, g, r),
        );
        (out_r, out_g, out_b)
    })
}

/// Measure a pattern's round trip through a **real, already-decoded** frame.
///
/// Unlike [`measure_transform_roundtrip`], which isolates pure colour-space
/// arithmetic entirely in-process, this compares against pixels that actually
/// travelled through a real encoder, a real bitstream, and a real decoder --
/// on this machine, on a different one, or both. That number therefore also
/// carries whatever the codec itself did to the signal (quantisation, chroma
/// subsampling, in-loop filtering, ...). **The two measurements are not the
/// same claim.** Report both, label which is which, and never let one stand
/// in for the other: "our colour maths is exact" does not imply "the codec is
/// lossless", and this function measures the second claim, not the first.
///
/// `TestPattern` is a pure function of `(column, row, width, height)`, so the
/// reference here is regenerated locally rather than transmitted -- the
/// whole point of using a deterministic pattern for this measurement is that
/// only the *coded bitstream* needs to cross a process or machine boundary,
/// never the reference pixels themselves.
///
/// `recovered_bgra` must be a tightly packed `width * height * 4`-byte BGRA
/// buffer (matching [`TestPattern::render_bgra`]'s own layout: `[b, g, r,
/// a]` per pixel, row-major). Returns `None` if the buffer is shorter than
/// that, so a caller with a mismatched or truncated decode never silently
/// compares out-of-bounds data or produces a meaningless figure from a
/// partial frame.
#[must_use]
pub fn measure_rgb_roundtrip(
    pattern: TestPattern,
    width: usize,
    height: usize,
    recovered_bgra: &[u8],
) -> Option<ColorAccuracy> {
    let needed = width.checked_mul(height)?.checked_mul(4)?;
    if recovered_bgra.len() < needed {
        return None;
    }
    Some(compare_to_pattern(pattern, width, height, |x, y| {
        let offset = (y * width + x) * 4;
        let b = recovered_bgra[offset];
        let g = recovered_bgra[offset + 1];
        let r = recovered_bgra[offset + 2];
        (r, g, b)
    }))
}

/// What a host turns a desktop pixel into before coding it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternSignal {
    /// The desktop's own code values, unchanged (the Linux depth-30 and
    /// macOS paths, and every eight-bit path).
    Desktop,
    /// The desktop's sRGB values decoded to light and encoded again with the
    /// BT.709 OETF: what a Pier that captures linear FP16 (Windows Graphics
    /// Capture) serves for a BT.709 Grading stream.
    SrgbReencodedBt709,
}

impl PatternSignal {
    /// The stream's eight-bit value for one desktop pixel.
    #[must_use]
    pub fn map(self, (r, g, b): (u8, u8, u8)) -> (u8, u8, u8) {
        match self {
            Self::Desktop => (r, g, b),
            Self::SrgbReencodedBt709 => {
                let channel = |code: u8| {
                    let signal = f64::from(code) / 255.0;
                    let linear = if signal <= 0.040_45 {
                        signal / 12.92
                    } else {
                        ((signal + 0.055) / 1.055).powf(2.4)
                    };
                    let encoded = if linear < 0.018 {
                        4.5 * linear
                    } else {
                        1.099 * linear.powf(0.45) - 0.099
                    };
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    {
                        (encoded * 255.0).round().clamp(0.0, 255.0) as u8
                    }
                };
                (channel(r), channel(g), channel(b))
            }
        }
    }
}

/// How one decoded two-plane Y′CbCr picture is laid out in memory: a luma
/// plane and one interleaved Cb/Cr plane, the shape a hardware decoder hands
/// back (`420v`/`420f`, `x420`, `x444`/`xf44`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BiplanarLayout {
    /// Picture width in pixels.
    pub width: usize,
    /// Picture height in pixels.
    pub height: usize,
    /// Chroma sampling of the second plane.
    pub chroma: ChromaSubsampling,
    /// Samples are 16-bit little-endian words holding the code MSB-aligned
    /// (`x444`, `xf44`, `x420`); otherwise one byte each.
    pub wide: bool,
    /// Bytes from one luma row to the next.
    pub luma_stride: usize,
    /// Bytes from one chroma row to the next.
    pub chroma_stride: usize,
}

/// Worst-case and mean error in the stream's own coded samples (Y′, Cb, Cr),
/// at the stream's bit depth: the code values a colourist reads with a
/// picker, before any display conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodeAccuracy {
    /// Largest absolute deviation of any coded sample, in codes.
    pub max_error: u16,
    /// Mean absolute deviation per coded sample.
    pub mean_error: f64,
    /// The code scale: 255 for eight-bit, 1023 for ten-bit.
    pub max_code: u16,
    /// Pixels compared.
    pub pixels: usize,
    /// Position of the worst pixel.
    pub worst_at: (usize, usize),
}

impl CodeAccuracy {
    /// Whether every coded sample was exactly the expected one.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        self.max_error == 0
    }
}

/// One measurement of a decoded Y′CbCr picture against a pattern.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanarAccuracy {
    /// Back in eight-bit RGB, as the host's desktop drew the pattern.
    pub rgb: ColorAccuracy,
    /// In coded samples. `None` for subsampled chroma: there is no single
    /// expected chroma sample without knowing the host's downsampling
    /// filter, so only the RGB figure is honest there.
    pub codes: Option<CodeAccuracy>,
}

/// Measures a decoded two-plane Y′CbCr picture against `pattern`.
///
/// The reference is the pattern's own eight-bit RGB, coded through
/// `transform` (the stream's matrix, range and depth): that is what a host
/// produces when its desktop shows the pattern at one pixel per pixel. The
/// decoded codes are read straight from the planes, so no display transform,
/// colour management or scaling of this machine is involved. Subsampled
/// chroma is read at its co-sited sample (nearest), so a 4:2:0 stream shows
/// exactly the loss subsampling causes.
///
/// Returns `None` when a plane is too short for `layout`, so a mismatched
/// buffer never produces a figure.
#[must_use]
pub fn measure_biplanar(
    pattern: TestPattern,
    transform: ColorTransform,
    layout: BiplanarLayout,
    luma: &[u8],
    chroma: &[u8],
) -> Option<PlanarAccuracy> {
    measure_biplanar_as(
        pattern,
        PatternSignal::Desktop,
        transform,
        layout,
        luma,
        chroma,
    )
}

/// [`measure_biplanar`] against the pattern as `signal` turns it into stream
/// values. Coded samples are expected from the eight-bit result, so a ten-bit
/// host path that keeps more precision reads up to a code or two off there.
#[must_use]
pub fn measure_biplanar_as(
    pattern: TestPattern,
    signal: PatternSignal,
    transform: ColorTransform,
    layout: BiplanarLayout,
    luma: &[u8],
    chroma: &[u8],
) -> Option<PlanarAccuracy> {
    let BiplanarLayout {
        width,
        height,
        chroma: sampling,
        wide,
        luma_stride,
        chroma_stride,
    } = layout;
    if width == 0 || height == 0 {
        return None;
    }
    let sample_bytes = if wide { 2 } else { 1 };
    let (chroma_width, chroma_height) = match sampling {
        ChromaSubsampling::Yuv444 => (width, height),
        ChromaSubsampling::Yuv422 => (width.div_ceil(2), height),
        ChromaSubsampling::Yuv420 => (width.div_ceil(2), height.div_ceil(2)),
    };
    let luma_needed = luma_stride
        .checked_mul(height - 1)?
        .checked_add(width.checked_mul(sample_bytes)?)?;
    let chroma_needed = chroma_stride
        .checked_mul(chroma_height - 1)?
        .checked_add(chroma_width.checked_mul(sample_bytes * 2)?)?;
    if luma.len() < luma_needed
        || chroma.len() < chroma_needed
        || luma_stride < width * sample_bytes
        || chroma_stride < chroma_width * sample_bytes * 2
    {
        return None;
    }
    let read = |plane: &[u8], offset: usize| -> i32 {
        if wide {
            transform.unpack_p16(u16::from_le_bytes([plane[offset], plane[offset + 1]]))
        } else {
            i32::from(plane[offset])
        }
    };
    let sample_at = |x: usize, y: usize| -> (i32, i32, i32) {
        let (cx, cy) = match sampling {
            ChromaSubsampling::Yuv444 => (x, y),
            ChromaSubsampling::Yuv422 => (x / 2, y),
            ChromaSubsampling::Yuv420 => (x / 2, y / 2),
        };
        let chroma_offset = cy * chroma_stride + cx * sample_bytes * 2;
        (
            read(luma, y * luma_stride + x * sample_bytes),
            read(chroma, chroma_offset),
            read(chroma, chroma_offset + sample_bytes),
        )
    };
    let rgb = compare_to_signal(pattern, signal, width, height, |x, y| {
        let (y_code, cb, cr) = sample_at(x, y);
        let (b, g, r) = transform.to_bgr8(y_code, cb, cr);
        (r, g, b)
    });
    let codes = (sampling == ChromaSubsampling::Yuv444).then(|| {
        let mut max_error = 0u16;
        let mut total = 0u64;
        let mut worst_at = (0, 0);
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = signal.map(pattern.pixel(x, y, width, height));
                let expected = [
                    transform.luma(b, g, r),
                    transform.cb(b, g, r),
                    transform.cr(b, g, r),
                ];
                let (y_code, cb, cr) = sample_at(x, y);
                for (want, got) in expected.into_iter().zip([y_code, cb, cr]) {
                    let error = u16::try_from(want.abs_diff(got)).unwrap_or(u16::MAX);
                    total += u64::from(error);
                    if error > max_error {
                        max_error = error;
                        worst_at = (x, y);
                    }
                }
            }
        }
        let pixels = width * height;
        CodeAccuracy {
            max_error,
            #[allow(clippy::cast_precision_loss)]
            mean_error: total as f64 / (pixels * 3) as f64,
            max_code: transform.depth().max_code(),
            pixels,
            worst_at,
        }
    });
    Some(PlanarAccuracy { rgb, codes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BitDepth, ColorMatrix, ColorRange};

    const WIDTH: usize = 64;
    const HEIGHT: usize = 32;

    #[test]
    fn patterns_are_deterministic_and_cover_the_range() {
        for pattern in TestPattern::ALL.iter().copied() {
            let first = pattern.render_bgra(WIDTH, HEIGHT);
            let second = pattern.render_bgra(WIDTH, HEIGHT);
            assert_eq!(first, second, "{} must be deterministic", pattern.token());
            assert_eq!(first.len(), WIDTH * HEIGHT * 4);
        }
        // The ramp must actually reach both endpoints, or it cannot detect a
        // scaling error at the ends, which is where they show up.
        assert_eq!(TestPattern::GreyRamp.pixel(0, 0, WIDTH, HEIGHT), (0, 0, 0));
        assert_eq!(
            TestPattern::GreyRamp.pixel(WIDTH - 1, 0, WIDTH, HEIGHT),
            (255, 255, 255)
        );
    }

    #[test]
    fn from_token_round_trips_for_every_pattern() {
        for pattern in TestPattern::ALL.iter().copied() {
            assert_eq!(TestPattern::from_token(pattern.token()), Some(pattern));
        }
    }

    #[test]
    fn from_token_rejects_unknown_tokens_rather_than_defaulting() {
        assert_eq!(TestPattern::from_token("not_a_real_pattern"), None);
        assert_eq!(TestPattern::from_token(""), None);
        // Tokens are case-sensitive: a caller that mis-cases a token learns
        // about it rather than silently landing on the wrong pattern.
        assert_eq!(TestPattern::from_token("Grey_Ramp"), None);
    }

    #[test]
    fn chroma_detail_alternates_every_single_pixel() {
        // If this ever stopped alternating per pixel it would silently stop
        // testing subsampling at all.
        let pattern = TestPattern::ChromaDetail;
        assert_ne!(
            pattern.pixel(0, 0, WIDTH, HEIGHT),
            pattern.pixel(1, 0, WIDTH, HEIGHT)
        );
        assert_eq!(
            pattern.pixel(0, 0, WIDTH, HEIGHT),
            pattern.pixel(2, 0, WIDTH, HEIGHT)
        );
    }

    #[test]
    fn shadow_wedge_reaches_absolute_black_and_white() {
        let pattern = TestPattern::ShadowHighlightWedge;
        assert_eq!(pattern.pixel(0, 0, WIDTH, HEIGHT), (0, 0, 0));
        assert_eq!(pattern.pixel(0, HEIGHT - 1, WIDTH, HEIGHT), (255, 255, 255));
    }

    #[test]
    fn ten_bit_full_range_is_exact_for_every_pattern() {
        // The central product claim, measured across every pattern rather than
        // a single ramp.
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        for pattern in TestPattern::ALL.iter().copied() {
            let accuracy = measure_transform_roundtrip(pattern, transform, WIDTH, HEIGHT);
            assert!(
                accuracy.is_exact(),
                "{} lost {} codes at {:?} under 10-bit full range",
                pattern.token(),
                accuracy.max_error,
                accuracy.worst_at
            );
        }
    }

    #[test]
    fn eight_bit_limited_range_measurably_destroys_the_wedge() {
        // The quantified case against the format Arcen shipped before: the
        // shadow/highlight wedge cannot survive limited range, because codes
        // outside 16..=235 have nowhere to go.
        let transform =
            ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight);
        let accuracy = measure_transform_roundtrip(
            TestPattern::ShadowHighlightWedge,
            transform,
            WIDTH,
            HEIGHT,
        );
        assert!(
            accuracy.max_error > 0,
            "8-bit limited range was unexpectedly exact on the wedge; \
             the case for full range needs revisiting"
        );
    }

    #[test]
    fn identity_matrix_is_exact_at_eight_bit_full_range() {
        // GBR does no conversion at all, so it should be exact even at eight
        // bits. If it is not, the identity path has a bug rather than a
        // precision limit.
        let transform =
            ColorTransform::new(ColorMatrix::Identity, ColorRange::Full, BitDepth::Eight);
        for pattern in TestPattern::ALL.iter().copied() {
            let accuracy = measure_transform_roundtrip(pattern, transform, WIDTH, HEIGHT);
            assert!(
                accuracy.is_exact(),
                "identity/GBR must be lossless; {} lost {} codes",
                pattern.token(),
                accuracy.max_error
            );
        }
    }

    #[test]
    fn accuracy_reports_the_worst_pixel_not_just_an_average() {
        // A mean would hide a single wrong pixel on a matte edge, which is
        // exactly the defect that matters.
        let transform =
            ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight);
        let accuracy = measure_transform_roundtrip(TestPattern::GreyRamp, transform, WIDTH, HEIGHT);
        assert!(accuracy.mean_error.round() <= f64::from(accuracy.max_error));
        assert_eq!(accuracy.pixels, WIDTH * HEIGHT);
    }

    // ---- measure_rgb_roundtrip ----

    #[test]
    fn rgb_roundtrip_is_exact_when_the_recovered_buffer_is_untouched() {
        // Feeding the pattern's own render back in is the null case: a real
        // decode that recovered the source exactly would look like this.
        let pattern = TestPattern::SaturatedPrimaries;
        let buffer = pattern.render_bgra(WIDTH, HEIGHT);
        let accuracy = measure_rgb_roundtrip(pattern, WIDTH, HEIGHT, &buffer)
            .expect("a correctly sized buffer must be accepted");
        assert!(accuracy.is_exact());
        assert_eq!(accuracy.pixels, WIDTH * HEIGHT);
    }

    #[test]
    fn rgb_roundtrip_detects_a_perturbed_pixel_and_locates_it() {
        let pattern = TestPattern::GreyRamp;
        let mut buffer = pattern.render_bgra(WIDTH, HEIGHT);
        // Perturb one pixel's green channel (BGRA layout: offset + 1) by a
        // known amount, at a known position, so the measurement's own
        // `max_error`/`worst_at` can be checked against ground truth rather
        // than merely asserted non-zero.
        let (px, py) = (5usize, 7usize);
        let offset = (py * WIDTH + px) * 4;
        buffer[offset + 1] = buffer[offset + 1].wrapping_add(9);
        let accuracy = measure_rgb_roundtrip(pattern, WIDTH, HEIGHT, &buffer)
            .expect("a correctly sized buffer must be accepted");
        assert_eq!(accuracy.max_error, 9);
        assert_eq!(accuracy.worst_at, (px, py));
        assert!(!accuracy.is_exact());
    }

    #[test]
    fn rgb_roundtrip_rejects_a_short_buffer_instead_of_reading_out_of_bounds() {
        let pattern = TestPattern::GreyRamp;
        let short = vec![0u8; WIDTH * HEIGHT * 4 - 1];
        assert_eq!(measure_rgb_roundtrip(pattern, WIDTH, HEIGHT, &short), None);
        assert_eq!(measure_rgb_roundtrip(pattern, WIDTH, HEIGHT, &[]), None);
    }

    /// Codes `pattern` into two planes exactly as a host would, with row
    /// padding so a stride bug cannot hide.
    fn biplanar(
        pattern: TestPattern,
        transform: ColorTransform,
        sampling: ChromaSubsampling,
        wide: bool,
    ) -> (BiplanarLayout, Vec<u8>, Vec<u8>) {
        let bytes = if wide { 2 } else { 1 };
        let (chroma_width, chroma_height, step) = match sampling {
            ChromaSubsampling::Yuv444 => (WIDTH, HEIGHT, 1),
            ChromaSubsampling::Yuv422 => (WIDTH / 2, HEIGHT, 2),
            ChromaSubsampling::Yuv420 => (WIDTH / 2, HEIGHT / 2, 2),
        };
        let layout = BiplanarLayout {
            width: WIDTH,
            height: HEIGHT,
            chroma: sampling,
            wide,
            luma_stride: WIDTH * bytes + 16,
            chroma_stride: chroma_width * bytes * 2 + 16,
        };
        let put = |plane: &mut Vec<u8>, offset: usize, code: i32| {
            if wide {
                plane[offset..offset + 2].copy_from_slice(&transform.pack_p16(code).to_le_bytes());
            } else {
                plane[offset] = u8::try_from(code).unwrap();
            }
        };
        let mut luma = vec![0u8; layout.luma_stride * HEIGHT];
        let mut chroma = vec![0u8; layout.chroma_stride * chroma_height];
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (r, g, b) = pattern.pixel(x, y, WIDTH, HEIGHT);
                put(
                    &mut luma,
                    y * layout.luma_stride + x * bytes,
                    transform.luma(b, g, r),
                );
            }
        }
        let vertical = if sampling == ChromaSubsampling::Yuv420 {
            2
        } else {
            1
        };
        for cy in 0..chroma_height {
            for cx in 0..chroma_width {
                let (r, g, b) = pattern.pixel(cx * step, cy * vertical, WIDTH, HEIGHT);
                let offset = cy * layout.chroma_stride + cx * bytes * 2;
                put(&mut chroma, offset, transform.cb(b, g, r));
                put(&mut chroma, offset + bytes, transform.cr(b, g, r));
            }
        }
        (layout, luma, chroma)
    }

    #[test]
    fn ten_bit_444_planes_coded_from_the_pattern_measure_exact_in_codes_and_rgb() {
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        for pattern in TestPattern::ALL.iter().copied() {
            let (layout, luma, chroma) =
                biplanar(pattern, transform, ChromaSubsampling::Yuv444, true);
            let accuracy = measure_biplanar(pattern, transform, layout, &luma, &chroma).unwrap();
            assert!(accuracy.rgb.is_exact(), "{}", pattern.token());
            let codes = accuracy.codes.unwrap();
            assert!(codes.is_exact(), "{}", pattern.token());
            assert_eq!(codes.max_code, 1023);
            assert_eq!(codes.pixels, WIDTH * HEIGHT);
        }
    }

    #[test]
    fn one_code_off_in_one_ten_bit_sample_is_reported_in_codes_and_located() {
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let pattern = TestPattern::GreyRamp;
        let (layout, mut luma, chroma) =
            biplanar(pattern, transform, ChromaSubsampling::Yuv444, true);
        let offset = 7 * layout.luma_stride + 9 * 2;
        let word = u16::from_le_bytes([luma[offset], luma[offset + 1]]);
        let bumped = transform.pack_p16(transform.unpack_p16(word) + 1);
        luma[offset..offset + 2].copy_from_slice(&bumped.to_le_bytes());
        let codes = measure_biplanar(pattern, transform, layout, &luma, &chroma)
            .unwrap()
            .codes
            .unwrap();
        assert_eq!(codes.max_error, 1);
        assert_eq!(codes.worst_at, (9, 7));
    }

    #[test]
    fn subsampled_chroma_loses_the_chroma_detail_pattern_and_reports_no_code_figure() {
        let transform =
            ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight);
        let pattern = TestPattern::ChromaDetail;
        let (layout, luma, chroma) = biplanar(pattern, transform, ChromaSubsampling::Yuv420, false);
        let accuracy = measure_biplanar(pattern, transform, layout, &luma, &chroma).unwrap();
        assert!(accuracy.codes.is_none());
        assert!(accuracy.rgb.max_error > 100, "{:?}", accuracy.rgb);
    }

    #[test]
    fn the_saved_png_decodes_to_the_pattern_pixel_for_pixel() {
        let pattern = TestPattern::FullGamutNoise;
        let encoded = pattern.render_png(WIDTH, HEIGHT).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(encoded))
            .read_info()
            .unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((info.width, info.height), (64, 32));
        let bgra: Vec<u8> = pixels[..info.buffer_size()]
            .chunks_exact(3)
            .flat_map(|rgb| [rgb[2], rgb[1], rgb[0], 0xff])
            .collect();
        assert!(
            measure_rgb_roundtrip(pattern, WIDTH, HEIGHT, &bgra)
                .unwrap()
                .is_exact()
        );
        assert!(pattern.render_png(0, HEIGHT).is_none());
    }

    #[test]
    fn a_bt709_reencoded_ramp_is_off_as_desktop_codes_and_exact_as_reencoded() {
        // What a Windows Grading host serves for an sRGB desktop: measured
        // on the lab as mean 10.3/255 off the desktop's own codes.
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let pattern = TestPattern::GreyRamp;
        let (layout, luma, chroma) = biplanar(pattern, transform, ChromaSubsampling::Yuv444, true);
        // Re-code the planes as the host would.
        let mut reencoded_luma = luma.clone();
        let mut reencoded_chroma = chroma.clone();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let (r, g, b) =
                    PatternSignal::SrgbReencodedBt709.map(pattern.pixel(x, y, WIDTH, HEIGHT));
                let offset = y * layout.luma_stride + x * 2;
                reencoded_luma[offset..offset + 2]
                    .copy_from_slice(&transform.pack_p16(transform.luma(b, g, r)).to_le_bytes());
                let offset = y * layout.chroma_stride + x * 4;
                reencoded_chroma[offset..offset + 2]
                    .copy_from_slice(&transform.pack_p16(transform.cb(b, g, r)).to_le_bytes());
                reencoded_chroma[offset + 2..offset + 4]
                    .copy_from_slice(&transform.pack_p16(transform.cr(b, g, r)).to_le_bytes());
            }
        }
        let as_desktop = measure_biplanar(
            pattern,
            transform,
            layout,
            &reencoded_luma,
            &reencoded_chroma,
        )
        .unwrap();
        assert!(as_desktop.rgb.max_error > 10, "{:?}", as_desktop.rgb);
        let as_reencoded = measure_biplanar_as(
            pattern,
            PatternSignal::SrgbReencodedBt709,
            transform,
            layout,
            &reencoded_luma,
            &reencoded_chroma,
        )
        .unwrap();
        assert!(as_reencoded.rgb.is_exact());
        assert!(as_reencoded.codes.unwrap().is_exact());
        assert_eq!(
            PatternSignal::SrgbReencodedBt709.map((0, 128, 255)),
            (0, 115, 255)
        );
    }

    #[test]
    fn biplanar_measurement_refuses_short_planes() {
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let pattern = TestPattern::GreyRamp;
        let (layout, luma, chroma) = biplanar(pattern, transform, ChromaSubsampling::Yuv444, true);
        assert!(
            measure_biplanar(
                pattern,
                transform,
                layout,
                &luma[..luma.len() - 17],
                &chroma
            )
            .is_none()
        );
        assert!(measure_biplanar(pattern, transform, layout, &luma, &chroma[..10]).is_none());
    }

    // ---- per-PROBE_MATRIX-row pure-transform bounds (w7) ----

    /// Whether a `PROBE_MATRIX` row's pure colour-transform round trip is
    /// expected to be exact for a given pattern and, when it is not, the
    /// empirical worst-case bound measured for it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ExpectedTransformAccuracy {
        /// Must be `max_error == 0`.
        Exact,
        /// Must be `0 < max_error <= bound`: genuinely lossy (proving this
        /// is a real measurement, not a tautology that always reports
        /// zero), but no worse than last measured.
        Bounded(u16),
    }

    /// The bound table backing
    /// [`every_probe_matrix_row_meets_its_measured_pure_transform_bound`].
    ///
    /// Every non-exact bound here is **empirical, not derived**: it was read
    /// directly off a `measure_transform_roundtrip` run over this module's
    /// own `WIDTH`x`HEIGHT` canvas for every [`TestPattern`], one time, on
    /// the machine that wrote this table (see the per-arm comments below for
    /// the numbers that run produced). A bound *tightening* because someone
    /// improved the coefficient rounding is an expected, welcome test
    /// update, not a sign this function is wrong; a bound that needs
    /// *widening* to keep the suite green is a real regression and should be
    /// looked at, not silently accepted.
    ///
    /// Scoped deliberately to exactly the `(matrix, range, depth)`
    /// combinations [`PROBE_MATRIX`] actually contains (all `Bt709`/
    /// `Identity`, never `Bt601`/`Bt2020Ncl`) -- this is not a general claim
    /// about every possible triple, only the ones this product offers and
    /// this matrix probes.
    fn expected_pure_transform_accuracy(
        matrix: ColorMatrix,
        range: ColorRange,
        depth: BitDepth,
        pattern: TestPattern,
    ) -> ExpectedTransformAccuracy {
        use ExpectedTransformAccuracy::{Bounded, Exact};
        match (matrix, range, depth) {
            // The format Arcen shipped before 4:4:4 ten-bit: limited range
            // discards codes outside 16..=235/16..=240 before any matrix
            // rounding is even considered, and eight bits leaves no
            // headroom to absorb the forward/inverse rounding either, so
            // this is lossy for every pattern, including flat neutral
            // colour. Measured: grey_ramp=1, shadow_highlight_wedge=1,
            // saturated_primaries=1, chroma_detail=1, full_gamut_noise=2.
            (ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight) => match pattern {
                TestPattern::FullGamutNoise => Bounded(2),
                _ => Bounded(1),
            },
            // Full range at eight bits has enough headroom to round-trip
            // *achromatic* content (r == g == b) exactly, because the luma
            // coefficients are constructed to sum exactly to the luma
            // scale -- but a real (non-identity) chroma conversion still
            // rounds, so saturated or fine-detail colour loses up to one
            // code even though range is already full. Measured:
            // grey_ramp=0, shadow_highlight_wedge=0, saturated_primaries=1,
            // chroma_detail=1, full_gamut_noise=1. This is precisely why
            // full range alone is not the product's target and 10-bit is
            // -- see docs/architecture/color-fidelity.md.
            (ColorMatrix::Bt709, ColorRange::Full, BitDepth::Eight) => match pattern {
                TestPattern::GreyRamp | TestPattern::ShadowHighlightWedge => Exact,
                _ => Bounded(1),
            },
            // Every other combination this matrix probes -- ten- or
            // twelve-bit BT.709 at either range, and the identity/GBR
            // matrix -- measured exactly `0` for all five patterns.
            // Ten-bit *limited* range is included deliberately: this shows
            // it is bit depth, not range, that removes the rounding error
            // for 8-bit-sourced content here. Limited range's real cost --
            // crushed super-blacks/whites -- only shows up for source
            // values limited range cannot represent at all, which an
            // 8-bit RGB pattern can never produce, so this measurement
            // does not (and cannot) contradict `ColorRange`'s own
            // documented rationale for preferring full range.
            _ => Exact,
        }
    }

    #[test]
    fn every_probe_matrix_row_meets_its_measured_pure_transform_bound() {
        use crate::video::PROBE_MATRIX;
        // Every row PROBE_MATRIX actually contains, not a hand-picked
        // subset, and every golden pattern, not just one: a regression in
        // any (variant, pattern) combination fails here with the variant
        // id, the pattern name and the worst pixel, so it is diagnosable
        // from CI output alone.
        for row in PROBE_MATRIX.iter().copied() {
            let transform =
                ColorTransform::new(row.video.matrix, row.video.range, row.video.bit_depth);
            for pattern in TestPattern::ALL.iter().copied() {
                let accuracy = measure_transform_roundtrip(pattern, transform, WIDTH, HEIGHT);
                match expected_pure_transform_accuracy(
                    row.video.matrix,
                    row.video.range,
                    row.video.bit_depth,
                    pattern,
                ) {
                    ExpectedTransformAccuracy::Exact => assert!(
                        accuracy.is_exact(),
                        "{} on variant `{}` should be an exact pure colour-transform round \
                         trip (the central product claim for full-range 10-bit-or-deeper \
                         BT.709/identity) but lost {} codes at pixel {:?}",
                        pattern.token(),
                        row.id(),
                        accuracy.max_error,
                        accuracy.worst_at
                    ),
                    ExpectedTransformAccuracy::Bounded(bound) => {
                        assert!(
                            accuracy.max_error > 0,
                            "{} on variant `{}` was unexpectedly exact (max_error=0); if this \
                             combination genuinely stopped losing precision, the falsifiable \
                             case for higher fidelity needs revisiting, not just this test",
                            pattern.token(),
                            row.id()
                        );
                        assert!(
                            accuracy.max_error <= bound,
                            "{} on variant `{}` regressed: max_error={} exceeds the \
                             empirically measured bound of {} at pixel {:?}",
                            pattern.token(),
                            row.id(),
                            accuracy.max_error,
                            bound,
                            accuracy.worst_at
                        );
                    }
                }
            }
        }
    }
}
