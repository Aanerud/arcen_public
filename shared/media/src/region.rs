use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::num::{NonZeroU32, NonZeroU64};

/// Number of fixed-point units in one logical pixel.
pub const LOGICAL_UNITS_PER_PIXEL: i64 = 120;
/// Maximum number of regions in the first shared tranche.
pub const MAX_REGION_COUNT: usize = 4;
/// Maximum UTF-8 byte length of an output identity.
pub const MAX_OUTPUT_IDENTITY_BYTES: usize = 255;

const SCALE_DENOMINATOR: u32 = 120;

/// Stable nonzero identity for one independently rendered region.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(NonZeroU32);

impl RegionId {
    /// Creates a nonzero region identity.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::ZeroRegionId`] for zero.
    pub const fn new(value: u32) -> Result<Self, RegionContractError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(RegionContractError::ZeroRegionId),
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Endpoint-local stable identity of the output represented by a region.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OutputIdentity(String);

impl OutputIdentity {
    /// Creates a nonempty bounded output identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is empty or exceeds
    /// [`MAX_OUTPUT_IDENTITY_BYTES`].
    pub fn new(value: impl Into<String>) -> Result<Self, RegionContractError> {
        let value = value.into();
        if value.is_empty() {
            return Err(RegionContractError::EmptyOutputIdentity);
        }
        if value.len() > MAX_OUTPUT_IDENTITY_BYTES {
            return Err(RegionContractError::OutputIdentityTooLong(value.len()));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Monotonic nonzero generation of a complete region set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionGeneration(NonZeroU64);

impl RegionGeneration {
    /// Creates a nonzero region generation.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::ZeroGeneration`] for zero.
    pub const fn new(value: u64) -> Result<Self, RegionContractError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(RegionContractError::ZeroGeneration),
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A point in signed 1/120-logical-pixel fixed-point coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalPoint {
    pub x: i64,
    pub y: i64,
}

impl LogicalPoint {
    #[must_use]
    pub const fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }

    /// Creates a point from whole logical pixels.
    ///
    /// # Errors
    ///
    /// Returns an error when fixed-point conversion overflows.
    pub fn from_pixels(x: i64, y: i64) -> Result<Self, RegionContractError> {
        Ok(Self {
            x: x.checked_mul(LOGICAL_UNITS_PER_PIXEL)
                .ok_or(RegionContractError::CoordinateOverflow)?,
            y: y.checked_mul(LOGICAL_UNITS_PER_PIXEL)
                .ok_or(RegionContractError::CoordinateOverflow)?,
        })
    }
}

/// A nonempty size in 1/120-logical-pixel fixed-point units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogicalSize {
    width: u64,
    height: u64,
}

impl LogicalSize {
    /// Creates a nonempty fixed-point size.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::EmptyLogicalSize`] for a zero extent.
    pub const fn new(width: u64, height: u64) -> Result<Self, RegionContractError> {
        if width == 0 || height == 0 {
            return Err(RegionContractError::EmptyLogicalSize);
        }
        Ok(Self { width, height })
    }

    /// Creates a size from whole logical pixels.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero extent or conversion overflow.
    pub fn from_pixels(width: u64, height: u64) -> Result<Self, RegionContractError> {
        Self::new(
            width
                .checked_mul(LOGICAL_UNITS_PER_PIXEL as u64)
                .ok_or(RegionContractError::CoordinateOverflow)?,
            height
                .checked_mul(LOGICAL_UNITS_PER_PIXEL as u64)
                .ok_or(RegionContractError::CoordinateOverflow)?,
        )
    }

    #[must_use]
    pub const fn width(self) -> u64 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u64 {
        self.height
    }
}

/// A nonempty logical rectangle with exclusive right and bottom bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LogicalRect {
    origin: LogicalPoint,
    size: LogicalSize,
}

impl LogicalRect {
    /// Creates a rectangle whose exclusive bounds fit in `i64`.
    ///
    /// # Errors
    ///
    /// Returns an error when either exclusive bound overflows.
    pub fn new(origin: LogicalPoint, size: LogicalSize) -> Result<Self, RegionContractError> {
        checked_end(origin.x, size.width)?;
        checked_end(origin.y, size.height)?;
        Ok(Self { origin, size })
    }

    #[must_use]
    pub const fn origin(self) -> LogicalPoint {
        self.origin
    }

    #[must_use]
    pub const fn size(self) -> LogicalSize {
        self.size
    }

    /// Tests membership using exclusive right and bottom bounds.
    #[must_use]
    pub fn contains(self, point: LogicalPoint) -> bool {
        let right = i128::from(self.origin.x) + i128::from(self.size.width);
        let bottom = i128::from(self.origin.y) + i128::from(self.size.height);
        i128::from(point.x) >= i128::from(self.origin.x)
            && i128::from(point.x) < right
            && i128::from(point.y) >= i128::from(self.origin.y)
            && i128::from(point.y) < bottom
    }
}

fn checked_end(origin: i64, extent: u64) -> Result<i64, RegionContractError> {
    i64::try_from(i128::from(origin) + i128::from(extent))
        .map_err(|_| RegionContractError::CoordinateOverflow)
}

/// Fractional presentation scale expressed in units of 1/120.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Scale120(NonZeroU32);

impl Scale120 {
    /// Creates a positive fractional presentation scale.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::ZeroScale`] for zero.
    pub const fn new(value: u32) -> Result<Self, RegionContractError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(RegionContractError::ZeroScale),
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    #[must_use]
    pub const fn denominator() -> u32 {
        SCALE_DENOMINATOR
    }
}

/// Rotation and optional reflection applied to the physical stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OutputTransform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl OutputTransform {
    pub const ALL: [Self; 8] = [
        Self::Normal,
        Self::Rotate90,
        Self::Rotate180,
        Self::Rotate270,
        Self::Flipped,
        Self::Flipped90,
        Self::Flipped180,
        Self::Flipped270,
    ];

    #[must_use]
    pub const fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }
}

/// Explicit pre-transform physical stream extent in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalSize {
    width: u32,
    height: u32,
}

impl PhysicalSize {
    /// Creates a nonempty physical stream size.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::EmptyPhysicalSize`] for a zero extent.
    pub const fn new(width: u32, height: u32) -> Result<Self, RegionContractError> {
        if width == 0 || height == 0 {
            return Err(RegionContractError::EmptyPhysicalSize);
        }
        Ok(Self { width, height })
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Signed pixel index in the applied desktop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppliedPoint {
    pub x: i64,
    pub y: i64,
}

impl AppliedPoint {
    #[must_use]
    pub const fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }
}

/// Nonempty applied desktop size in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppliedSize {
    width: u32,
    height: u32,
}

impl AppliedSize {
    /// Creates a nonempty applied pixel size.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::EmptyAppliedSize`] for a zero extent.
    pub const fn new(width: u32, height: u32) -> Result<Self, RegionContractError> {
        if width == 0 || height == 0 {
            return Err(RegionContractError::EmptyAppliedSize);
        }
        Ok(Self { width, height })
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// Nonempty applied rectangle with exclusive right and bottom bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AppliedRect {
    origin: AppliedPoint,
    size: AppliedSize,
}

impl AppliedRect {
    /// Creates an applied rectangle with checked exclusive bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when either exclusive bound overflows.
    pub fn new(origin: AppliedPoint, size: AppliedSize) -> Result<Self, RegionContractError> {
        checked_end(origin.x, u64::from(size.width))?;
        checked_end(origin.y, u64::from(size.height))?;
        Ok(Self { origin, size })
    }

    #[must_use]
    pub const fn origin(self) -> AppliedPoint {
        self.origin
    }

    #[must_use]
    pub const fn size(self) -> AppliedSize {
        self.size
    }

    /// Tests pixel-index membership using exclusive right and bottom bounds.
    #[must_use]
    pub fn contains(self, point: AppliedPoint) -> bool {
        let right = i128::from(self.origin.x) + i128::from(self.size.width);
        let bottom = i128::from(self.origin.y) + i128::from(self.size.height);
        i128::from(point.x) >= i128::from(self.origin.x)
            && i128::from(point.x) < right
            && i128::from(point.y) >= i128::from(self.origin.y)
            && i128::from(point.y) < bottom
    }
}

/// Validated requested region geometry and output treatment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegionDescriptor {
    id: RegionId,
    output_identity: OutputIdentity,
    logical_rect: LogicalRect,
    physical_size: PhysicalSize,
    scale: Scale120,
    transform: OutputTransform,
    primary: bool,
}

impl RegionDescriptor {
    #[must_use]
    pub const fn new(
        id: RegionId,
        output_identity: OutputIdentity,
        logical_rect: LogicalRect,
        physical_size: PhysicalSize,
        scale: Scale120,
        transform: OutputTransform,
        primary: bool,
    ) -> Self {
        Self {
            id,
            output_identity,
            logical_rect,
            physical_size,
            scale,
            transform,
            primary,
        }
    }

    #[must_use]
    pub const fn id(&self) -> RegionId {
        self.id
    }

    #[must_use]
    pub const fn output_identity(&self) -> &OutputIdentity {
        &self.output_identity
    }

    #[must_use]
    pub const fn logical_rect(&self) -> LogicalRect {
        self.logical_rect
    }

    #[must_use]
    pub const fn physical_size(&self) -> PhysicalSize {
        self.physical_size
    }

    #[must_use]
    pub const fn scale(&self) -> Scale120 {
        self.scale
    }

    #[must_use]
    pub const fn transform(&self) -> OutputTransform {
        self.transform
    }

    #[must_use]
    pub const fn is_primary(&self) -> bool {
        self.primary
    }

    /// Returns the applied desktop footprint after output transformation.
    ///
    /// # Errors
    ///
    /// Returns an error only if a future size representation cannot fit the
    /// applied domain.
    pub const fn expected_applied_size(&self) -> Result<AppliedSize, RegionContractError> {
        if self.transform.swaps_axes() {
            AppliedSize::new(self.physical_size.height, self.physical_size.width)
        } else {
            AppliedSize::new(self.physical_size.width, self.physical_size.height)
        }
    }
}

/// A complete validated requested region generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionSet {
    generation: RegionGeneration,
    regions: Vec<RegionDescriptor>,
    primary_index: usize,
}

impl RegionSet {
    /// Creates a 1..=4 set with unique region/output identities and one primary.
    ///
    /// # Errors
    ///
    /// Returns the first roster invariant that is not satisfied.
    pub fn new(
        generation: RegionGeneration,
        regions: Vec<RegionDescriptor>,
    ) -> Result<Self, RegionContractError> {
        let primary_index = validate_descriptors(&regions)?;
        Ok(Self {
            generation,
            regions,
            primary_index,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> RegionGeneration {
        self.generation
    }

    #[must_use]
    pub fn regions(&self) -> &[RegionDescriptor] {
        &self.regions
    }

    #[must_use]
    pub fn primary(&self) -> &RegionDescriptor {
        &self.regions[self.primary_index]
    }

    #[must_use]
    pub fn get(&self, id: RegionId) -> Option<&RegionDescriptor> {
        self.regions.iter().find(|region| region.id == id)
    }
}

fn validate_descriptors(regions: &[RegionDescriptor]) -> Result<usize, RegionContractError> {
    if !(1..=MAX_REGION_COUNT).contains(&regions.len()) {
        return Err(RegionContractError::UnsupportedRegionCount(regions.len()));
    }
    let primary_count = regions.iter().filter(|region| region.primary).count();
    if primary_count != 1 {
        return Err(RegionContractError::PrimaryRegionCount(primary_count));
    }
    let mut region_ids = BTreeSet::new();
    let mut output_ids = BTreeSet::new();
    for region in regions {
        if !region_ids.insert(region.id) {
            return Err(RegionContractError::DuplicateRegionId(region.id));
        }
        if !output_ids.insert(region.output_identity.as_str()) {
            return Err(RegionContractError::DuplicateOutputIdentity(
                region.output_identity.clone(),
            ));
        }
    }
    regions
        .iter()
        .position(|region| region.primary)
        .ok_or(RegionContractError::PrimaryRegionCount(0))
}

/// One requested region placed in the applied pixel desktop.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppliedRegionDescriptor {
    descriptor: RegionDescriptor,
    applied_rect: AppliedRect,
}

impl AppliedRegionDescriptor {
    /// Creates an applied region with the transformed physical stream extent.
    ///
    /// # Errors
    ///
    /// Returns [`RegionContractError::AppliedSizeMismatch`] when the supplied
    /// footprint differs from the explicit stream size after transformation.
    pub fn new(
        descriptor: RegionDescriptor,
        applied_rect: AppliedRect,
    ) -> Result<Self, RegionContractError> {
        let expected = descriptor.expected_applied_size()?;
        if expected != applied_rect.size {
            return Err(RegionContractError::AppliedSizeMismatch {
                expected,
                actual: applied_rect.size,
            });
        }
        Ok(Self {
            descriptor,
            applied_rect,
        })
    }

    #[must_use]
    pub const fn id(&self) -> RegionId {
        self.descriptor.id
    }

    #[must_use]
    pub const fn descriptor(&self) -> &RegionDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub const fn applied_rect(&self) -> AppliedRect {
        self.applied_rect
    }
}

/// A complete applied region generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRegionSet {
    generation: RegionGeneration,
    regions: Vec<AppliedRegionDescriptor>,
    primary_index: usize,
}

impl AppliedRegionSet {
    /// Creates a 1..=4 applied set with unique identities and one primary.
    ///
    /// # Errors
    ///
    /// Returns the first roster invariant that is not satisfied.
    pub fn new(
        generation: RegionGeneration,
        regions: Vec<AppliedRegionDescriptor>,
    ) -> Result<Self, RegionContractError> {
        let descriptors = regions
            .iter()
            .map(|region| region.descriptor.clone())
            .collect::<Vec<_>>();
        let primary_index = validate_descriptors(&descriptors)?;
        Ok(Self {
            generation,
            regions,
            primary_index,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> RegionGeneration {
        self.generation
    }

    #[must_use]
    pub fn regions(&self) -> &[AppliedRegionDescriptor] {
        &self.regions
    }

    #[must_use]
    pub fn primary(&self) -> &AppliedRegionDescriptor {
        &self.regions[self.primary_index]
    }

    #[must_use]
    pub fn get(&self, id: RegionId) -> Option<&AppliedRegionDescriptor> {
        self.regions.iter().find(|region| region.id() == id)
    }
}

/// Region-domain validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionContractError {
    ZeroRegionId,
    EmptyOutputIdentity,
    OutputIdentityTooLong(usize),
    ZeroGeneration,
    EmptyLogicalSize,
    ZeroScale,
    EmptyPhysicalSize,
    EmptyAppliedSize,
    CoordinateOverflow,
    UnsupportedRegionCount(usize),
    PrimaryRegionCount(usize),
    DuplicateRegionId(RegionId),
    DuplicateOutputIdentity(OutputIdentity),
    AppliedSizeMismatch {
        expected: AppliedSize,
        actual: AppliedSize,
    },
}

impl Display for RegionContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroRegionId => formatter.write_str("region identity must be nonzero"),
            Self::EmptyOutputIdentity => formatter.write_str("output identity must not be empty"),
            Self::OutputIdentityTooLong(length) => {
                write!(formatter, "output identity is too long: {length} bytes")
            }
            Self::ZeroGeneration => formatter.write_str("region generation must be nonzero"),
            Self::EmptyLogicalSize => formatter.write_str("logical size must be nonempty"),
            Self::ZeroScale => formatter.write_str("Scale120 must be nonzero"),
            Self::EmptyPhysicalSize => formatter.write_str("physical stream size must be nonempty"),
            Self::EmptyAppliedSize => formatter.write_str("applied size must be nonempty"),
            Self::CoordinateOverflow => formatter.write_str("region coordinate overflow"),
            Self::UnsupportedRegionCount(count) => {
                write!(
                    formatter,
                    "region count {count} is outside 1..={MAX_REGION_COUNT}"
                )
            }
            Self::PrimaryRegionCount(count) => {
                write!(
                    formatter,
                    "expected exactly one primary region, found {count}"
                )
            }
            Self::DuplicateRegionId(id) => write!(formatter, "duplicate region id {}", id.get()),
            Self::DuplicateOutputIdentity(id) => {
                write!(formatter, "duplicate output identity {}", id.as_str())
            }
            Self::AppliedSizeMismatch { expected, actual } => write!(
                formatter,
                "applied size {}x{} does not match expected {}x{}",
                actual.width, actual.height, expected.width, expected.height
            ),
        }
    }
}

impl Error for RegionContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(
        id: u32,
        output: &str,
        transform: OutputTransform,
        primary: bool,
    ) -> RegionDescriptor {
        RegionDescriptor::new(
            RegionId::new(id).unwrap(),
            OutputIdentity::new(output).unwrap(),
            LogicalRect::new(
                LogicalPoint::from_pixels(-4, 3).unwrap(),
                LogicalSize::from_pixels(8, 12).unwrap(),
            )
            .unwrap(),
            PhysicalSize::new(10, 15).unwrap(),
            Scale120::new(150).unwrap(),
            transform,
            primary,
        )
    }

    #[test]
    fn identity_generation_and_sizes_reject_invalid_values() {
        assert_eq!(RegionId::new(0), Err(RegionContractError::ZeroRegionId));
        assert_eq!(
            OutputIdentity::new(""),
            Err(RegionContractError::EmptyOutputIdentity)
        );
        assert_eq!(
            RegionGeneration::new(0),
            Err(RegionContractError::ZeroGeneration)
        );
        assert_eq!(
            LogicalSize::new(0, 1),
            Err(RegionContractError::EmptyLogicalSize)
        );
        assert_eq!(Scale120::new(0), Err(RegionContractError::ZeroScale));
        assert_eq!(
            PhysicalSize::new(0, 1),
            Err(RegionContractError::EmptyPhysicalSize)
        );
    }

    #[test]
    fn physical_stream_size_is_explicit_and_only_transform_changes_footprint() {
        for transform in OutputTransform::ALL {
            let descriptor = descriptor(1, "deck-a", transform, true);
            let size = descriptor.expected_applied_size().unwrap();
            let expected = if transform.swaps_axes() {
                (15, 10)
            } else {
                (10, 15)
            };
            assert_eq!((size.width(), size.height()), expected);
            assert_eq!(descriptor.scale().get(), 150);
        }
    }

    #[test]
    fn sets_enforce_count_primary_and_both_identity_domains() {
        assert_eq!(
            RegionSet::new(RegionGeneration::new(1).unwrap(), vec![]),
            Err(RegionContractError::UnsupportedRegionCount(0))
        );
        let primary = descriptor(1, "deck-a", OutputTransform::Normal, true);
        let non_primary = descriptor(2, "deck-b", OutputTransform::Normal, false);
        assert_eq!(
            RegionSet::new(RegionGeneration::new(1).unwrap(), vec![non_primary.clone()]),
            Err(RegionContractError::PrimaryRegionCount(0))
        );
        assert_eq!(
            RegionSet::new(
                RegionGeneration::new(1).unwrap(),
                vec![primary.clone(), primary.clone()]
            ),
            Err(RegionContractError::PrimaryRegionCount(2))
        );
        assert_eq!(
            RegionSet::new(
                RegionGeneration::new(1).unwrap(),
                vec![
                    primary.clone(),
                    descriptor(1, "deck-b", OutputTransform::Normal, false)
                ]
            ),
            Err(RegionContractError::DuplicateRegionId(primary.id()))
        );
        assert_eq!(
            RegionSet::new(
                RegionGeneration::new(1).unwrap(),
                vec![
                    primary.clone(),
                    descriptor(2, "deck-a", OutputTransform::Normal, false)
                ]
            ),
            Err(RegionContractError::DuplicateOutputIdentity(
                OutputIdentity::new("deck-a").unwrap()
            ))
        );
        let five = vec![
            primary,
            non_primary,
            descriptor(3, "deck-c", OutputTransform::Normal, false),
            descriptor(4, "deck-d", OutputTransform::Normal, false),
            descriptor(5, "deck-e", OutputTransform::Normal, false),
        ];
        assert_eq!(
            RegionSet::new(RegionGeneration::new(1).unwrap(), five),
            Err(RegionContractError::UnsupportedRegionCount(5))
        );
    }

    #[test]
    fn logical_and_applied_membership_use_exclusive_bounds() {
        let logical = LogicalRect::new(
            LogicalPoint::new(-240, -120),
            LogicalSize::new(360, 240).unwrap(),
        )
        .unwrap();
        assert!(logical.contains(LogicalPoint::new(-240, -120)));
        assert!(logical.contains(LogicalPoint::new(119, 119)));
        assert!(!logical.contains(LogicalPoint::new(120, 120)));

        let applied =
            AppliedRect::new(AppliedPoint::new(-10, 4), AppliedSize::new(3, 2).unwrap()).unwrap();
        assert!(applied.contains(AppliedPoint::new(-10, 4)));
        assert!(applied.contains(AppliedPoint::new(-8, 5)));
        assert!(!applied.contains(AppliedPoint::new(-7, 5)));
    }
}
