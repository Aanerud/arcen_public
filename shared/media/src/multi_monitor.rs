use std::collections::BTreeSet;
use std::num::{NonZeroU16, NonZeroU64};

use arcen_protocol::messages::{
    ClientDisplayId, ClientDisplayIdError, CursorMode, MonitorQualityIntentMsg,
    MultiMonitorValidationError, RequestedMonitorDescriptorMsg, RequestedMonitorTopologyMsg,
    RotationMsg, SafeAreaPolicyMsg,
};

use crate::video::{AcceleratorClass, ResolvedMediaPlan};
use crate::{MediaContractError, Monitor, MonitorIdentity, MonitorTopology, Rotation};

/// The maximum monitor count for the first approved multi-monitor tranche.
pub const MAX_MULTI_MONITOR_COUNT: usize = 4;

fn validate_monitor_count(count: usize) -> Result<(), MediaContractError> {
    if count == 0 || count > MAX_MULTI_MONITOR_COUNT {
        return Err(MediaContractError::UnsupportedMonitorCount(count));
    }
    Ok(())
}

impl TryFrom<&Monitor> for ClientDisplayId {
    type Error = ClientDisplayIdError;

    fn try_from(monitor: &Monitor) -> Result<Self, Self::Error> {
        Self::try_from(monitor.identity.id.as_str())
    }
}

fn map_client_display_id_error(error: ClientDisplayIdError) -> MediaContractError {
    match error {
        ClientDisplayIdError::Empty => MediaContractError::EmptyMonitorId,
        error => MediaContractError::InvalidClientDisplayId(error),
    }
}

/// Host-assigned nonzero monitor identifier, unique within one committed
/// topology. `0` stays reserved for the legacy single-monitor wire frame id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionMonitorId(NonZeroU16);

impl SessionMonitorId {
    /// Creates a host-assigned session monitor identifier in the range
    /// `1..=65535`.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is zero.
    pub const fn new(value: u16) -> Result<Self, MediaContractError> {
        match NonZeroU16::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(MediaContractError::ZeroSessionMonitorId),
        }
    }

    /// Returns the raw identifier.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl TryFrom<u16> for SessionMonitorId {
    type Error = MediaContractError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SessionMonitorId> for u16 {
    fn from(value: SessionMonitorId) -> Self {
        value.get()
    }
}

/// Nonzero epoch identifying one region's current encoded stream.
///
/// The epoch is advertised with the region roster and fences decoder state
/// across pipeline restarts without changing the legacy video frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MediaStreamEpoch(NonZeroU64);

impl MediaStreamEpoch {
    /// Creates a nonzero stream epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is zero.
    pub const fn new(value: u64) -> Result<Self, MediaContractError> {
        match NonZeroU64::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(MediaContractError::ZeroStreamEpoch),
        }
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Validated per-region encoder bitrate budget, in kilobits per second.
///
/// This is the single host-authoritative bitrate truth for one negotiated
/// monitor. It is required (never optional, never zero) and bounded to
/// [`BitrateBudgetKbps::MIN_KBPS`]`..=`[`BitrateBudgetKbps::MAX_KBPS`].
///
/// # Invariant rationale
///
/// * **Nonzero, at least [`BitrateBudgetKbps::MIN_KBPS`] (100 kbps).** No
///   codec/geometry pair this workspace can negotiate carries a usable desktop
///   stream below 100 kbps, so a smaller value signals a mis-derived plan
///   rather than a deliberately frugal one. The host planning floor
///   ([`BitrateBudgetKbps::NOMINAL_FLOOR_KBPS`], 500 kbps) sits well above the
///   bound, so real plans never approach it.
/// * **At most [`BitrateBudgetKbps::MAX_KBPS`] (500 Mbps).** That is ten times
///   the host planning ceiling ([`BitrateBudgetKbps::NOMINAL_CEILING_KBPS`],
///   50 000 kbps, itself a 4K60 pixel-rate-derived value), leaving room for
///   future higher-fidelity policy without another shared-API change while
///   still rejecting absurd values. [`MAX_MULTI_MONITOR_COUNT`] regions at the
///   cap sum to 2 000 000 kbps, far inside `u32` aggregate accounting.
///
/// # Wire compatibility
///
/// `arcen_protocol`'s `AppliedMonitorMediaPlanMsg::bitrate_kbps` stays a plain
/// `u32` whose wire invariant is unchanged (nonzero). This tighter band is a
/// media-domain invariant applied when a host publishes a plan and when a
/// client reads one back; every value the shared planning policy can emit
/// (`500..=50_000` kbps) is inside it, as are all values already present on
/// the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BitrateBudgetKbps(u32);

impl BitrateBudgetKbps {
    /// Smallest accepted budget, in kbps.
    pub const MIN_KBPS: u32 = 100;
    /// Largest accepted budget, in kbps.
    pub const MAX_KBPS: u32 = 500_000;
    /// Floor applied by [`BitrateBudgetKbps::nominal_for_geometry`].
    pub const NOMINAL_FLOOR_KBPS: u32 = 500;
    /// Ceiling applied by [`BitrateBudgetKbps::nominal_for_geometry`].
    pub const NOMINAL_CEILING_KBPS: u32 = 50_000;

    /// Pixel-rate divisor behind [`BitrateBudgetKbps::nominal_for_geometry`]:
    /// ~0.05 bits per pixel per frame of typical desktop content, converted to
    /// kbps (`pixel_rate * 0.05 / 1000 == pixel_rate / 20_000`).
    const NOMINAL_PIXEL_RATE_DIVISOR: u64 = 20_000;

    /// Creates a validated budget.
    ///
    /// # Errors
    ///
    /// Returns [`MediaContractError::InvalidBitrateKbps`] for zero and
    /// [`MediaContractError::BitrateBudgetOutOfRange`] for any other value
    /// outside `MIN_KBPS..=MAX_KBPS`.
    pub const fn new(kbps: u32) -> Result<Self, MediaContractError> {
        if kbps == 0 {
            return Err(MediaContractError::InvalidBitrateKbps);
        }
        if kbps < Self::MIN_KBPS || kbps > Self::MAX_KBPS {
            return Err(MediaContractError::BitrateBudgetOutOfRange(kbps));
        }
        Ok(Self(kbps))
    }

    /// Returns the budget in kbps, exactly as published on the applied wire
    /// media plan.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The one nominal per-region bitrate policy calculation in this
    /// workspace.
    ///
    /// Hosts have no live per-region adaptive-bitrate feedback loop yet;
    /// runtime encoder bitrate stays `QoS`/`quality_settings` driven. A single
    /// fixed constant would be wildly wrong across, say, a 720p and a 4K head
    /// applied in the same session, so this derives a conservative initial
    /// budget from pixel rate and clamps it to
    /// `NOMINAL_FLOOR_KBPS..=NOMINAL_CEILING_KBPS`. That is a documented
    /// planning heuristic, not a solved adaptive-bitrate model — but it is the
    /// only place the heuristic exists.
    ///
    /// The clamp band is a strict subset of `MIN_KBPS..=MAX_KBPS`, so this is
    /// total: it never fails and never panics, including for zero geometry.
    #[must_use]
    pub fn nominal_for_geometry(width: u32, height: u32, fps: u32) -> Self {
        let pixel_rate = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(u64::from(fps.max(1)));
        let kbps = u32::try_from(pixel_rate / Self::NOMINAL_PIXEL_RATE_DIVISOR)
            .unwrap_or(Self::NOMINAL_CEILING_KBPS);
        Self(kbps.clamp(Self::NOMINAL_FLOOR_KBPS, Self::NOMINAL_CEILING_KBPS))
    }
}

impl TryFrom<u32> for BitrateBudgetKbps {
    type Error = MediaContractError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<BitrateBudgetKbps> for u32 {
    fn from(value: BitrateBudgetKbps) -> Self {
        value.get()
    }
}

/// Host-authoritative codec/backend contract for one negotiated monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionMediaPlan {
    pub session_monitor_id: SessionMonitorId,
    pub stream_epoch: MediaStreamEpoch,
    pub backend: crate::video::EncoderBackend,
    pub video: crate::VideoConfiguration,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Host-authoritative bitrate budget for this region, published verbatim
    /// as the applied wire media plan's `bitrate_kbps`.
    pub bitrate_budget: BitrateBudgetKbps,
}

impl RegionMediaPlan {
    /// Creates one validated region media plan.
    ///
    /// # Errors
    ///
    /// Returns an error for zero dimensions or frame rate.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        session_monitor_id: SessionMonitorId,
        stream_epoch: MediaStreamEpoch,
        backend: crate::video::EncoderBackend,
        video: crate::VideoConfiguration,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_budget: BitrateBudgetKbps,
    ) -> Result<Self, MediaContractError> {
        if width == 0 || height == 0 {
            return Err(MediaContractError::InvalidMediaDimensions);
        }
        if fps == 0 {
            return Err(MediaContractError::InvalidMediaFps);
        }
        Ok(Self {
            session_monitor_id,
            stream_epoch,
            backend,
            video,
            width,
            height,
            fps,
            bitrate_budget,
        })
    }

    /// Returns this region's budget in the exact units and value the applied
    /// wire media plan's `bitrate_kbps` field carries.
    #[must_use]
    pub const fn applied_bitrate_kbps(self) -> u32 {
        self.bitrate_budget.get()
    }
}

/// Bounded negotiated media roster with exactly one plan per monitor id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionMediaRoster {
    plans: Vec<RegionMediaPlan>,
}

impl RegionMediaRoster {
    /// Creates a validated 1..=4 entry media roster.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid roster size or duplicate monitor id.
    pub fn new(plans: Vec<RegionMediaPlan>) -> Result<Self, MediaContractError> {
        validate_monitor_count(plans.len())?;
        let mut ids = BTreeSet::new();
        for plan in &plans {
            if !ids.insert(plan.session_monitor_id.get()) {
                return Err(MediaContractError::DuplicateSessionMonitorId(
                    plan.session_monitor_id.get(),
                ));
            }
        }
        Ok(Self { plans })
    }

    /// Returns plans in negotiated roster order.
    #[must_use]
    pub fn plans(&self) -> &[RegionMediaPlan] {
        &self.plans
    }

    /// Looks up one monitor's plan.
    #[must_use]
    pub fn plan(&self, monitor_id: SessionMonitorId) -> Option<RegionMediaPlan> {
        self.plans
            .iter()
            .copied()
            .find(|plan| plan.session_monitor_id == monitor_id)
    }
}

/// Monotonic generation for one committed host topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TopologyGeneration(u64);

impl TopologyGeneration {
    /// The generation every session's first committed topology carries.
    ///
    /// A session's first admitted topology is always generation 1, so the
    /// value is named once here rather than reconstructed through a
    /// panicking `TopologyGeneration::new(1).expect(..)` at every admission
    /// site.
    pub const FIRST: Self = Self(1);

    /// Creates a nonzero topology generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the generation is zero.
    pub const fn new(value: u64) -> Result<Self, MediaContractError> {
        if value == 0 {
            return Err(MediaContractError::ZeroTopologyGeneration);
        }
        Ok(Self(value))
    }

    /// Returns the raw generation value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for TopologyGeneration {
    type Error = MediaContractError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

fn validate_layout_end(origin: i32, extent: u32) -> Result<(), MediaContractError> {
    let end = i64::from(origin)
        .checked_add(i64::from(extent))
        .ok_or(MediaContractError::CoordinateOverflow)?;
    if end > i64::from(i32::MAX) + 1 {
        return Err(MediaContractError::CoordinateOverflow);
    }
    Ok(())
}

fn checked_i64_to_i32(value: i64) -> Result<i32, MediaContractError> {
    i32::try_from(value).map_err(|_| MediaContractError::CoordinateOverflow)
}

const fn desktop_footprint_px(width_px: u32, height_px: u32, rotation: Rotation) -> (u32, u32) {
    match rotation {
        Rotation::Degrees0 | Rotation::Degrees180 => (width_px, height_px),
        Rotation::Degrees90 | Rotation::Degrees270 => (height_px, width_px),
    }
}

/// One signed rectangle inside a caller-defined desktop coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl LayoutRect {
    /// Creates a checked signed layout rectangle.
    ///
    /// # Errors
    ///
    /// Returns an error when the rectangle has zero size or its checked bounds
    /// overflow the signed desktop domain.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, MediaContractError> {
        if width == 0 || height == 0 {
            return Err(MediaContractError::InvalidLayoutDimensions);
        }
        validate_layout_end(x, width)?;
        validate_layout_end(y, height)?;
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    #[must_use]
    pub fn right_exclusive(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    #[must_use]
    pub fn bottom_exclusive(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    /// Returns this rectangle translated by `translation`.
    ///
    /// # Errors
    ///
    /// Returns an error when the translated coordinates overflow `i32`.
    pub fn translated(self, translation: LayoutTranslation) -> Result<Self, MediaContractError> {
        let x = i64::from(self.x)
            .checked_add(translation.dx)
            .ok_or(MediaContractError::CoordinateOverflow)?;
        let y = i64::from(self.y)
            .checked_add(translation.dy)
            .ok_or(MediaContractError::CoordinateOverflow)?;
        Self::new(
            checked_i64_to_i32(x)?,
            checked_i64_to_i32(y)?,
            self.width,
            self.height,
        )
    }
}

/// Checked translation applied to signed monitor rectangles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutTranslation {
    pub dx: i64,
    pub dy: i64,
}

impl LayoutTranslation {
    /// Creates a layout translation.
    #[must_use]
    pub const fn new(dx: i64, dy: i64) -> Self {
        Self { dx, dy }
    }
}

/// Bounding desktop rectangle covering every rectangle in one coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl LayoutBounds {
    /// Computes checked bounds from signed monitor rectangles.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty input or when the aggregate bounds
    /// overflow the bounding raster domain.
    pub fn from_rects(rectangles: &[LayoutRect]) -> Result<Self, MediaContractError> {
        let Some(first) = rectangles.first().copied() else {
            return Err(MediaContractError::EmptyTopology);
        };
        let mut min_x = i64::from(first.x);
        let mut min_y = i64::from(first.y);
        let mut max_right = first.right_exclusive();
        let mut max_bottom = first.bottom_exclusive();
        for rectangle in &rectangles[1..] {
            min_x = min_x.min(i64::from(rectangle.x));
            min_y = min_y.min(i64::from(rectangle.y));
            max_right = max_right.max(rectangle.right_exclusive());
            max_bottom = max_bottom.max(rectangle.bottom_exclusive());
        }
        let width = u32::try_from(
            max_right
                .checked_sub(min_x)
                .ok_or(MediaContractError::CoordinateOverflow)?,
        )
        .map_err(|_| MediaContractError::CoordinateOverflow)?;
        let height = u32::try_from(
            max_bottom
                .checked_sub(min_y)
                .ok_or(MediaContractError::CoordinateOverflow)?,
        )
        .map_err(|_| MediaContractError::CoordinateOverflow)?;
        Ok(Self {
            x: checked_i64_to_i32(min_x)?,
            y: checked_i64_to_i32(min_y)?,
            width,
            height,
        })
    }

    /// Translation that moves these bounds to a non-negative origin while
    /// preserving every relative offset.
    #[must_use]
    pub fn translation_to_origin(self) -> LayoutTranslation {
        LayoutTranslation {
            dx: if self.x < 0 { -i64::from(self.x) } else { 0 },
            dy: if self.y < 0 { -i64::from(self.y) } else { 0 },
        }
    }

    /// Returns these bounds translated by `translation`.
    ///
    /// # Errors
    ///
    /// Returns an error when the translated coordinates overflow `i32`.
    pub fn translated(self, translation: LayoutTranslation) -> Result<Self, MediaContractError> {
        let x = i64::from(self.x)
            .checked_add(translation.dx)
            .ok_or(MediaContractError::CoordinateOverflow)?;
        let y = i64::from(self.y)
            .checked_add(translation.dy)
            .ok_or(MediaContractError::CoordinateOverflow)?;
        let x = checked_i64_to_i32(x)?;
        let y = checked_i64_to_i32(y)?;
        validate_layout_end(x, self.width)?;
        validate_layout_end(y, self.height)?;
        Ok(Self {
            x,
            y,
            width: self.width,
            height: self.height,
        })
    }
}

impl Monitor {
    /// Returns this monitor's validated client display identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the monitor identity is empty or cannot satisfy
    /// the bounded opaque client-display-id contract.
    pub fn client_display_id(&self) -> Result<ClientDisplayId, MediaContractError> {
        ClientDisplayId::try_from(self).map_err(map_client_display_id_error)
    }
}

impl From<RotationMsg> for Rotation {
    fn from(rotation: RotationMsg) -> Self {
        match rotation {
            RotationMsg::Degrees0 => Self::Degrees0,
            RotationMsg::Degrees90 => Self::Degrees90,
            RotationMsg::Degrees180 => Self::Degrees180,
            RotationMsg::Degrees270 => Self::Degrees270,
        }
    }
}

impl From<Rotation> for RotationMsg {
    fn from(rotation: Rotation) -> Self {
        match rotation {
            Rotation::Degrees0 => Self::Degrees0,
            Rotation::Degrees90 => Self::Degrees90,
            Rotation::Degrees180 => Self::Degrees180,
            Rotation::Degrees270 => Self::Degrees270,
        }
    }
}

/// One requested client monitor with explicit logical arrangement extents.
///
/// `monitor.x/y` stay in logical desktop coordinates. `logical_width` and
/// `logical_height` therefore describe the requested arrangement size in that
/// same logical space, while `monitor.width_px/height_px` remain the requested
/// physical/stream extent.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestedMonitor {
    pub monitor: Monitor,
    pub logical_width: u32,
    pub logical_height: u32,
}

impl RequestedMonitor {
    /// Creates one requested monitor description.
    ///
    /// # Errors
    ///
    /// Returns an error when the client display identifier is invalid, or the
    /// logical arrangement rectangle is zero-sized or overflows.
    pub fn new(
        monitor: Monitor,
        logical_width: u32,
        logical_height: u32,
    ) -> Result<Self, MediaContractError> {
        let requested = Self {
            monitor,
            logical_width,
            logical_height,
        };
        let _ = requested.client_display_id()?;
        let _ = requested.logical_arrangement_rect()?;
        Ok(requested)
    }

    /// Returns the underlying endpoint monitor facts.
    #[must_use]
    pub fn monitor(&self) -> &Monitor {
        &self.monitor
    }

    /// Returns the requested client display identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the client display identifier is invalid.
    pub fn client_display_id(&self) -> Result<ClientDisplayId, MediaContractError> {
        self.monitor.client_display_id()
    }

    /// Returns this monitor's requested logical arrangement rectangle.
    ///
    /// # Errors
    ///
    /// Returns an error when the logical rectangle is zero-sized or overflows
    /// the signed desktop domain.
    pub fn logical_arrangement_rect(&self) -> Result<LayoutRect, MediaContractError> {
        LayoutRect::new(
            self.monitor.x,
            self.monitor.y,
            self.logical_width,
            self.logical_height,
        )
    }

    /// Returns a copy translated in logical desktop coordinates.
    ///
    /// # Errors
    ///
    /// Returns an error when the translated coordinates overflow `i32`.
    pub fn translated(&self, translation: LayoutTranslation) -> Result<Self, MediaContractError> {
        let rectangle = self.logical_arrangement_rect()?.translated(translation)?;
        let mut translated = self.clone();
        translated.monitor.x = rectangle.x;
        translated.monitor.y = rectangle.y;
        Ok(translated)
    }

    /// Converts this validated domain monitor into its requested wire
    /// descriptor.
    ///
    /// The safe-area and quality fields are negotiation policy supplied by the
    /// caller; all monitor geometry and identity fields come from this domain
    /// value.
    ///
    /// # Errors
    ///
    /// Returns a wire validation error when the monitor identity cannot be
    /// represented as a valid client display identifier.
    pub fn to_wire_descriptor(
        &self,
        safe_area_policy: SafeAreaPolicyMsg,
        quality_intent: MonitorQualityIntentMsg,
    ) -> Result<RequestedMonitorDescriptorMsg, MultiMonitorValidationError> {
        let monitor = self.monitor();
        let client_display_id = monitor.client_display_id().map_err(|_| {
            MultiMonitorValidationError::DuplicateClientDisplayId(monitor.identity.id.clone())
        })?;
        Ok(RequestedMonitorDescriptorMsg {
            client_display_id,
            client_monitor_id: monitor.identity.id.parse().unwrap_or(0),
            x: monitor.x,
            y: monitor.y,
            width_px: monitor.width_px,
            height_px: monitor.height_px,
            logical_width: self.logical_width,
            logical_height: self.logical_height,
            scale: monitor.scale,
            refresh_hz: monitor.refresh_hz,
            rotation: monitor.rotation.into(),
            is_primary: monitor.primary,
            name: monitor.identity.name.clone(),
            width_mm: monitor.width_mm,
            height_mm: monitor.height_mm,
            vendor: monitor.identity.vendor,
            model: monitor.identity.model,
            serial: monitor.identity.serial,
            edid: String::new(),
            safe_area_policy,
            quality_intent,
        })
    }
}

fn validate_requested_monitor_roster(
    monitors: &[RequestedMonitor],
) -> Result<usize, MediaContractError> {
    validate_monitor_count(monitors.len())?;
    let mut roster = Vec::with_capacity(monitors.len());
    for monitor in monitors {
        let _ = monitor.client_display_id()?;
        let _ = monitor.logical_arrangement_rect()?;
        roster.push(monitor.monitor.clone());
    }
    let _ = MonitorTopology::new(roster)?;
    monitors
        .iter()
        .position(|monitor| monitor.monitor.primary)
        .ok_or(MediaContractError::PrimaryMonitorCount(0))
}

/// Validated 1..=4 requested client-display topology.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestedMonitorTopology {
    monitors: Vec<RequestedMonitor>,
    primary_index: usize,
}

impl RequestedMonitorTopology {
    /// Validates a first-tranche requested topology.
    ///
    /// # Errors
    ///
    /// Returns an error when the layout is empty, exceeds four monitors, lacks
    /// exactly one primary, duplicates a client display id, or has invalid
    /// requested logical geometry.
    pub fn new(monitors: Vec<RequestedMonitor>) -> Result<Self, MediaContractError> {
        let primary_index = validate_requested_monitor_roster(&monitors)?;
        Ok(Self {
            monitors,
            primary_index,
        })
    }

    /// Returns the requested monitors in client-defined layout order.
    #[must_use]
    pub fn monitors(&self) -> &[RequestedMonitor] {
        &self.monitors
    }

    /// Returns the requested primary monitor.
    #[must_use]
    pub fn primary(&self) -> &RequestedMonitor {
        &self.monitors[self.primary_index]
    }

    /// Returns checked bounds covering every requested monitor in logical
    /// desktop coordinates.
    ///
    /// # Errors
    ///
    /// Returns an error when a checked bounding calculation overflows.
    pub fn logical_bounds(&self) -> Result<LayoutBounds, MediaContractError> {
        let rectangles = self
            .monitors()
            .iter()
            .map(RequestedMonitor::logical_arrangement_rect)
            .collect::<Result<Vec<_>, _>>()?;
        LayoutBounds::from_rects(&rectangles)
    }

    /// Returns a translated copy of the requested logical topology.
    ///
    /// # Errors
    ///
    /// Returns an error when the translated coordinates overflow `i32`.
    pub fn translated(&self, translation: LayoutTranslation) -> Result<Self, MediaContractError> {
        let monitors = self
            .monitors()
            .iter()
            .map(|monitor| monitor.translated(translation))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(monitors)
    }

    /// Returns a copy translated so the requested logical bounds start at a
    /// non-negative origin.
    ///
    /// # Errors
    ///
    /// Returns an error when the translation overflows.
    pub fn translated_to_origin(&self) -> Result<Self, MediaContractError> {
        self.translated(self.logical_bounds()?.translation_to_origin())
    }

    /// Converts this validated domain topology into a requested wire
    /// topology using one shared safe-area and quality policy for every
    /// monitor.
    ///
    /// # Errors
    ///
    /// Returns a wire validation error when any monitor cannot be represented
    /// as a valid requested descriptor or when the resulting roster violates
    /// the wire topology invariants.
    pub fn to_wire_topology(
        &self,
        safe_area_policy: SafeAreaPolicyMsg,
        quality_intent: MonitorQualityIntentMsg,
    ) -> Result<RequestedMonitorTopologyMsg, MultiMonitorValidationError> {
        let monitors = self
            .monitors()
            .iter()
            .map(|monitor| monitor.to_wire_descriptor(safe_area_policy, quality_intent))
            .collect::<Result<Vec<_>, _>>()?;
        RequestedMonitorTopologyMsg::new(monitors)
    }
}

impl TryFrom<&RequestedMonitorDescriptorMsg> for RequestedMonitor {
    type Error = MediaContractError;

    fn try_from(descriptor: &RequestedMonitorDescriptorMsg) -> Result<Self, Self::Error> {
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: descriptor.client_display_id.as_str().to_owned(),
                name: descriptor.name.clone(),
                vendor: descriptor.vendor,
                model: descriptor.model,
                serial: descriptor.serial,
            },
            x: descriptor.x,
            y: descriptor.y,
            width_px: descriptor.width_px,
            height_px: descriptor.height_px,
            scale: descriptor.scale,
            refresh_hz: descriptor.refresh_hz,
            rotation: descriptor.rotation.into(),
            primary: descriptor.is_primary,
            width_mm: descriptor.width_mm,
            height_mm: descriptor.height_mm,
        };
        Self::new(monitor, descriptor.logical_width, descriptor.logical_height)
    }
}

impl TryFrom<&RequestedMonitorTopologyMsg> for RequestedMonitorTopology {
    type Error = MediaContractError;

    fn try_from(topology: &RequestedMonitorTopologyMsg) -> Result<Self, Self::Error> {
        topology
            .monitors()
            .iter()
            .map(RequestedMonitor::try_from)
            .collect::<Result<Vec<_>, _>>()
            .and_then(Self::new)
    }
}

/// One applied host monitor with explicit host-pixel desktop placement.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedMonitor {
    pub session_monitor_id: SessionMonitorId,
    pub requested_monitor: RequestedMonitor,
    pub desktop_x_px: i32,
    pub desktop_y_px: i32,
}

impl AppliedMonitor {
    /// Creates one applied monitor mapping.
    ///
    /// # Errors
    ///
    /// Returns an error when the client display identifier is invalid, the
    /// requested logical arrangement rectangle is invalid, or the applied host
    /// pixel rectangle overflows.
    pub fn new(
        session_monitor_id: SessionMonitorId,
        requested_monitor: RequestedMonitor,
        desktop_left_px: i32,
        desktop_top_px: i32,
    ) -> Result<Self, MediaContractError> {
        let applied = Self {
            session_monitor_id,
            requested_monitor,
            desktop_x_px: desktop_left_px,
            desktop_y_px: desktop_top_px,
        };
        let _ = applied.client_display_id()?;
        let _ = applied.requested_monitor.logical_arrangement_rect()?;
        let _ = applied.desktop_rect_px()?;
        Ok(applied)
    }

    /// Returns the requested monitor facts that this host rectangle applies.
    #[must_use]
    pub fn requested_monitor(&self) -> &RequestedMonitor {
        &self.requested_monitor
    }

    /// Returns the underlying monitor stream facts.
    #[must_use]
    pub fn monitor(&self) -> &Monitor {
        self.requested_monitor.monitor()
    }

    /// Returns the mapped client display identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the client display identifier is invalid.
    pub fn client_display_id(&self) -> Result<ClientDisplayId, MediaContractError> {
        self.requested_monitor.client_display_id()
    }

    /// Returns this monitor's applied host desktop rectangle in pixels.
    ///
    /// `Monitor::width_px/height_px` stay the native stream/mode dimensions.
    /// This rectangle instead uses the rotation-aware on-desktop footprint, so
    /// 90/270-degree monitors swap width/height while keeping the same origin.
    ///
    /// # Errors
    ///
    /// Returns an error when the host pixel rectangle is zero-sized or
    /// overflows the signed desktop domain.
    pub fn desktop_rect_px(&self) -> Result<LayoutRect, MediaContractError> {
        let (width_px, height_px) = desktop_footprint_px(
            self.monitor().width_px,
            self.monitor().height_px,
            self.monitor().rotation,
        );
        LayoutRect::new(self.desktop_x_px, self.desktop_y_px, width_px, height_px)
    }
}

/// Validated applied host topology with host-assigned session monitor ids.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedMonitorTopology {
    generation: TopologyGeneration,
    monitors: Vec<AppliedMonitor>,
}

impl AppliedMonitorTopology {
    /// Validates an applied 1..=4 monitor topology.
    ///
    /// # Errors
    ///
    /// Returns an error when the layout exceeds the first-tranche monitor
    /// bound, duplicates a session monitor id, or fails the underlying
    /// requested-roster or applied-pixel invariants.
    pub fn new(
        generation: TopologyGeneration,
        monitors: Vec<AppliedMonitor>,
    ) -> Result<Self, MediaContractError> {
        validate_monitor_count(monitors.len())?;
        let mut session_monitor_ids = BTreeSet::new();
        let mut requested_monitors = Vec::with_capacity(monitors.len());
        for applied in &monitors {
            if !session_monitor_ids.insert(applied.session_monitor_id.get()) {
                return Err(MediaContractError::DuplicateSessionMonitorId(
                    applied.session_monitor_id.get(),
                ));
            }
            let _ = applied.client_display_id()?;
            let _ = applied.desktop_rect_px()?;
            requested_monitors.push(applied.requested_monitor.clone());
        }
        let _ = validate_requested_monitor_roster(&requested_monitors)?;
        Ok(Self {
            generation,
            monitors,
        })
    }

    /// Returns the committed topology generation.
    #[must_use]
    pub const fn generation(&self) -> TopologyGeneration {
        self.generation
    }

    /// Returns the applied monitors in host-defined order.
    #[must_use]
    pub fn monitors(&self) -> &[AppliedMonitor] {
        &self.monitors
    }

    /// Returns the applied primary monitor.
    #[must_use]
    pub fn primary(&self) -> &AppliedMonitor {
        if let Some(primary) = self
            .monitors
            .iter()
            .find(|monitor| monitor.monitor().primary)
        {
            return primary;
        }
        &self.monitors[0]
    }

    /// Returns checked bounds covering every applied monitor in host pixel
    /// desktop coordinates, using each monitor's rotation-aware desktop
    /// footprint.
    ///
    /// # Errors
    ///
    /// Returns an error when a checked bounding calculation overflows.
    pub fn desktop_bounds_px(&self) -> Result<LayoutBounds, MediaContractError> {
        let rectangles = self
            .monitors
            .iter()
            .map(AppliedMonitor::desktop_rect_px)
            .collect::<Result<Vec<_>, _>>()?;
        LayoutBounds::from_rects(&rectangles)
    }
}

/// One concrete applied per-monitor media plan.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerMonitorMediaPlan {
    pub session_monitor_id: SessionMonitorId,
    pub backend: crate::video::EncoderBackend,
    pub accelerator_class: AcceleratorClass,
    pub video: crate::VideoConfiguration,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub cursor_mode: CursorMode,
    pub bitrate_kbps: u32,
    pub degraded: bool,
}

impl PerMonitorMediaPlan {
    /// Creates one per-monitor plan from the already resolved single-monitor
    /// plan truth.
    ///
    /// # Errors
    ///
    /// Returns an error when the bitrate budget is zero.
    pub fn from_resolved(
        session_monitor_id: SessionMonitorId,
        plan: ResolvedMediaPlan,
        bitrate_kbps: u32,
        degraded: bool,
    ) -> Result<Self, MediaContractError> {
        if bitrate_kbps == 0 {
            return Err(MediaContractError::InvalidBitrateKbps);
        }
        Ok(Self {
            session_monitor_id,
            backend: plan.backend,
            accelerator_class: plan.backend.accelerator_class(),
            video: plan.video,
            width: plan.width,
            height: plan.height,
            fps: plan.fps,
            cursor_mode: plan.cursor_mode,
            bitrate_kbps,
            degraded,
        })
    }

    fn pixel_rate(self) -> Result<u64, MediaContractError> {
        let pixels_per_frame = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .ok_or(MediaContractError::BudgetOverflow("pixel_rate"))?;
        pixels_per_frame
            .checked_mul(u64::from(self.fps))
            .ok_or(MediaContractError::BudgetOverflow("pixel_rate"))
    }
}

/// Aggregate budgets for one committed multi-monitor media plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateMediaBudget {
    pub hardware_sessions: u8,
    pub software_sessions: u8,
    pub encoder_contexts: u8,
    pub cpu_millis_per_second: u32,
    pub vram_bytes: u64,
    pub pixel_rate: u64,
    pub connection_bitrate_kbps: u32,
}

impl AggregateMediaBudget {
    /// Computes checked aggregate budgets from concrete per-monitor plans.
    ///
    /// # Errors
    ///
    /// Returns an error when the plan is empty, exceeds four monitors, or a
    /// checked budget accumulation overflows.
    pub fn from_monitor_plans(
        plans: &[PerMonitorMediaPlan],
        cpu_millis_per_second: u32,
        vram_bytes: u64,
    ) -> Result<Self, MediaContractError> {
        validate_monitor_count(plans.len())?;
        let mut hardware_sessions = 0u8;
        let mut software_sessions = 0u8;
        let mut encoder_contexts = 0u8;
        let mut pixel_rate = 0u64;
        let mut connection_bitrate_kbps = 0u32;
        for plan in plans {
            encoder_contexts = encoder_contexts
                .checked_add(1)
                .ok_or(MediaContractError::BudgetOverflow("encoder_contexts"))?;
            connection_bitrate_kbps = connection_bitrate_kbps
                .checked_add(plan.bitrate_kbps)
                .ok_or(MediaContractError::BudgetOverflow(
                    "connection_bitrate_kbps",
                ))?;
            pixel_rate = pixel_rate
                .checked_add(plan.pixel_rate()?)
                .ok_or(MediaContractError::BudgetOverflow("pixel_rate"))?;
            match plan.accelerator_class {
                AcceleratorClass::Hardware => {
                    hardware_sessions = hardware_sessions
                        .checked_add(1)
                        .ok_or(MediaContractError::BudgetOverflow("hardware_sessions"))?;
                }
                AcceleratorClass::Software => {
                    software_sessions = software_sessions
                        .checked_add(1)
                        .ok_or(MediaContractError::BudgetOverflow("software_sessions"))?;
                }
            }
        }
        Ok(Self {
            hardware_sessions,
            software_sessions,
            encoder_contexts,
            cpu_millis_per_second,
            vram_bytes,
            pixel_rate,
            connection_bitrate_kbps,
        })
    }
}

/// Complete aggregate media plan for one committed topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateMediaPlan {
    monitors: Vec<PerMonitorMediaPlan>,
    budget: AggregateMediaBudget,
}

impl AggregateMediaPlan {
    /// Creates an aggregate plan from validated per-monitor plans.
    ///
    /// # Errors
    ///
    /// Returns an error when the plan is empty, exceeds four monitors,
    /// duplicates a session monitor id, or a checked aggregate budget
    /// accumulation overflows.
    pub fn new(
        monitors: Vec<PerMonitorMediaPlan>,
        cpu_millis_per_second: u32,
        vram_bytes: u64,
    ) -> Result<Self, MediaContractError> {
        validate_monitor_count(monitors.len())?;
        let mut session_monitor_ids = BTreeSet::new();
        for monitor in &monitors {
            if !session_monitor_ids.insert(monitor.session_monitor_id.get()) {
                return Err(MediaContractError::DuplicateSessionMonitorId(
                    monitor.session_monitor_id.get(),
                ));
            }
        }
        let budget =
            AggregateMediaBudget::from_monitor_plans(&monitors, cpu_millis_per_second, vram_bytes)?;
        Ok(Self { monitors, budget })
    }

    /// Returns the concrete per-monitor plans in roster order.
    #[must_use]
    pub fn monitors(&self) -> &[PerMonitorMediaPlan] {
        &self.monitors
    }

    /// Returns the aggregate media budget.
    #[must_use]
    pub const fn budget(&self) -> AggregateMediaBudget {
        self.budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::EncoderBackend;
    use crate::{ChromaSet, ChromaSubsampling, CodecSet, MonitorIdentity, Rotation, VideoCodec};

    #[allow(clippy::too_many_arguments)]
    fn monitor_with_scale(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        scale: f32,
        primary: bool,
        rotation: Rotation,
    ) -> Monitor {
        Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                ..MonitorIdentity::default()
            },
            x,
            y,
            width_px,
            height_px,
            scale,
            refresh_hz: 60,
            rotation,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        }
    }

    fn monitor(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        primary: bool,
        rotation: Rotation,
    ) -> Monitor {
        monitor_with_scale(id, x, y, width_px, height_px, 1.0, primary, rotation)
    }

    fn requested_monitor(
        monitor: Monitor,
        logical_width: u32,
        logical_height: u32,
    ) -> RequestedMonitor {
        RequestedMonitor::new(monitor, logical_width, logical_height).expect("requested monitor")
    }

    #[test]
    fn requested_monitor_wire_conversion_round_trips_all_rotations_and_fields() {
        let safe_area_policy = SafeAreaPolicyMsg::FullFrame;
        let quality_intent = MonitorQualityIntentMsg::FullColorRequired;

        for rotation in Rotation::ALL {
            let requested = requested_monitor(
                Monitor {
                    identity: MonitorIdentity {
                        id: "1001".to_owned(),
                        name: "Studio Panel".to_owned(),
                        vendor: 0x1234,
                        model: 0x5678,
                        serial: 0x9abc_def0,
                    },
                    x: -1920,
                    y: 48,
                    width_px: 2560,
                    height_px: 1440,
                    scale: 1.25,
                    refresh_hz: 144,
                    rotation,
                    primary: true,
                    width_mm: 598.0,
                    height_mm: 336.0,
                },
                2048,
                1152,
            );

            let wire = requested
                .to_wire_descriptor(safe_area_policy, quality_intent)
                .expect("wire conversion");
            assert_eq!(wire.client_display_id.as_str(), "1001");
            assert_eq!(wire.client_monitor_id, 1001);
            assert_eq!(wire.x, -1920);
            assert_eq!(wire.y, 48);
            assert_eq!(wire.width_px, 2560);
            assert_eq!(wire.height_px, 1440);
            assert_eq!(wire.logical_width, 2048);
            assert_eq!(wire.logical_height, 1152);
            assert_eq!(wire.scale, 1.25);
            assert_eq!(wire.refresh_hz, 144);
            assert_eq!(wire.rotation, RotationMsg::from(rotation));
            assert!(wire.is_primary);
            assert_eq!(wire.name, "Studio Panel");
            assert_eq!(wire.width_mm, 598.0);
            assert_eq!(wire.height_mm, 336.0);
            assert_eq!(wire.vendor, 0x1234);
            assert_eq!(wire.model, 0x5678);
            assert_eq!(wire.serial, 0x9abc_def0);
            assert_eq!(wire.safe_area_policy, safe_area_policy);
            assert_eq!(wire.quality_intent, quality_intent);

            let restored = RequestedMonitor::try_from(&wire).expect("domain conversion");
            assert_eq!(restored, requested);
            assert_eq!(Rotation::from(wire.rotation), rotation);
        }
    }

    #[test]
    fn requested_topology_wire_conversion_preserves_roster_fields() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor(
                monitor("1001", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                1920,
                1080,
            ),
            requested_monitor(
                monitor("1002", 1920, 0, 1280, 720, false, Rotation::Degrees90),
                1280,
                720,
            ),
        ])
        .expect("valid topology");

        let wire = requested
            .to_wire_topology(
                SafeAreaPolicyMsg::StandardFullscreen,
                MonitorQualityIntentMsg::BandwidthOptimized,
            )
            .expect("wire topology");
        assert_eq!(wire.monitors().len(), 2);
        assert!(wire.primary().is_primary);
        assert_eq!(wire.monitors()[1].rotation, RotationMsg::Degrees90);
        let restored = RequestedMonitorTopology::try_from(&wire).expect("domain topology");
        assert_eq!(restored, requested);
    }

    fn resolved_plan(
        backend: EncoderBackend,
        width: u32,
        height: u32,
        fps: u32,
    ) -> ResolvedMediaPlan {
        ResolvedMediaPlan {
            backend,
            video: crate::VideoConfiguration {
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv420,
                ..crate::VideoConfiguration::legacy_h264()
            },
            width,
            height,
            fps,
            cursor_mode: CursorMode::Local,
            cursor_in_video: false,
            codecs: CodecSet::from_slice(&[VideoCodec::H264]),
            chroma: ChromaSet::from_slice(&[ChromaSubsampling::Yuv420]),
            bit_depths: crate::BitDepthSet::from_slice(&[crate::BitDepth::Eight]),
            ranges: crate::ColorRangeSet::from_slice(&[crate::ColorRange::Limited]),
        }
    }

    #[test]
    fn requested_topology_rejects_invalid_monitor_counts() {
        assert_eq!(
            RequestedMonitorTopology::new(Vec::new()),
            Err(MediaContractError::UnsupportedMonitorCount(0))
        );

        let monitors = (0..5)
            .map(|index| {
                requested_monitor(
                    monitor(
                        &index.to_string(),
                        index * 1920,
                        0,
                        1920,
                        1080,
                        index == 0,
                        Rotation::Degrees0,
                    ),
                    1920,
                    1080,
                )
            })
            .collect();
        assert_eq!(
            RequestedMonitorTopology::new(monitors),
            Err(MediaContractError::UnsupportedMonitorCount(5))
        );
    }

    #[test]
    fn requested_topology_rejects_duplicate_ids_and_primary_mismatch() {
        assert_eq!(
            RequestedMonitorTopology::new(vec![
                requested_monitor(
                    monitor("same", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                    1920,
                    1080,
                ),
                requested_monitor(
                    monitor("same", 1920, 0, 1920, 1080, false, Rotation::Degrees0),
                    1920,
                    1080,
                ),
            ]),
            Err(MediaContractError::DuplicateMonitorId("same".to_owned()))
        );
        assert_eq!(
            RequestedMonitorTopology::new(vec![
                requested_monitor(
                    monitor("one", 0, 0, 1920, 1080, false, Rotation::Degrees0),
                    1920,
                    1080,
                ),
                requested_monitor(
                    monitor("two", 1920, 0, 1920, 1080, false, Rotation::Degrees0),
                    1920,
                    1080,
                ),
            ]),
            Err(MediaContractError::PrimaryMonitorCount(0))
        );
        assert_eq!(
            RequestedMonitorTopology::new(vec![
                requested_monitor(
                    monitor("one", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                    1920,
                    1080,
                ),
                requested_monitor(
                    monitor("two", 1920, 0, 1920, 1080, true, Rotation::Degrees0),
                    1920,
                    1080,
                ),
            ]),
            Err(MediaContractError::PrimaryMonitorCount(2))
        );
    }

    #[test]
    fn requested_monitor_rejects_invalid_client_display_ids() {
        assert_eq!(
            RequestedMonitor::new(
                monitor("bad\nid", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                1920,
                1080,
            ),
            Err(MediaContractError::InvalidClientDisplayId(
                ClientDisplayIdError::ControlCharacter
            ))
        );

        let too_long = "x".repeat(arcen_protocol::messages::MAX_CLIENT_DISPLAY_ID_BYTES + 1);
        assert_eq!(
            RequestedMonitor::new(
                monitor(&too_long, 0, 0, 1920, 1080, true, Rotation::Degrees0),
                1920,
                1080,
            ),
            Err(MediaContractError::InvalidClientDisplayId(
                ClientDisplayIdError::TooLong
            ))
        );
    }

    #[test]
    fn logical_bounds_and_translation_handle_negative_coordinates() {
        let topology = RequestedMonitorTopology::new(vec![
            requested_monitor(
                monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
                1920,
                1080,
            ),
            requested_monitor(
                monitor("left", -1280, 200, 1280, 1024, false, Rotation::Degrees0),
                1280,
                1024,
            ),
        ])
        .expect("valid topology");
        let bounds = topology.logical_bounds().expect("bounds");
        assert_eq!(
            bounds,
            LayoutBounds {
                x: -1280,
                y: 0,
                width: 3200,
                height: 1224,
            }
        );
        let translation = bounds.translation_to_origin();
        assert_eq!(translation, LayoutTranslation::new(1280, 0));
        let translated = topology.translated_to_origin().expect("translated");
        assert_eq!(translated.logical_bounds().expect("translated bounds").x, 0);
        assert_eq!(translated.logical_bounds().expect("translated bounds").y, 0);
        assert_eq!(
            translated.monitors()[0].monitor.x - translated.monitors()[1].monitor.x,
            1280
        );
    }

    #[test]
    fn mixed_scale_requested_bounds_stay_logical_and_applied_bounds_stay_pixel() {
        let left = requested_monitor(
            monitor("left", 0, 0, 1800, 1169, true, Rotation::Degrees0),
            1800,
            1169,
        );
        let retina = requested_monitor(
            monitor_with_scale(
                "retina",
                1800,
                0,
                3600,
                2338,
                2.0,
                false,
                Rotation::Degrees0,
            ),
            1800,
            1169,
        );
        let requested =
            RequestedMonitorTopology::new(vec![left.clone(), retina.clone()]).expect("requested");
        assert_eq!(
            requested.logical_bounds().expect("logical bounds"),
            LayoutBounds {
                x: 0,
                y: 0,
                width: 3600,
                height: 1169,
            }
        );

        let applied = AppliedMonitorTopology::new(
            TopologyGeneration::new(5).expect("generation"),
            vec![
                AppliedMonitor::new(
                    SessionMonitorId::new(1).expect("session monitor id"),
                    left,
                    0,
                    0,
                )
                .expect("left"),
                AppliedMonitor::new(
                    SessionMonitorId::new(2).expect("session monitor id"),
                    retina,
                    1800,
                    0,
                )
                .expect("retina"),
            ],
        )
        .expect("applied");
        assert_eq!(
            applied.desktop_bounds_px().expect("desktop bounds"),
            LayoutBounds {
                x: 0,
                y: 0,
                width: 5400,
                height: 2338,
            }
        );
    }

    #[test]
    fn adr_0009_pixel_bounds_do_not_mix_logical_origins_with_physical_stream_sizes() {
        let primary = requested_monitor(
            monitor_with_scale("primary", 0, 0, 1920, 1080, 2.0, true, Rotation::Degrees0),
            960,
            540,
        );
        let secondary = requested_monitor(
            monitor("secondary", 960, 0, 1280, 720, false, Rotation::Degrees0),
            1280,
            720,
        );
        let requested = RequestedMonitorTopology::new(vec![primary.clone(), secondary.clone()])
            .expect("requested");
        assert_eq!(
            requested.logical_bounds().expect("logical bounds"),
            LayoutBounds {
                x: 0,
                y: 0,
                width: 2240,
                height: 720,
            }
        );

        // ADR 0009 forbids this mixed-unit derivation: these origins are
        // logical points while these extents are physical stream pixels.
        let mixed_unit_bounds = LayoutBounds::from_rects(&[
            LayoutRect::new(
                primary.monitor().x,
                primary.monitor().y,
                primary.monitor().width_px,
                primary.monitor().height_px,
            )
            .expect("mixed primary"),
            LayoutRect::new(
                secondary.monitor().x,
                secondary.monitor().y,
                secondary.monitor().width_px,
                secondary.monitor().height_px,
            )
            .expect("mixed secondary"),
        ])
        .expect("mixed-unit bounds");
        assert_eq!(
            mixed_unit_bounds,
            LayoutBounds {
                x: 0,
                y: 0,
                width: 2240,
                height: 1080,
            }
        );

        let applied = AppliedMonitorTopology::new(
            TopologyGeneration::new(9).expect("generation"),
            vec![
                AppliedMonitor::new(SessionMonitorId::new(1).expect("primary id"), primary, 0, 0)
                    .expect("applied primary"),
                AppliedMonitor::new(
                    SessionMonitorId::new(2).expect("secondary id"),
                    secondary,
                    1920,
                    0,
                )
                .expect("applied secondary"),
            ],
        )
        .expect("applied topology");
        let pixel_bounds = applied.desktop_bounds_px().expect("pixel bounds");
        assert_eq!(
            pixel_bounds,
            LayoutBounds {
                x: 0,
                y: 0,
                width: 3200,
                height: 1080,
            }
        );
        assert_ne!(pixel_bounds, mixed_unit_bounds);
    }

    #[test]
    fn layout_rect_checks_coordinate_overflow() {
        assert_eq!(
            LayoutRect::new(i32::MAX, 0, 2, 1),
            Err(MediaContractError::CoordinateOverflow)
        );
        assert_eq!(
            LayoutRect::new(0, i32::MAX, 1, 2),
            Err(MediaContractError::CoordinateOverflow)
        );
        let rect = LayoutRect::new(i32::MAX, 0, 1, 1).expect("max-edge rectangle");
        assert_eq!(
            rect.translated(LayoutTranslation::new(1, 0)),
            Err(MediaContractError::CoordinateOverflow)
        );
    }

    #[test]
    fn layout_bounds_translation_rechecks_end_domain_edges() {
        let x_overflow = LayoutBounds {
            x: i32::MAX - 1,
            y: 0,
            width: 2,
            height: 1,
        };
        assert_eq!(
            x_overflow.translated(LayoutTranslation::new(1, 0)),
            Err(MediaContractError::CoordinateOverflow)
        );

        let y_overflow = LayoutBounds {
            x: 0,
            y: i32::MAX - 1,
            width: 1,
            height: 2,
        };
        assert_eq!(
            y_overflow.translated(LayoutTranslation::new(0, 1)),
            Err(MediaContractError::CoordinateOverflow)
        );

        let exact_edge = LayoutBounds {
            x: i32::MAX - 1,
            y: i32::MAX - 1,
            width: 1,
            height: 1,
        };
        assert_eq!(
            exact_edge
                .translated(LayoutTranslation::new(1, 1))
                .expect("translation at exact edge"),
            LayoutBounds {
                x: i32::MAX,
                y: i32::MAX,
                width: 1,
                height: 1,
            }
        );
    }

    #[test]
    fn topology_generation_must_be_nonzero() {
        assert_eq!(
            TopologyGeneration::new(0),
            Err(MediaContractError::ZeroTopologyGeneration)
        );
        assert_eq!(TopologyGeneration::new(7).expect("generation").get(), 7);
    }

    #[test]
    fn session_monitor_id_must_be_nonzero() {
        assert_eq!(
            SessionMonitorId::new(0),
            Err(MediaContractError::ZeroSessionMonitorId)
        );
        assert_eq!(
            SessionMonitorId::try_from(0),
            Err(MediaContractError::ZeroSessionMonitorId)
        );
        assert_eq!(
            SessionMonitorId::new(7).expect("session monitor id").get(),
            7
        );
    }

    #[test]
    fn applied_topology_requires_unique_session_ids_and_preserves_rotation() {
        let rotated = requested_monitor(
            monitor("rotated", 0, -900, 1600, 900, true, Rotation::Degrees90),
            1600,
            900,
        );
        let applied = AppliedMonitorTopology::new(
            TopologyGeneration::new(3).expect("generation"),
            vec![
                AppliedMonitor::new(
                    SessionMonitorId::new(10).expect("session monitor id"),
                    rotated.clone(),
                    0,
                    -900,
                )
                .expect("monitor"),
                AppliedMonitor::new(
                    SessionMonitorId::new(11).expect("session monitor id"),
                    requested_monitor(
                        monitor("side", 1600, -900, 1600, 900, false, Rotation::Degrees270),
                        1600,
                        900,
                    ),
                    1600,
                    -900,
                )
                .expect("monitor"),
            ],
        )
        .expect("applied topology");
        assert_eq!(applied.primary().monitor().rotation, Rotation::Degrees90);
        assert_eq!(applied.desktop_bounds_px().expect("bounds").y, -900);

        assert_eq!(
            AppliedMonitorTopology::new(
                TopologyGeneration::new(4).expect("generation"),
                vec![
                    AppliedMonitor::new(
                        SessionMonitorId::new(7).expect("session monitor id"),
                        rotated.clone(),
                        0,
                        -900,
                    )
                    .expect("first"),
                    AppliedMonitor::new(
                        SessionMonitorId::new(7).expect("session monitor id"),
                        rotated,
                        1600,
                        -900,
                    )
                    .expect("second"),
                ],
            ),
            Err(MediaContractError::DuplicateSessionMonitorId(7))
        );
    }

    #[test]
    fn applied_monitor_desktop_bounds_use_rotation_aware_footprints() {
        let landscape = requested_monitor(
            monitor("landscape", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            1920,
            1080,
        );
        let portrait = requested_monitor(
            monitor("portrait", 1920, 0, 1920, 1080, false, Rotation::Degrees90),
            1920,
            1080,
        );
        let landscape_applied = AppliedMonitor::new(
            SessionMonitorId::new(1).expect("session monitor id"),
            landscape,
            0,
            0,
        )
        .expect("landscape");
        let portrait_applied = AppliedMonitor::new(
            SessionMonitorId::new(2).expect("session monitor id"),
            portrait,
            1920,
            0,
        )
        .expect("portrait");

        assert_eq!(
            landscape_applied.desktop_rect_px().expect("landscape rect"),
            LayoutRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        );
        assert_eq!(
            portrait_applied.monitor().width_px,
            1920,
            "native mode width stays unswapped on the monitor"
        );
        assert_eq!(
            portrait_applied.monitor().height_px,
            1080,
            "native mode height stays unswapped on the monitor"
        );
        assert_eq!(
            portrait_applied.desktop_rect_px().expect("portrait rect"),
            LayoutRect {
                x: 1920,
                y: 0,
                width: 1080,
                height: 1920,
            }
        );

        let topology = AppliedMonitorTopology::new(
            TopologyGeneration::new(6).expect("generation"),
            vec![landscape_applied, portrait_applied],
        )
        .expect("applied topology");
        assert_eq!(
            topology.desktop_bounds_px().expect("desktop bounds"),
            LayoutBounds {
                x: 0,
                y: 0,
                width: 3000,
                height: 1920,
            }
        );
    }

    #[test]
    fn every_supported_rotation_keeps_the_same_checked_bounds() {
        for rotation in [
            Rotation::Degrees0,
            Rotation::Degrees90,
            Rotation::Degrees180,
            Rotation::Degrees270,
        ] {
            let topology = RequestedMonitorTopology::new(vec![requested_monitor(
                monitor("rot", -40, 120, 800, 600, true, rotation),
                800,
                600,
            )])
            .expect("single-monitor topology");
            assert_eq!(topology.primary().monitor().rotation, rotation);
            assert_eq!(
                topology.logical_bounds().expect("bounds"),
                LayoutBounds {
                    x: -40,
                    y: 120,
                    width: 800,
                    height: 600,
                }
            );
        }
    }

    #[test]
    fn aggregate_media_plan_counts_backend_classes_and_checked_budgets() {
        let plans = vec![
            PerMonitorMediaPlan::from_resolved(
                SessionMonitorId::new(1).expect("session monitor id"),
                resolved_plan(EncoderBackend::NativeNvenc, 1920, 1080, 60),
                12_000,
                false,
            )
            .expect("nvenc plan"),
            PerMonitorMediaPlan::from_resolved(
                SessionMonitorId::new(2).expect("session monitor id"),
                resolved_plan(EncoderBackend::OpenH264, 1280, 720, 30),
                4_000,
                true,
            )
            .expect("software plan"),
        ];
        let aggregate = AggregateMediaPlan::new(plans, 240, 64 * 1024 * 1024).expect("aggregate");
        assert_eq!(aggregate.budget().hardware_sessions, 1);
        assert_eq!(aggregate.budget().software_sessions, 1);
        assert_eq!(aggregate.budget().encoder_contexts, 2);
        assert_eq!(aggregate.budget().connection_bitrate_kbps, 16_000);
        assert_eq!(
            aggregate.budget().pixel_rate,
            (1920_u64 * 1080 * 60) + (1280_u64 * 720 * 30)
        );
        assert!(aggregate.monitors()[1].degraded);
    }

    #[test]
    fn aggregate_media_plan_checks_budget_overflow_and_duplicate_ids() {
        let overflow = PerMonitorMediaPlan::from_resolved(
            SessionMonitorId::new(1).expect("session monitor id"),
            resolved_plan(EncoderBackend::NativeNvenc, u32::MAX, u32::MAX, u32::MAX),
            1,
            false,
        )
        .expect("overflow plan");
        assert_eq!(
            AggregateMediaPlan::new(vec![overflow], 0, 0),
            Err(MediaContractError::BudgetOverflow("pixel_rate"))
        );

        let plan = PerMonitorMediaPlan::from_resolved(
            SessionMonitorId::new(9).expect("session monitor id"),
            resolved_plan(EncoderBackend::OpenH264, 640, 480, 30),
            2_000,
            false,
        )
        .expect("plan");
        assert_eq!(
            AggregateMediaPlan::new(vec![plan, plan], 0, 0),
            Err(MediaContractError::DuplicateSessionMonitorId(9))
        );
    }

    #[test]
    fn region_media_roster_preserves_mixed_h265_and_h264_plans() {
        let h265 = RegionMediaPlan::new(
            SessionMonitorId::new(1).expect("id"),
            MediaStreamEpoch::new(11).expect("epoch"),
            crate::video::EncoderBackend::NativeNvenc,
            crate::VideoConfiguration {
                codec: crate::VideoCodec::H265,
                chroma: crate::ChromaSubsampling::Yuv420,
                ..crate::VideoConfiguration::legacy_h264()
            },
            3840,
            2160,
            60,
            BitrateBudgetKbps::nominal_for_geometry(3840, 2160, 60),
        )
        .expect("h265 plan");
        let h264 = RegionMediaPlan::new(
            SessionMonitorId::new(2).expect("id"),
            MediaStreamEpoch::new(12).expect("epoch"),
            crate::video::EncoderBackend::OpenH264,
            crate::VideoConfiguration {
                codec: crate::VideoCodec::H264,
                chroma: crate::ChromaSubsampling::Yuv420,
                ..crate::VideoConfiguration::legacy_h264()
            },
            1920,
            1080,
            30,
            BitrateBudgetKbps::nominal_for_geometry(1920, 1080, 30),
        )
        .expect("h264 plan");

        let roster = RegionMediaRoster::new(vec![h265, h264]).expect("mixed roster");
        assert_eq!(roster.plan(h265.session_monitor_id), Some(h265));
        assert_eq!(roster.plan(h264.session_monitor_id), Some(h264));
        assert_eq!(roster.plans()[0].video.codec, crate::VideoCodec::H265);
        assert_eq!(roster.plans()[1].video.codec, crate::VideoCodec::H264);
        assert_eq!(roster.plans()[0].applied_bitrate_kbps(), 24_883);
        assert_eq!(roster.plans()[1].applied_bitrate_kbps(), 3_110);
    }

    #[test]
    fn bitrate_budget_rejects_zero_and_out_of_band_values() {
        assert_eq!(
            BitrateBudgetKbps::new(0),
            Err(MediaContractError::InvalidBitrateKbps)
        );
        assert_eq!(
            BitrateBudgetKbps::new(BitrateBudgetKbps::MIN_KBPS - 1),
            Err(MediaContractError::BitrateBudgetOutOfRange(
                BitrateBudgetKbps::MIN_KBPS - 1
            ))
        );
        assert_eq!(
            BitrateBudgetKbps::new(BitrateBudgetKbps::MAX_KBPS + 1),
            Err(MediaContractError::BitrateBudgetOutOfRange(
                BitrateBudgetKbps::MAX_KBPS + 1
            ))
        );
        assert_eq!(
            BitrateBudgetKbps::new(BitrateBudgetKbps::MIN_KBPS)
                .expect("min is accepted")
                .get(),
            BitrateBudgetKbps::MIN_KBPS
        );
        assert_eq!(
            BitrateBudgetKbps::new(BitrateBudgetKbps::MAX_KBPS)
                .expect("max is accepted")
                .get(),
            BitrateBudgetKbps::MAX_KBPS
        );
    }

    #[test]
    fn nominal_bitrate_policy_scales_with_pixel_rate_and_stays_in_band() {
        let small = BitrateBudgetKbps::nominal_for_geometry(640, 480, 30);
        let large = BitrateBudgetKbps::nominal_for_geometry(3840, 2160, 60);
        assert!(small.get() < large.get());
        assert_eq!(small.get(), BitrateBudgetKbps::NOMINAL_FLOOR_KBPS);
        assert_eq!(large.get(), 24_883);
        assert_eq!(
            BitrateBudgetKbps::nominal_for_geometry(u32::MAX, u32::MAX, u32::MAX).get(),
            BitrateBudgetKbps::NOMINAL_CEILING_KBPS
        );
        // Degenerate and saturating inputs stay total and in band.
        for (width, height, fps) in [
            (0, 0, 0),
            (1, 1, 1),
            (u32::MAX, u32::MAX, u32::MAX),
            (1_920, 1_080, 60),
        ] {
            let budget = BitrateBudgetKbps::nominal_for_geometry(width, height, fps);
            assert!(budget.get() >= BitrateBudgetKbps::NOMINAL_FLOOR_KBPS);
            assert!(budget.get() <= BitrateBudgetKbps::NOMINAL_CEILING_KBPS);
            assert_eq!(
                BitrateBudgetKbps::new(budget.get()),
                Ok(budget),
                "the policy band must stay inside the value object's invariant"
            );
        }
    }

    #[test]
    fn nominal_bitrate_policy_is_deterministic() {
        assert_eq!(
            BitrateBudgetKbps::nominal_for_geometry(1920, 1080, 60),
            BitrateBudgetKbps::nominal_for_geometry(1920, 1080, 60)
        );
        assert_eq!(
            BitrateBudgetKbps::nominal_for_geometry(1920, 1080, 60).get(),
            6_220
        );
    }

    #[test]
    fn region_media_plan_carries_the_budget_to_the_applied_wire_value() {
        let budget = BitrateBudgetKbps::new(12_345).expect("in-band budget");
        let plan = RegionMediaPlan::new(
            SessionMonitorId::new(3).expect("id"),
            MediaStreamEpoch::new(7).expect("epoch"),
            crate::video::EncoderBackend::NativeNvenc,
            crate::VideoConfiguration {
                codec: crate::VideoCodec::H264,
                chroma: crate::ChromaSubsampling::Yuv420,
                ..crate::VideoConfiguration::legacy_h264()
            },
            2560,
            1440,
            60,
            budget,
        )
        .expect("plan");
        assert_eq!(plan.bitrate_budget, budget);
        assert_eq!(plan.applied_bitrate_kbps(), 12_345);
    }
}
