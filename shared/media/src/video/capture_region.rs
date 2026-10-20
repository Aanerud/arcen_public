//! Which part of a damage-gated copying capture to read again.
//!
//! A host that reads its frame back from the GPU (Xorg `XShmGetImage` of a
//! depth-30 screen) pays for every pixel it reads and converts. When the
//! source names what changed, only that rectangle needs reading, converting
//! and uploading; the previous frame's converted planes hold the rest.
//!
//! Damage is accumulated as one bounding rectangle in output pixels. Anything
//! the source cannot vouch for is the whole frame: the first frame, a source
//! without damage reports, a recreated source, and every keyframe or refresh
//! the caller decides on.

use super::convert::{ConversionError, FrameRegion};

/// What to read for the next picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRegion {
    /// Read, convert and upload the whole frame.
    Full,
    /// Only this rectangle changed since the last picture.
    Rect(FrameRegion),
}

/// When a region is worth reading on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionPolicy {
    /// A region larger than this share of the frame reads the whole frame:
    /// one large read is cheaper than a rectangle plus bookkeeping, and the
    /// whole frame is never wrong.
    pub max_area_percent: u8,
    /// Regions grow outward to multiples of this, so chroma-subsampled
    /// formats never split a sample pair.
    pub align: usize,
}

impl RegionPolicy {
    /// Half the frame, two-pixel alignment.
    pub const DEFAULT: Self = Self {
        max_area_percent: 50,
        align: 2,
    };
}

/// Damage reported since the last picture, as one bounding rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureDamage {
    width: usize,
    height: usize,
    bounds: Option<FrameRegion>,
    full: bool,
}

impl CaptureDamage {
    /// A tracker for a `width` x `height` output. The first picture is always
    /// the whole frame.
    #[must_use]
    pub const fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            bounds: None,
            full: true,
        }
    }

    /// Adds a reported rectangle in output pixels. Parts outside the output
    /// are dropped; an empty rectangle adds nothing.
    pub fn note_rect(&mut self, x: i64, y: i64, width: u64, height: u64) {
        let clamp = |start: i64, length: u64, limit: usize| -> Option<(usize, usize)> {
            let limit = i64::try_from(limit).ok()?;
            let end = start.saturating_add(i64::try_from(length).unwrap_or(i64::MAX));
            let (start, end) = (start.clamp(0, limit), end.clamp(0, limit));
            (end > start).then(|| {
                (
                    usize::try_from(start).unwrap_or(0),
                    usize::try_from(end).unwrap_or(0),
                )
            })
        };
        let (Some((left, right)), Some((top, bottom))) =
            (clamp(x, width, self.width), clamp(y, height, self.height))
        else {
            return;
        };
        self.bounds = Some(match self.bounds {
            None => FrameRegion {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            },
            Some(bounds) => {
                let left = left.min(bounds.x);
                let top = top.min(bounds.y);
                let right = right.max(bounds.x + bounds.width);
                let bottom = bottom.max(bounds.y + bounds.height);
                FrameRegion {
                    x: left,
                    y: top,
                    width: right - left,
                    height: bottom - top,
                }
            }
        });
    }

    /// The source cannot say what changed: read everything next time.
    pub const fn note_full(&mut self) {
        self.full = true;
    }

    /// Whether anything is pending.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.full || self.bounds.is_some()
    }

    /// The region the next picture must read, and a fresh start for the one
    /// after. A picture taken with nothing pending (a keepalive) reads the
    /// whole frame.
    pub fn take(&mut self, policy: RegionPolicy) -> CaptureRegion {
        let full = std::mem::replace(&mut self.full, false);
        let bounds = self.bounds.take();
        if full {
            return CaptureRegion::Full;
        }
        let Some(bounds) = bounds else {
            return CaptureRegion::Full;
        };
        let align = policy.align.max(1);
        let down = |value: usize| value / align * align;
        let up =
            |value: usize, limit: usize| value.div_ceil(align).saturating_mul(align).min(limit);
        let left = down(bounds.x);
        let top = down(bounds.y);
        let right = up(bounds.x + bounds.width, self.width);
        let bottom = up(bounds.y + bounds.height, self.height);
        let region = FrameRegion {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        };
        let area = region.width.saturating_mul(region.height);
        let frame = self.width.saturating_mul(self.height).max(1);
        if area.saturating_mul(100) > frame.saturating_mul(usize::from(policy.max_area_percent)) {
            CaptureRegion::Full
        } else {
            CaptureRegion::Rect(region)
        }
    }
}

/// The smallest rectangle holding every pixel that differs between two
/// frames of the same geometry, or `None` when they are identical.
///
/// For sources whose damage reports cover more than what changed (a
/// compositor that swaps its whole frame reports the whole screen), the
/// pixels themselves bound what has to be converted and uploaded. Callers
/// may split a frame into row bands, call this on each, and
/// [`union_regions`] the results with the bands' row offsets applied.
///
/// # Errors
///
/// [`ConversionError::GeometryMismatch`] when either frame is too short for
/// `height` rows of `width` pixels at `stride`.
pub fn changed_bounds(
    previous: &[u8],
    current: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
) -> Result<Option<FrameRegion>, ConversionError> {
    let row_bytes = width
        .checked_mul(bytes_per_pixel)
        .ok_or(ConversionError::GeometryMismatch)?;
    let needed = if height == 0 {
        0
    } else {
        stride
            .checked_mul(height - 1)
            .and_then(|bytes| bytes.checked_add(row_bytes))
            .ok_or(ConversionError::GeometryMismatch)?
    };
    if bytes_per_pixel == 0
        || stride < row_bytes
        || previous.len() < needed
        || current.len() < needed
    {
        return Err(ConversionError::GeometryMismatch);
    }
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for row in 0..height {
        let start = row * stride;
        let old = &previous[start..start + row_bytes];
        let new = &current[start..start + row_bytes];
        if old == new {
            continue;
        }
        let pixels = old
            .chunks_exact(bytes_per_pixel)
            .zip(new.chunks_exact(bytes_per_pixel));
        let left = pixels
            .clone()
            .position(|(old, new)| old != new)
            .unwrap_or(0);
        let right = width - pixels.rev().position(|(old, new)| old != new).unwrap_or(0);
        bounds = Some(match bounds {
            None => (left, right, row, row + 1),
            Some((l, r, top, _)) => (l.min(left), r.max(right), top, row + 1),
        });
    }
    Ok(bounds.map(|(left, right, top, bottom)| FrameRegion {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }))
}

/// The bounding rectangle of two optional regions.
#[must_use]
pub fn union_regions(a: Option<FrameRegion>, b: Option<FrameRegion>) -> Option<FrameRegion> {
    match (a, b) {
        (None, other) | (other, None) => other,
        (Some(a), Some(b)) => {
            let left = a.x.min(b.x);
            let top = a.y.min(b.y);
            let right = (a.x + a.width).max(b.x + b.width);
            let bottom = (a.y + a.height).max(b.y + b.height);
            Some(FrameRegion {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: RegionPolicy = RegionPolicy::DEFAULT;

    fn rect(x: usize, y: usize, width: usize, height: usize) -> CaptureRegion {
        CaptureRegion::Rect(FrameRegion {
            x,
            y,
            width,
            height,
        })
    }

    #[test]
    fn changed_bounds_find_the_differing_pixels() {
        let (width, height, stride) = (9usize, 7usize, 9 * 4 + 8);
        let previous = vec![0u8; stride * height];
        assert_eq!(
            changed_bounds(&previous, &previous, stride, width, height, 4),
            Ok(None)
        );
        let mut current = previous.clone();
        current[2 * stride + 3 * 4 + 1] = 1;
        current[5 * stride + 6 * 4 + 3] = 1;
        // Row padding never counts.
        current[stride - 1] = 9;
        assert_eq!(
            changed_bounds(&previous, &current, stride, width, height, 4),
            Ok(Some(FrameRegion {
                x: 3,
                y: 2,
                width: 4,
                height: 4,
            }))
        );
        assert_eq!(
            changed_bounds(&previous[..10], &current, stride, width, height, 4),
            Err(ConversionError::GeometryMismatch)
        );
    }

    #[test]
    fn regions_union_into_one_bounding_box() {
        let a = FrameRegion {
            x: 2,
            y: 3,
            width: 4,
            height: 1,
        };
        let b = FrameRegion {
            x: 1,
            y: 10,
            width: 2,
            height: 2,
        };
        assert_eq!(union_regions(None, None), None);
        assert_eq!(union_regions(Some(a), None), Some(a));
        assert_eq!(
            union_regions(Some(a), Some(b)),
            Some(FrameRegion {
                x: 1,
                y: 3,
                width: 5,
                height: 9,
            })
        );
    }

    #[test]
    fn the_first_picture_is_whole() {
        let mut damage = CaptureDamage::new(2560, 1440);
        damage.note_rect(10, 10, 4, 4);
        assert_eq!(damage.take(POLICY), CaptureRegion::Full);
        assert!(!damage.is_pending());
    }

    #[test]
    fn reports_union_into_one_aligned_bounding_rectangle() {
        let mut damage = CaptureDamage::new(2560, 1440);
        let _ = damage.take(POLICY);
        damage.note_rect(101, 51, 10, 10);
        damage.note_rect(300, 40, 5, 3);
        assert_eq!(damage.take(POLICY), rect(100, 40, 206, 22));
        assert!(!damage.is_pending());
    }

    #[test]
    fn rectangles_are_clamped_to_the_output() {
        let mut damage = CaptureDamage::new(1920, 1080);
        let _ = damage.take(POLICY);
        damage.note_rect(-20, 1070, 40, 100);
        assert_eq!(damage.take(POLICY), rect(0, 1070, 20, 10));
        damage.note_rect(5000, 5000, 10, 10);
        damage.note_rect(10, 10, 0, 10);
        assert!(!damage.is_pending());
    }

    #[test]
    fn large_or_unvouched_damage_reads_the_whole_frame() {
        let mut damage = CaptureDamage::new(100, 100);
        let _ = damage.take(POLICY);
        damage.note_rect(0, 0, 80, 70);
        assert_eq!(damage.take(POLICY), CaptureRegion::Full);
        damage.note_rect(0, 0, 4, 4);
        damage.note_full();
        assert_eq!(damage.take(POLICY), CaptureRegion::Full);
        damage.note_rect(0, 0, 50, 100);
        assert_eq!(damage.take(POLICY), rect(0, 0, 50, 100));
    }

    #[test]
    fn odd_frame_edges_stay_inside_the_frame() {
        let mut damage = CaptureDamage::new(37, 23);
        let _ = damage.take(POLICY);
        damage.note_rect(35, 21, 2, 2);
        assert_eq!(damage.take(POLICY), rect(34, 20, 3, 3));
    }
}
