use core::fmt;
use core::ops::Range;

pub const BLOCK_SIZE: usize = 16;
const BGRA_BYTES_PER_PIXEL: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockGrid {
    width: usize,
    height: usize,
    blocks_wide: usize,
    blocks_tall: usize,
    block_count: usize,
}

impl BlockGrid {
    /// Creates checked 16x16 block geometry.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::ZeroDimension`] for an empty frame or
    /// [`KeelError::GeometryOverflow`] when the block count cannot fit `usize`.
    pub fn new(width: usize, height: usize) -> Result<Self, KeelError> {
        if width == 0 || height == 0 {
            return Err(KeelError::ZeroDimension);
        }
        let blocks_wide = width.div_ceil(BLOCK_SIZE);
        let blocks_tall = height.div_ceil(BLOCK_SIZE);
        let block_count = blocks_wide
            .checked_mul(blocks_tall)
            .ok_or(KeelError::GeometryOverflow)?;
        Ok(Self {
            width,
            height,
            blocks_wide,
            blocks_tall,
            block_count,
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
    pub const fn blocks_wide(self) -> usize {
        self.blocks_wide
    }

    #[must_use]
    pub const fn blocks_tall(self) -> usize {
        self.blocks_tall
    }

    #[must_use]
    pub const fn block_count(self) -> usize {
        self.block_count
    }

    #[must_use]
    pub fn block_index(self, block_x: usize, block_y: usize) -> Option<usize> {
        if block_x >= self.blocks_wide || block_y >= self.blocks_tall {
            return None;
        }
        block_y
            .checked_mul(self.blocks_wide)
            .and_then(|row| row.checked_add(block_x))
    }

    #[must_use]
    pub fn block_bounds(self, index: usize) -> Option<BlockBounds> {
        if index >= self.block_count {
            return None;
        }
        let block_x = index % self.blocks_wide;
        let block_y = index / self.blocks_wide;
        let x = block_x * BLOCK_SIZE;
        let y = block_y * BLOCK_SIZE;
        Some(BlockBounds {
            x,
            y,
            width: BLOCK_SIZE.min(self.width - x),
            height: BLOCK_SIZE.min(self.height - y),
        })
    }

    #[must_use]
    pub fn block_row_pixels(self, block_row: usize) -> Option<Range<usize>> {
        if block_row >= self.blocks_tall {
            return None;
        }
        let start = block_row * BLOCK_SIZE;
        Some(start..(start + BLOCK_SIZE).min(self.height))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockBounds {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct BgraFrame<'a> {
    pixels: &'a [u8],
    grid: BlockGrid,
    stride: usize,
}

impl<'a> BgraFrame<'a> {
    /// Creates a borrowed view over active BGRA pixels.
    ///
    /// # Errors
    ///
    /// Returns an error for zero/overflowing geometry, a stride smaller than
    /// `width * 4`, or storage shorter than `stride * height`.
    pub fn new(
        pixels: &'a [u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<Self, KeelError> {
        let grid = BlockGrid::new(width, height)?;
        let active_row_bytes = width
            .checked_mul(BGRA_BYTES_PER_PIXEL)
            .ok_or(KeelError::GeometryOverflow)?;
        if stride < active_row_bytes {
            return Err(KeelError::StrideTooSmall {
                stride,
                minimum: active_row_bytes,
            });
        }
        let required = stride
            .checked_mul(height)
            .ok_or(KeelError::GeometryOverflow)?;
        if pixels.len() < required {
            return Err(KeelError::BufferTooSmall {
                actual: pixels.len(),
                required,
            });
        }
        Ok(Self {
            pixels,
            grid,
            stride,
        })
    }

    #[must_use]
    pub const fn grid(self) -> BlockGrid {
        self.grid
    }

    #[must_use]
    pub const fn stride(self) -> usize {
        self.stride
    }

    #[must_use]
    pub const fn pixels(self) -> &'a [u8] {
        self.pixels
    }

    #[must_use]
    pub fn active_row(self, row: usize) -> Option<&'a [u8]> {
        if row >= self.grid.height {
            return None;
        }
        let start = row * self.stride;
        let end = start + self.grid.width * BGRA_BYTES_PER_PIXEL;
        Some(&self.pixels[start..end])
    }

    pub(crate) fn block_row_segment(self, pixel_row: usize, bounds: BlockBounds) -> &'a [u8] {
        let row_start = pixel_row * self.stride;
        let start = row_start + bounds.x * BGRA_BYTES_PER_PIXEL;
        let end = start + bounds.width * BGRA_BYTES_PER_PIXEL;
        &self.pixels[start..end]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeelError {
    ZeroDimension,
    GeometryOverflow,
    StrideTooSmall {
        stride: usize,
        minimum: usize,
    },
    BufferTooSmall {
        actual: usize,
        required: usize,
    },
    GeometryChanged {
        expected: BlockGrid,
        actual: BlockGrid,
    },
    ExternalBlockSizeZero,
    ExternalMapGeometry {
        expected_wide: usize,
        expected_tall: usize,
        actual_wide: usize,
        actual_tall: usize,
    },
    ExternalMapTooSmall {
        actual: usize,
        required: usize,
    },
}

impl fmt::Display for KeelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroDimension => formatter.write_str("frame dimensions must be non-zero"),
            Self::GeometryOverflow => formatter.write_str("frame geometry overflowed usize"),
            Self::StrideTooSmall { stride, minimum } => {
                write!(formatter, "BGRA stride {stride} is smaller than {minimum}")
            }
            Self::BufferTooSmall { actual, required } => {
                write!(
                    formatter,
                    "BGRA buffer has {actual} bytes, requires {required}"
                )
            }
            Self::GeometryChanged { expected, actual } => write!(
                formatter,
                "frame geometry changed from {}x{} to {}x{}",
                expected.width, expected.height, actual.width, actual.height
            ),
            Self::ExternalBlockSizeZero => {
                formatter.write_str("external damage block size must be non-zero")
            }
            Self::ExternalMapGeometry {
                expected_wide,
                expected_tall,
                actual_wide,
                actual_tall,
            } => write!(
                formatter,
                "external damage map is {actual_wide}x{actual_tall}, expected \
                 {expected_wide}x{expected_tall}"
            ),
            Self::ExternalMapTooSmall { actual, required } => write!(
                formatter,
                "external damage map has {actual} bytes, requires {required}"
            ),
        }
    }
}

impl std::error::Error for KeelError {}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn grid_handles_tail_blocks() {
        let grid = BlockGrid::new(33, 17).unwrap();
        assert_eq!(grid.blocks_wide(), 3);
        assert_eq!(grid.blocks_tall(), 2);
        assert_eq!(grid.block_count(), 6);
        assert_eq!(
            grid.block_bounds(5),
            Some(BlockBounds {
                x: 32,
                y: 16,
                width: 1,
                height: 1,
            })
        );
    }

    #[test]
    fn frame_rejects_stride_and_length_errors() {
        assert!(matches!(
            BgraFrame::new(&[0; 64], 16, 1, 63),
            Err(KeelError::StrideTooSmall { .. })
        ));
        assert!(matches!(
            BgraFrame::new(&[0; 63], 16, 1, 64),
            Err(KeelError::BufferTooSmall { .. })
        ));
    }
}
