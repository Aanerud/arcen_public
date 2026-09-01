use std::error::Error;
use std::fmt::{Display, Formatter};
use std::ops::Range;
use std::sync::OnceLock;

use arcen_keel::BgraFrame;

use super::frame::{I420FrameMut, I444FrameMut, I444P16FrameMut, Nv12FrameMut};
use crate::{BitDepth, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics};

/// Checked conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionError {
    GeometryMismatch,
    InvalidRowRange,
}

impl Display for ConversionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GeometryMismatch => {
                formatter.write_str("source and destination geometry do not match")
            }
            Self::InvalidRowRange => formatter
                .write_str("conversion row range must be ordered, in bounds, and even-aligned"),
        }
    }
}

impl Error for ConversionError {}

/// Channel positions for one packed 32-bit RGB source with ten bits per
/// colour component.
///
/// X11 depth 30 does not mandate one red/blue ordering. The live NVIDIA Xorg
/// server used by Arcen exposes red in bits 0..=9 and blue in bits 20..=29,
/// while `XRGB2101010` uses the opposite ordering. Callers must derive this
/// layout from the visual masks instead of naming the format from memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedRgb10Layout {
    red: u8,
    green: u8,
    blue: u8,
}

impl PackedRgb10Layout {
    /// Packed `XRGB2101010`: red high, blue low.
    pub const XRGB2101010: Self = Self {
        red: 20,
        green: 10,
        blue: 0,
    };

    /// Packed `XBGR2101010`: blue high, red low.
    pub const XBGR2101010: Self = Self {
        red: 0,
        green: 10,
        blue: 20,
    };

    /// Build a layout from three disjoint, contiguous ten-bit channel masks.
    #[must_use]
    pub fn from_masks(red: u32, green: u32, blue: u32) -> Option<Self> {
        fn shift(mask: u32) -> Option<u8> {
            if mask.count_ones() != 10 {
                return None;
            }
            let shift = mask.trailing_zeros();
            if 0x3ff_u32.checked_shl(shift)? != mask {
                return None;
            }
            u8::try_from(shift).ok()
        }

        if red & green != 0 || red & blue != 0 || green & blue != 0 {
            return None;
        }
        Some(Self {
            red: shift(red)?,
            green: shift(green)?,
            blue: shift(blue)?,
        })
    }

    /// Extract red, green, and blue from one packed word.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn components(self, word: u32) -> [u16; 3] {
        let component = |shift| ((word >> shift) & 0x3ff) as u16;
        [
            component(self.red),
            component(self.green),
            component(self.blue),
        ]
    }
}

/// Fixed-point fractional bits used by the conversion coefficients.
///
/// Sixteen keeps the worst-case intermediate (255 * a twelve-bit-scaled
/// coefficient, summed over three components) inside `i32` while leaving far
/// more headroom than the eight bits the original limited-range-only path
/// used. That headroom is what makes the ten-bit round trip exact.
const COEFF_SHIFT: u32 = 16;
const COEFF_ONE: i64 = 1 << COEFF_SHIFT;

/// Luma coefficients (Kr, Kg, Kb) for a matrix.
const fn luma_weights(matrix: ColorMatrix) -> (f64, f64, f64) {
    match matrix {
        // Identity carries G/B/R directly and never uses these.
        ColorMatrix::Identity | ColorMatrix::Bt709 => (0.2126, 0.7152, 0.0722),
        ColorMatrix::Bt601 => (0.299, 0.587, 0.114),
        ColorMatrix::Bt2020Ncl => (0.2627, 0.6780, 0.0593),
    }
}

/// How 8-bit BGRA is converted into coded samples at a chosen depth, range and
/// matrix.
///
/// The previous implementation hardcoded BT.709 limited range at eight bits
/// directly into the integer expressions, which meant the encoder could not
/// state what colour it was producing because it had no way to produce
/// anything else. Deriving the coefficients instead makes range and depth
/// negotiable, and lets two properties be *enforced* rather than hoped for:
///
/// * the luma coefficients sum exactly to the luma scale, so full white lands
///   on the top code rather than one below it; and
/// * each chroma triple sums exactly to zero, so a neutral grey produces
///   exactly the chroma centre instead of drifting by a code.
///
/// Both matter more than they sound: a colourist checking a grey ramp with an
/// eyedropper sees a one-code chroma drift immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorTransform {
    matrix: ColorMatrix,
    range: ColorRange,
    depth: BitDepth,
    /// Coefficients as [r, g, b] in `COEFF_SHIFT` fixed point.
    luma: [i32; 3],
    cb: [i32; 3],
    cr: [i32; 3],
    luma_offset: i32,
    chroma_center: i32,
    max_code: i32,
    /// Coded span of the luma channel: 219<<shift limited, `max_code` full.
    ///
    /// Stored rather than recomputed so the forward and inverse conversions
    /// cannot disagree about it, which is exactly how an identity round trip
    /// ends up one code short.
    luma_span: i32,
    /// Coded span of each chroma channel.
    chroma_span: i32,
    /// Left shift applied when storing into a 16-bit word (MSB alignment).
    store_shift: u32,
    /// Identity-matrix plane scaling, in fixed point.
    identity_scale: i32,
    identity_offset: i32,
}

#[allow(clippy::cast_possible_truncation)]
fn fixed(value: f64) -> i32 {
    #[allow(clippy::cast_precision_loss)]
    let scaled = value * COEFF_ONE as f64;
    scaled.round() as i32
}

impl ColorTransform {
    /// Build a transform for one coded format.
    #[must_use]
    pub fn new(matrix: ColorMatrix, range: ColorRange, depth: BitDepth) -> Self {
        Self::for_input_max(matrix, range, depth, 255.0)
    }

    /// Build a transform whose source components span `0..=input_max`.
    ///
    /// [`Self::new`] folds `1/255` into every coefficient because its input is
    /// 8-bit BGRA. A wide source — scRGB half-float promoted to 10-bit RGB
    /// codes, or a depth-30 X visual — spans a different range, and feeding it
    /// through the 8-bit coefficients is wrong by exactly the ratio of the two
    /// maxima. Rather than a second copy of the matrix derivation, which is how
    /// two conversions drift apart, the normalisation is a parameter and
    /// `new` is defined in terms of it.
    ///
    /// `input_max_8bit_matches_new` asserts the two agree bit for bit at 255,
    /// so the existing path is provably unchanged by this generalisation.
    #[must_use]
    pub fn for_input_max(
        matrix: ColorMatrix,
        range: ColorRange,
        depth: BitDepth,
        input_max: f64,
    ) -> Self {
        let (kr, _, kb) = luma_weights(matrix);
        let shift = u32::from(depth.bits()) - 8;
        let max_code = i32::from(depth.max_code());

        let (luma_scale, luma_offset, chroma_scale, chroma_center) = match range {
            ColorRange::Limited => (
                219 << shift,
                16 << shift,
                224 << shift,
                i32::from(128u16 << shift),
            ),
            // Full range spans every code, and centres chroma on the midpoint.
            ColorRange::Full => (max_code, 0, max_code, 1 << (u32::from(depth.bits()) - 1)),
        };

        // Every coefficient folds in the source normalisation as well as the
        // matrix and the range scaling, so the source span is the one thing
        // that must be right here.
        #[allow(clippy::cast_precision_loss)]
        let luma_unit = f64::from(luma_scale) / input_max;
        #[allow(clippy::cast_precision_loss)]
        let chroma_unit = f64::from(chroma_scale) / input_max;

        // Round the outer two and derive the middle so the triple sums exactly
        // to the luma scale: white must reach the top code exactly.
        let luma_r = fixed(kr * luma_unit);
        let luma_b = fixed(kb * luma_unit);
        let luma_total = fixed(luma_unit);
        let luma_g = luma_total - luma_r - luma_b;

        // Cb = (B - Y) * 0.5 / (1 - Kb); Cr = (R - Y) * 0.5 / (1 - Kr).
        let blue_gain = 0.5 / (1.0 - kb);
        let red_gain = 0.5 / (1.0 - kr);
        // The blue-difference and red-difference gains are the ones that set
        // saturation, so those are rounded and the green term absorbs the
        // residue, guaranteeing a zero sum.
        let cb_b = fixed((1.0 - kb) * blue_gain * chroma_unit);
        let cb_r = fixed(-kr * blue_gain * chroma_unit);
        let cb_g = -(cb_b + cb_r);
        let red_diff_r = fixed((1.0 - kr) * red_gain * chroma_unit);
        let red_diff_b = fixed(-kb * red_gain * chroma_unit);
        let red_diff_g = -(red_diff_r + red_diff_b);

        Self {
            matrix,
            range,
            depth,
            luma: [luma_r, luma_g, luma_b],
            cb: [cb_r, cb_g, cb_b],
            cr: [red_diff_r, red_diff_g, red_diff_b],
            luma_offset,
            chroma_center,
            max_code,
            luma_span: luma_scale,
            chroma_span: chroma_scale,
            store_shift: 16 - u32::from(depth.bits()),
            identity_scale: fixed(luma_unit),
            identity_offset: luma_offset,
        }
    }

    /// The BT.709 limited-range eight-bit transform Arcen shipped before
    /// colour was negotiable.
    #[must_use]
    pub fn legacy_bt709_limited() -> Self {
        Self::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight)
    }

    #[must_use]
    pub const fn matrix(self) -> ColorMatrix {
        self.matrix
    }

    #[must_use]
    pub const fn range(self) -> ColorRange {
        self.range
    }

    #[must_use]
    pub const fn depth(self) -> BitDepth {
        self.depth
    }

    /// Whether the coded planes carry G, B and R rather than Y, Cb and Cr.
    #[must_use]
    pub const fn is_identity(self) -> bool {
        self.matrix.is_identity()
    }

    #[inline]
    const fn clamp(self, value: i32) -> i32 {
        if value < 0 {
            0
        } else if value > self.max_code {
            self.max_code
        } else {
            value
        }
    }

    #[inline]
    fn apply(self, coefficients: [i32; 3], offset: i32, b: i32, g: i32, r: i32) -> i32 {
        let sum = coefficients[0] * r + coefficients[1] * g + coefficients[2] * b;
        self.clamp(offset + ((sum + (1 << (COEFF_SHIFT - 1))) >> COEFF_SHIFT))
    }

    /// Coded luma (or, for the identity matrix, the G plane) for one pixel.
    #[inline]
    #[must_use]
    pub fn luma(self, b: u8, g: u8, r: u8) -> i32 {
        let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
        if self.is_identity() {
            return self.scale_identity(g);
        }
        self.apply(self.luma, self.luma_offset, b, g, r)
    }

    /// Coded Cb (or, for the identity matrix, the B plane) for one pixel.
    #[inline]
    #[must_use]
    pub fn cb(self, b: u8, g: u8, r: u8) -> i32 {
        let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
        if self.is_identity() {
            return self.scale_identity(b);
        }
        self.apply(self.cb, self.chroma_center, b, g, r)
    }

    /// Coded Cr (or, for the identity matrix, the R plane) for one pixel.
    #[inline]
    #[must_use]
    pub fn cr(self, b: u8, g: u8, r: u8) -> i32 {
        let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
        if self.is_identity() {
            return self.scale_identity(r);
        }
        self.apply(self.cr, self.chroma_center, b, g, r)
    }

    /// Scale one 8-bit component into the coded range for identity/GBR.
    ///
    /// ITU-T H.273 leaves the three identity planes on the luma scaling, so a
    /// limited-range GBR stream compresses all three the same way rather than
    /// treating two of them as chroma.
    #[inline]
    const fn scale_identity(self, component: i32) -> i32 {
        self.clamp(
            self.identity_offset
                + ((self.identity_scale * component + (1 << (COEFF_SHIFT - 1))) >> COEFF_SHIFT),
        )
    }

    /// Coded luma for a wide source whose components span `0..=WIDE_INPUT_MAX`.
    ///
    /// Deliberately separate from [`Self::luma`] rather than a generic over the
    /// component type: the two take different input scales, and a transform
    /// built for one will silently produce wrong codes for the other. Two names
    /// make that a compile-time distinction instead of a runtime surprise.
    #[inline]
    #[must_use]
    pub fn luma_wide(self, b: u16, g: u16, r: u16) -> i32 {
        let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
        if self.is_identity() {
            return self.scale_identity(g);
        }
        self.apply(self.luma, self.luma_offset, b, g, r)
    }

    /// Coded Cb for a wide source. See [`Self::luma_wide`].
    #[inline]
    #[must_use]
    pub fn cb_wide(self, b: u16, g: u16, r: u16) -> i32 {
        let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
        if self.is_identity() {
            return self.scale_identity(b);
        }
        self.apply(self.cb, self.chroma_center, b, g, r)
    }

    /// Coded Cr for a wide source. See [`Self::luma_wide`].
    #[inline]
    #[must_use]
    pub fn cr_wide(self, b: u16, g: u16, r: u16) -> i32 {
        let (b, g, r) = (i32::from(b), i32::from(g), i32::from(r));
        if self.is_identity() {
            return self.scale_identity(r);
        }
        self.apply(self.cr, self.chroma_center, b, g, r)
    }

    /// Pack a coded sample into a 16-bit word, MSB-aligned.
    ///
    /// A ten-bit code becomes `code << 6`. Storing it unshifted is the classic
    /// way to get a picture four stops too dark, so this is the only place the
    /// shift is applied and [`ColorTransform::store_shift`] is derived from
    /// the depth rather than passed in.
    #[inline]
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub const fn pack_p16(self, code: i32) -> u16 {
        (self.clamp(code) as u16) << self.store_shift
    }

    /// Recover a coded sample from an MSB-aligned 16-bit word.
    #[inline]
    #[must_use]
    pub const fn unpack_p16(self, word: u16) -> i32 {
        (word >> self.store_shift) as i32
    }

    /// Convert one coded pixel back to 8-bit BGRA order (b, g, r).
    ///
    /// Present so round-trip accuracy can be asserted in tests and measured by
    /// the probe harness rather than assumed.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn to_bgr8(self, luma: i32, cb: i32, cr: i32) -> (u8, u8, u8) {
        if self.is_identity() {
            // Forward is `offset + round(span * component / 255)`, so the
            // inverse is `round((coded - offset) * 255 / span)`. Deriving it
            // from the same stored span is what makes a full-range identity
            // round trip exactly lossless rather than one code short.
            let unscale = |value: i32| -> u8 {
                if self.luma_span == 0 {
                    return 0;
                }
                let numerator = (value - self.identity_offset) * 255;
                let scaled = (numerator + self.luma_span / 2) / self.luma_span;
                scaled.clamp(0, 255) as u8
            };
            return (unscale(cb), unscale(luma), unscale(cr));
        }
        let (kr, kg, kb) = luma_weights(self.matrix);
        let luma_norm = f64::from(luma - self.luma_offset) / f64::from(self.luma_span);
        let blue_diff = f64::from(cb - self.chroma_center) / f64::from(self.chroma_span);
        let red_diff = f64::from(cr - self.chroma_center) / f64::from(self.chroma_span);
        let red = luma_norm + 2.0 * (1.0 - kr) * red_diff;
        let blue = luma_norm + 2.0 * (1.0 - kb) * blue_diff;
        let green = (luma_norm - kr * red - kb * blue) / kg;
        let to_u8 = |value: f64| -> u8 { (value * 255.0).round().clamp(0.0, 255.0) as u8 };
        (to_u8(blue), to_u8(green), to_u8(red))
    }
}

impl Default for ColorTransform {
    fn default() -> Self {
        Self::legacy_bt709_limited()
    }
}

#[inline]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn as_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn validate_rows(
    source: BgraFrame<'_>,
    width: usize,
    height: usize,
    rows: &Range<usize>,
) -> Result<(), ConversionError> {
    let grid = source.grid();
    if grid.width() != width || grid.height() != height {
        return Err(ConversionError::GeometryMismatch);
    }
    if rows.start > rows.end || rows.end > height || rows.start % 2 != 0 || rows.end % 2 != 0 {
        return Err(ConversionError::InvalidRowRange);
    }
    Ok(())
}

/// Validate a row range for an unsubsampled destination.
///
/// 4:4:4 rows are independent, so there is no even-alignment requirement.
fn validate_rows_unsubsampled(
    source: BgraFrame<'_>,
    width: usize,
    height: usize,
    rows: &Range<usize>,
) -> Result<(), ConversionError> {
    let grid = source.grid();
    if grid.width() != width || grid.height() != height {
        return Err(ConversionError::GeometryMismatch);
    }
    if rows.start > rows.end || rows.end > height {
        return Err(ConversionError::InvalidRowRange);
    }
    Ok(())
}

/// Convert a complete checked BGRA frame to NV12 under `transform`.
///
/// # Errors
///
/// Returns an error when source and destination geometry differ.
pub fn convert_bgra_to_nv12(
    source: BgraFrame<'_>,
    destination: &mut Nv12FrameMut<'_>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    let height = destination.height;
    convert_bgra_to_nv12_rows(source, destination, 0..height, transform)
}

/// Convert an even, full-width row range to NV12 under `transform`.
///
/// Padding remains untouched and the conversion allocates no memory.
///
/// # Errors
///
/// Rejects geometry mismatch and invalid row ranges.
pub fn convert_bgra_to_nv12_rows(
    source: BgraFrame<'_>,
    destination: &mut Nv12FrameMut<'_>,
    rows: Range<usize>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    validate_rows(source, destination.width, destination.height, &rows)?;
    convert_rows(
        source,
        destination.width,
        rows,
        destination.y,
        destination.y_stride,
        ChromaPlanes::Nv12 {
            uv: destination.uv,
            stride: destination.uv_stride,
        },
        transform,
    )
}

/// Convert a complete checked BGRA frame directly to I420 under `transform`.
///
/// # Errors
///
/// Returns an error when source and destination geometry differ.
pub fn convert_bgra_to_i420(
    source: BgraFrame<'_>,
    destination: &mut I420FrameMut<'_>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    let height = destination.height;
    convert_bgra_to_i420_rows(source, destination, 0..height, transform)
}

/// Convert an even, full-width row range directly to I420 under `transform`.
///
/// Padding remains untouched and the conversion allocates no memory.
///
/// # Errors
///
/// Rejects geometry mismatch and invalid row ranges.
pub fn convert_bgra_to_i420_rows(
    source: BgraFrame<'_>,
    destination: &mut I420FrameMut<'_>,
    rows: Range<usize>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    validate_rows(source, destination.width, destination.height, &rows)?;
    convert_rows(
        source,
        destination.width,
        rows,
        destination.y,
        destination.y_stride,
        ChromaPlanes::I420 {
            u: destination.u,
            u_stride: destination.u_stride,
            v: destination.v,
            v_stride: destination.v_stride,
        },
        transform,
    )
}

/// Convert a complete checked BGRA frame to 8-bit planar 4:4:4.
///
/// # Errors
///
/// Returns an error when source and destination geometry differ.
pub fn convert_bgra_to_i444(
    source: BgraFrame<'_>,
    destination: &mut I444FrameMut<'_>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    let height = destination.height;
    convert_bgra_to_i444_rows(source, destination, 0..height, transform)
}

/// Convert a full-width row range to 8-bit planar 4:4:4.
///
/// # Errors
///
/// Rejects geometry mismatch and invalid row ranges.
pub fn convert_bgra_to_i444_rows(
    source: BgraFrame<'_>,
    destination: &mut I444FrameMut<'_>,
    rows: Range<usize>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    validate_rows_unsubsampled(source, destination.width, destination.height, &rows)?;
    let width = destination.width;
    let strides = destination.strides;
    for row in rows {
        let Some(pixels) = source.active_row(row) else {
            return Err(ConversionError::GeometryMismatch);
        };
        for (column, pixel) in pixels.chunks_exact(4).take(width).enumerate() {
            let (b, g, r) = (pixel[0], pixel[1], pixel[2]);
            destination.planes[0][row * strides[0] + column] = as_u8(transform.luma(b, g, r));
            destination.planes[1][row * strides[1] + column] = as_u8(transform.cb(b, g, r));
            destination.planes[2][row * strides[2] + column] = as_u8(transform.cr(b, g, r));
        }
    }
    Ok(())
}

/// Convert a complete checked BGRA frame to 16-bit planar 4:4:4.
///
/// Samples are stored MSB-aligned; see [`I444P16FrameMut`].
///
/// # Errors
///
/// Returns an error when source and destination geometry differ.
pub fn convert_bgra_to_i444_p16(
    source: BgraFrame<'_>,
    destination: &mut I444P16FrameMut<'_>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    let height = destination.height;
    convert_bgra_to_i444_p16_rows(source, destination, 0..height, transform)
}

/// Convert a full-width row range to 16-bit planar 4:4:4.
///
/// This is the grading hot path: at 3008x1692 it runs five million pixels per
/// frame, and it was measured at 47-48 ms serial before the conversion was
/// split across row workers. The arithmetic is therefore hoisted into
/// [`FusedI444Kernel`] rather than re-derived per component per pixel, but it
/// is **the same arithmetic** — [`ColorTransform`] remains the reference
/// oracle and `fused_i444_p16_conversion_matches_the_scalar_oracle` proves the
/// two agree bit for bit across every matrix, range, depth, padded stride and
/// edge component value.
///
/// # Errors
///
/// Rejects geometry mismatch and invalid row ranges.
pub fn convert_bgra_to_i444_p16_rows(
    source: BgraFrame<'_>,
    destination: &mut I444P16FrameMut<'_>,
    rows: Range<usize>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    validate_rows_unsubsampled(source, destination.width, destination.height, &rows)?;
    let width = destination.width;
    let strides = destination.strides;
    let kernel = FusedI444Kernel::new(transform);
    // Borrowed once, outside the loop: three separate indexed writes per pixel
    // into `destination.planes[n][row * stride + column]` cost three bounds
    // checks per pixel and defeat vectorisation. Row slices taken up front cost
    // three per row.
    let [y_plane, u_plane, v_plane] = &mut destination.planes;
    for row in rows {
        let Some(pixels) = source.active_row(row) else {
            return Err(ConversionError::GeometryMismatch);
        };
        let y_start = row * strides[0];
        let u_start = row * strides[1];
        let v_start = row * strides[2];
        // In range for every `row < height`: `I444P16FrameMut::new` proves
        // `stride >= width` and truncates each plane to exactly
        // `stride * height` samples.
        kernel.convert_row(
            pixels,
            &mut y_plane[y_start..y_start + width],
            &mut u_plane[u_start..u_start + width],
            &mut v_plane[v_start..v_start + width],
        );
    }
    Ok(())
}

/// Per-frame constants for the 4:4:4 inner loop, lifted out of the pixel loop.
///
/// Every field is exactly what [`ColorTransform`] would have recomputed or
/// re-branched on for each of the three components of each pixel:
///
/// * `identity` replaces three `is_identity()` tests per pixel with one test
///   per row;
/// * the coefficient triples, offsets and centre are loaded once instead of
///   through a `self` field access per component; and
/// * the clamp and the MSB-alignment shift are fused, because
///   `ColorTransform::apply` already clamps to `0..=max_code` and
///   `pack_p16` then clamps the same value again. Clamping is idempotent, so
///   dropping the second one cannot change a single output code.
///
/// Nothing here changes the fixed-point rounding, the clamping bounds, the
/// plane order or the MSB alignment. Those are the properties a grader would
/// notice, so they are pinned by tests against the scalar oracle rather than
/// by inspection.
#[derive(Clone, Copy)]
struct FusedI444Kernel {
    identity: bool,
    luma: [i32; 3],
    cb: [i32; 3],
    cr: [i32; 3],
    luma_offset: i32,
    chroma_center: i32,
    identity_scale: i32,
    identity_offset: i32,
    max_code: i32,
    store_shift: u32,
}

impl FusedI444Kernel {
    fn new(transform: ColorTransform) -> Self {
        Self {
            identity: transform.is_identity(),
            luma: transform.luma,
            cb: transform.cb,
            cr: transform.cr,
            luma_offset: transform.luma_offset,
            chroma_center: transform.chroma_center,
            identity_scale: transform.identity_scale,
            identity_offset: transform.identity_offset,
            max_code: transform.max_code,
            store_shift: transform.store_shift,
        }
    }

    /// Clamp to the coded range and store MSB-aligned, in one step.
    #[inline]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    const fn pack(self, value: i32) -> u16 {
        let clamped = if value < 0 {
            0
        } else if value > self.max_code {
            self.max_code
        } else {
            value
        };
        (clamped as u16) << self.store_shift
    }

    #[inline]
    fn matrix_sample(self, coefficients: [i32; 3], offset: i32, b: i32, g: i32, r: i32) -> u16 {
        let sum = coefficients[0] * r + coefficients[1] * g + coefficients[2] * b;
        self.pack(offset + ((sum + (1 << (COEFF_SHIFT - 1))) >> COEFF_SHIFT))
    }

    #[inline]
    const fn identity_sample(self, component: i32) -> u16 {
        self.pack(
            self.identity_offset
                + ((self.identity_scale * component + (1 << (COEFF_SHIFT - 1))) >> COEFF_SHIFT),
        )
    }

    /// Convert one row into three pre-sliced destination rows.
    ///
    /// The identity test is resolved here, once per row, so neither inner loop
    /// carries it. Both loops load each pixel's B, G and R exactly once and
    /// write all three planes in the same iteration.
    #[inline]
    fn convert_row(
        self,
        pixels: &[u8],
        plane_y: &mut [u16],
        plane_u: &mut [u16],
        plane_v: &mut [u16],
    ) {
        if self.identity {
            self.convert_row_identity(pixels, plane_y, plane_u, plane_v);
        } else {
            self.convert_row_matrix(pixels, plane_y, plane_u, plane_v);
        }
    }

    #[inline]
    fn convert_row_matrix(
        self,
        pixels: &[u8],
        plane_y: &mut [u16],
        plane_u: &mut [u16],
        plane_v: &mut [u16],
    ) {
        for (pixel, ((luma, blue_diff), red_diff)) in pixels.chunks_exact(4).zip(
            plane_y
                .iter_mut()
                .zip(plane_u.iter_mut())
                .zip(plane_v.iter_mut()),
        ) {
            let b = i32::from(pixel[0]);
            let g = i32::from(pixel[1]);
            let r = i32::from(pixel[2]);
            *luma = self.matrix_sample(self.luma, self.luma_offset, b, g, r);
            *blue_diff = self.matrix_sample(self.cb, self.chroma_center, b, g, r);
            *red_diff = self.matrix_sample(self.cr, self.chroma_center, b, g, r);
        }
    }

    /// Identity carries G, B and R directly, on the luma scaling (H.273).
    #[inline]
    fn convert_row_identity(
        self,
        pixels: &[u8],
        plane_y: &mut [u16],
        plane_u: &mut [u16],
        plane_v: &mut [u16],
    ) {
        for (pixel, ((green, blue), red)) in pixels.chunks_exact(4).zip(
            plane_y
                .iter_mut()
                .zip(plane_u.iter_mut())
                .zip(plane_v.iter_mut()),
        ) {
            *green = self.identity_sample(i32::from(pixel[1]));
            *blue = self.identity_sample(i32::from(pixel[0]));
            *red = self.identity_sample(i32::from(pixel[2]));
        }
    }
}

enum ChromaPlanes<'a> {
    Nv12 {
        uv: &'a mut [u8],
        stride: usize,
    },
    I420 {
        u: &'a mut [u8],
        u_stride: usize,
        v: &'a mut [u8],
        v_stride: usize,
    },
}

#[allow(clippy::many_single_char_names)]
fn convert_rows(
    source: BgraFrame<'_>,
    width: usize,
    rows: Range<usize>,
    y: &mut [u8],
    y_stride: usize,
    mut chroma: ChromaPlanes<'_>,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    for row_pair in rows.start / 2..rows.end / 2 {
        let top = row_pair * 2;
        let Some(s0) = source.active_row(top) else {
            return Err(ConversionError::GeometryMismatch);
        };
        let Some(s1) = source.active_row(top + 1) else {
            return Err(ConversionError::GeometryMismatch);
        };
        let (y_head, y_tail) = y.split_at_mut((top + 1) * y_stride);
        let y0 = &mut y_head[top * y_stride..top * y_stride + width];
        let y1 = &mut y_tail[..width];

        for (pair, (p0, p1)) in s0.chunks_exact(8).zip(s1.chunks_exact(8)).enumerate() {
            let x = pair * 2;
            y0[x] = as_u8(transform.luma(p0[0], p0[1], p0[2]));
            y0[x + 1] = as_u8(transform.luma(p0[4], p0[5], p0[6]));
            y1[x] = as_u8(transform.luma(p1[0], p1[1], p1[2]));
            y1[x + 1] = as_u8(transform.luma(p1[4], p1[5], p1[6]));

            // Average in linear-code space before converting, matching the
            // original behaviour: averaging the four RGB samples and then
            // converting once is both cheaper and closer to a box filter than
            // converting four times and averaging the results.
            let mean = |a: u8, b: u8, c: u8, d: u8| -> u8 {
                as_u8((i32::from(a) + i32::from(b) + i32::from(c) + i32::from(d)) >> 2)
            };
            let b = mean(p0[0], p0[4], p1[0], p1[4]);
            let g = mean(p0[1], p0[5], p1[1], p1[5]);
            let r = mean(p0[2], p0[6], p1[2], p1[6]);
            let u = as_u8(transform.cb(b, g, r));
            let v = as_u8(transform.cr(b, g, r));

            match &mut chroma {
                ChromaPlanes::Nv12 { uv, stride } => {
                    let offset = row_pair * *stride + x;
                    uv[offset] = u;
                    uv[offset + 1] = v;
                }
                ChromaPlanes::I420 {
                    u: u_plane,
                    u_stride,
                    v: v_plane,
                    v_stride,
                } => {
                    u_plane[row_pair * *u_stride + pair] = u;
                    v_plane[row_pair * *v_stride + pair] = v;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Wide (>8 bpc) source conversion
// ---------------------------------------------------------------------------

/// The sRGB encoding transfer function: linear light in, signal out.
///
/// scRGB carries **linear** light, so a wide capture cannot go straight into
/// the colour matrix — that expects a nonlinear signal, exactly as the 8-bit
/// BGRA path supplies. Skipping this step produces a picture several stops too
/// dark and is the easiest way to get a wide pipeline visibly wrong.
#[must_use]
pub fn linear_to_srgb(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

/// ITU-R BT.709 OETF: linear light in, nonlinear signal out.
#[must_use]
pub fn linear_to_bt709(linear: f32) -> f32 {
    if linear < 0.018 {
        4.5 * linear
    } else {
        1.099 * linear.powf(0.45) - 0.099
    }
}

/// Decode an IEEE-754 binary16 into `f32`.
#[must_use]
pub fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let out = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            let mut e = exponent;
            let mut m = mantissa;
            while m & 0x400 == 0 {
                m <<= 1;
                e = e.wrapping_sub(1);
            }
            sign | ((e.wrapping_add(1).wrapping_add(127 - 15)) << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => sign | (0xff << 23) | (mantissa << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(out)
}

/// Source span for a wide RGB signal quantised before the matrix.
///
/// Ten bits is the widest depth NVENC accepts here, so carrying more through
/// the matrix buys nothing the encoder can express.
pub const WIDE_INPUT_MAX: u16 = 1023;

/// Absolute luminance represented by linear scRGB `1.0` on Windows.
///
/// Windows' Advanced Color composition space fixes scRGB reference white at
/// 80 cd/m². Values above `1.0` therefore carry real HDR headroom and must not
/// be clamped before the PQ transfer.
const SCRGB_REFERENCE_WHITE_NITS: f32 = 80.0;

const PQ_PEAK_NITS: f32 = 10_000.0;
const PQ_CODE_COUNT: usize = WIDE_INPUT_MAX as usize + 1;

/// Convert linear-light sRGB/scRGB primaries into the negotiated linear RGB
/// primaries before applying a nonlinear transfer function.
fn scrgb_primary_matrix(output: ColorPrimaries) -> [[f32; 3]; 3] {
    match output {
        ColorPrimaries::Bt709 => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        // Linear sRGB/BT.709 D65 -> linear BT.2020 D65.
        ColorPrimaries::Bt2020 => [
            [0.627_403_9, 0.329_283_03, 0.043_313_07],
            [0.069_097_29, 0.919_540_4, 0.011_362_32],
            [0.016_391_44, 0.088_013_31, 0.895_595_25],
        ],
        // Linear sRGB/BT.709 D65 -> linear Display P3 D65.
        ColorPrimaries::DisplayP3 => [
            [0.822_461_96, 0.177_538_04, 0.0],
            [0.033_194_2, 0.966_805_8, 0.0],
            [0.017_082_63, 0.072_397_44, 0.910_519_96],
        ],
    }
}

fn convert_scrgb_primaries(r: f32, g: f32, b: f32, output: ColorPrimaries) -> [f32; 3] {
    let matrix = scrgb_primary_matrix(output);
    let apply = |row: [f32; 3]| row[0] * r + row[1] * g + row[2] * b;
    [apply(matrix[0]), apply(matrix[1]), apply(matrix[2])]
}

/// SMPTE ST 2084 inverse EOTF: absolute luminance in nits to PQ signal.
#[must_use]
pub fn linear_nits_to_pq_signal(nits: f32) -> f32 {
    if !nits.is_finite() || nits <= 0.0 {
        return 0.0;
    }
    let normalized = (nits / PQ_PEAK_NITS).min(1.0);
    let m1 = 2610.0 / 16_384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let powered = normalized.powf(m1);
    ((c1 + c2 * powered) / (1.0 + c3 * powered)).powf(m2)
}

/// SMPTE ST 2084 EOTF: PQ signal to absolute luminance in nits.
fn pq_signal_to_linear_nits(signal: f64) -> f64 {
    if !signal.is_finite() || signal <= 0.0 {
        return 0.0;
    }
    let m1 = 2610.0 / 16_384.0;
    let m2 = 2523.0 / 32.0;
    let c1 = 3424.0 / 4096.0;
    let c2 = 2413.0 / 128.0;
    let c3 = 2392.0 / 128.0;
    let powered = signal.min(1.0).powf(1.0 / m2);
    let numerator = (powered - c1).max(0.0);
    let denominator = c2 - c3 * powered;
    (numerator / denominator).powf(1.0 / m1) * f64::from(PQ_PEAK_NITS)
}

/// Linear-scRGB thresholds at which nearest-integer ten-bit PQ quantisation
/// advances to the next code.
///
/// The expensive inverse EOTF runs only while this process-wide table is built.
/// Each hot-path component then needs a ten-comparison binary search rather
/// than two `powf` calls. Threshold `n` is the exact half-code boundary between
/// output codes `n` and `n + 1`.
fn scrgb_pq_code_thresholds() -> &'static [f64; PQ_CODE_COUNT - 1] {
    static THRESHOLDS: OnceLock<[f64; PQ_CODE_COUNT - 1]> = OnceLock::new();
    THRESHOLDS.get_or_init(|| {
        std::array::from_fn(|index| {
            // `index` is bounded by this 1023-entry array, far below f64's
            // exact-integer limit.
            #[allow(clippy::cast_precision_loss)]
            let signal = (index as f64 + 0.5) / f64::from(WIDE_INPUT_MAX);
            pq_signal_to_linear_nits(signal) / f64::from(SCRGB_REFERENCE_WHITE_NITS)
        })
    })
}

/// Quantise one linear component in the target primaries to a ten-bit PQ code.
#[must_use]
pub fn scrgb_component_to_pq_code(linear: f32) -> u16 {
    if !linear.is_finite() || linear <= 0.0 {
        return 0;
    }
    let linear = f64::from(linear);
    let code = scrgb_pq_code_thresholds().partition_point(|threshold| linear >= *threshold);
    u16::try_from(code).unwrap_or(WIDE_INPUT_MAX)
}

/// Complete colour transform for Windows FP16 scRGB capture into PQ YCbCr.
///
/// Construction binds target primaries, matrix, range and depth together once
/// so the per-frame conversion cannot accidentally apply PQ with a transform
/// built for 8-bit RGB input or for a different matrix.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrgbPqTransform {
    output_primaries: ColorPrimaries,
    ycbcr: ColorTransform,
}

impl ScrgbPqTransform {
    #[must_use]
    pub fn new(
        matrix: ColorMatrix,
        output_primaries: ColorPrimaries,
        range: ColorRange,
        depth: BitDepth,
    ) -> Self {
        Self {
            output_primaries,
            ycbcr: ColorTransform::for_input_max(matrix, range, depth, f64::from(WIDE_INPUT_MAX)),
        }
    }

    fn pq_rgb(self, r: f32, g: f32, b: f32) -> [u16; 3] {
        convert_scrgb_primaries(r, g, b, self.output_primaries).map(scrgb_component_to_pq_code)
    }

    fn convert_pq_rgb(self, [r, g, b]: [u16; 3]) -> [u16; 3] {
        [
            self.ycbcr.pack_p16(self.ycbcr.luma_wide(b, g, r)),
            self.ycbcr.pack_p16(self.ycbcr.cb_wide(b, g, r)),
            self.ycbcr.pack_p16(self.ycbcr.cr_wide(b, g, r)),
        ]
    }

    fn convert_pixel(self, r: f32, g: f32, b: f32) -> [u16; 3] {
        self.convert_pq_rgb(self.pq_rgb(r, g, b))
    }
}

const HALF_CODE_COUNT: usize = 65_536;

fn scrgb_sdr_code_table(transfer: TransferCharacteristics) -> &'static [u16; HALF_CODE_COUNT] {
    static BT709: OnceLock<[u16; HALF_CODE_COUNT]> = OnceLock::new();
    static SRGB: OnceLock<[u16; HALF_CODE_COUNT]> = OnceLock::new();
    let build = |transfer| {
        std::array::from_fn(|bits| {
            let linear = half_to_f32(u16::try_from(bits).unwrap_or(u16::MAX)).clamp(0.0, 1.0);
            let signal = match transfer {
                TransferCharacteristics::Bt709 => linear_to_bt709(linear),
                TransferCharacteristics::Srgb => linear_to_srgb(linear),
                TransferCharacteristics::Pq | TransferCharacteristics::Hlg => {
                    unreachable!("SDR lookup tables are built only for SDR transfers")
                }
            };
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            {
                (signal * f32::from(WIDE_INPUT_MAX))
                    .round()
                    .clamp(0.0, f32::from(WIDE_INPUT_MAX)) as u16
            }
        })
    };
    match transfer {
        TransferCharacteristics::Bt709 => {
            BT709.get_or_init(|| build(TransferCharacteristics::Bt709))
        }
        TransferCharacteristics::Srgb => SRGB.get_or_init(|| build(TransferCharacteristics::Srgb)),
        TransferCharacteristics::Pq | TransferCharacteristics::Hlg => {
            unreachable!("SDR transform refuses HDR transfer characteristics")
        }
    }
}

/// Transform Windows FP16 scRGB capture into ten-bit SDR YCbCr.
///
/// The source is linear BT.709/scRGB. Values outside SDR range are clamped,
/// the requested SDR OETF is applied through a process-wide half-float lookup,
/// and the result is packed through the negotiated matrix/range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrgbSdrTransform {
    transfer: TransferCharacteristics,
    ycbcr: ColorTransform,
}

impl ScrgbSdrTransform {
    #[must_use]
    pub fn new(
        matrix: ColorMatrix,
        range: ColorRange,
        depth: BitDepth,
        transfer: TransferCharacteristics,
    ) -> Option<Self> {
        matches!(
            transfer,
            TransferCharacteristics::Bt709 | TransferCharacteristics::Srgb
        )
        .then(|| Self {
            transfer,
            ycbcr: ColorTransform::for_input_max(matrix, range, depth, f64::from(WIDE_INPUT_MAX)),
        })
    }

    fn convert_half_pixel(self, r: u16, g: u16, b: u16) -> [u16; 3] {
        let table = scrgb_sdr_code_table(self.transfer);
        let [r, g, b] = [
            table[usize::from(r)],
            table[usize::from(g)],
            table[usize::from(b)],
        ];
        [
            self.ycbcr.pack_p16(self.ycbcr.luma_wide(b, g, r)),
            self.ycbcr.pack_p16(self.ycbcr.cb_wide(b, g, r)),
            self.ycbcr.pack_p16(self.ycbcr.cr_wide(b, g, r)),
        ]
    }
}

/// Convert one FP16 scRGB frame into PQ-coded planar 10-bit 4:4:4.
///
/// This is the capture half of HDR meeting the encoder. WGC's only wide pool
/// format is `R16G16B16A16Float`, which carries linear scRGB with 1.0 at SDR
/// white; NVENC wants `YUV444_10BIT`. Nothing in the 8-bit path can be reused,
/// because reading half-floats as bytes is not a lossy conversion, it is a
/// meaningless one.
///
/// The conversion is deliberately ordered:
///
/// 1. convert linear scRGB/BT.709 primaries into `output_primaries`;
/// 2. interpret scRGB `1.0` as 80 nits and apply SMPTE ST 2084;
/// 3. apply the negotiated nonlinear RGB-to-YCbCr matrix; and
/// 4. store the result MSB-aligned for NVENC.
///
/// Values above scRGB `1.0` survive through PQ instead of being clipped at SDR
/// white. Negative and non-finite target-primary components map to black.
///
/// # Errors
///
/// [`ConversionError`] when a source row or a destination plane is too small
/// for the stated geometry, checked before any write.
pub fn convert_scrgb_to_pq_i444_p16(
    src: &[u8],
    src_stride: usize,
    planes: [&mut [u16]; 3],
    plane_strides: [usize; 3],
    width: usize,
    height: usize,
    transform: ScrgbPqTransform,
) -> Result<(), ConversionError> {
    // Four half-float components per pixel.
    let row_bytes = width
        .checked_mul(8)
        .ok_or(ConversionError::GeometryMismatch)?;
    if src_stride < row_bytes || src.len() < src_stride.saturating_mul(height) {
        return Err(ConversionError::GeometryMismatch);
    }
    let [y_plane, u_plane, v_plane] = planes;
    for (plane, stride) in [
        (&*y_plane, plane_strides[0]),
        (&*u_plane, plane_strides[1]),
        (&*v_plane, plane_strides[2]),
    ] {
        if stride < width || plane.len() < stride.saturating_mul(height) {
            return Err(ConversionError::GeometryMismatch);
        }
    }

    for row in 0..height {
        let src_row = &src[row * src_stride..row * src_stride + row_bytes];
        let y_row = &mut y_plane[row * plane_strides[0]..row * plane_strides[0] + width];
        let u_row = &mut u_plane[row * plane_strides[1]..row * plane_strides[1] + width];
        let v_row = &mut v_plane[row * plane_strides[2]..row * plane_strides[2] + width];
        for column in 0..width {
            let base = column * 8;
            let half = |offset: usize| {
                u16::from_le_bytes([src_row[base + offset], src_row[base + offset + 1]])
            };
            // scRGB is RGBA order; alpha is ignored, as it is in the 8-bit path.
            let [y, u, v] = transform.convert_pixel(
                half_to_f32(half(0)),
                half_to_f32(half(2)),
                half_to_f32(half(4)),
            );
            y_row[column] = y;
            u_row[column] = u;
            v_row[column] = v;
        }
    }
    Ok(())
}

/// Convert one FP16 scRGB frame into SDR-coded planar 10-bit 4:4:4.
///
/// # Errors
///
/// Returns [`ConversionError::GeometryMismatch`] when source or destination
/// geometry cannot cover the complete frame.
pub fn convert_scrgb_to_sdr_i444_p16(
    src: &[u8],
    src_stride: usize,
    planes: [&mut [u16]; 3],
    plane_strides: [usize; 3],
    width: usize,
    height: usize,
    transform: ScrgbSdrTransform,
) -> Result<(), ConversionError> {
    let row_bytes = width
        .checked_mul(8)
        .ok_or(ConversionError::GeometryMismatch)?;
    if src_stride < row_bytes || src.len() < src_stride.saturating_mul(height) {
        return Err(ConversionError::GeometryMismatch);
    }
    let [y_plane, u_plane, v_plane] = planes;
    for (plane, stride) in [
        (&*y_plane, plane_strides[0]),
        (&*u_plane, plane_strides[1]),
        (&*v_plane, plane_strides[2]),
    ] {
        if stride < width || plane.len() < stride.saturating_mul(height) {
            return Err(ConversionError::GeometryMismatch);
        }
    }

    for row in 0..height {
        let src_row = &src[row * src_stride..row * src_stride + row_bytes];
        let y_row = &mut y_plane[row * plane_strides[0]..row * plane_strides[0] + width];
        let u_row = &mut u_plane[row * plane_strides[1]..row * plane_strides[1] + width];
        let v_row = &mut v_plane[row * plane_strides[2]..row * plane_strides[2] + width];
        for column in 0..width {
            let base = column * 8;
            let half = |offset: usize| {
                u16::from_le_bytes([src_row[base + offset], src_row[base + offset + 1]])
            };
            let [y, u, v] = transform.convert_half_pixel(half(0), half(2), half(4));
            y_row[column] = y;
            u_row[column] = u;
            v_row[column] = v;
        }
    }
    Ok(())
}

/// Convert a packed RGB10 frame to planar 4:4:4 sixteen-bit.
///
/// This is the portable conversion used by the X11 depth-30 capture path.
/// `XShmGetImage` returns the screen visual's native channel ordering, which
/// differs across drivers, so `layout` is derived from the visual masks.
///
/// No rescaling happens here, deliberately. Each channel already arrives as
/// a `0..=1023` code, which is precisely the domain
/// [`ColorTransform::luma_wide`] and friends expect
/// ([`WIDE_INPUT_MAX`]), so the components are handed over untouched. The
/// transform must have been built with
/// [`ColorTransform::for_input_max`] against [`WIDE_INPUT_MAX`]; one built
/// by `ColorTransform::new` folds a `1/255` factor into every coefficient
/// and would quietly scale a ten-bit input down by roughly four, worst in
/// the highlights.
///
/// Alpha is discarded, as it is in every other capture path here.
///
/// # Errors
///
/// [`ConversionError`] when a source row or a destination plane is too small
/// for the stated geometry, checked before any write.
#[allow(clippy::too_many_arguments)]
pub fn convert_packed_rgb10_to_i444_p16(
    src: &[u8],
    src_stride: usize,
    planes: [&mut [u16]; 3],
    plane_strides: [usize; 3],
    width: usize,
    height: usize,
    layout: PackedRgb10Layout,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    // One 32-bit word per pixel.
    let row_bytes = width
        .checked_mul(4)
        .ok_or(ConversionError::GeometryMismatch)?;
    if src_stride < row_bytes || src.len() < src_stride.saturating_mul(height) {
        return Err(ConversionError::GeometryMismatch);
    }
    let [y_plane, u_plane, v_plane] = planes;
    for (plane, stride) in [
        (&*y_plane, plane_strides[0]),
        (&*u_plane, plane_strides[1]),
        (&*v_plane, plane_strides[2]),
    ] {
        if stride < width || plane.len() < stride.saturating_mul(height) {
            return Err(ConversionError::GeometryMismatch);
        }
    }

    for row in 0..height {
        let src_row = &src[row * src_stride..row * src_stride + row_bytes];
        let y_row = &mut y_plane[row * plane_strides[0]..row * plane_strides[0] + width];
        let u_row = &mut u_plane[row * plane_strides[1]..row * plane_strides[1] + width];
        let v_row = &mut v_plane[row * plane_strides[2]..row * plane_strides[2] + width];
        for column in 0..width {
            let base = column * 4;
            let word = u32::from_le_bytes([
                src_row[base],
                src_row[base + 1],
                src_row[base + 2],
                src_row[base + 3],
            ]);
            let [r, g, b] = layout.components(word);
            y_row[column] = transform.pack_p16(transform.luma_wide(b, g, r));
            u_row[column] = transform.pack_p16(transform.cb_wide(b, g, r));
            v_row[column] = transform.pack_p16(transform.cr_wide(b, g, r));
        }
    }
    Ok(())
}

#[allow(clippy::cast_possible_truncation)]
fn mean_rgb10(a: u16, b: u16, c: u16, d: u16) -> u16 {
    ((u32::from(a) + u32::from(b) + u32::from(c) + u32::from(d)) / 4) as u16
}

/// Convert a packed RGB10 frame to MSB-aligned P010.
///
/// Luma is converted at full resolution. Each interleaved Cb/Cr pair is
/// derived from the mean RGB value of its 2×2 source block, matching the
/// chroma filter used by the existing BGRA-to-4:2:0 paths.
///
/// # Errors
///
/// Returns [`ConversionError::GeometryMismatch`] for zero or odd dimensions,
/// overflow, or any source/destination row that is too short.
#[allow(clippy::many_single_char_names, clippy::too_many_arguments)]
pub fn convert_packed_rgb10_to_p010(
    src: &[u8],
    src_stride: usize,
    y: &mut [u16],
    y_stride: usize,
    uv: &mut [u16],
    uv_stride: usize,
    width: usize,
    height: usize,
    layout: PackedRgb10Layout,
    transform: ColorTransform,
) -> Result<(), ConversionError> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or(ConversionError::GeometryMismatch)?;
    if width == 0
        || height == 0
        || width % 2 != 0
        || height % 2 != 0
        || src_stride < row_bytes
        || y_stride < width
        || uv_stride < width
        || src.len() < src_stride.saturating_mul(height)
        || y.len() < y_stride.saturating_mul(height)
        || uv.len() < uv_stride.saturating_mul(height / 2)
    {
        return Err(ConversionError::GeometryMismatch);
    }

    let pixel = |row: &[u8], column: usize| {
        let base = column * 4;
        layout.components(u32::from_le_bytes([
            row[base],
            row[base + 1],
            row[base + 2],
            row[base + 3],
        ]))
    };
    for row_pair in 0..height / 2 {
        let top = row_pair * 2;
        let src0 = &src[top * src_stride..top * src_stride + row_bytes];
        let src1 = &src[(top + 1) * src_stride..(top + 1) * src_stride + row_bytes];
        let (y_head, y_tail) = y.split_at_mut((top + 1) * y_stride);
        let y0 = &mut y_head[top * y_stride..top * y_stride + width];
        let y1 = &mut y_tail[..width];
        let uv_row = &mut uv[row_pair * uv_stride..row_pair * uv_stride + width];

        for column in (0..width).step_by(2) {
            let p00 = pixel(src0, column);
            let p01 = pixel(src0, column + 1);
            let p10 = pixel(src1, column);
            let p11 = pixel(src1, column + 1);

            y0[column] = transform.pack_p16(transform.luma_wide(p00[2], p00[1], p00[0]));
            y0[column + 1] = transform.pack_p16(transform.luma_wide(p01[2], p01[1], p01[0]));
            y1[column] = transform.pack_p16(transform.luma_wide(p10[2], p10[1], p10[0]));
            y1[column + 1] = transform.pack_p16(transform.luma_wide(p11[2], p11[1], p11[0]));

            let r = mean_rgb10(p00[0], p01[0], p10[0], p11[0]);
            let g = mean_rgb10(p00[1], p01[1], p10[1], p11[1]);
            let b = mean_rgb10(p00[2], p01[2], p10[2], p11[2]);
            uv_row[column] = transform.pack_p16(transform.cb_wide(b, g, r));
            uv_row[column + 1] = transform.pack_p16(transform.cr_wide(b, g, r));
        }
    }
    Ok(())
}

/// Reduce a packed RGB10 frame to checked 8-bit BGRA.
///
/// This exists for the software H.264 fallback on a depth-30 Xorg session.
/// Hardware eight-bit sessions never use it: they remain on the `NvFBC`
/// device-to-device path.
///
/// # Errors
///
/// Returns [`ConversionError::GeometryMismatch`] when either buffer or stride
/// cannot cover the stated geometry.
#[allow(clippy::too_many_arguments)]
pub fn convert_packed_rgb10_to_bgra8(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    width: usize,
    height: usize,
    layout: PackedRgb10Layout,
) -> Result<(), ConversionError> {
    let row_bytes = width
        .checked_mul(4)
        .ok_or(ConversionError::GeometryMismatch)?;
    if src_stride < row_bytes
        || dst_stride < row_bytes
        || src.len() < src_stride.saturating_mul(height)
        || dst.len() < dst_stride.saturating_mul(height)
    {
        return Err(ConversionError::GeometryMismatch);
    }

    let to_u8 = |component: u16| {
        #[allow(clippy::cast_possible_truncation)]
        {
            ((u32::from(component) * 255 + 511) / 1023) as u8
        }
    };
    for row in 0..height {
        let src_row = &src[row * src_stride..row * src_stride + row_bytes];
        let dst_row = &mut dst[row * dst_stride..row * dst_stride + row_bytes];
        for column in 0..width {
            let base = column * 4;
            let word = u32::from_le_bytes([
                src_row[base],
                src_row[base + 1],
                src_row[base + 2],
                src_row[base + 3],
            ]);
            let [r, g, b] = layout.components(word);
            dst_row[base] = to_u8(b);
            dst_row[base + 1] = to_u8(g);
            dst_row[base + 2] = to_u8(r);
            dst_row[base + 3] = u8::MAX;
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::video::{I420FrameMut, I444FrameMut, I444P16FrameMut, Nv12FrameMut};

    fn half_bits(value: f32) -> [u8; 2] {
        // Minimal f32 -> binary16 for test fixtures; only needs to be exact
        // for the handful of values used here.
        let bits = value.to_bits();
        let sign = ((bits >> 16) & 0x8000) as u16;
        let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
        let mantissa = ((bits >> 13) & 0x3ff) as u16;
        let half = if value == 0.0 {
            sign
        } else {
            sign | ((exponent.clamp(0, 31) as u16) << 10) | mantissa
        };
        half.to_le_bytes()
    }

    fn scrgb_pixel(r: f32, g: f32, b: f32) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..2].copy_from_slice(&half_bits(r));
        out[2..4].copy_from_slice(&half_bits(g));
        out[4..6].copy_from_slice(&half_bits(b));
        out[6..8].copy_from_slice(&half_bits(1.0));
        out
    }

    /// The conversion the wide capture path depends on. SDR reference white
    /// must land at 80-nit PQ while values above 1.0 retain HDR headroom.
    #[test]
    fn scrgb_reference_white_and_hdr_highlight_map_to_absolute_pq() {
        let transform = ScrgbPqTransform::new(
            ColorMatrix::Bt2020Ncl,
            ColorPrimaries::Bt2020,
            ColorRange::Full,
            BitDepth::Ten,
        );
        let mut src = Vec::new();
        src.extend_from_slice(&scrgb_pixel(0.0, 0.0, 0.0));
        src.extend_from_slice(&scrgb_pixel(1.0, 1.0, 1.0));
        src.extend_from_slice(&scrgb_pixel(12.5, 12.5, 12.5));
        let (mut y, mut u, mut v) = ([0u16; 3], [0u16; 3], [0u16; 3]);

        convert_scrgb_to_pq_i444_p16(
            &src,
            24,
            [&mut y, &mut u, &mut v],
            [3, 3, 3],
            3,
            1,
            transform,
        )
        .expect("geometry is valid");

        assert_eq!(y[0], 0, "scRGB black must reach code 0");
        assert_eq!(y[1], 497 << 6, "scRGB 1.0 is 80-nit PQ reference white");
        assert_eq!(y[2], 769 << 6, "scRGB 12.5 is a 1000-nit highlight");
        assert!(y[2] > y[1], "HDR headroom must survive above SDR white");
        for plane in [&u, &v] {
            assert!(
                plane.iter().all(|sample| *sample == 0x8000),
                "neutral input must remain neutral"
            );
        }
    }

    #[test]
    fn scrgb_sdr_conversion_clamps_hdr_headroom_and_preserves_ten_bit_output() {
        let transform = ScrgbSdrTransform::new(
            ColorMatrix::Bt709,
            ColorRange::Full,
            BitDepth::Ten,
            TransferCharacteristics::Bt709,
        )
        .expect("BT.709 is an SDR transfer");
        let mut src = Vec::new();
        src.extend_from_slice(&scrgb_pixel(0.0, 0.0, 0.0));
        src.extend_from_slice(&scrgb_pixel(0.5, 0.5, 0.5));
        src.extend_from_slice(&scrgb_pixel(1.0, 1.0, 1.0));
        src.extend_from_slice(&scrgb_pixel(4.0, 4.0, 4.0));
        let (mut y, mut u, mut v) = ([0u16; 4], [0u16; 4], [0u16; 4]);

        convert_scrgb_to_sdr_i444_p16(
            &src,
            32,
            [&mut y, &mut u, &mut v],
            [4, 4, 4],
            4,
            1,
            transform,
        )
        .expect("geometry is valid");

        assert_eq!(y[0], 0);
        assert!(y[1] > 0 && y[1] < 0xffc0);
        assert_eq!(y[2], 0xffc0);
        assert_eq!(y[3], 0xffc0, "SDR conversion clamps values above white");
        assert!(u.iter().all(|sample| *sample == 0x8000));
        assert!(v.iter().all(|sample| *sample == 0x8000));
    }

    #[test]
    fn scrgb_sdr_transform_rejects_hdr_transfer_characteristics() {
        assert!(
            ScrgbSdrTransform::new(
                ColorMatrix::Bt2020Ncl,
                ColorRange::Full,
                BitDepth::Ten,
                TransferCharacteristics::Pq,
            )
            .is_none()
        );
    }

    /// Geometry is checked before any write, so a short buffer is an error
    /// rather than a partially converted frame.
    #[test]
    fn scrgb_conversion_refuses_geometry_it_cannot_satisfy() {
        let transform = ScrgbPqTransform::new(
            ColorMatrix::Bt709,
            ColorPrimaries::Bt2020,
            ColorRange::Full,
            BitDepth::Ten,
        );
        let src = [0u8; 8];
        let (mut y, mut u, mut v) = ([0u16; 1], [0u16; 1], [0u16; 1]);
        assert!(
            convert_scrgb_to_pq_i444_p16(
                &src,
                8,
                [&mut y, &mut u, &mut v],
                [1, 1, 1],
                2,
                1,
                transform
            )
            .is_err()
        );
    }

    fn bgra(r: u8, g: u8, b: u8) -> [u8; 4] {
        [b, g, r, 0xff]
    }

    fn legacy() -> ColorTransform {
        ColorTransform::legacy_bt709_limited()
    }

    #[test]
    fn white_reaches_the_top_code_and_black_the_bottom_for_every_format() {
        // The luma coefficients are derived so their sum is exactly the luma
        // scale. Without that, white lands one code short at some depths,
        // which is precisely the kind of silent endpoint error a grader
        // notices on a ramp.
        for depth in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                let transform = ColorTransform::new(ColorMatrix::Bt709, range, depth);
                let (black, white) = range.luma_bounds(depth);
                assert_eq!(
                    transform.luma(0, 0, 0),
                    i32::from(black),
                    "black at {depth:?}/{range:?}"
                );
                assert_eq!(
                    transform.luma(255, 255, 255),
                    i32::from(white),
                    "white at {depth:?}/{range:?}"
                );
            }
        }
    }

    #[test]
    fn neutral_grey_produces_exactly_the_chroma_centre() {
        // Each chroma triple sums to zero by construction, so a grey can never
        // drift off-centre by a code. An eyedropper on a grey ramp is one of
        // the first things a colourist does.
        for depth in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                let transform = ColorTransform::new(ColorMatrix::Bt709, range, depth);
                let centre = match range {
                    ColorRange::Limited => i32::from(128u16 << (depth.bits() - 8)),
                    ColorRange::Full => 1 << (depth.bits() - 1),
                };
                for level in [0u8, 1, 64, 127, 128, 200, 254, 255] {
                    assert_eq!(
                        transform.cb(level, level, level),
                        centre,
                        "Cb at grey {level} {depth:?}/{range:?}"
                    );
                    assert_eq!(
                        transform.cr(level, level, level),
                        centre,
                        "Cr at grey {level} {depth:?}/{range:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn ten_and_twelve_bit_samples_are_msb_aligned_in_the_word() {
        // NV_ENC_BUFFER_FORMAT_*_10BIT and CoreVideo's x-prefixed formats both
        // put the data in the HIGH bits. Storing 0x03FF instead of 0xFFC0 is
        // the classic four-stops-too-dark bug, so it is pinned here rather
        // than discovered on a Mac.
        let ten = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        assert_eq!(ten.pack_p16(1023), 0xFFC0);
        assert_ne!(ten.pack_p16(1023), 0x03FF);
        assert_eq!(ten.pack_p16(0), 0x0000);
        assert_eq!(ten.pack_p16(512), 0x8000);
        assert_eq!(ten.unpack_p16(0xFFC0), 1023);

        let twelve = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Twelve);
        assert_eq!(twelve.pack_p16(4095), 0xFFF0);
        assert_eq!(twelve.unpack_p16(0xFFF0), 4095);

        // Eight bits in a 16-bit word is still MSB-aligned, for consistency:
        // the shift is always `16 - depth`, never special-cased.
        let eight = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Eight);
        assert_eq!(eight.pack_p16(255), 0xFF00);
        assert_eq!(eight.unpack_p16(0xFF00), 255);
    }

    #[test]
    fn full_range_uses_every_code_and_limited_range_does_not() {
        let full = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Eight);
        let limited = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight);
        assert_eq!((full.luma(0, 0, 0), full.luma(255, 255, 255)), (0, 255));
        assert_eq!(
            (limited.luma(0, 0, 0), limited.luma(255, 255, 255)),
            (16, 235)
        );
    }

    #[test]
    fn identity_matrix_carries_gbr_in_plane_order() {
        // H.273 matrix_coefficients = 0: plane 0 is G, plane 1 is B, plane 2
        // is R, and nothing is converted.
        let transform =
            ColorTransform::new(ColorMatrix::Identity, ColorRange::Full, BitDepth::Eight);
        let (r, g, b) = (10u8, 20u8, 30u8);
        assert_eq!(transform.luma(b, g, r), i32::from(g));
        assert_eq!(transform.cb(b, g, r), i32::from(b));
        assert_eq!(transform.cr(b, g, r), i32::from(r));
    }

    #[test]
    fn nv12_preserves_existing_black_white_and_red_goldens() {
        for (pixel, expected_y) in [(bgra(0, 0, 0), 16), (bgra(255, 255, 255), 235)] {
            let source = [pixel; 4].concat();
            let frame = BgraFrame::new(&source, 2, 2, 8).expect("BGRA");
            let mut y = [0; 4];
            let mut uv = [0; 2];
            let mut destination = Nv12FrameMut::new(2, 2, &mut y, 2, &mut uv, 2).expect("NV12");
            convert_bgra_to_nv12(frame, &mut destination, legacy()).expect("conversion");
            assert!(destination.y().iter().all(|value| *value == expected_y));
            assert!(
                destination
                    .uv()
                    .iter()
                    .all(|value| (127..=129).contains(value))
            );
        }

        let source = [bgra(255, 0, 0); 4].concat();
        let frame = BgraFrame::new(&source, 2, 2, 8).expect("BGRA");
        let mut y = [0; 4];
        let mut uv = [0; 2];
        let mut destination = Nv12FrameMut::new(2, 2, &mut y, 2, &mut uv, 2).expect("NV12");
        convert_bgra_to_nv12(frame, &mut destination, legacy()).expect("conversion");
        assert!(destination.uv()[0] < 128);
        assert!(destination.uv()[1] > 128);
    }

    #[test]
    fn i444_keeps_every_chroma_sample_unlike_i420() {
        // A single red pixel among white ones survives 4:4:4 exactly and is
        // smeared by 4:2:0. This is the whole reason 4:4:4 matters for UI text
        // and thin mattes.
        let mut source = Vec::new();
        for index in 0..4 {
            source.extend_from_slice(&if index == 0 {
                bgra(255, 0, 0)
            } else {
                bgra(255, 255, 255)
            });
        }
        let frame = BgraFrame::new(&source, 2, 2, 8).expect("BGRA");
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Eight);

        let (mut p0, mut p1, mut p2) = ([0u8; 4], [0u8; 4], [0u8; 4]);
        let mut destination =
            I444FrameMut::new(2, 2, [&mut p0, &mut p1, &mut p2], [2, 2, 2]).expect("I444");
        convert_bgra_to_i444(frame, &mut destination, transform).expect("conversion");
        let planes = destination.as_frame().planes();
        // The red pixel keeps its own chroma; its neighbours keep theirs.
        assert_ne!(planes[1][0], planes[1][1]);
        assert_ne!(planes[2][0], planes[2][1]);
    }

    #[test]
    fn i444_accepts_odd_dimensions_because_nothing_is_subsampled() {
        let source = [bgra(10, 20, 30); 9].concat();
        let frame = BgraFrame::new(&source, 3, 3, 12).expect("BGRA");
        let (mut p0, mut p1, mut p2) = ([0u8; 9], [0u8; 9], [0u8; 9]);
        let mut destination =
            I444FrameMut::new(3, 3, [&mut p0, &mut p1, &mut p2], [3, 3, 3]).expect("odd I444");
        convert_bgra_to_i444(frame, &mut destination, legacy()).expect("conversion");
    }

    #[test]
    fn eight_bit_rgb_round_trips_exactly_through_ten_bit_444_full_range() {
        // This is the central claim of the whole colour-fidelity feature: the
        // two extra bits absorb the RGB->YCbCr matrix rounding, so a desktop
        // that is natively 8-bit RGB survives the trip unchanged. If this ever
        // stops holding, the "Grading Reference" preset is a lie, so the test
        // asserts exactness rather than a tolerance.
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let mut worst = 0i32;
        let mut worst_pixel = (0u8, 0u8, 0u8);
        for r in (0u16..=255).step_by(5) {
            for g in (0u16..=255).step_by(5) {
                for b in (0u16..=255).step_by(5) {
                    let (r, g, b) = (r as u8, g as u8, b as u8);
                    let (out_b, out_g, out_r) = transform.to_bgr8(
                        transform.luma(b, g, r),
                        transform.cb(b, g, r),
                        transform.cr(b, g, r),
                    );
                    let error = [
                        (i32::from(out_r) - i32::from(r)).abs(),
                        (i32::from(out_g) - i32::from(g)).abs(),
                        (i32::from(out_b) - i32::from(b)).abs(),
                    ]
                    .into_iter()
                    .max()
                    .unwrap_or_default();
                    if error > worst {
                        worst = error;
                        worst_pixel = (r, g, b);
                    }
                }
            }
        }
        assert_eq!(
            worst, 0,
            "10-bit 4:4:4 full range must be exact for 8-bit sources; \
             worst error {worst} at RGB{worst_pixel:?}"
        );
    }

    #[test]
    fn eight_bit_444_full_range_is_measurably_worse_than_ten_bit() {
        // The counterpart to the exactness test: at eight bits the same round
        // trip is NOT exact, which is the quantified reason to prefer ten.
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Eight);
        let mut worst = 0i32;
        for r in (0u16..=255).step_by(5) {
            for g in (0u16..=255).step_by(5) {
                for b in (0u16..=255).step_by(5) {
                    let (r, g, b) = (r as u8, g as u8, b as u8);
                    let (out_b, out_g, out_r) = transform.to_bgr8(
                        transform.luma(b, g, r),
                        transform.cb(b, g, r),
                        transform.cr(b, g, r),
                    );
                    worst = worst
                        .max((i32::from(out_r) - i32::from(r)).abs())
                        .max((i32::from(out_g) - i32::from(g)).abs())
                        .max((i32::from(out_b) - i32::from(b)).abs());
                }
            }
        }
        assert!(
            worst > 0,
            "8-bit 4:4:4 was unexpectedly exact; the 10-bit argument needs revisiting"
        );
    }

    #[test]
    fn ten_bit_444_conversion_fills_planes_msb_aligned() {
        let source = [bgra(255, 255, 255); 4].concat();
        let frame = BgraFrame::new(&source, 2, 2, 8).expect("BGRA");
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let (mut p0, mut p1, mut p2) = ([0u16; 4], [0u16; 4], [0u16; 4]);
        let mut destination =
            I444P16FrameMut::new(2, 2, [&mut p0, &mut p1, &mut p2], [2, 2, 2]).expect("P16");
        convert_bgra_to_i444_p16(frame, &mut destination, transform).expect("conversion");
        // White at 10-bit full range is code 1023, stored MSB-aligned.
        assert!(destination.planes()[0].iter().all(|word| *word == 0xFFC0));
    }

    #[test]
    fn nv12_and_i420_are_byte_identical_with_padding() {
        let width = 4;
        let height = 4;
        let source = (0usize..width * height)
            .flat_map(|index| {
                let value = u8::try_from(index * 13).expect("test range");
                bgra(value, value.wrapping_add(31), value.wrapping_add(63))
            })
            .collect::<Vec<_>>();
        let frame = BgraFrame::new(&source, width, height, width * 4).expect("BGRA");

        let mut nv_y = vec![0xa5; (width + 2) * height];
        let mut nv_uv = vec![0x5a; (width + 2) * height / 2];
        let mut nv12 = Nv12FrameMut::new(
            width as u32,
            height as u32,
            &mut nv_y,
            width + 2,
            &mut nv_uv,
            width + 2,
        )
        .expect("NV12");
        convert_bgra_to_nv12(frame, &mut nv12, legacy()).expect("NV12 conversion");

        let mut i_y = vec![0xa5; (width + 2) * height];
        let mut i_u = vec![0x5a; (width / 2 + 1) * height / 2];
        let mut i_v = vec![0x5a; (width / 2 + 1) * height / 2];
        let mut i420 = I420FrameMut::new(
            width as u32,
            height as u32,
            &mut i_y,
            width + 2,
            &mut i_u,
            width / 2 + 1,
            &mut i_v,
            width / 2 + 1,
        )
        .expect("I420");
        convert_bgra_to_i420(frame, &mut i420, legacy()).expect("I420 conversion");

        for row in 0..height {
            assert_eq!(
                &nv12.y()[row * (width + 2)..row * (width + 2) + width],
                &i420.as_frame().planes().0[row * (width + 2)..row * (width + 2) + width]
            );
        }
        for row in 0..height / 2 {
            for column in 0..width / 2 {
                assert_eq!(
                    nv12.uv()[row * (width + 2) + column * 2],
                    i420.as_frame().planes().1[row * (width / 2 + 1) + column]
                );
                assert_eq!(
                    nv12.uv()[row * (width + 2) + column * 2 + 1],
                    i420.as_frame().planes().2[row * (width / 2 + 1) + column]
                );
            }
        }
        assert_eq!(nv12.y()[width], 0xa5);
        assert_eq!(i420.as_frame().planes().1[width / 2], 0x5a);
    }

    #[test]
    fn row_ranges_match_full_conversion_and_retain_clean_rows() {
        let width = 32;
        let height = 32;
        let source = (0usize..width * height)
            .flat_map(|index| {
                let value = index.to_le_bytes()[0];
                bgra(value, value.wrapping_add(31), value.wrapping_add(63))
            })
            .collect::<Vec<_>>();
        let frame = BgraFrame::new(&source, width, height, width * 4).expect("BGRA");
        let mut full_y = vec![0; width * height];
        let mut full_u = vec![0; width * height / 4];
        let mut full_v = vec![0; width * height / 4];
        let mut full = I420FrameMut::new(
            width as u32,
            height as u32,
            &mut full_y,
            width,
            &mut full_u,
            width / 2,
            &mut full_v,
            width / 2,
        )
        .expect("I420");
        convert_bgra_to_i420(frame, &mut full, legacy()).expect("full");

        let mut selected_y = vec![0xa5; width * height];
        let mut selected_u = vec![0x5a; width * height / 4];
        let mut selected_v = vec![0x5a; width * height / 4];
        let mut selected = I420FrameMut::new(
            width as u32,
            height as u32,
            &mut selected_y,
            width,
            &mut selected_u,
            width / 2,
            &mut selected_v,
            width / 2,
        )
        .expect("I420");
        convert_bgra_to_i420_rows(frame, &mut selected, 16..32, legacy()).expect("rows");
        let (full_y, full_u, full_v) = full.as_frame().planes();
        let (selected_y, selected_u, selected_v) = selected.as_frame().planes();
        assert_eq!(&selected_y[16 * width..], &full_y[16 * width..]);
        assert_eq!(&selected_u[8 * width / 2..], &full_u[8 * width / 2..]);
        assert_eq!(&selected_v[8 * width / 2..], &full_v[8 * width / 2..]);
        assert!(selected_y[..16 * width].iter().all(|value| *value == 0xa5));
    }

    #[test]
    fn conversion_rejects_mismatch_and_bad_rows() {
        let source = [bgra(0, 0, 0); 4].concat();
        let frame = BgraFrame::new(&source, 2, 2, 8).expect("BGRA");
        let mut y = [0; 16];
        let mut uv = [0; 8];
        let mut destination = Nv12FrameMut::new(4, 4, &mut y, 4, &mut uv, 4).expect("NV12");
        assert_eq!(
            convert_bgra_to_nv12(frame, &mut destination, legacy()),
            Err(ConversionError::GeometryMismatch)
        );
        let mut y = [0; 4];
        let mut uv = [0; 2];
        let mut destination = Nv12FrameMut::new(2, 2, &mut y, 2, &mut uv, 2).expect("NV12");
        assert_eq!(
            convert_bgra_to_nv12_rows(frame, &mut destination, 1..2, legacy()),
            Err(ConversionError::InvalidRowRange)
        );
    }

    /// Every coded format the 4:4:4 16-bit path can be asked for.
    fn every_i444_p16_transform() -> Vec<ColorTransform> {
        let mut transforms = Vec::new();
        for matrix in [
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
            ColorMatrix::Identity,
        ] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                for depth in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
                    transforms.push(ColorTransform::new(matrix, range, depth));
                }
            }
        }
        transforms
    }

    /// The pre-fusion implementation, kept verbatim as the correctness oracle.
    ///
    /// It calls [`ColorTransform::luma`], [`ColorTransform::cb`],
    /// [`ColorTransform::cr`] and [`ColorTransform::pack_p16`] one component at
    /// a time, exactly as the shipped code did before the inner loop was
    /// hoisted. If the optimised kernel ever disagrees with this, the
    /// optimisation is wrong.
    fn scalar_i444_p16_oracle(
        source: BgraFrame<'_>,
        width: usize,
        height: usize,
        strides: [usize; 3],
        planes: &mut [Vec<u16>; 3],
        transform: ColorTransform,
    ) {
        for row in 0..height {
            let pixels = source.active_row(row).expect("source row");
            for (column, pixel) in pixels.chunks_exact(4).take(width).enumerate() {
                let (b, g, r) = (pixel[0], pixel[1], pixel[2]);
                planes[0][row * strides[0] + column] = transform.pack_p16(transform.luma(b, g, r));
                planes[1][row * strides[1] + column] = transform.pack_p16(transform.cb(b, g, r));
                planes[2][row * strides[2] + column] = transform.pack_p16(transform.cr(b, g, r));
            }
        }
    }

    /// Deterministic, dependency-free 64-bit xorshift.
    struct Deterministic(u64);

    impl Deterministic {
        fn next_byte(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 24) as u8
        }
    }

    /// Untouched stride padding is proved by filling the destination with this
    /// first: a zero fill could not tell "left alone" from "written as zero".
    const PLANE_PADDING_SENTINEL: u16 = 0xA5A5;

    #[test]
    fn fused_i444_p16_conversion_matches_the_scalar_oracle() {
        // Padded source stride, padded and mutually different destination
        // strides, an odd width and a height that no worker count divides
        // evenly: every place a hoisted index could drift.
        let (width, height) = (37usize, 23usize);
        let source_stride = width * 4 + 24;
        let strides = [width + 5, width + 1, width + 13];

        let mut random = Deterministic(0x5eed_1234_9abc_def1);
        let mut pixels = vec![0u8; source_stride * height];
        for byte in &mut pixels {
            *byte = random.next_byte();
        }
        // Seed the endpoints and the codes either side of the limited-range
        // clamps into the first row, so clamping is exercised rather than
        // hoped for.
        for (index, value) in [0u8, 1, 15, 16, 127, 128, 235, 240, 254, 255]
            .into_iter()
            .enumerate()
        {
            for component in 0..4 {
                pixels[index * 4 + component] = value;
            }
        }
        let source = BgraFrame::new(&pixels, width, height, source_stride).expect("BGRA");

        for transform in every_i444_p16_transform() {
            let mut expected = [
                vec![0u16; strides[0] * height],
                vec![0u16; strides[1] * height],
                vec![0u16; strides[2] * height],
            ];
            scalar_i444_p16_oracle(source, width, height, strides, &mut expected, transform);

            let mut actual = [
                vec![0u16; strides[0] * height],
                vec![0u16; strides[1] * height],
                vec![0u16; strides[2] * height],
            ];
            {
                let [y, u, v] = &mut actual;
                let mut destination = I444P16FrameMut::new(
                    width as u32,
                    height as u32,
                    [y.as_mut_slice(), u.as_mut_slice(), v.as_mut_slice()],
                    strides,
                )
                .expect("I444P16");
                convert_bgra_to_i444_p16(source, &mut destination, transform).expect("conversion");
            }
            assert_eq!(
                actual, expected,
                "fused conversion drifted from the scalar oracle for {transform:?}"
            );
        }
    }

    #[test]
    fn fused_i444_p16_row_ranges_match_the_oracle_and_leave_padding_untouched() {
        let (width, height) = (16usize, 12usize);
        let source_stride = width * 4 + 8;
        let strides = [width + 3, width + 3, width + 3];
        let mut random = Deterministic(0x0bad_c0de_0bad_c0de);
        let mut pixels = vec![0u8; source_stride * height];
        for byte in &mut pixels {
            *byte = random.next_byte();
        }
        let source = BgraFrame::new(&pixels, width, height, source_stride).expect("BGRA");
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);

        let mut expected = [
            vec![0u16; strides[0] * height],
            vec![0u16; strides[1] * height],
            vec![0u16; strides[2] * height],
        ];
        scalar_i444_p16_oracle(source, width, height, strides, &mut expected, transform);

        // Uneven, non-even-aligned chunks, exactly as the row workers produce
        // when the height does not divide by the worker count.
        let mut actual = [
            vec![PLANE_PADDING_SENTINEL; strides[0] * height],
            vec![PLANE_PADDING_SENTINEL; strides[1] * height],
            vec![PLANE_PADDING_SENTINEL; strides[2] * height],
        ];
        {
            let [y, u, v] = &mut actual;
            let mut destination = I444P16FrameMut::new(
                width as u32,
                height as u32,
                [y.as_mut_slice(), u.as_mut_slice(), v.as_mut_slice()],
                strides,
            )
            .expect("I444P16");
            for rows in [0..5usize, 5..5, 5..11, 11..12] {
                convert_bgra_to_i444_p16_rows(source, &mut destination, rows, transform)
                    .expect("row conversion");
            }
        }

        for plane in 0..3 {
            for row in 0..height {
                let start = row * strides[plane];
                assert_eq!(
                    &actual[plane][start..start + width],
                    &expected[plane][start..start + width],
                    "plane {plane} row {row} drifted from the oracle"
                );
                assert!(
                    actual[plane][start + width..start + strides[plane]]
                        .iter()
                        .all(|value| *value == PLANE_PADDING_SENTINEL),
                    "plane {plane} row {row} wrote into its stride padding"
                );
            }
        }
    }

    #[test]
    fn fused_i444_p16_conversion_covers_every_component_value_exactly() {
        // One pixel per 8-bit component value on each channel independently,
        // so a coefficient loaded into the wrong slot cannot pass.
        let width = 256usize;
        let height = 3usize;
        let mut pixels = vec![0u8; width * 4 * height];
        for value in 0..width {
            let byte = value as u8;
            pixels[value * 4] = byte; // blue only
            pixels[width * 4 + value * 4 + 1] = byte; // green only
            pixels[width * 8 + value * 4 + 2] = byte; // red only
        }
        let source = BgraFrame::new(&pixels, width, height, width * 4).expect("BGRA");
        let strides = [width, width, width];

        for transform in every_i444_p16_transform() {
            let mut expected = [
                vec![0u16; width * height],
                vec![0u16; width * height],
                vec![0u16; width * height],
            ];
            scalar_i444_p16_oracle(source, width, height, strides, &mut expected, transform);
            let mut actual = [
                vec![0u16; width * height],
                vec![0u16; width * height],
                vec![0u16; width * height],
            ];
            {
                let [y, u, v] = &mut actual;
                let mut destination = I444P16FrameMut::new(
                    width as u32,
                    height as u32,
                    [y.as_mut_slice(), u.as_mut_slice(), v.as_mut_slice()],
                    strides,
                )
                .expect("I444P16");
                convert_bgra_to_i444_p16(source, &mut destination, transform).expect("conversion");
            }
            assert_eq!(
                actual, expected,
                "component sweep drifted for {transform:?}"
            );
        }
    }

    /// The fused kernel folds `pack_p16`'s second clamp into the first. That
    /// is only safe because clamping is idempotent, so it is asserted rather
    /// than assumed.
    #[test]
    fn folding_the_second_clamp_cannot_change_a_code() {
        for transform in every_i444_p16_transform() {
            let kernel = FusedI444Kernel::new(transform);
            for code in [
                i32::MIN,
                -70_000,
                -1,
                0,
                1,
                255,
                1_023,
                4_095,
                65_535,
                i32::MAX,
            ] {
                assert_eq!(
                    kernel.pack(code),
                    transform.pack_p16(code),
                    "clamp/store disagreed at {code} for {transform:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod wide_source_tests {
    use super::*;

    /// The guard for the generalisation: `new` must be exactly what it was.
    ///
    /// `for_input_max` exists so a wide source needs no second copy of the
    /// matrix derivation — two copies is how conversions drift apart. That is
    /// only safe if the 8-bit case is provably untouched, so assert the two
    /// constructions agree on every coded output, not merely on their fields.
    #[test]
    fn input_max_255_matches_the_original_constructor() {
        for matrix in [
            ColorMatrix::Bt709,
            ColorMatrix::Bt601,
            ColorMatrix::Bt2020Ncl,
            ColorMatrix::Identity,
        ] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                for depth in [BitDepth::Eight, BitDepth::Ten, BitDepth::Twelve] {
                    let baseline = ColorTransform::new(matrix, range, depth);
                    let generalised = ColorTransform::for_input_max(matrix, range, depth, 255.0);
                    for b in [0u8, 1, 17, 128, 254, 255] {
                        for g in [0u8, 1, 17, 128, 254, 255] {
                            for r in [0u8, 1, 17, 128, 254, 255] {
                                assert_eq!(
                                    baseline.luma(b, g, r),
                                    generalised.luma(b, g, r),
                                    "luma drift at {matrix:?}/{range:?}/{depth:?} {b},{g},{r}"
                                );
                                assert_eq!(baseline.cb(b, g, r), generalised.cb(b, g, r));
                                assert_eq!(baseline.cr(b, g, r), generalised.cr(b, g, r));
                            }
                        }
                    }
                }
            }
        }
    }

    /// An 8-bit desktop carried through the wide path must reach the same codes
    /// the 8-bit path produces. If a wide capture of ordinary content differed
    /// from an ordinary capture of it, the two pipelines would not be two views
    /// of one desktop.
    /// Pack an `A2R10G10B10` pixel the way an X depth-30 framebuffer does.
    fn a2r10g10b10(r: u16, g: u16, b: u16) -> [u8; 4] {
        let word = ((u32::from(r) & 0x3ff) << 20)
            | ((u32::from(g) & 0x3ff) << 10)
            | (u32::from(b) & 0x3ff)
            | (0x3 << 30);
        word.to_le_bytes()
    }

    fn wide_full_bt709() -> ColorTransform {
        ColorTransform::for_input_max(
            ColorMatrix::Bt709,
            ColorRange::Full,
            BitDepth::Ten,
            f64::from(WIDE_INPUT_MAX),
        )
    }

    fn convert_one_a2r10g10b10(r: u16, g: u16, b: u16) -> (u16, u16, u16) {
        let src = a2r10g10b10(r, g, b);
        let mut y = [0u16; 1];
        let mut u = [0u16; 1];
        let mut v = [0u16; 1];
        convert_packed_rgb10_to_i444_p16(
            &src,
            4,
            [&mut y, &mut u, &mut v],
            [1, 1, 1],
            1,
            1,
            PackedRgb10Layout::XRGB2101010,
            wide_full_bt709(),
        )
        .expect("single pixel converts");
        (y[0], u[0], v[0])
    }

    /// Black, white and neutral mid-grey land exactly where full-range
    /// ten-bit says they must.
    ///
    /// These three are the whole reason this converter takes its components
    /// untouched: an `A2R10G10B10` channel is already a `0..=1023` code, so
    /// white must be 1023 and not 255-scaled-up. A transform built by
    /// `ColorTransform::new` instead of `for_input_max` would put white at
    /// roughly a quarter of that, and the error would be invisible in the
    /// shadows and glaring in the highlights.
    #[test]
    fn a2r10g10b10_maps_black_white_and_neutral_exactly() {
        assert_eq!(convert_one_a2r10g10b10(0, 0, 0).0, 0, "black luma");
        assert_eq!(
            convert_one_a2r10g10b10(1023, 1023, 1023).0,
            0xffc0,
            "white luma must be MSB-aligned"
        );

        let (_, u, v) = convert_one_a2r10g10b10(512, 512, 512);
        assert_eq!((u, v), (0x8000, 0x8000), "neutral grey must not tint");
    }

    /// The channels are unpacked from the right bit positions.
    ///
    /// Getting red and blue the wrong way round produces a picture that is
    /// obviously wrong but still *works*, so this pins the layout rather
    /// than trusting it: blue occupies the low ten bits, red the high ones.
    #[test]
    fn a2r10g10b10_unpacks_red_green_and_blue_from_the_right_bits() {
        let (red_y, _, red_v) = convert_one_a2r10g10b10(1023, 0, 0);
        let (blue_y, blue_u, _) = convert_one_a2r10g10b10(0, 0, 1023);
        let (green_y, _, _) = convert_one_a2r10g10b10(0, 1023, 0);

        // BT.709 luma weights: green dominates, red is next, blue is least.
        assert!(
            green_y > red_y && red_y > blue_y,
            "luma ordering G({green_y}) > R({red_y}) > B({blue_y}) identifies the channels"
        );
        // Pure red pushes Cr up; pure blue pushes Cb up.
        assert!(red_v > 0x8000, "pure red must raise Cr, got {red_v}");
        assert!(blue_u > 0x8000, "pure blue must raise Cb, got {blue_u}");
    }

    /// Ten-bit input carries detail an eight-bit path cannot.
    ///
    /// Adjacent ten-bit codes that would collapse to the same eight-bit
    /// value must stay distinguishable, which is the entire point of running
    /// the X server at depth 30.
    #[test]
    fn a2r10g10b10_preserves_detail_below_the_eight_bit_grid() {
        let low = convert_one_a2r10g10b10(512, 512, 512).0;
        let high = convert_one_a2r10g10b10(513, 513, 513).0;
        assert_ne!(
            low, high,
            "codes one apart at ten bits must not collapse to the same luma"
        );
    }

    /// Geometry is checked before any write.
    #[test]
    fn a2r10g10b10_rejects_a_short_source_row() {
        let src = [0u8; 4];
        let mut y = [0u16; 2];
        let mut u = [0u16; 2];
        let mut v = [0u16; 2];
        assert!(
            convert_packed_rgb10_to_i444_p16(
                &src,
                4,
                [&mut y, &mut u, &mut v],
                [2, 2, 2],
                2,
                1,
                PackedRgb10Layout::XRGB2101010,
                wide_full_bt709(),
            )
            .is_err()
        );
    }

    #[test]
    fn packed_rgb10_layout_uses_the_visual_masks_not_a_fixed_channel_order() {
        let nvidia_xorg = PackedRgb10Layout::from_masks(0x0000_03ff, 0x000f_fc00, 0x3ff0_0000)
            .expect("live NVIDIA depth-30 masks");
        assert_eq!(nvidia_xorg, PackedRgb10Layout::XBGR2101010);
        assert_eq!(
            PackedRgb10Layout::from_masks(0x3ff0_0000, 0x000f_fc00, 0x0000_03ff),
            Some(PackedRgb10Layout::XRGB2101010)
        );
        assert!(PackedRgb10Layout::from_masks(0xff, 0xff00, 0xff0000).is_none());
        assert!(PackedRgb10Layout::from_masks(0x3ff, 0x3ff, 0x3ff00000).is_none());

        let word = (0x3ff_u32 << 30) | (1023_u32 << 20);
        let src = word.to_le_bytes();
        let mut y = [0u16; 1];
        let mut u = [0u16; 1];
        let mut v = [0u16; 1];
        convert_packed_rgb10_to_i444_p16(
            &src,
            4,
            [&mut y, &mut u, &mut v],
            [1, 1, 1],
            1,
            1,
            nvidia_xorg,
            wide_full_bt709(),
        )
        .expect("NVIDIA depth-30 pixel converts");
        assert!(u[0] > 0x8000, "blue in bits 20..29 must raise Cb");
        assert!(v[0] < 0x8000, "blue in bits 20..29 must lower Cr");
    }

    #[test]
    fn packed_rgb10_to_bgra8_is_the_inverse_of_eight_bit_expansion() {
        let layout = PackedRgb10Layout::XBGR2101010;
        for code in 0u16..=255 {
            let widened = (code << 2) | (code >> 6);
            let word = (0x3_u32 << 30)
                | (u32::from(widened) << 20)
                | (u32::from(widened) << 10)
                | u32::from(widened);
            let mut bgra = [0u8; 4];
            convert_packed_rgb10_to_bgra8(&word.to_le_bytes(), 4, &mut bgra, 4, 1, 1, layout)
                .expect("ten-bit pixel reduces");
            assert_eq!(bgra, [code as u8, code as u8, code as u8, u8::MAX]);
        }
    }

    #[test]
    fn packed_rgb10_to_p010_preserves_full_range_white_and_neutral_chroma() {
        let pixel = ((0x3_u32 << 30) | 0x3ff | (0x3ff << 10) | (0x3ff << 20)).to_le_bytes();
        let source = [pixel, pixel, pixel, pixel].concat();
        let mut y = [0u16; 4];
        let mut uv = [0u16; 2];
        convert_packed_rgb10_to_p010(
            &source,
            8,
            &mut y,
            2,
            &mut uv,
            2,
            2,
            2,
            PackedRgb10Layout::XBGR2101010,
            wide_full_bt709(),
        )
        .expect("P010 conversion");
        assert_eq!(y, [0xffc0; 4]);
        assert_eq!(uv, [0x8000; 2]);
    }

    #[test]
    fn eight_bit_content_agrees_between_the_narrow_and_wide_paths() {
        let narrow = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let wide = ColorTransform::for_input_max(
            ColorMatrix::Bt709,
            ColorRange::Full,
            BitDepth::Ten,
            f64::from(WIDE_INPUT_MAX),
        );
        for code in 0u16..=255 {
            let narrow_component = u8::try_from(code).expect("8-bit");
            // The same signal on the wide grid: bit replication, which maps
            // 0->0 and 255->1023 exactly.
            let widened = (code << 2) | (code >> 6);
            let expected = narrow.luma(narrow_component, narrow_component, narrow_component);
            let actual = wide.luma_wide(widened, widened, widened);
            assert!(
                (expected - actual).abs() <= 1,
                "grey {code}: narrow {expected} vs wide {actual}"
            );
        }
    }

    /// scRGB is absolute linear light at 80 nits per unit. Labelling an sRGB
    /// signal as PQ would map reference white to 10,000 nits instead.
    #[test]
    fn pq_maps_reference_white_to_80_nits_not_peak_white() {
        assert_eq!(scrgb_component_to_pq_code(1.0), 497);
        assert!((linear_nits_to_pq_signal(80.0) - 0.485_856_77).abs() < 1e-6);
    }

    #[test]
    fn pq_threshold_lookup_stays_within_one_code_of_the_f32_formula() {
        for bits in 0u16..=u16::MAX {
            let linear = half_to_f32(bits);
            let expected = if !linear.is_finite() || linear <= 0.0 {
                0
            } else {
                let signal = linear_nits_to_pq_signal(linear * SCRGB_REFERENCE_WHITE_NITS);
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    (signal * f32::from(WIDE_INPUT_MAX))
                        .round()
                        .clamp(0.0, f32::from(WIDE_INPUT_MAX)) as u16
                }
            };
            let actual = scrgb_component_to_pq_code(linear);
            assert!(
                actual.abs_diff(expected) <= 1,
                "half bits 0x{bits:04x}, linear={linear}, actual={actual}, expected={expected}"
            );
        }
    }

    #[test]
    fn scrgb_extremes_clamp_rather_than_wrap() {
        assert_eq!(scrgb_component_to_pq_code(0.0), 0);
        assert_eq!(scrgb_component_to_pq_code(-4.0), 0);
        assert_eq!(scrgb_component_to_pq_code(1.0), 497);
        assert_eq!(scrgb_component_to_pq_code(12.5), 769);
        assert_eq!(scrgb_component_to_pq_code(125.0), WIDE_INPUT_MAX);
        assert_eq!(scrgb_component_to_pq_code(500.0), WIDE_INPUT_MAX);
        assert_eq!(scrgb_component_to_pq_code(f32::NAN), 0);
    }

    /// The wide path must actually carry more than 8 bits, or it is pointless.
    #[test]
    fn sub_eight_bit_differences_survive_quantisation() {
        let a = scrgb_component_to_pq_code(0.2140);
        let b = scrgb_component_to_pq_code(0.2180);
        assert_ne!(a, b, "a sub-8-bit-step difference collapsed to one code");
    }

    #[test]
    fn primary_conversion_preserves_white_and_maps_srgb_red_into_bt2020() {
        let white = convert_scrgb_primaries(1.0, 1.0, 1.0, ColorPrimaries::Bt2020);
        for component in white {
            assert!((component - 1.0).abs() < 1e-6);
        }
        let red = convert_scrgb_primaries(1.0, 0.0, 0.0, ColorPrimaries::Bt2020);
        assert!((red[0] - 0.627_403_9).abs() < 1e-6);
        assert!((red[1] - 0.069_097_29).abs() < 1e-6);
        assert!((red[2] - 0.016_391_44).abs() < 1e-6);
    }

    #[test]
    fn half_decoding_round_trips() {
        assert!((half_to_f32(0x3C00) - 1.0).abs() < 1e-6);
        assert!((half_to_f32(0x3800) - 0.5).abs() < 1e-6);
        assert!(half_to_f32(0x0000).abs() < 1e-9);
    }
}
