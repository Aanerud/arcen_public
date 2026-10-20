//! Which output a desktop point lands on, and where on it.
//!
//! A host that injects absolute input per output (a compositor that takes a
//! pointer position relative to one monitor's stream, rather than one
//! position across a combined raster) needs one more step after the shared
//! region transform: find the output whose desktop rectangle contains the
//! applied point and express the point in that output's pixels. Points in a
//! gap between outputs, or outside every output, belong to none and are
//! refused rather than snapped.

use arcen_media::AppliedPoint;

/// One output's desktop rectangle, in host pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRect {
    /// Left edge.
    pub x: i64,
    /// Top edge.
    pub y: i64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl OutputRect {
    fn contains(&self, point: AppliedPoint) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x < self.x + i64::from(self.width)
            && point.y < self.y + i64::from(self.height)
    }
}

/// A point inside one output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLocalPoint {
    /// Index of the output in the caller's list.
    pub output: usize,
    /// Pixels from the output's left edge.
    pub x: u32,
    /// Pixels from the output's top edge.
    pub y: u32,
}

/// The output containing `point` and the point in its pixels; `None` when
/// no output does. Rectangles are half-open (a point on an output's right or
/// bottom edge belongs to its neighbour); the first match wins, so callers
/// pass non-overlapping rectangles.
#[must_use]
pub fn locate_output(point: AppliedPoint, outputs: &[OutputRect]) -> Option<OutputLocalPoint> {
    outputs
        .iter()
        .position(|output| output.contains(point))
        .and_then(|index| {
            let output = outputs[index];
            Some(OutputLocalPoint {
                output: index,
                x: u32::try_from(point.x - output.x).ok()?,
                y: u32::try_from(point.y - output.y).ok()?,
            })
        })
}

/// The output containing the normalised position `(nx, ny)` of the whole
/// desktop (`0.0..=1.0` across its bounding box), for single-position input
/// that names no region. The far edge maps to the last pixel.
#[must_use]
pub fn locate_normalized(
    nx: f64,
    ny: f64,
    desktop_width: u32,
    desktop_height: u32,
    outputs: &[OutputRect],
) -> Option<OutputLocalPoint> {
    if !nx.is_finite() || !ny.is_finite() || desktop_width == 0 || desktop_height == 0 {
        return None;
    }
    let axis = |normalized: f64, extent: u32| -> i64 {
        let last = f64::from(extent - 1);
        // Clamped to the raster, so the cast is exact.
        #[allow(clippy::cast_possible_truncation)]
        let pixel = (normalized.clamp(0.0, 1.0) * last).round() as i64;
        pixel
    };
    locate_output(
        AppliedPoint::new(axis(nx, desktop_width), axis(ny, desktop_height)),
        outputs,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE_BY_SIDE: [OutputRect; 2] = [
        OutputRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        },
        OutputRect {
            x: 1920,
            y: 0,
            width: 1280,
            height: 1024,
        },
    ];

    #[test]
    fn a_point_lands_on_the_output_that_contains_it() {
        assert_eq!(
            locate_output(AppliedPoint::new(1919, 1079), &SIDE_BY_SIDE),
            Some(OutputLocalPoint {
                output: 0,
                x: 1919,
                y: 1079
            })
        );
        assert_eq!(
            locate_output(AppliedPoint::new(1920, 0), &SIDE_BY_SIDE),
            Some(OutputLocalPoint {
                output: 1,
                x: 0,
                y: 0
            })
        );
        assert_eq!(
            locate_output(AppliedPoint::new(3199, 1023), &SIDE_BY_SIDE),
            Some(OutputLocalPoint {
                output: 1,
                x: 1279,
                y: 1023
            })
        );
    }

    #[test]
    fn gaps_and_points_outside_are_refused() {
        // Below the shorter right-hand output.
        assert_eq!(
            locate_output(AppliedPoint::new(2000, 1050), &SIDE_BY_SIDE),
            None
        );
        assert_eq!(locate_output(AppliedPoint::new(-1, 0), &SIDE_BY_SIDE), None);
        assert_eq!(
            locate_output(AppliedPoint::new(3200, 0), &SIDE_BY_SIDE),
            None
        );
    }

    #[test]
    fn normalised_positions_span_the_whole_desktop() {
        assert_eq!(
            locate_normalized(0.0, 0.0, 3200, 1080, &SIDE_BY_SIDE),
            Some(OutputLocalPoint {
                output: 0,
                x: 0,
                y: 0
            })
        );
        assert_eq!(
            locate_normalized(1.0, 0.5, 3200, 1080, &SIDE_BY_SIDE).map(|point| point.output),
            Some(1)
        );
        assert_eq!(
            locate_normalized(1.0, 1.0, 3200, 1080, &SIDE_BY_SIDE),
            None,
            "the bottom-right corner is in the gap below the shorter output"
        );
        assert_eq!(
            locate_normalized(f64::NAN, 0.0, 3200, 1080, &SIDE_BY_SIDE),
            None
        );
    }
}
