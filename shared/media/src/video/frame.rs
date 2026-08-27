use std::error::Error;
use std::fmt::{Display, Formatter};

/// Checked planar-frame layout failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayoutError {
    ZeroDimensions,
    OddDimensions,
    GeometryOverflow,
    StrideTooSmall {
        plane: &'static str,
        actual: usize,
        minimum: usize,
    },
    PlaneTooSmall {
        plane: &'static str,
        actual: usize,
        required: usize,
    },
}

impl Display for FrameLayoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDimensions => formatter.write_str("frame dimensions must be non-zero"),
            Self::OddDimensions => formatter.write_str("YUV420 frame dimensions must both be even"),
            Self::GeometryOverflow => formatter.write_str("frame geometry overflows address space"),
            Self::StrideTooSmall {
                plane,
                actual,
                minimum,
            } => write!(
                formatter,
                "{plane} stride {actual} is smaller than required {minimum}"
            ),
            Self::PlaneTooSmall {
                plane,
                actual,
                required,
            } => write!(
                formatter,
                "{plane} plane length {actual} is smaller than required {required}"
            ),
        }
    }
}

impl Error for FrameLayoutError {}

fn dimensions(width: u32, height: u32) -> Result<(usize, usize), FrameLayoutError> {
    if width == 0 || height == 0 {
        return Err(FrameLayoutError::ZeroDimensions);
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err(FrameLayoutError::OddDimensions);
    }
    let width = usize::try_from(width).map_err(|_| FrameLayoutError::GeometryOverflow)?;
    let height = usize::try_from(height).map_err(|_| FrameLayoutError::GeometryOverflow)?;
    Ok((width, height))
}

/// Validate dimensions for a format with no chroma subsampling.
///
/// 4:4:4 stores a full-resolution sample for every component, so unlike 4:2:0
/// it has no reason to demand even dimensions. Requiring them anyway would
/// force an odd-width display to be cropped or padded for no coding reason.
fn dimensions_unsubsampled(width: u32, height: u32) -> Result<(usize, usize), FrameLayoutError> {
    if width == 0 || height == 0 {
        return Err(FrameLayoutError::ZeroDimensions);
    }
    let width = usize::try_from(width).map_err(|_| FrameLayoutError::GeometryOverflow)?;
    let height = usize::try_from(height).map_err(|_| FrameLayoutError::GeometryOverflow)?;
    Ok((width, height))
}

fn plane_len(
    plane: &'static str,
    actual: usize,
    stride: usize,
    minimum_stride: usize,
    rows: usize,
) -> Result<usize, FrameLayoutError> {
    if stride < minimum_stride {
        return Err(FrameLayoutError::StrideTooSmall {
            plane,
            actual: stride,
            minimum: minimum_stride,
        });
    }
    let required = stride
        .checked_mul(rows)
        .ok_or(FrameLayoutError::GeometryOverflow)?;
    if actual < required {
        return Err(FrameLayoutError::PlaneTooSmall {
            plane,
            actual,
            required,
        });
    }
    Ok(required)
}

/// Checked mutable NV12 view with exact active plane prefixes.
#[derive(Debug)]
pub struct Nv12FrameMut<'a> {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) y_stride: usize,
    pub(super) uv_stride: usize,
    pub(super) y: &'a mut [u8],
    pub(super) uv: &'a mut [u8],
}

impl<'a> Nv12FrameMut<'a> {
    /// Validate and borrow an NV12 destination.
    ///
    /// # Errors
    ///
    /// Rejects zero/odd dimensions, overflow, short strides, and short planes.
    pub fn new(
        width: u32,
        height: u32,
        y: &'a mut [u8],
        y_stride: usize,
        uv: &'a mut [u8],
        uv_stride: usize,
    ) -> Result<Self, FrameLayoutError> {
        let (width, height) = dimensions(width, height)?;
        let y_len = plane_len("Y", y.len(), y_stride, width, height)?;
        let uv_len = plane_len("UV", uv.len(), uv_stride, width, height / 2)?;
        Ok(Self {
            width,
            height,
            y_stride,
            uv_stride,
            y: &mut y[..y_len],
            uv: &mut uv[..uv_len],
        })
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    #[must_use]
    pub fn y(&self) -> &[u8] {
        self.y
    }

    #[must_use]
    pub fn uv(&self) -> &[u8] {
        self.uv
    }
}

/// Checked borrowed I420 view with exact active plane prefixes.
#[derive(Clone, Copy, Debug)]
pub struct I420Frame<'a> {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) y_stride: usize,
    pub(super) u_stride: usize,
    pub(super) v_stride: usize,
    pub(super) y: &'a [u8],
    pub(super) u: &'a [u8],
    pub(super) v: &'a [u8],
}

impl<'a> I420Frame<'a> {
    /// Validate and borrow an I420 source.
    ///
    /// # Errors
    ///
    /// Rejects zero/odd dimensions, geometry overflow, short per-plane
    /// strides, and short planes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        y: &'a [u8],
        y_stride: usize,
        u: &'a [u8],
        u_stride: usize,
        v: &'a [u8],
        v_stride: usize,
    ) -> Result<Self, FrameLayoutError> {
        let (width, height) = dimensions(width, height)?;
        let y_len = plane_len("Y", y.len(), y_stride, width, height)?;
        let u_len = plane_len("U", u.len(), u_stride, width / 2, height / 2)?;
        let v_len = plane_len("V", v.len(), v_stride, width / 2, height / 2)?;
        Ok(Self {
            width,
            height,
            y_stride,
            u_stride,
            v_stride,
            y: &y[..y_len],
            u: &u[..u_len],
            v: &v[..v_len],
        })
    }

    #[must_use]
    pub const fn width(self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> usize {
        self.height
    }

    #[must_use]
    pub const fn strides(self) -> (usize, usize, usize) {
        (self.y_stride, self.u_stride, self.v_stride)
    }

    #[must_use]
    pub const fn planes(self) -> (&'a [u8], &'a [u8], &'a [u8]) {
        (self.y, self.u, self.v)
    }
}

/// Checked mutable I420 view with exact active plane prefixes.
#[derive(Debug)]
pub struct I420FrameMut<'a> {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) y_stride: usize,
    pub(super) u_stride: usize,
    pub(super) v_stride: usize,
    pub(super) y: &'a mut [u8],
    pub(super) u: &'a mut [u8],
    pub(super) v: &'a mut [u8],
}

impl<'a> I420FrameMut<'a> {
    /// Validate and borrow an I420 destination.
    ///
    /// Separate mutable borrows make overlapping safe-Rust planes
    /// unrepresentable.
    ///
    /// # Errors
    ///
    /// Rejects zero/odd dimensions, geometry overflow, short per-plane
    /// strides, and short planes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u32,
        height: u32,
        y: &'a mut [u8],
        y_stride: usize,
        u: &'a mut [u8],
        u_stride: usize,
        v: &'a mut [u8],
        v_stride: usize,
    ) -> Result<Self, FrameLayoutError> {
        let (width, height) = dimensions(width, height)?;
        let y_len = plane_len("Y", y.len(), y_stride, width, height)?;
        let u_len = plane_len("U", u.len(), u_stride, width / 2, height / 2)?;
        let v_len = plane_len("V", v.len(), v_stride, width / 2, height / 2)?;
        Ok(Self {
            width,
            height,
            y_stride,
            u_stride,
            v_stride,
            y: &mut y[..y_len],
            u: &mut u[..u_len],
            v: &mut v[..v_len],
        })
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    #[must_use]
    pub fn as_frame(&self) -> I420Frame<'_> {
        I420Frame {
            width: self.width,
            height: self.height,
            y_stride: self.y_stride,
            u_stride: self.u_stride,
            v_stride: self.v_stride,
            y: self.y,
            u: self.u,
            v: self.v,
        }
    }
}

/// Plane labels used by the 4:4:4 layout errors.
const PLANAR_NAMES: [&str; 3] = ["plane 0", "plane 1", "plane 2"];

/// Checked borrowed planar 4:4:4 view with exact active plane prefixes.
///
/// Every plane is full resolution, so this carries no chroma subsampling loss.
/// That is the point of it: for screen content, subsampling is the single
/// largest source of visible error, well ahead of quantisation.
#[derive(Clone, Copy, Debug)]
pub struct I444Frame<'a> {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) strides: [usize; 3],
    pub(super) planes: [&'a [u8]; 3],
}

impl<'a> I444Frame<'a> {
    /// Validate and borrow a planar 4:4:4 source.
    ///
    /// Planes are in coded order: for a YCbCr matrix that is Y, Cb, Cr; for
    /// the identity matrix it is G, B, R.
    ///
    /// # Errors
    ///
    /// Rejects zero dimensions, geometry overflow, short strides, and short
    /// planes.
    pub fn new(
        width: u32,
        height: u32,
        planes: [&'a [u8]; 3],
        strides: [usize; 3],
    ) -> Result<Self, FrameLayoutError> {
        let (width, height) = dimensions_unsubsampled(width, height)?;
        let mut bounded: [&[u8]; 3] = [&[], &[], &[]];
        for index in 0..3 {
            let len = plane_len(
                PLANAR_NAMES[index],
                planes[index].len(),
                strides[index],
                width,
                height,
            )?;
            bounded[index] = &planes[index][..len];
        }
        Ok(Self {
            width,
            height,
            strides,
            planes: bounded,
        })
    }

    #[must_use]
    pub const fn width(self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> usize {
        self.height
    }

    #[must_use]
    pub const fn strides(self) -> [usize; 3] {
        self.strides
    }

    #[must_use]
    pub const fn planes(self) -> [&'a [u8]; 3] {
        self.planes
    }
}

/// Checked mutable planar 4:4:4 destination with exact active plane prefixes.
#[derive(Debug)]
pub struct I444FrameMut<'a> {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) strides: [usize; 3],
    pub(super) planes: [&'a mut [u8]; 3],
}

impl<'a> I444FrameMut<'a> {
    /// Validate and borrow a planar 4:4:4 destination.
    ///
    /// Separate mutable borrows make overlapping safe-Rust planes
    /// unrepresentable.
    ///
    /// # Errors
    ///
    /// Rejects zero dimensions, geometry overflow, short strides, and short
    /// planes.
    pub fn new(
        width: u32,
        height: u32,
        planes: [&'a mut [u8]; 3],
        strides: [usize; 3],
    ) -> Result<Self, FrameLayoutError> {
        let (width, height) = dimensions_unsubsampled(width, height)?;
        let [p0, p1, p2] = planes;
        let l0 = plane_len(PLANAR_NAMES[0], p0.len(), strides[0], width, height)?;
        let l1 = plane_len(PLANAR_NAMES[1], p1.len(), strides[1], width, height)?;
        let l2 = plane_len(PLANAR_NAMES[2], p2.len(), strides[2], width, height)?;
        Ok(Self {
            width,
            height,
            strides,
            planes: [&mut p0[..l0], &mut p1[..l1], &mut p2[..l2]],
        })
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    #[must_use]
    pub fn as_frame(&self) -> I444Frame<'_> {
        I444Frame {
            width: self.width,
            height: self.height,
            strides: self.strides,
            planes: [self.planes[0], self.planes[1], self.planes[2]],
        }
    }
}

/// Checked mutable planar 4:4:4 destination with 16-bit samples.
///
/// Used for every depth above eight. Samples are stored **MSB-aligned** in the
/// 16-bit word, matching `NV_ENC_BUFFER_FORMAT_YUV444_10BIT` and `CoreVideo`'s
/// `x`-prefixed formats: a ten-bit code `v` is stored as `v << 6`, not as `v`.
///
/// **Strides are counted in samples, not bytes.** The plane type is `[u16]`,
/// so a byte-denominated stride would be silently wrong by a factor of two.
#[derive(Debug)]
pub struct I444P16FrameMut<'a> {
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) strides: [usize; 3],
    pub(super) planes: [&'a mut [u16]; 3],
}

impl<'a> I444P16FrameMut<'a> {
    /// Validate and borrow a 16-bit planar 4:4:4 destination.
    ///
    /// `strides` are in samples.
    ///
    /// # Errors
    ///
    /// Rejects zero dimensions, geometry overflow, short strides, and short
    /// planes.
    pub fn new(
        width: u32,
        height: u32,
        planes: [&'a mut [u16]; 3],
        strides: [usize; 3],
    ) -> Result<Self, FrameLayoutError> {
        let (width, height) = dimensions_unsubsampled(width, height)?;
        let [p0, p1, p2] = planes;
        let l0 = plane_len(PLANAR_NAMES[0], p0.len(), strides[0], width, height)?;
        let l1 = plane_len(PLANAR_NAMES[1], p1.len(), strides[1], width, height)?;
        let l2 = plane_len(PLANAR_NAMES[2], p2.len(), strides[2], width, height)?;
        Ok(Self {
            width,
            height,
            strides,
            planes: [&mut p0[..l0], &mut p1[..l1], &mut p2[..l2]],
        })
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Sample-denominated plane strides.
    #[must_use]
    pub const fn strides(&self) -> [usize; 3] {
        self.strides
    }

    /// Borrowed plane contents, for assertions and readback.
    #[must_use]
    pub fn planes(&self) -> [&[u16]; 3] {
        [self.planes[0], self.planes[1], self.planes[2]]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_reject_invalid_geometry_and_storage() {
        let mut y = [0; 16];
        let mut uv = [0; 8];
        assert_eq!(
            Nv12FrameMut::new(0, 4, &mut y, 4, &mut uv, 4).unwrap_err(),
            FrameLayoutError::ZeroDimensions
        );
        assert_eq!(
            Nv12FrameMut::new(3, 4, &mut y, 4, &mut uv, 4).unwrap_err(),
            FrameLayoutError::OddDimensions
        );
        assert!(matches!(
            Nv12FrameMut::new(4, 4, &mut y, 3, &mut uv, 4),
            Err(FrameLayoutError::StrideTooSmall { plane: "Y", .. })
        ));
        assert!(matches!(
            Nv12FrameMut::new(4, 4, &mut y[..15], 4, &mut uv, 4),
            Err(FrameLayoutError::PlaneTooSmall { plane: "Y", .. })
        ));
    }

    #[test]
    fn i420_exposes_only_exact_active_prefixes() {
        let y = [0; 20];
        let u = [0; 8];
        let v = [0; 8];
        let frame = I420Frame::new(4, 4, &y, 4, &u, 2, &v, 2).expect("valid I420");
        assert_eq!(frame.planes().0.len(), 16);
        assert_eq!(frame.planes().1.len(), 4);
        assert_eq!(frame.planes().2.len(), 4);
    }

    #[test]
    fn immutable_i420_accepts_independent_chroma_strides() {
        let y = [0; 24];
        let u = [0; 8];
        let v = [0; 8];
        let frame = I420Frame::new(4, 4, &y, 5, &u, 2, &v, 3).expect("valid I420");
        assert_eq!(frame.strides(), (5, 2, 3));
        let (y, u, v) = frame.planes();
        assert_eq!((y.len(), u.len(), v.len()), (20, 4, 6));
    }

    #[test]
    fn mutable_i420_accepts_independent_chroma_strides() {
        let mut y = [0; 24];
        let mut u = [0; 8];
        let mut v = [0; 8];
        let frame = I420FrameMut::new(4, 4, &mut y, 5, &mut u, 2, &mut v, 3).expect("valid I420");
        let borrowed = frame.as_frame();
        assert_eq!(borrowed.strides(), (5, 2, 3));
        let (y, u, v) = borrowed.planes();
        assert_eq!((y.len(), u.len(), v.len()), (20, 4, 6));
    }
}
