use std::error::Error;
use std::fmt::{Display, Formatter};
use std::ops::Range;

use arcen_keel::BgraFrame;

use super::frame::{I420FrameMut, I444FrameMut, I444P16FrameMut, Nv12FrameMut};
use crate::{BitDepth, ColorMatrix, ColorRange};

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

        // Source components are 8-bit, so every coefficient folds in the 1/255
        // normalisation as well as the matrix and the range scaling.
        #[allow(clippy::cast_precision_loss)]
        let luma_unit = f64::from(luma_scale) / 255.0;
        #[allow(clippy::cast_precision_loss)]
        let chroma_unit = f64::from(chroma_scale) / 255.0;

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

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use crate::video::{I420FrameMut, I444FrameMut, I444P16FrameMut, Nv12FrameMut};

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
