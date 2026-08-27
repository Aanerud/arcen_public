//! Host-local libei/EIS provider seam and pure region mapping.
//!
//! The model mirrors the EIS region contract: unsigned desktop-wide logical
//! offsets, nonempty logical sizes, optional mapping IDs, and physical scale.
//! It reconciles those advertised regions with `arcen-media` logical regions
//! and translates negative Wayland origins into a nonnegative EIS layout.
//! Physical scale is metadata for relative-to-absolute conversion; absolute
//! region coordinates remain in EIS logical pixels.
//!
//! No libei, portal, D-Bus, or compositor code is linked here. The
//! default-off `wayland-provider` feature exposes the provider trait, while
//! runtime selection remains fail-closed because the native adapter
//! implementation constant is false.

use std::collections::BTreeSet;
use std::num::NonZeroU64;

use arcen_media::{
    LogicalPoint, OutputIdentity, RegionId, RegionSet, Scale120, LOGICAL_UNITS_PER_PIXEL,
};
#[cfg(feature = "wayland-provider")]
use std::error::Error;
use thiserror::Error;

use crate::display::wayland::{ProbeState, WAYLAND_PROVIDER_FEATURE_ENABLED};

/// Native portal/libei connection and event injection are not implemented.
pub const NATIVE_LIBEI_ADAPTER_IMPLEMENTED: bool = false;

/// Runtime evidence required before a libei input provider may be selected.
///
/// These facts must come from an authoritative portal/libei adapter. An
/// environment variable or a library file existing on disk is not sufficient
/// proof that the compositor granted an EIS connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EisRuntimeFacts {
    pub output_regions: ProbeState,
    pub remote_desktop_portal: ProbeState,
    pub eis_connection: ProbeState,
    pub absolute_pointer: ProbeState,
    pub mapping_ids: ProbeState,
}

/// Why the Wayland input path must not be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EisInputUnavailableReason {
    #[error("the binary was not compiled with the `wayland-provider` feature")]
    FeatureDisabled,
    #[error("the EIS input provider is supported only on Linux targets")]
    UnsupportedTarget,
    #[error("Wayland output regions could not be authoritatively established")]
    OutputRegionsUnknown,
    #[error("Wayland output regions are unavailable")]
    OutputRegionsUnavailable,
    #[error("the RemoteDesktop portal could not be authoritatively probed")]
    PortalUnknown,
    #[error("the RemoteDesktop portal is unavailable")]
    PortalUnavailable,
    #[error("an EIS connection was not authoritatively established")]
    EisConnectionUnknown,
    #[error("the portal did not provide an EIS connection")]
    EisConnectionUnavailable,
    #[error("absolute-pointer capability was not authoritatively negotiated")]
    AbsolutePointerUnknown,
    #[error("the EIS seat does not provide an absolute-pointer device")]
    AbsolutePointerUnavailable,
    #[error("no native libei adapter is implemented in this binary")]
    NativeAdapterUnavailable,
}

/// Capabilities an actual libei provider implementation may expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputProviderCapabilities {
    pub absolute_pointer: bool,
    pub region_mapping: bool,
    pub mapping_ids: bool,
    pub physical_scale: bool,
}

/// Fail-closed runtime detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputCapabilityReport {
    availability: Result<InputProviderCapabilities, EisInputUnavailableReason>,
}

impl InputCapabilityReport {
    pub const fn availability(
        &self,
    ) -> &Result<InputProviderCapabilities, EisInputUnavailableReason> {
        &self.availability
    }
}

#[derive(Debug, Clone, Copy)]
struct InputBuildFacts {
    feature_enabled: bool,
    linux_target: bool,
    native_adapter: bool,
}

/// Evaluates the current binary and supplied portal/libei runtime facts.
#[must_use]
pub fn detect_input_capability(facts: EisRuntimeFacts) -> InputCapabilityReport {
    evaluate_input_capability(
        InputBuildFacts {
            feature_enabled: WAYLAND_PROVIDER_FEATURE_ENABLED,
            linux_target: cfg!(target_os = "linux"),
            native_adapter: NATIVE_LIBEI_ADAPTER_IMPLEMENTED,
        },
        facts,
    )
}

fn evaluate_input_capability(
    build: InputBuildFacts,
    facts: EisRuntimeFacts,
) -> InputCapabilityReport {
    let availability = (|| {
        if !build.feature_enabled {
            return Err(EisInputUnavailableReason::FeatureDisabled);
        }
        if !build.linux_target {
            return Err(EisInputUnavailableReason::UnsupportedTarget);
        }
        require_probe(
            facts.output_regions,
            EisInputUnavailableReason::OutputRegionsUnknown,
            EisInputUnavailableReason::OutputRegionsUnavailable,
        )?;
        require_probe(
            facts.remote_desktop_portal,
            EisInputUnavailableReason::PortalUnknown,
            EisInputUnavailableReason::PortalUnavailable,
        )?;
        require_probe(
            facts.eis_connection,
            EisInputUnavailableReason::EisConnectionUnknown,
            EisInputUnavailableReason::EisConnectionUnavailable,
        )?;
        require_probe(
            facts.absolute_pointer,
            EisInputUnavailableReason::AbsolutePointerUnknown,
            EisInputUnavailableReason::AbsolutePointerUnavailable,
        )?;
        if !build.native_adapter {
            return Err(EisInputUnavailableReason::NativeAdapterUnavailable);
        }
        Ok(InputProviderCapabilities {
            absolute_pointer: true,
            region_mapping: true,
            mapping_ids: facts.mapping_ids == ProbeState::Available,
            physical_scale: true,
        })
    })();
    InputCapabilityReport { availability }
}

fn require_probe(
    state: ProbeState,
    unknown: EisInputUnavailableReason,
    unavailable: EisInputUnavailableReason,
) -> Result<(), EisInputUnavailableReason> {
    match state {
        ProbeState::Unknown => Err(unknown),
        ProbeState::Unavailable => Err(unavailable),
        ProbeState::Available => Ok(()),
    }
}

/// Host-local libei provider seam. No implementation is supplied here.
#[cfg(feature = "wayland-provider")]
pub trait InputProvider {
    type Error: Error + Send + Sync + 'static;

    fn capabilities(&self) -> InputProviderCapabilities;
    fn advertised_regions(&mut self) -> Result<Vec<EisRegion>, Self::Error>;
}

/// Opaque nonzero handle assigned by a native adapter to one advertised EIS
/// region. It is process-local and never crosses the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EisRegionHandle(NonZeroU64);

impl EisRegionHandle {
    /// # Errors
    ///
    /// Returns [`EisRegionMapError::ZeroRegionHandle`] for zero.
    pub const fn new(value: u64) -> Result<Self, EisRegionMapError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(EisRegionMapError::ZeroRegionHandle),
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One region advertised by EIS for an absolute input device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EisRegion {
    handle: EisRegionHandle,
    mapping_id: Option<String>,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    physical_scale: Scale120,
}

impl EisRegion {
    /// # Errors
    ///
    /// Rejects an empty region or an explicitly empty mapping ID.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        handle: EisRegionHandle,
        mapping_id: Option<String>,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        physical_scale: Scale120,
    ) -> Result<Self, EisRegionMapError> {
        if width == 0 || height == 0 {
            return Err(EisRegionMapError::EmptyAdvertisedRegion(handle));
        }
        if mapping_id.as_deref() == Some("") {
            return Err(EisRegionMapError::EmptyMappingId(handle));
        }
        Ok(Self {
            handle,
            mapping_id,
            x,
            y,
            width,
            height,
            physical_scale,
        })
    }

    #[must_use]
    pub const fn handle(&self) -> EisRegionHandle {
        self.handle
    }

    #[must_use]
    pub fn mapping_id(&self) -> Option<&str> {
        self.mapping_id.as_deref()
    }

    #[must_use]
    pub const fn x(&self) -> u32 {
        self.x
    }

    #[must_use]
    pub const fn y(&self) -> u32 {
        self.y
    }

    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn physical_scale(&self) -> Scale120 {
        self.physical_scale
    }

    fn has_geometry(&self, expected: &ExpectedRegion) -> bool {
        self.x == expected.x
            && self.y == expected.y
            && self.width == expected.width
            && self.height == expected.height
    }
}

/// Finite EIS desktop-wide logical point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EisPoint {
    pub x: f64,
    pub y: f64,
}

impl EisPoint {
    /// # Errors
    ///
    /// Rejects NaN and infinite coordinates.
    pub fn new(x: f64, y: f64) -> Result<Self, EisRegionMapError> {
        if !x.is_finite() || !y.is_finite() {
            return Err(EisRegionMapError::NonFinitePoint);
        }
        Ok(Self { x, y })
    }
}

#[derive(Debug, Clone)]
struct ExpectedRegion {
    region_id: RegionId,
    output_identity: OutputIdentity,
    presentation_scale: Scale120,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    width_units: u64,
    height_units: u64,
}

/// One reconciled Arcen region to EIS region binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EisRegionBinding {
    region_id: RegionId,
    output_identity: OutputIdentity,
    presentation_scale: Scale120,
    eis_region: EisRegion,
    logical_width_units: u64,
    logical_height_units: u64,
}

impl EisRegionBinding {
    #[must_use]
    pub const fn region_id(&self) -> RegionId {
        self.region_id
    }

    #[must_use]
    pub const fn output_identity(&self) -> &OutputIdentity {
        &self.output_identity
    }

    /// Wayland/output presentation scale. This is intentionally distinct from
    /// [`EisRegion::physical_scale`].
    #[must_use]
    pub const fn presentation_scale(&self) -> Scale120 {
        self.presentation_scale
    }

    #[must_use]
    pub const fn eis_region(&self) -> &EisRegion {
        &self.eis_region
    }
}

/// Validated one-to-one mapping between Arcen logical regions and EIS
/// absolute-device regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EisRegionMap {
    bindings: Vec<EisRegionBinding>,
}

impl EisRegionMap {
    /// Reconciles a current Arcen region set with regions authoritatively
    /// advertised by EIS.
    ///
    /// Mapping ID is preferred. Duplicate IDs during resize are tolerated only
    /// when exact geometry selects one current region. Without a mapping ID,
    /// exact translated geometry must identify exactly one unused EIS region.
    ///
    /// # Errors
    ///
    /// Fails closed on non-integral logical pixels, coordinate overflow,
    /// duplicate handles, missing/ambiguous matches, or mapping-ID geometry
    /// disagreement.
    pub fn reconcile(
        regions: &RegionSet,
        advertised: &[EisRegion],
    ) -> Result<Self, EisRegionMapError> {
        let expected = expected_regions(regions)?;
        let mut handles = BTreeSet::new();
        for region in advertised {
            if !handles.insert(region.handle) {
                return Err(EisRegionMapError::DuplicateRegionHandle(region.handle));
            }
        }

        let mut used = BTreeSet::new();
        let mut bindings = Vec::with_capacity(expected.len());
        for expected in expected {
            let matching_id = advertised
                .iter()
                .filter(|region| {
                    !used.contains(&region.handle)
                        && region.mapping_id() == Some(expected.output_identity.as_str())
                })
                .collect::<Vec<_>>();
            let selected = if matching_id.is_empty() {
                select_exact_geometry(&expected, advertised, &used)?
            } else {
                let exact = matching_id
                    .into_iter()
                    .filter(|region| region.has_geometry(&expected))
                    .collect::<Vec<_>>();
                match exact.as_slice() {
                    [region] => *region,
                    [] => {
                        return Err(EisRegionMapError::MappingIdGeometryMismatch(
                            expected.region_id,
                        ));
                    }
                    _ => return Err(EisRegionMapError::AmbiguousRegion(expected.region_id)),
                }
            };
            used.insert(selected.handle);
            bindings.push(EisRegionBinding {
                region_id: expected.region_id,
                output_identity: expected.output_identity,
                presentation_scale: expected.presentation_scale,
                eis_region: selected.clone(),
                logical_width_units: expected.width_units,
                logical_height_units: expected.height_units,
            });
        }
        Ok(Self { bindings })
    }

    #[must_use]
    pub fn bindings(&self) -> &[EisRegionBinding] {
        &self.bindings
    }

    #[must_use]
    pub fn get(&self, region_id: RegionId) -> Option<&EisRegionBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.region_id == region_id)
    }

    /// Converts a region-local Arcen fixed-point point to EIS desktop-wide
    /// logical coordinates.
    ///
    /// # Errors
    ///
    /// Rejects an unknown region or a point outside its exclusive bounds.
    #[allow(clippy::cast_precision_loss)]
    pub fn region_local_to_eis(
        &self,
        region_id: RegionId,
        point: LogicalPoint,
    ) -> Result<EisPoint, EisRegionMapError> {
        let binding = self
            .get(region_id)
            .ok_or(EisRegionMapError::UnknownRegion(region_id))?;
        let x = local_units(point.x, binding.logical_width_units)?;
        let y = local_units(point.y, binding.logical_height_units)?;
        EisPoint::new(
            f64::from(binding.eis_region.x) + x as f64 / LOGICAL_UNITS_PER_PIXEL as f64,
            f64::from(binding.eis_region.y) + y as f64 / LOGICAL_UNITS_PER_PIXEL as f64,
        )
    }

    /// Converts an EIS desktop-wide point to region-local Arcen fixed-point
    /// coordinates.
    ///
    /// # Errors
    ///
    /// Rejects an unknown handle, non-finite point, or a point outside the
    /// region's exclusive EIS bounds.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn eis_to_region_local(
        &self,
        handle: EisRegionHandle,
        point: EisPoint,
    ) -> Result<LogicalPoint, EisRegionMapError> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(EisRegionMapError::NonFinitePoint);
        }
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.eis_region.handle == handle)
            .ok_or(EisRegionMapError::UnknownHandle(handle))?;
        let region = &binding.eis_region;
        let right = f64::from(region.x) + f64::from(region.width);
        let bottom = f64::from(region.y) + f64::from(region.height);
        if point.x < f64::from(region.x)
            || point.x >= right
            || point.y < f64::from(region.y)
            || point.y >= bottom
        {
            return Err(EisRegionMapError::PointOutsideRegion);
        }
        let x = ((point.x - f64::from(region.x)) * LOGICAL_UNITS_PER_PIXEL as f64)
            .round()
            .clamp(0.0, binding.logical_width_units.saturating_sub(1) as f64)
            as i64;
        let y = ((point.y - f64::from(region.y)) * LOGICAL_UNITS_PER_PIXEL as f64)
            .round()
            .clamp(0.0, binding.logical_height_units.saturating_sub(1) as f64)
            as i64;
        Ok(LogicalPoint::new(x, y))
    }
}

fn select_exact_geometry<'a>(
    expected: &ExpectedRegion,
    advertised: &'a [EisRegion],
    used: &BTreeSet<EisRegionHandle>,
) -> Result<&'a EisRegion, EisRegionMapError> {
    let exact = advertised
        .iter()
        .filter(|region| !used.contains(&region.handle) && region.has_geometry(expected))
        .collect::<Vec<_>>();
    match exact.as_slice() {
        [region] => Ok(*region),
        [] => Err(EisRegionMapError::MissingRegion(expected.region_id)),
        _ => Err(EisRegionMapError::AmbiguousRegion(expected.region_id)),
    }
}

fn expected_regions(regions: &RegionSet) -> Result<Vec<ExpectedRegion>, EisRegionMapError> {
    let mut raw = Vec::with_capacity(regions.regions().len());
    for region in regions.regions() {
        let rect = region.logical_rect();
        let x = origin_pixels(region.id(), rect.origin().x)?;
        let y = origin_pixels(region.id(), rect.origin().y)?;
        let width = extent_pixels(region.id(), rect.size().width())?;
        let height = extent_pixels(region.id(), rect.size().height())?;
        raw.push((
            region.id(),
            region.output_identity().clone(),
            region.scale(),
            x,
            y,
            width,
            height,
            rect.size().width(),
            rect.size().height(),
        ));
    }
    let min_x = raw.iter().map(|region| region.3).min().unwrap_or(0);
    let min_y = raw.iter().map(|region| region.4).min().unwrap_or(0);
    raw.into_iter()
        .map(
            |(
                region_id,
                output_identity,
                presentation_scale,
                x,
                y,
                width,
                height,
                width_units,
                height_units,
            )| {
                Ok(ExpectedRegion {
                    region_id,
                    output_identity,
                    presentation_scale,
                    x: u32::try_from(
                        x.checked_sub(min_x)
                            .ok_or(EisRegionMapError::CoordinateOverflow)?,
                    )
                    .map_err(|_| EisRegionMapError::CoordinateOverflow)?,
                    y: u32::try_from(
                        y.checked_sub(min_y)
                            .ok_or(EisRegionMapError::CoordinateOverflow)?,
                    )
                    .map_err(|_| EisRegionMapError::CoordinateOverflow)?,
                    width,
                    height,
                    width_units,
                    height_units,
                })
            },
        )
        .collect()
}

fn origin_pixels(region_id: RegionId, value: i64) -> Result<i64, EisRegionMapError> {
    if value % LOGICAL_UNITS_PER_PIXEL != 0 {
        return Err(EisRegionMapError::NonIntegralLogicalRegion(region_id));
    }
    Ok(value / LOGICAL_UNITS_PER_PIXEL)
}

fn extent_pixels(region_id: RegionId, value: u64) -> Result<u32, EisRegionMapError> {
    let denominator = u64::try_from(LOGICAL_UNITS_PER_PIXEL)
        .map_err(|_| EisRegionMapError::CoordinateOverflow)?;
    if !value.is_multiple_of(denominator) {
        return Err(EisRegionMapError::NonIntegralLogicalRegion(region_id));
    }
    u32::try_from(value / denominator).map_err(|_| EisRegionMapError::CoordinateOverflow)
}

fn local_units(value: i64, extent: u64) -> Result<u64, EisRegionMapError> {
    let value = u64::try_from(value).map_err(|_| EisRegionMapError::PointOutsideRegion)?;
    if value >= extent {
        return Err(EisRegionMapError::PointOutsideRegion);
    }
    Ok(value)
}

/// Validation failure for EIS region models and reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EisRegionMapError {
    #[error("EIS region handle must be nonzero")]
    ZeroRegionHandle,
    #[error("EIS region {0:?} has an empty width or height")]
    EmptyAdvertisedRegion(EisRegionHandle),
    #[error("EIS region {0:?} has an explicitly empty mapping id")]
    EmptyMappingId(EisRegionHandle),
    #[error("EIS region handle {0:?} is duplicated")]
    DuplicateRegionHandle(EisRegionHandle),
    #[error("Arcen region {0:?} is not aligned to whole EIS logical pixels")]
    NonIntegralLogicalRegion(RegionId),
    #[error("EIS region coordinate conversion overflowed")]
    CoordinateOverflow,
    #[error("no EIS region matches Arcen region {0:?}")]
    MissingRegion(RegionId),
    #[error("more than one EIS region matches Arcen region {0:?}")]
    AmbiguousRegion(RegionId),
    #[error("mapping id matched Arcen region {0:?}, but its EIS geometry did not")]
    MappingIdGeometryMismatch(RegionId),
    #[error("unknown Arcen region {0:?}")]
    UnknownRegion(RegionId),
    #[error("unknown EIS region handle {0:?}")]
    UnknownHandle(EisRegionHandle),
    #[error("input point is outside the selected EIS region")]
    PointOutsideRegion,
    #[error("input point contains NaN or infinity")]
    NonFinitePoint,
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{
        LogicalRect, LogicalSize, OutputTransform, PhysicalSize, RegionContractError,
        RegionDescriptor, RegionGeneration,
    };

    fn descriptor(
        id: u32,
        output: &str,
        x: i64,
        width: u64,
        scale: u32,
        primary: bool,
    ) -> RegionDescriptor {
        RegionDescriptor::new(
            RegionId::new(id).expect("region id"),
            OutputIdentity::new(output).expect("output identity"),
            LogicalRect::new(
                LogicalPoint::from_pixels(x, 0).expect("origin"),
                LogicalSize::from_pixels(width, 1080).expect("size"),
            )
            .expect("rect"),
            PhysicalSize::new(u32::try_from(width).expect("physical width"), 1080)
                .expect("physical size"),
            Scale120::new(scale).expect("scale"),
            OutputTransform::Normal,
            primary,
        )
    }

    fn region_set() -> RegionSet {
        RegionSet::new(
            RegionGeneration::new(1).expect("generation"),
            vec![
                descriptor(1, "DP-1", -1920, 1920, 120, true),
                descriptor(2, "DP-2", 0, 2560, 180, false),
            ],
        )
        .expect("region set")
    }

    fn eis_region(
        handle: u64,
        mapping_id: Option<&str>,
        x: u32,
        width: u32,
        scale: u32,
    ) -> EisRegion {
        EisRegion::new(
            EisRegionHandle::new(handle).expect("handle"),
            mapping_id.map(str::to_owned),
            x,
            0,
            width,
            1080,
            Scale120::new(scale).expect("scale"),
        )
        .expect("EIS region")
    }

    #[test]
    fn negative_wayland_origins_translate_to_unsigned_eis_layout() {
        let map = EisRegionMap::reconcile(
            &region_set(),
            &[
                eis_region(10, Some("DP-1"), 0, 1920, 120),
                eis_region(20, Some("DP-2"), 1920, 2560, 180),
            ],
        )
        .expect("map");
        assert_eq!(map.bindings().len(), 2);
        assert_eq!(
            map.get(RegionId::new(1).expect("id"))
                .expect("left")
                .eis_region()
                .x(),
            0
        );
        assert_eq!(
            map.get(RegionId::new(2).expect("id"))
                .expect("right")
                .eis_region()
                .x(),
            1920
        );
    }

    #[test]
    fn duplicate_mapping_ids_during_resize_select_exact_current_geometry() {
        let regions = RegionSet::new(
            RegionGeneration::new(1).expect("generation"),
            vec![descriptor(1, "DP-1", 0, 1920, 120, true)],
        )
        .expect("regions");
        let map = EisRegionMap::reconcile(
            &regions,
            &[
                eis_region(1, Some("DP-1"), 0, 1280, 120),
                eis_region(2, Some("DP-1"), 0, 1920, 120),
            ],
        )
        .expect("map");
        assert_eq!(map.bindings()[0].eis_region().handle().get(), 2);
    }

    #[test]
    fn mapping_id_geometry_disagreement_fails_closed() {
        let regions = RegionSet::new(
            RegionGeneration::new(1).expect("generation"),
            vec![descriptor(1, "DP-1", 0, 1920, 120, true)],
        )
        .expect("regions");
        assert_eq!(
            EisRegionMap::reconcile(&regions, &[eis_region(1, Some("DP-1"), 5, 1920, 120)]),
            Err(EisRegionMapError::MappingIdGeometryMismatch(
                RegionId::new(1).expect("id")
            ))
        );
    }

    #[test]
    fn exact_geometry_can_bind_when_mapping_ids_are_unavailable() {
        let map = EisRegionMap::reconcile(
            &region_set(),
            &[
                eis_region(10, None, 0, 1920, 120),
                eis_region(20, None, 1920, 2560, 240),
            ],
        )
        .expect("map");
        assert_eq!(map.bindings()[1].eis_region().handle().get(), 20);
        assert_eq!(map.bindings()[1].presentation_scale().get(), 180);
        assert_eq!(map.bindings()[1].eis_region().physical_scale().get(), 240);
    }

    #[test]
    fn region_local_and_eis_desktop_points_round_trip() {
        let map = EisRegionMap::reconcile(
            &region_set(),
            &[
                eis_region(10, Some("DP-1"), 0, 1920, 120),
                eis_region(20, Some("DP-2"), 1920, 2560, 180),
            ],
        )
        .expect("map");
        let region_id = RegionId::new(2).expect("id");
        let local = LogicalPoint::new(120 * 10 + 30, 120 * 20 + 60);
        let eis = map.region_local_to_eis(region_id, local).expect("to EIS");
        assert!((eis.x - 1930.25).abs() < f64::EPSILON);
        assert!((eis.y - 20.5).abs() < f64::EPSILON);
        assert_eq!(
            map.eis_to_region_local(EisRegionHandle::new(20).expect("handle"), eis)
                .expect("from EIS"),
            local
        );
    }

    #[test]
    fn non_integral_logical_regions_cannot_be_claimed_as_eis_regions() {
        let descriptor = RegionDescriptor::new(
            RegionId::new(1).expect("id"),
            OutputIdentity::new("DP-1").expect("identity"),
            LogicalRect::new(
                LogicalPoint::new(1, 0),
                LogicalSize::new(120, 120).expect("size"),
            )
            .expect("rect"),
            PhysicalSize::new(1, 1).expect("physical"),
            Scale120::new(120).expect("scale"),
            OutputTransform::Normal,
            true,
        );
        let regions = RegionSet::new(
            RegionGeneration::new(1).expect("generation"),
            vec![descriptor],
        )
        .expect("regions");
        assert_eq!(
            EisRegionMap::reconcile(&regions, &[eis_region(1, None, 0, 1, 120)]),
            Err(EisRegionMapError::NonIntegralLogicalRegion(
                RegionId::new(1).expect("id")
            ))
        );
    }

    #[test]
    fn runtime_detection_requires_portal_eis_and_native_adapter() {
        let facts = EisRuntimeFacts {
            output_regions: ProbeState::Available,
            remote_desktop_portal: ProbeState::Available,
            eis_connection: ProbeState::Available,
            absolute_pointer: ProbeState::Available,
            mapping_ids: ProbeState::Available,
        };
        let disabled = evaluate_input_capability(
            InputBuildFacts {
                feature_enabled: false,
                linux_target: true,
                native_adapter: false,
            },
            facts,
        );
        assert_eq!(
            disabled.availability(),
            &Err(EisInputUnavailableReason::FeatureDisabled)
        );
        let model_only = evaluate_input_capability(
            InputBuildFacts {
                feature_enabled: true,
                linux_target: true,
                native_adapter: false,
            },
            facts,
        );
        assert_eq!(
            model_only.availability(),
            &Err(EisInputUnavailableReason::NativeAdapterUnavailable)
        );
    }

    #[test]
    fn current_binary_detection_obeys_compile_time_feature_and_target_gates() {
        let report = detect_input_capability(EisRuntimeFacts {
            output_regions: ProbeState::Available,
            remote_desktop_portal: ProbeState::Available,
            eis_connection: ProbeState::Available,
            absolute_pointer: ProbeState::Available,
            mapping_ids: ProbeState::Available,
        });
        #[cfg(not(feature = "wayland-provider"))]
        assert_eq!(
            report.availability(),
            &Err(EisInputUnavailableReason::FeatureDisabled)
        );
        #[cfg(all(feature = "wayland-provider", not(target_os = "linux")))]
        assert_eq!(
            report.availability(),
            &Err(EisInputUnavailableReason::UnsupportedTarget)
        );
        #[cfg(all(feature = "wayland-provider", target_os = "linux"))]
        assert_eq!(
            report.availability(),
            &Err(EisInputUnavailableReason::NativeAdapterUnavailable)
        );
    }

    #[test]
    fn unknown_portal_state_never_authorizes_input() {
        let report = evaluate_input_capability(
            InputBuildFacts {
                feature_enabled: true,
                linux_target: true,
                native_adapter: true,
            },
            EisRuntimeFacts {
                output_regions: ProbeState::Available,
                remote_desktop_portal: ProbeState::Unknown,
                eis_connection: ProbeState::Available,
                absolute_pointer: ProbeState::Available,
                mapping_ids: ProbeState::Unknown,
            },
        );
        assert_eq!(
            report.availability(),
            &Err(EisInputUnavailableReason::PortalUnknown)
        );
    }

    #[test]
    fn zero_and_empty_eis_values_are_rejected() {
        assert_eq!(
            EisRegionHandle::new(0),
            Err(EisRegionMapError::ZeroRegionHandle)
        );
        let handle = EisRegionHandle::new(1).expect("handle");
        assert_eq!(
            EisRegion::new(
                handle,
                Some(String::new()),
                0,
                0,
                1,
                1,
                Scale120::new(120).expect("scale")
            ),
            Err(EisRegionMapError::EmptyMappingId(handle))
        );
        assert_eq!(
            LogicalSize::new(0, 1),
            Err(RegionContractError::EmptyLogicalSize)
        );
    }
}
