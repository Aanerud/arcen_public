//! `CGAffineTransform` and `CGVector` value types.
//!
//! `CGAffineTransform` is the 2-D transformation matrix used throughout
//! Core Graphics / Core Animation / Core Image:
//!
//! ```text
//!     [ a   b   0 ]
//!     [ c   d   0 ]
//!     [ tx  ty  1 ]
//! ```
//!
//! Wraps Apple's `CGAffineTransformMake*` / `CGAffineTransformConcat`
//! helpers. Layout-compatible with the C struct so it crosses the FFI
//! boundary by value.

/// A 2-D vector — `(dx, dy)` in screen coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C)]
pub struct CGVector {
    pub dx: f64,
    pub dy: f64,
}

impl CGVector {
    /// Creates a Core Graphics vector with the supplied components.
    #[must_use]
    pub const fn new(dx: f64, dy: f64) -> Self {
        Self { dx, dy }
    }
}

/// Affine transformation matrix.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct CGAffineTransform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Default for CGAffineTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl CGAffineTransform {
    /// The identity transform — no scale, rotation, or translation.
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Raw constructor; component values match Apple's
    /// `CGAffineTransformMake(a, b, c, d, tx, ty)`.
    #[must_use]
    pub const fn new(a: f64, b: f64, c: f64, d: f64, tx: f64, ty: f64) -> Self {
        Self { a, b, c, d, tx, ty }
    }

    /// Translation-only transform — wraps `CGAffineTransformMakeTranslation`.
    #[must_use]
    pub fn translation(tx: f64, ty: f64) -> Self {
        unsafe { CGAffineTransformMakeTranslation(tx, ty) }
    }

    /// Scale-only transform — wraps `CGAffineTransformMakeScale`.
    #[must_use]
    pub fn scale(sx: f64, sy: f64) -> Self {
        unsafe { CGAffineTransformMakeScale(sx, sy) }
    }

    /// Rotation-only transform (radians) — wraps `CGAffineTransformMakeRotation`.
    #[must_use]
    pub fn rotation(radians: f64) -> Self {
        unsafe { CGAffineTransformMakeRotation(radians) }
    }

    /// `self` then `other` (Apple's matrix-multiply order). Wraps
    /// `CGAffineTransformConcat`.
    #[must_use]
    pub fn concat(self, other: Self) -> Self {
        unsafe { CGAffineTransformConcat(self, other) }
    }

    /// Inverse transform. Wraps `CGAffineTransformInvert`. Returns
    /// the identity if `self` is non-invertible.
    #[must_use]
    pub fn invert(self) -> Self {
        unsafe { CGAffineTransformInvert(self) }
    }

    /// True if this is the identity transform. Wraps
    /// `CGAffineTransformIsIdentity`.
    #[must_use]
    pub fn is_identity(self) -> bool {
        unsafe { CGAffineTransformIsIdentity(self) }
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGAffineTransformMakeTranslation(tx: f64, ty: f64) -> CGAffineTransform;
    fn CGAffineTransformMakeScale(sx: f64, sy: f64) -> CGAffineTransform;
    fn CGAffineTransformMakeRotation(radians: f64) -> CGAffineTransform;
    fn CGAffineTransformConcat(a: CGAffineTransform, b: CGAffineTransform) -> CGAffineTransform;
    fn CGAffineTransformInvert(t: CGAffineTransform) -> CGAffineTransform;
    fn CGAffineTransformIsIdentity(t: CGAffineTransform) -> bool;
}
