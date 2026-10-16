//! `CGSize` type for 2D dimensions

use std::fmt;

/// `CGSize` representation
///
/// Represents a 2D size with width and height.
///
/// # Examples
///
/// ```
/// use apple_cf::cg::CGSize;
///
/// let size = CGSize::new(1920.0, 1080.0);
/// assert_eq!(size.aspect_ratio(), 1920.0 / 1080.0);
/// assert_eq!(size.area(), 1920.0 * 1080.0);
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

impl std::hash::Hash for CGSize {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.width.to_bits().hash(state);
        self.height.to_bits().hash(state);
    }
}

impl Eq for CGSize {}

impl CGSize {
    /// Create a new size
    ///
    /// # Examples
    ///
    /// ```
    /// use apple_cf::cg::CGSize;
    ///
    /// let size = CGSize::new(800.0, 600.0);
    /// assert_eq!(size.width, 800.0);
    /// ```
    #[must_use]
    pub const fn new(width: f64, height: f64) -> Self {
        Self { width, height }
    }

    /// Create a zero-sized size
    ///
    /// # Examples
    ///
    /// ```
    /// use apple_cf::cg::CGSize;
    ///
    /// let size = CGSize::zero();
    /// assert!(size.is_null());
    /// ```
    #[must_use]
    pub const fn zero() -> Self {
        Self::new(0.0, 0.0)
    }

    /// Get the area (width * height)
    #[must_use]
    pub const fn area(&self) -> f64 {
        self.width * self.height
    }

    /// Get the aspect ratio (width / height)
    #[must_use]
    pub fn aspect_ratio(&self) -> f64 {
        if self.height == 0.0 {
            0.0
        } else {
            self.width / self.height
        }
    }

    /// Check if this is a square (width == height)
    /// Note: Uses exact comparison, may not work well with computed values
    #[allow(clippy::float_cmp)]
    #[must_use]
    pub const fn is_square(&self) -> bool {
        self.width == self.height
    }

    /// Returns whether the size has a non-positive width or height.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.width <= 0.0 || self.height <= 0.0
    }

    /// Check if size is null (both dimensions are zero)
    #[must_use]
    pub const fn is_null(&self) -> bool {
        self.width == 0.0 && self.height == 0.0
    }
}

impl Default for CGSize {
    fn default() -> Self {
        Self::zero()
    }
}

impl fmt::Display for CGSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}
