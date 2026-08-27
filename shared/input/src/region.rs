use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_media::{
    AppliedPoint, AppliedRegionDescriptor, AppliedRegionSet, LogicalPoint, OutputTransform,
    RegionId,
};

/// Pure coordinate service for region-scoped logical input.
#[derive(Debug, Clone, Copy)]
pub struct RegionCoordinateTransformer<'a> {
    regions: &'a AppliedRegionSet,
}

impl<'a> RegionCoordinateTransformer<'a> {
    #[must_use]
    pub const fn new(regions: &'a AppliedRegionSet) -> Self {
        Self { regions }
    }

    /// Maps a region-local fixed-point logical coordinate to an applied pixel
    /// index.
    ///
    /// Logical membership is exclusive at the right and bottom extents. The
    /// first and last representable logical coordinates map to pixel indices
    /// zero and `extent - 1`.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown region, an out-of-region point, or
    /// arithmetic overflow.
    pub fn logical_to_applied(
        self,
        region_id: RegionId,
        point: LogicalPoint,
    ) -> Result<AppliedPoint, CoordinateTransformError> {
        let region = self.region(region_id)?;
        let logical_size = region.descriptor().logical_rect().size();
        let x = local_coordinate(point.x, logical_size.width())?;
        let y = local_coordinate(point.y, logical_size.height())?;
        let physical = region.descriptor().physical_size();
        let x = map_axis_forward(x, logical_size.width(), physical.width())?;
        let y = map_axis_forward(y, logical_size.height(), physical.height())?;
        let max_x = i64::from(physical.width() - 1);
        let max_y = i64::from(physical.height() - 1);
        let (x, y) = transform_forward(region.descriptor().transform(), x, y, max_x, max_y);
        let origin = region.applied_rect().origin();
        Ok(AppliedPoint::new(
            checked_add(origin.x, x)?,
            checked_add(origin.y, y)?,
        ))
    }

    /// Maps an applied pixel index to a region-local fixed-point logical
    /// coordinate.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown region, an out-of-region pixel index,
    /// or arithmetic overflow.
    pub fn applied_to_logical(
        self,
        region_id: RegionId,
        point: AppliedPoint,
    ) -> Result<LogicalPoint, CoordinateTransformError> {
        let region = self.region(region_id)?;
        if !region.applied_rect().contains(point) {
            return Err(CoordinateTransformError::AppliedPointOutsideRegion);
        }
        let origin = region.applied_rect().origin();
        let x = checked_sub(point.x, origin.x)?;
        let y = checked_sub(point.y, origin.y)?;
        let physical = region.descriptor().physical_size();
        let max_x = i64::from(physical.width() - 1);
        let max_y = i64::from(physical.height() - 1);
        let (x, y) = transform_inverse(region.descriptor().transform(), x, y, max_x, max_y);
        let logical_size = region.descriptor().logical_rect().size();
        Ok(LogicalPoint::new(
            map_axis_inverse(x, logical_size.width(), physical.width())?,
            map_axis_inverse(y, logical_size.height(), physical.height())?,
        ))
    }

    fn region(
        self,
        region_id: RegionId,
    ) -> Result<&'a AppliedRegionDescriptor, CoordinateTransformError> {
        self.regions
            .get(region_id)
            .ok_or(CoordinateTransformError::UnknownRegion(region_id))
    }
}

fn local_coordinate(coordinate: i64, extent: u64) -> Result<u64, CoordinateTransformError> {
    let coordinate = u64::try_from(coordinate)
        .map_err(|_| CoordinateTransformError::LogicalPointOutsideRegion)?;
    if coordinate >= extent {
        return Err(CoordinateTransformError::LogicalPointOutsideRegion);
    }
    Ok(coordinate)
}

fn map_axis_forward(
    coordinate: u64,
    logical_extent: u64,
    physical_extent: u32,
) -> Result<i64, CoordinateTransformError> {
    if logical_extent == 1 || physical_extent == 1 {
        return Ok(0);
    }
    let numerator = u128::from(coordinate)
        .checked_mul(u128::from(physical_extent - 1))
        .ok_or(CoordinateTransformError::CoordinateOverflow)?;
    rounded_ratio(numerator, u128::from(logical_extent - 1))
}

fn map_axis_inverse(
    coordinate: i64,
    logical_extent: u64,
    physical_extent: u32,
) -> Result<i64, CoordinateTransformError> {
    if logical_extent == 1 || physical_extent == 1 {
        return Ok(0);
    }
    let coordinate =
        u64::try_from(coordinate).map_err(|_| CoordinateTransformError::CoordinateOverflow)?;
    let numerator = u128::from(coordinate)
        .checked_mul(u128::from(logical_extent - 1))
        .ok_or(CoordinateTransformError::CoordinateOverflow)?;
    rounded_ratio(numerator, u128::from(physical_extent - 1))
}

fn rounded_ratio(numerator: u128, denominator: u128) -> Result<i64, CoordinateTransformError> {
    let rounded = numerator
        .checked_add(denominator / 2)
        .ok_or(CoordinateTransformError::CoordinateOverflow)?
        / denominator;
    i64::try_from(rounded).map_err(|_| CoordinateTransformError::CoordinateOverflow)
}

fn checked_add(left: i64, right: i64) -> Result<i64, CoordinateTransformError> {
    left.checked_add(right)
        .ok_or(CoordinateTransformError::CoordinateOverflow)
}

fn checked_sub(left: i64, right: i64) -> Result<i64, CoordinateTransformError> {
    left.checked_sub(right)
        .ok_or(CoordinateTransformError::CoordinateOverflow)
}

fn transform_forward(
    transform: OutputTransform,
    x: i64,
    y: i64,
    max_x: i64,
    max_y: i64,
) -> (i64, i64) {
    match transform {
        OutputTransform::Normal => (x, y),
        OutputTransform::Rotate90 => (max_y - y, x),
        OutputTransform::Rotate180 => (max_x - x, max_y - y),
        OutputTransform::Rotate270 => (y, max_x - x),
        OutputTransform::Flipped => (max_x - x, y),
        OutputTransform::Flipped90 => (max_y - y, max_x - x),
        OutputTransform::Flipped180 => (x, max_y - y),
        OutputTransform::Flipped270 => (y, x),
    }
}

fn transform_inverse(
    transform: OutputTransform,
    x: i64,
    y: i64,
    max_x: i64,
    max_y: i64,
) -> (i64, i64) {
    match transform {
        OutputTransform::Normal => (x, y),
        OutputTransform::Rotate90 => (y, max_y - x),
        OutputTransform::Rotate180 => (max_x - x, max_y - y),
        OutputTransform::Rotate270 => (max_x - y, x),
        OutputTransform::Flipped => (max_x - x, y),
        OutputTransform::Flipped90 => (max_x - y, max_y - x),
        OutputTransform::Flipped180 => (x, max_y - y),
        OutputTransform::Flipped270 => (y, x),
    }
}

/// Coordinate transformation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoordinateTransformError {
    UnknownRegion(RegionId),
    LogicalPointOutsideRegion,
    AppliedPointOutsideRegion,
    CoordinateOverflow,
}

impl Display for CoordinateTransformError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRegion(id) => write!(formatter, "unknown region {}", id.get()),
            Self::LogicalPointOutsideRegion => {
                formatter.write_str("logical point is outside the region")
            }
            Self::AppliedPointOutsideRegion => {
                formatter.write_str("applied point is outside the region")
            }
            Self::CoordinateOverflow => formatter.write_str("coordinate transform overflow"),
        }
    }
}

impl Error for CoordinateTransformError {}
