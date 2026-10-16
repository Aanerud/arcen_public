//! `CGRect` type for 2D rectangles

use std::fmt;

use super::{CGPoint, CGSize};

/// `CGRect` representation
///
/// Represents a rectangle with an `origin` point and `size` dimensions.
///
/// # Examples
///
/// ```
/// use apple_cf::cg::CGRect;
///
/// let rect = CGRect::new(10.0, 20.0, 100.0, 200.0);
/// assert_eq!(rect.origin.x, 10.0);
/// assert_eq!(rect.size.width, 100.0);
/// assert_eq!(rect.max_x(), 110.0);
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

impl std::hash::Hash for CGRect {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.origin.hash(state);
        self.size.hash(state);
    }
}

impl Eq for CGRect {}

impl CGRect {
    /// Create a new rectangle
    ///
    /// # Examples
    ///
    /// ```
    /// use apple_cf::cg::CGRect;
    ///
    /// let rect = CGRect::new(0.0, 0.0, 1920.0, 1080.0);
    /// assert_eq!(rect.size.width, 1920.0);
    /// ```
    #[must_use]
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            origin: CGPoint::new(x, y),
            size: CGSize::new(width, height),
        }
    }

    /// Create a zero-sized rectangle at origin
    ///
    /// # Examples
    ///
    /// ```
    /// use apple_cf::cg::CGRect;
    ///
    /// let rect = CGRect::zero();
    /// assert!(rect.is_null());
    /// ```
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            origin: CGPoint::zero(),
            size: CGSize::zero(),
        }
    }

    /// Create a rect with origin and size.
    #[must_use]
    pub const fn from_origin_size(origin: CGPoint, size: CGSize) -> Self {
        Self { origin, size }
    }

    /// Create a rect with origin and size.
    #[must_use]
    pub const fn with_origin_and_size(origin: CGPoint, size: CGSize) -> Self {
        Self::from_origin_size(origin, size)
    }

    /// Get the origin point
    #[must_use]
    pub const fn origin(&self) -> CGPoint {
        self.origin
    }

    /// Get the size
    #[must_use]
    pub const fn size(&self) -> CGSize {
        self.size
    }

    /// Get the center point
    #[must_use]
    pub const fn center(&self) -> CGPoint {
        CGPoint::new(
            self.origin.x + self.size.width / 2.0,
            self.origin.y + self.size.height / 2.0,
        )
    }

    /// Get the minimum X coordinate
    #[must_use]
    pub const fn min_x(&self) -> f64 {
        self.origin.x
    }

    /// Get the minimum Y coordinate
    #[must_use]
    pub const fn min_y(&self) -> f64 {
        self.origin.y
    }

    /// Get the maximum X coordinate
    #[must_use]
    pub const fn max_x(&self) -> f64 {
        self.origin.x + self.size.width
    }

    /// Get the maximum Y coordinate
    #[must_use]
    pub const fn max_y(&self) -> f64 {
        self.origin.y + self.size.height
    }

    /// Get the mid X coordinate
    #[must_use]
    pub const fn mid_x(&self) -> f64 {
        self.origin.x + self.size.width / 2.0
    }

    /// Get the mid Y coordinate
    #[must_use]
    pub const fn mid_y(&self) -> f64 {
        self.origin.y + self.size.height / 2.0
    }

    /// Returns whether the rectangle has a non-positive width or height.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.size.width <= 0.0 || self.size.height <= 0.0
    }

    /// Returns whether the rectangle contains the provided point.
    #[must_use]
    pub fn contains_point(&self, p: CGPoint) -> bool {
        !self.is_empty()
            && p.x >= self.min_x()
            && p.x < self.max_x()
            && p.y >= self.min_y()
            && p.y < self.max_y()
    }

    /// Check if rect is null (both position and size are zero)
    #[must_use]
    pub const fn is_null(&self) -> bool {
        self.origin.is_zero() && self.size.is_null()
    }
}

impl Default for CGRect {
    fn default() -> Self {
        Self::zero()
    }
}

impl fmt::Display for CGRect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "({}, {}, {}, {})",
            self.origin.x, self.origin.y, self.size.width, self.size.height
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{CGPoint, CGRect, CGSize};

    #[test]
    fn from_origin_size_preserves_components() {
        let rect = CGRect::from_origin_size(CGPoint::new(10.0, 20.0), CGSize::new(30.0, 40.0));

        assert_eq!(rect.origin, CGPoint::new(10.0, 20.0));
        assert_eq!(rect.size, CGSize::new(30.0, 40.0));
    }

    #[test]
    fn contains_point_matches_rect_bounds() {
        let rect = CGRect::new(10.0, 20.0, 30.0, 40.0);

        assert!(rect.contains_point(CGPoint::new(10.0, 20.0)));
        assert!(rect.contains_point(CGPoint::new(39.999, 59.999)));
        assert!(!rect.contains_point(CGPoint::new(40.0, 20.0)));
        assert!(!rect.contains_point(CGPoint::new(10.0, 60.0)));
        assert!(!rect.contains_point(CGPoint::new(9.999, 20.0)));
    }

    #[test]
    fn empty_rect_does_not_contain_points() {
        let rect = CGRect::from_origin_size(CGPoint::new(10.0, 20.0), CGSize::new(0.0, 40.0));

        assert!(rect.is_empty());
        assert!(!rect.contains_point(CGPoint::new(10.0, 20.0)));
    }
}
