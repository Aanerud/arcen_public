//! Pure Linux multi-monitor topology planner.
//!
//! Maps a validated [`RequestedMonitorTopology`] onto one dedicated Xorg
//! screen: exactly one applied RandR output rectangle per requested monitor,
//! drawn from this session's configured NVIDIA head inventory, plus the
//! virtual framebuffer size that Xorg screen must declare. This module is
//! pure planning logic only — no RandR/`nvidia-settings`/process I/O happens
//! here, and it never touches an X server. Real available-head/CRTC discovery
//! on the target GPU profile remains a pier-linux.example.internal lab gate (see
//! `hosts/linux/AGENTS.md`); production callers feed in the
//! operator-configured, already-verified head roster.
//!
//! Coordinates stay in exactly one unit throughout: host RandR/Xorg screen
//! pixels. A requested monitor's `logical_*` fields (client points) are used
//! only to derive each monitor's host-pixel placement, then discarded --
//! every [`LinuxMonitorPlan`] rectangle and the [`LinuxTopologyPlan`] virtual
//! framebuffer are host pixels. Placement is edge-aware and lives in the
//! shared crate (`arcen_media::plan_edge_aware_offsets`): a monitor that
//! logically touches an already-placed neighbor is placed flush against that
//! neighbor's own host-pixel footprint using the neighbor's own scale, so a
//! chain of differently-scaled monitors stays gap-/overlap-free exactly where
//! the client's logical layout has them touching, rather than compounding a
//! single global primary-derived scale across every hop.
//!
//! Both shared conventions are passed in explicitly and never inferred from
//! the platform: this planner drives real RandR outputs at their native
//! pre-rotation mode and lets the X server apply the transform, so it uses
//! [`arcen_media::TransformConvention::NativeNeedsTransform`]; an Xorg screen's
//! virtual framebuffer must start at `(0, 0)`, so it uses
//! [`arcen_media::OriginPolicy::TranslateToNonNegative`].

use arcen_media::{
    AppliedMonitor, AppliedMonitorTopology, AppliedPoint, AppliedRect, AppliedRegionSet,
    AppliedSize, LayoutRect, LogicalRect, MediaContractError, OriginPolicy, OutputIdentity,
    PhysicalSize, RegionContractError, RegionGeneration, RegionId, RegionPlacement, RegionSet,
    RequestedMonitor, RequestedMonitorTopology, Rotation, Scale120, SessionMonitorId,
    TopologyGeneration, TopologyPlacementError, TransformConvention,
};
use arcen_protocol::messages::MonitorQualityIntentMsg;
use thiserror::Error;

/// Explicit shared transform convention for this planner: RandR outputs are
/// driven at their native pre-rotation mode and the X server applies the
/// rotation, so region descriptors carry the native stream extent plus a
/// separate output transform.
const TRANSFORM_CONVENTION: TransformConvention = TransformConvention::NativeNeedsTransform;
/// Explicit shared origin policy for this planner: an Xorg screen's virtual
/// framebuffer must start at a non-negative origin.
const ORIGIN_POLICY: OriginPolicy = OriginPolicy::TranslateToNonNegative;

/// Minimum per-monitor pixel extent this tranche accepts, matching the
/// existing single-head `nvctrl::Resolution` floor.
pub const MIN_MONITOR_DIMENSION_PX: u32 = 320;
/// Maximum per-monitor pixel extent this tranche accepts for one RandR
/// output, independent of the aggregate virtual framebuffer ceiling below.
pub const MAX_HEAD_DIMENSION_PX: u32 = 7680;
/// Maximum bounding virtual framebuffer extent this tranche accepts, matching
/// the encoder/backend dimension ceiling used elsewhere
/// (`shared/media/src/video/plan.rs` NVENC `BackendLimits`). Both hardware
/// release targets (4x1920x1080 and 2x3840x2160) fit well inside this bound.
pub const MAX_VIRTUAL_FRAMEBUFFER_DIMENSION_PX: u32 = 8192;

/// Recognized NVIDIA RandR output tokens for this Linux tranche's dedicated
/// Xorg session, matching `session::launcher::validate_gpu_head`'s existing
/// single-head allow-list.
pub const VALID_HEAD_TOKENS: [&str; 4] = ["DFP-0", "DFP-1", "DFP-2", "DFP-3"];

/// One NVIDIA output head available to a dedicated Xorg session and this
/// tranche's constraint on what that head's CRTC can drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadCapability {
    /// RandR/NV-CONTROL output name, e.g. `"DFP-0"`.
    pub head: String,
    /// Whether this head's assigned CRTC can apply the RandR rotations this
    /// tranche supports (90/180/270 clockwise). `false` restricts the head to
    /// [`Rotation::Degrees0`] — a documented, configured CRTC constraint, not
    /// a hardware probe.
    pub supports_rotation: bool,
}

impl HeadCapability {
    /// Creates a head capability that supports every RandR rotation this
    /// tranche uses.
    #[must_use]
    pub fn new(head: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            supports_rotation: true,
        }
    }

    /// Creates a head capability restricted to [`Rotation::Degrees0`].
    #[must_use]
    pub fn fixed_orientation(head: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            supports_rotation: false,
        }
    }
}

/// Validated, ordered inventory of NVIDIA output heads available to plan
/// against. Order is significant: heads are assigned to requested monitors in
/// inventory order (primary first), so operators list preferred heads first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadInventory {
    heads: Vec<HeadCapability>,
}

impl HeadInventory {
    /// Validates a configured/discovered head roster.
    ///
    /// # Errors
    ///
    /// Returns an error when the roster is empty, contains an unrecognized
    /// head token, or duplicates a head.
    pub fn new(heads: Vec<HeadCapability>) -> Result<Self, LinuxTopologyError> {
        if heads.is_empty() {
            return Err(LinuxTopologyError::NoHeadsConfigured);
        }
        let mut seen = std::collections::BTreeSet::new();
        for head in &heads {
            if !VALID_HEAD_TOKENS.contains(&head.head.as_str()) {
                return Err(LinuxTopologyError::InvalidHeadToken(head.head.clone()));
            }
            if !seen.insert(head.head.as_str()) {
                return Err(LinuxTopologyError::DuplicateHead(head.head.clone()));
            }
        }
        Ok(Self { heads })
    }

    /// Convenience constructor for a roster where every head supports every
    /// RandR rotation this tranche uses.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`Self::new`].
    pub fn uniform(
        heads: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, LinuxTopologyError> {
        Self::new(heads.into_iter().map(HeadCapability::new).collect())
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.heads.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heads.is_empty()
    }

    #[must_use]
    pub fn heads(&self) -> &[HeadCapability] {
        &self.heads
    }
}

/// Typed rejection from the pure Linux topology planner.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LinuxTopologyError {
    #[error("no NVIDIA output heads are configured for this session")]
    NoHeadsConfigured,
    #[error("configured head token {0:?} is not a recognized DFP-N output")]
    InvalidHeadToken(String),
    #[error("configured head token {0:?} is duplicated")]
    DuplicateHead(String),
    #[error("requested {requested} monitors but only {available} heads are available")]
    InsufficientHeads { requested: usize, available: usize },
    #[error(
        "monitor {0:?} requests {1}x{2}, below the minimum {MIN_MONITOR_DIMENSION_PX}x{MIN_MONITOR_DIMENSION_PX}"
    )]
    MonitorTooSmall(String, u32, u32),
    #[error(
        "monitor {0:?} requests {1}x{2}, above the maximum {MAX_HEAD_DIMENSION_PX}x{MAX_HEAD_DIMENSION_PX}"
    )]
    MonitorTooLarge(String, u32, u32),
    #[error(
        "monitor {0:?} requests an odd pixel dimension {1}x{2}; the encode path requires even width and height"
    )]
    OddMonitorDimensions(String, u32, u32),
    #[error("monitor {0:?} requests rotation {1:?}, which assigned head {2:?} does not support")]
    UnsupportedRotationForHead(String, Rotation, String),
    #[error(
        "planned virtual framebuffer {0}x{1} exceeds the maximum {MAX_VIRTUAL_FRAMEBUFFER_DIMENSION_PX}x{MAX_VIRTUAL_FRAMEBUFFER_DIMENSION_PX}"
    )]
    VirtualFramebufferTooLarge(u32, u32),
    #[error("monitor {0:?} scale cannot be represented in shared Scale120 units")]
    RegionScaleOutOfRange(String),
    #[error("requested/applied topology is invalid: {0}")]
    InvalidTopology(#[from] MediaContractError),
    #[error("shared region contract rejected the Linux topology: {0}")]
    InvalidRegion(#[from] RegionContractError),
    #[error("shared topology placement rejected the Linux layout: {0}")]
    Placement(TopologyPlacementError),
}

impl From<TopologyPlacementError> for LinuxTopologyError {
    /// Preserves this planner's existing typed rejections: shared contract and
    /// region failures keep mapping to [`Self::InvalidTopology`] and
    /// [`Self::InvalidRegion`] exactly as they did before placement moved into
    /// `arcen-media`.
    fn from(value: TopologyPlacementError) -> Self {
        match value {
            TopologyPlacementError::Contract(error) => Self::InvalidTopology(error),
            TopologyPlacementError::Region(error) => Self::InvalidRegion(error),
            other => Self::Placement(other),
        }
    }
}

/// One applied RandR output rectangle, in host Xorg screen pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxMonitorPlan {
    pub session_monitor_id: SessionMonitorId,
    pub client_display_id: String,
    /// Assigned RandR/NV-CONTROL output, e.g. `"DFP-0"`.
    pub head: String,
    /// Host screen-pixel horizontal origin (signed; translated so the overall
    /// bounding raster starts at a non-negative origin).
    pub x: i32,
    /// Host screen-pixel vertical origin (signed; translated so the overall
    /// bounding raster starts at a non-negative origin).
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Requested logical desktop rectangle retained in shared fixed-point
    /// units so region-scoped input never re-derives it from applied pixels.
    pub logical_rect: LogicalRect,
    /// Native pre-rotation stream extent used by the shared coordinate
    /// transformer before the RandR output transform is applied.
    pub physical_size: PhysicalSize,
    /// Requested presentation scale in the shared 1/120 representation.
    pub scale: Scale120,
    pub rotation: Rotation,
    pub primary: bool,
    pub quality_intent: MonitorQualityIntentMsg,
    /// Exact native (pre-rotation) NVIDIA MetaMode resolution token, e.g.
    /// `"1920x1080"`. This is the head's raster, not its extent in the X
    /// screen: for a rotated monitor the X-screen extent is
    /// [`Self::width`]x[`Self::height`], which is what
    /// `session::xorg_multihead` states as the head's `ViewPortIn`.
    pub mode_token: String,
}

/// Complete Linux dedicated-Xorg topology plan for one committed
/// [`TopologyGeneration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxTopologyPlan {
    pub generation: TopologyGeneration,
    /// Bounding virtual framebuffer width the Xorg `Screen` section's
    /// `Virtual` directive must declare.
    pub virtual_width: u32,
    /// Bounding virtual framebuffer height the Xorg `Screen` section's
    /// `Virtual` directive must declare.
    pub virtual_height: u32,
    /// Applied monitors, in requested-roster order (not head-assignment
    /// order).
    pub monitors: Vec<LinuxMonitorPlan>,
}

impl LinuxTopologyPlan {
    #[must_use]
    pub fn primary(&self) -> &LinuxMonitorPlan {
        self.monitors
            .iter()
            .find(|monitor| monitor.primary)
            .unwrap_or(&self.monitors[0])
    }

    /// Builds the shared requested and applied region aggregates represented
    /// by this committed Xorg topology, through the shared
    /// [`arcen_media::build_region_sets`] constructor under this planner's
    /// explicit [`TransformConvention::NativeNeedsTransform`] convention.
    ///
    /// # Errors
    ///
    /// Returns a shared region-contract error when a malformed launcher IPC
    /// plan has inconsistent identities, geometry, or transformed extents.
    pub fn region_sets(&self) -> Result<(RegionSet, AppliedRegionSet), LinuxTopologyError> {
        let generation = RegionGeneration::new(self.generation.get())?;
        let placements = self
            .monitors
            .iter()
            .map(LinuxMonitorPlan::region_placement)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(arcen_media::build_region_sets(
            generation,
            TRANSFORM_CONVENTION,
            &placements,
        )?)
    }
}

impl LinuxMonitorPlan {
    fn region_placement(&self) -> Result<RegionPlacement, LinuxTopologyError> {
        Ok(RegionPlacement {
            region_id: RegionId::new(u32::from(self.session_monitor_id.get()))?,
            output: OutputIdentity::new(self.head.clone())?,
            logical_rect: self.logical_rect,
            stream_size: self.physical_size,
            scale: self.scale,
            rotation: self.rotation,
            primary: self.primary,
            applied_rect: AppliedRect::new(
                AppliedPoint::new(i64::from(self.x), i64::from(self.y)),
                AppliedSize::new(self.width, self.height)?,
            )?,
        })
    }
}

fn region_logical_rect(monitor: &RequestedMonitor) -> Result<LogicalRect, LinuxTopologyError> {
    Ok(arcen_media::logical_rect_from_layout(
        monitor.logical_arrangement_rect()?,
    )?)
}

fn region_scale(client_display_id: &str, scale: f32) -> Result<Scale120, LinuxTopologyError> {
    arcen_media::scale120_from_scale(scale).map_err(|error| match error {
        TopologyPlacementError::ScaleOutOfRange => {
            LinuxTopologyError::RegionScaleOutOfRange(client_display_id.to_owned())
        }
        other => LinuxTopologyError::from(other),
    })
}

/// Derives every requested monitor's host-pixel desktop origin from the
/// client's logical arrangement, anchored at the primary, through the shared
/// edge-aware placement primitive.
///
/// # Errors
///
/// Returns an error when a requested monitor's logical rectangle is invalid,
/// or a coordinate conversion overflows the signed desktop domain.
fn plan_monitor_offsets(
    monitors: &[RequestedMonitor],
    primary_index: usize,
) -> Result<Vec<(i32, i32)>, LinuxTopologyError> {
    Ok(arcen_media::plan_edge_aware_offsets(
        monitors,
        primary_index,
        TRANSFORM_CONVENTION,
    )?)
}

/// Returns a monitor's actual on-screen desktop footprint, swapping its
/// native (pre-rotation) `width_px`/`height_px` NVIDIA MetaMode mode
/// dimensions when `rotation` is 90 or 270 degrees.
///
/// `width_px`/`height_px` always stay the native, unrotated mode the GPU
/// drives that head at (see `session::xorg_multihead`'s `mode_token`,
/// which must keep using them unswapped); a RandR `Rotation=` transform is
/// applied on top of that native mode to produce the footprint this monitor
/// actually occupies on the combined desktop. Every caller that reasons
/// about *where* a monitor sits on the desktop (virtual framebuffer bounds,
/// non-overlap, and the shared region adapter) must use this rotated
/// footprint, not the raw native dimensions.
const fn rotated_desktop_footprint(
    width_px: u32,
    height_px: u32,
    rotation: Rotation,
) -> (u32, u32) {
    TRANSFORM_CONVENTION.desktop_footprint(width_px, height_px, rotation)
}

fn validate_monitor_dimensions(
    client_display_id: &str,
    width: u32,
    height: u32,
) -> Result<(), LinuxTopologyError> {
    if width < MIN_MONITOR_DIMENSION_PX || height < MIN_MONITOR_DIMENSION_PX {
        return Err(LinuxTopologyError::MonitorTooSmall(
            client_display_id.to_owned(),
            width,
            height,
        ));
    }
    if width > MAX_HEAD_DIMENSION_PX || height > MAX_HEAD_DIMENSION_PX {
        return Err(LinuxTopologyError::MonitorTooLarge(
            client_display_id.to_owned(),
            width,
            height,
        ));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(LinuxTopologyError::OddMonitorDimensions(
            client_display_id.to_owned(),
            width,
            height,
        ));
    }
    Ok(())
}

/// Plans a validated 1-4 monitor [`RequestedMonitorTopology`] onto one
/// dedicated Xorg screen using `inventory`'s heads (primary first, remaining
/// heads assigned in inventory order to the remaining monitors in roster
/// order).
///
/// # Errors
///
/// Returns a typed rejection when the requested monitor count exceeds the
/// available head count, any monitor's requested pixel geometry is out of
/// this tranche's bounds, a monitor's requested rotation exceeds its assigned
/// head's capability, the planned virtual framebuffer exceeds this tranche's
/// ceiling, or an applied-topology invariant is violated. No partial plan is
/// ever returned: the caller must treat any `Err` as a whole-topology
/// rejection.
pub fn plan_topology(
    requested: &RequestedMonitorTopology,
    generation: TopologyGeneration,
    inventory: &HeadInventory,
) -> Result<LinuxTopologyPlan, LinuxTopologyError> {
    let monitors = requested.monitors();
    if monitors.len() > inventory.len() {
        return Err(LinuxTopologyError::InsufficientHeads {
            requested: monitors.len(),
            available: inventory.len(),
        });
    }
    for requested_monitor in monitors {
        let client_display_id = requested_monitor
            .client_display_id()
            .map_err(LinuxTopologyError::InvalidTopology)?;
        validate_monitor_dimensions(
            client_display_id.as_str(),
            requested_monitor.monitor().width_px,
            requested_monitor.monitor().height_px,
        )?;
    }

    let primary_index = monitors
        .iter()
        .position(|monitor| monitor.monitor().primary)
        .ok_or(LinuxTopologyError::InvalidTopology(
            MediaContractError::PrimaryMonitorCount(0),
        ))?;
    // Edge-aware: every monitor's host-pixel origin is derived by walking the
    // touching-edge graph from the primary, using each already-placed
    // neighbor's own scale for the shared edge -- see `plan_monitor_offsets`.
    let offsets = plan_monitor_offsets(monitors, primary_index)?;

    let mut applied = Vec::with_capacity(monitors.len());
    for (index, requested_monitor) in monitors.iter().enumerate() {
        let (desktop_x, desktop_y) = offsets[index];
        // Deterministic 1-based session monitor ids, in requested-roster
        // order; `SessionMonitorId::new` is fallible (nonzero-only), but
        // `index + 1` is always `>= 1`, so only the `u16` range conversion
        // above can realistically fail.
        let session_monitor_id = u16::try_from(index + 1)
            .map_err(|_| {
                LinuxTopologyError::InvalidTopology(MediaContractError::CoordinateOverflow)
            })
            .and_then(|value| {
                SessionMonitorId::new(value).map_err(LinuxTopologyError::InvalidTopology)
            })?;
        applied.push(AppliedMonitor::new(
            session_monitor_id,
            requested_monitor.clone(),
            desktop_x,
            desktop_y,
        )?);
    }

    let topology = AppliedMonitorTopology::new(generation, applied)?;

    // Rotation-aware desktop footprint per monitor, from the shared
    // `NativeNeedsTransform` convention: `Monitor::width_px`/`height_px` stay
    // the native, unrotated NVIDIA MetaMode dimensions, so a 90/270-degree
    // monitor occupies a swapped on-screen rectangle. `desktop_x_px`/
    // `desktop_y_px` (the top-left origin) are unaffected by rotation, since
    // only a monitor's own footprint dimensions swap, not its offset.
    let footprint_rects = topology
        .monitors()
        .iter()
        .map(|monitor| {
            let (width, height) = rotated_desktop_footprint(
                monitor.monitor().width_px,
                monitor.monitor().height_px,
                monitor.monitor().rotation,
            );
            LayoutRect::new(monitor.desktop_x_px, monitor.desktop_y_px, width, height)
        })
        .collect::<Result<Vec<_>, MediaContractError>>()?;
    // Explicit shared origin policy: an Xorg virtual framebuffer must start at
    // a non-negative origin, so the whole layout is translated by one offset
    // that preserves every relative placement.
    let placed = arcen_media::apply_origin_policy(footprint_rects, ORIGIN_POLICY)?;
    let translation = placed.translation();
    let bounds = placed.bounds();
    let footprint_rects = placed.rects().to_vec();
    let topology = if translation.dx == 0 && translation.dy == 0 {
        topology
    } else {
        let translated = topology
            .monitors()
            .iter()
            .zip(&footprint_rects)
            .map(|(monitor, rect)| {
                AppliedMonitor::new(
                    monitor.session_monitor_id,
                    monitor.requested_monitor().clone(),
                    rect.x,
                    rect.y,
                )
            })
            .collect::<Result<Vec<_>, MediaContractError>>()?;
        AppliedMonitorTopology::new(generation, translated)?
    };

    if bounds.width > MAX_VIRTUAL_FRAMEBUFFER_DIMENSION_PX
        || bounds.height > MAX_VIRTUAL_FRAMEBUFFER_DIMENSION_PX
    {
        return Err(LinuxTopologyError::VirtualFramebufferTooLarge(
            bounds.width,
            bounds.height,
        ));
    }

    // Assign heads: primary monitor gets the first inventory head; the
    // remaining monitors (in original roster order, primary excluded) get the
    // remaining heads in inventory order. Deterministic and stable across
    // identical requests.
    let primary_index = topology
        .monitors()
        .iter()
        .position(|monitor| monitor.monitor().primary)
        .unwrap_or(0);
    let mut head_order = Vec::with_capacity(topology.monitors().len());
    head_order.push(primary_index);
    for (index, _) in topology.monitors().iter().enumerate() {
        if index != primary_index {
            head_order.push(index);
        }
    }

    let mut plans: Vec<Option<LinuxMonitorPlan>> = vec![None; topology.monitors().len()];
    for (head_slot, monitor_index) in head_order.into_iter().enumerate() {
        let applied_monitor = &topology.monitors()[monitor_index];
        let head = &inventory.heads()[head_slot];
        let rect = footprint_rects[monitor_index];
        let rotation = applied_monitor.monitor().rotation;
        if rotation != Rotation::Degrees0 && !head.supports_rotation {
            return Err(LinuxTopologyError::UnsupportedRotationForHead(
                applied_monitor
                    .client_display_id()
                    .map_err(LinuxTopologyError::InvalidTopology)?
                    .as_str()
                    .to_owned(),
                rotation,
                head.head.clone(),
            ));
        }
        let client_display_id = applied_monitor
            .client_display_id()
            .map_err(LinuxTopologyError::InvalidTopology)?
            .as_str()
            .to_owned();
        plans[monitor_index] = Some(LinuxMonitorPlan {
            session_monitor_id: applied_monitor.session_monitor_id,
            client_display_id: client_display_id.clone(),
            head: head.head.clone(),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            logical_rect: region_logical_rect(applied_monitor.requested_monitor())?,
            physical_size: PhysicalSize::new(
                applied_monitor.monitor().width_px,
                applied_monitor.monitor().height_px,
            )?,
            scale: region_scale(&client_display_id, applied_monitor.monitor().scale)?,
            rotation,
            primary: applied_monitor.monitor().primary,
            quality_intent: MonitorQualityIntentMsg::HostDefault,
            // NVIDIA's MetaMode resolution token is always the native
            // (pre-rotation) mode. `rect.width`/`rect.height` above is the
            // rotation-aware desktop *footprint* and must not be used here —
            // it is what `session::xorg_multihead` states as the head's
            // `ViewPortIn` (its extent in the X screen), which is a different
            // quantity from this native raster whenever the monitor is
            // rotated.
            mode_token: format!(
                "{}x{}",
                applied_monitor.monitor().width_px,
                applied_monitor.monitor().height_px
            ),
        });
    }

    Ok(LinuxTopologyPlan {
        generation,
        virtual_width: bounds.width,
        virtual_height: bounds.height,
        monitors: plans
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .expect("every applied monitor index is assigned exactly one head"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{MonitorIdentity, OutputTransform, RequestedMonitor};

    fn requested_monitor(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        let monitor = arcen_media::Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                ..MonitorIdentity::default()
            },
            x,
            y,
            width_px,
            height_px,
            scale: 1.0,
            refresh_hz: 60,
            rotation,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        RequestedMonitor::new(monitor, width_px, height_px).expect("requested monitor")
    }

    /// Like [`requested_monitor`], but with independently controlled logical
    /// arrangement extents, so a monitor's own scale (`width_px`/`height_px`
    /// vs. `logical_width`/`logical_height`) can differ from `1.0` and from
    /// its neighbors' own scale.
    #[allow(clippy::too_many_arguments)]
    fn scaled_monitor(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        logical_width: u32,
        logical_height: u32,
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        let monitor = arcen_media::Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                ..MonitorIdentity::default()
            },
            x,
            y,
            width_px,
            height_px,
            scale: (f64::from(width_px) / f64::from(logical_width)) as f32,
            refresh_hz: 60,
            rotation,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        RequestedMonitor::new(monitor, logical_width, logical_height).expect("requested monitor")
    }

    fn generation() -> TopologyGeneration {
        TopologyGeneration::new(1).expect("generation")
    }

    fn inventory(count: usize) -> HeadInventory {
        HeadInventory::uniform(VALID_HEAD_TOKENS.iter().take(count).copied()).expect("inventory")
    }

    fn assert_horizontal_touching_chain(
        requested: Vec<RequestedMonitor>,
        expected: &[(&str, i32, u32)],
        expected_virtual_width: u32,
        expected_virtual_height: u32,
    ) {
        let plan = plan_topology(
            &RequestedMonitorTopology::new(requested).expect("requested"),
            generation(),
            &inventory(expected.len()),
        )
        .expect("plan");
        let plans = expected
            .iter()
            .map(|(id, expected_x, expected_width)| {
                let monitor = plan
                    .monitors
                    .iter()
                    .find(|monitor| monitor.client_display_id == *id)
                    .unwrap_or_else(|| panic!("{id} monitor"));
                assert_eq!((monitor.x, monitor.width), (*expected_x, *expected_width));
                monitor
            })
            .collect::<Vec<_>>();
        for adjacent in plans.windows(2) {
            assert_eq!(
                i64::from(adjacent[0].x) + i64::from(adjacent[0].width),
                i64::from(adjacent[1].x),
                "{} and {} must remain exactly edge-adjacent",
                adjacent[0].client_display_id,
                adjacent[1].client_display_id,
            );
        }
        assert_eq!(plan.virtual_width, expected_virtual_width);
        assert_eq!(plan.virtual_height, expected_virtual_height);
    }

    #[test]
    fn plans_one_monitor_onto_the_first_head() {
        let requested = RequestedMonitorTopology::new(vec![requested_monitor(
            "primary",
            0,
            0,
            1920,
            1080,
            true,
            Rotation::Degrees0,
        )])
        .expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(1)).expect("plan");
        assert_eq!(plan.virtual_width, 1920);
        assert_eq!(plan.virtual_height, 1080);
        assert_eq!(plan.monitors.len(), 1);
        assert_eq!(plan.monitors[0].head, "DFP-0");
        assert_eq!(plan.monitors[0].x, 0);
        assert_eq!(plan.monitors[0].y, 0);
        assert!(plan.monitors[0].primary);
        assert_eq!(plan.monitors[0].mode_token, "1920x1080");
    }

    #[test]
    fn plans_two_monitors_side_by_side_with_primary_on_the_first_head() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            requested_monitor("second", 1920, 0, 1280, 720, false, Rotation::Degrees0),
        ])
        .expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");
        assert_eq!(plan.virtual_width, 3200);
        assert_eq!(plan.virtual_height, 1080);
        let primary = plan.primary();
        assert_eq!(primary.head, "DFP-0");
        assert_eq!((primary.x, primary.y), (0, 0));
        let second = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "second")
            .expect("second monitor");
        assert_eq!(second.head, "DFP-1");
        assert_eq!((second.x, second.y), (1920, 0));
        assert_eq!((second.width, second.height), (1280, 720));
    }

    #[test]
    fn plans_four_monitors_in_a_row_and_assigns_all_four_heads() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("a", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            requested_monitor("b", 1920, 0, 1920, 1080, false, Rotation::Degrees0),
            requested_monitor("c", 3840, 0, 1920, 1080, false, Rotation::Degrees0),
            requested_monitor("d", 5760, 0, 1920, 1080, false, Rotation::Degrees0),
        ])
        .expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(4)).expect("plan");
        assert_eq!(plan.virtual_width, 7680);
        assert_eq!(plan.virtual_height, 1080);
        let mut heads: Vec<&str> = plan.monitors.iter().map(|m| m.head.as_str()).collect();
        heads.sort_unstable();
        assert_eq!(heads, ["DFP-0", "DFP-1", "DFP-2", "DFP-3"]);
        for monitor in &plan.monitors {
            assert_eq!(monitor.width, 1920);
            assert_eq!(monitor.height, 1080);
        }
    }

    #[test]
    fn negative_logical_origin_translates_to_a_non_negative_applied_origin() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("primary", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            requested_monitor("left", -1280, 0, 1280, 1024, false, Rotation::Degrees0),
        ])
        .expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");
        assert_eq!(plan.virtual_width, 3200);
        let left = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "left")
            .expect("left monitor");
        assert_eq!(left.x, 0);
        let primary = plan.primary();
        assert_eq!(primary.x, 1280);
        for monitor in &plan.monitors {
            assert!(monitor.x >= 0);
            assert!(monitor.y >= 0);
        }
    }

    #[test]
    fn rejects_more_monitors_than_available_heads() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("a", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            requested_monitor("b", 1920, 0, 1920, 1080, false, Rotation::Degrees0),
        ])
        .expect("requested");
        assert_eq!(
            plan_topology(&requested, generation(), &inventory(1)),
            Err(LinuxTopologyError::InsufficientHeads {
                requested: 2,
                available: 1,
            })
        );
    }

    #[test]
    fn rejects_monitor_dimensions_outside_this_tranches_bounds() {
        let too_small = RequestedMonitorTopology::new(vec![requested_monitor(
            "tiny",
            0,
            0,
            200,
            200,
            true,
            Rotation::Degrees0,
        )])
        .expect("requested");
        assert_eq!(
            plan_topology(&too_small, generation(), &inventory(1)),
            Err(LinuxTopologyError::MonitorTooSmall(
                "tiny".to_owned(),
                200,
                200
            ))
        );

        let odd = RequestedMonitorTopology::new(vec![requested_monitor(
            "odd",
            0,
            0,
            1921,
            1081,
            true,
            Rotation::Degrees0,
        )])
        .expect("requested");
        assert_eq!(
            plan_topology(&odd, generation(), &inventory(1)),
            Err(LinuxTopologyError::OddMonitorDimensions(
                "odd".to_owned(),
                1921,
                1081
            ))
        );

        let too_large = RequestedMonitorTopology::new(vec![requested_monitor(
            "huge",
            0,
            0,
            8000,
            8000,
            true,
            Rotation::Degrees0,
        )])
        .expect("requested");
        assert_eq!(
            plan_topology(&too_large, generation(), &inventory(1)),
            Err(LinuxTopologyError::MonitorTooLarge(
                "huge".to_owned(),
                8000,
                8000
            ))
        );
    }

    #[test]
    fn rejects_virtual_framebuffer_larger_than_the_configured_ceiling() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("a", 0, 0, 4096, 2160, true, Rotation::Degrees0),
            requested_monitor("b", 4096, 0, 4096, 2160, false, Rotation::Degrees0),
            requested_monitor("c", 8192, 0, 4096, 2160, false, Rotation::Degrees0),
        ])
        .expect("requested");
        assert_eq!(
            plan_topology(&requested, generation(), &inventory(3)),
            Err(LinuxTopologyError::VirtualFramebufferTooLarge(12288, 2160))
        );
    }

    #[test]
    fn rejects_rotation_on_a_head_that_does_not_support_it() {
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("a", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            requested_monitor("b", 1920, 0, 1920, 1080, false, Rotation::Degrees90),
        ])
        .expect("requested");
        let restricted = HeadInventory::new(vec![
            HeadCapability::new("DFP-0"),
            HeadCapability::fixed_orientation("DFP-1"),
        ])
        .expect("inventory");
        assert_eq!(
            plan_topology(&requested, generation(), &restricted),
            Err(LinuxTopologyError::UnsupportedRotationForHead(
                "b".to_owned(),
                Rotation::Degrees90,
                "DFP-1".to_owned(),
            ))
        );
    }

    #[test]
    fn head_inventory_rejects_empty_duplicate_and_unknown_tokens() {
        assert_eq!(
            HeadInventory::uniform(Vec::<&str>::new()),
            Err(LinuxTopologyError::NoHeadsConfigured)
        );
        assert_eq!(
            HeadInventory::uniform(["DFP-9"]),
            Err(LinuxTopologyError::InvalidHeadToken("DFP-9".to_owned()))
        );
        assert_eq!(
            HeadInventory::uniform(["DFP-0", "DFP-0"]),
            Err(LinuxTopologyError::DuplicateHead("DFP-0".to_owned()))
        );
    }

    #[test]
    fn mixed_scale_primary_places_a_lower_scale_monitor_by_the_primarys_own_scale() {
        // Primary is a 2x-scale (Retina-like) monitor: logical 960x540 maps to
        // 1920x1080 physical pixels, so primary_scale is 2.0 on both axes.
        // The second monitor sits immediately to the right in logical space
        // (logical x = 960, matching primary's own logical width) and must
        // land at physical x = 1920 (960 * 2.0), not at 960.
        let primary_monitor = arcen_media::Monitor {
            identity: MonitorIdentity {
                id: "retina".to_owned(),
                name: "Retina".to_owned(),
                ..MonitorIdentity::default()
            },
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 2.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary: true,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        let primary = RequestedMonitor::new(primary_monitor, 960, 540).expect("primary");
        let second = requested_monitor("second", 960, 0, 1280, 720, false, Rotation::Degrees0);
        let requested = RequestedMonitorTopology::new(vec![primary, second]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");
        let second_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "second")
            .expect("second monitor");
        assert_eq!(second_plan.x, 1920);
    }

    #[test]
    fn rotated_desktop_footprint_swaps_dimensions_only_for_quarter_turns() {
        assert_eq!(
            rotated_desktop_footprint(1920, 1080, Rotation::Degrees0),
            (1920, 1080)
        );
        assert_eq!(
            rotated_desktop_footprint(1920, 1080, Rotation::Degrees90),
            (1080, 1920)
        );
        assert_eq!(
            rotated_desktop_footprint(1920, 1080, Rotation::Degrees180),
            (1920, 1080)
        );
        assert_eq!(
            rotated_desktop_footprint(1920, 1080, Rotation::Degrees270),
            (1080, 1920)
        );
    }

    #[test]
    fn mixed_landscape_and_portrait_monitors_yield_a_rotation_aware_bounding_box() {
        // Primary is landscape and unrotated (1920x1080). The second monitor
        // sits immediately to its right in logical space and is rotated 90
        // degrees, so its native 1920x1080 mode must occupy a portrait
        // 1080-wide by 1920-tall desktop footprint, not its native landscape
        // footprint.
        let requested = RequestedMonitorTopology::new(vec![
            requested_monitor("a", 0, 0, 1920, 1080, true, Rotation::Degrees0),
            requested_monitor("b", 1920, 0, 1920, 1080, false, Rotation::Degrees90),
        ])
        .expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");

        // Without the rotation-aware footprint fix, the overall bounds would
        // incorrectly stay 3840x1080 (the portrait monitor's true 1920-tall
        // footprint would be invisible to the bounding box).
        assert_eq!(plan.virtual_width, 3000);
        assert_eq!(plan.virtual_height, 1920);

        let primary_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "a")
            .expect("primary monitor");
        assert_eq!((primary_plan.x, primary_plan.y), (0, 0));
        assert_eq!((primary_plan.width, primary_plan.height), (1920, 1080));
        assert_eq!(primary_plan.mode_token, "1920x1080");

        let rotated_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "b")
            .expect("rotated monitor");
        assert_eq!((rotated_plan.x, rotated_plan.y), (1920, 0));
        // The applied footprint is swapped (portrait)...
        assert_eq!((rotated_plan.width, rotated_plan.height), (1080, 1920));
        // ...but the NVIDIA MetaMode resolution token stays the monitor's
        // native, pre-rotation mode.
        assert_eq!(rotated_plan.mode_token, "1920x1080");
    }

    #[test]
    fn a_rotated_primary_scales_a_secondary_monitors_placement_coherently() {
        // Primary is a 1920x1080-native monitor mounted rotated 90 degrees,
        // so its apparent (already-rotated) logical footprint is 1080x1920 at
        // 1:1 scale. A second, unrotated 1920x1080 monitor sits immediately to
        // its right in that same apparent/logical space (logical x = 1080,
        // matching the primary's own apparent logical width).
        let primary_monitor = arcen_media::Monitor {
            identity: MonitorIdentity {
                id: "rotated-primary".to_owned(),
                name: "Rotated Primary".to_owned(),
                ..MonitorIdentity::default()
            },
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees90,
            primary: true,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        let primary = RequestedMonitor::new(primary_monitor, 1080, 1920).expect("primary");
        let second = requested_monitor("second", 1080, 0, 1920, 1080, false, Rotation::Degrees0);
        let requested = RequestedMonitorTopology::new(vec![primary, second]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");

        let primary_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "rotated-primary")
            .expect("primary monitor");
        assert_eq!((primary_plan.x, primary_plan.y), (0, 0));
        assert_eq!((primary_plan.width, primary_plan.height), (1080, 1920));

        let second_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "second")
            .expect("second monitor");
        // With the old (unswapped) scale factor this would incorrectly land
        // at x = 1920 (1080 * 1920/1080); the coherent rotated-footprint
        // scale keeps it at x = 1080, immediately beside the rotated primary.
        assert_eq!((second_plan.x, second_plan.y), (1080, 0));
        assert_eq!(plan.virtual_width, 3000);
        assert_eq!(plan.virtual_height, 1920);
    }

    #[test]
    fn chain_of_differently_scaled_monitors_stays_gap_and_overlap_free_to_the_right() {
        // Primary is 2x scale (logical 1920x1080 -> physical 3840x2160). `b`
        // is a 1x-scale monitor touching primary's right logical edge
        // (logical x = 1920). `c` is a *second* 1x-scale monitor touching
        // `b`'s right logical edge (logical x = 3840). With the previous
        // single global "primary scale" conversion, `c`'s offset from
        // *primary* (3840 logical units) would be scaled by 2.0, landing at
        // physical x = 7680 -- 1920px past `b`'s actual physical right edge
        // (3840 + 1920 = 5760), a large gap. The edge-aware placement must
        // instead walk `b`'s own (1x) scale for the b->c hop and land `c`
        // flush against `b`.
        let a = scaled_monitor("a", 0, 0, 3840, 2160, 1920, 1080, true, Rotation::Degrees0);
        let b = scaled_monitor(
            "b",
            1920,
            0,
            1920,
            1080,
            1920,
            1080,
            false,
            Rotation::Degrees0,
        );
        let c = scaled_monitor(
            "c",
            3840,
            0,
            1920,
            1080,
            1920,
            1080,
            false,
            Rotation::Degrees0,
        );
        let requested = RequestedMonitorTopology::new(vec![a, b, c]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(3)).expect("plan");

        let find = |id: &str| {
            plan.monitors
                .iter()
                .find(|monitor| monitor.client_display_id == id)
                .unwrap_or_else(|| panic!("{id} monitor"))
        };
        let plan_a = find("a");
        let plan_b = find("b");
        let plan_c = find("c");
        assert_eq!((plan_a.x, plan_a.width), (0, 3840));
        assert_eq!((plan_b.x, plan_b.width), (3840, 1920));
        // No gap: `c` starts exactly where `b` ends.
        assert_eq!(plan_c.x, plan_b.x + plan_b.width as i32);
        assert_eq!((plan_c.x, plan_c.width), (5760, 1920));
        assert_eq!(plan.virtual_width, 7680);
    }

    #[test]
    fn edge_aware_three_monitor_mixed_scale_touching_chain_is_exact() {
        assert_horizontal_touching_chain(
            vec![
                scaled_monitor("a", 0, 0, 1920, 1080, 960, 540, true, Rotation::Degrees0),
                scaled_monitor("b", 960, 0, 1280, 720, 1280, 720, false, Rotation::Degrees0),
                scaled_monitor("c", 2240, 0, 1200, 900, 800, 600, false, Rotation::Degrees0),
            ],
            &[("a", 0, 1920), ("b", 1920, 1280), ("c", 3200, 1200)],
            4400,
            1080,
        );
    }

    #[test]
    fn edge_aware_four_monitor_mixed_scale_touching_chain_is_exact() {
        assert_horizontal_touching_chain(
            vec![
                scaled_monitor("a", 0, 0, 1280, 720, 1280, 720, true, Rotation::Degrees0),
                scaled_monitor(
                    "b",
                    1280,
                    0,
                    1920,
                    1080,
                    960,
                    540,
                    false,
                    Rotation::Degrees0,
                ),
                scaled_monitor(
                    "c",
                    2240,
                    0,
                    1280,
                    960,
                    1024,
                    768,
                    false,
                    Rotation::Degrees0,
                ),
                scaled_monitor("d", 3264, 0, 1200, 900, 800, 600, false, Rotation::Degrees0),
            ],
            &[
                ("a", 0, 1280),
                ("b", 1280, 1920),
                ("c", 3200, 1280),
                ("d", 4480, 1200),
            ],
            5680,
            1080,
        );
    }

    #[test]
    fn negative_mixed_scale_chain_translates_to_origin_without_losing_adjacency() {
        let requested = RequestedMonitorTopology::new(vec![
            scaled_monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                960,
                540,
                true,
                Rotation::Degrees0,
            ),
            scaled_monitor(
                "left",
                -1280,
                0,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
            scaled_monitor(
                "far-left",
                -2080,
                0,
                1200,
                900,
                800,
                600,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested");
        let raw_offsets =
            plan_monitor_offsets(requested.monitors(), 0).expect("untranslated offsets");
        assert_eq!(raw_offsets, vec![(0, 0), (-1280, 0), (-2480, 0)]);

        let plan = plan_topology(&requested, generation(), &inventory(3)).expect("plan");
        assert_eq!(
            plan.monitors
                .iter()
                .map(|monitor| (monitor.client_display_id.as_str(), monitor.x, monitor.width))
                .collect::<Vec<_>>(),
            vec![
                ("primary", 2480, 1920),
                ("left", 1200, 1280),
                ("far-left", 0, 1200),
            ]
        );
        for (raw, applied) in raw_offsets.iter().zip(&plan.monitors) {
            assert_eq!(i64::from(applied.x), i64::from(raw.0) + 2480);
        }
        assert_eq!(plan.virtual_width, 4400);
        assert_eq!(plan.virtual_height, 1080);
    }

    #[test]
    fn host_regions_use_native_needs_transform_convention() {
        let rotated = arcen_media::Monitor {
            identity: MonitorIdentity {
                id: "rotated".to_owned(),
                name: "Rotated".to_owned(),
                ..MonitorIdentity::default()
            },
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees90,
            primary: true,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        let requested =
            RequestedMonitorTopology::new(vec![
                RequestedMonitor::new(rotated, 1080, 1920).expect("rotated monitor")
            ])
            .expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(1)).expect("plan");
        let (regions, applied) = plan.region_sets().expect("region sets");
        let region = regions.primary();

        // Host NativeNeedsTransform: the stream stays in its native
        // pre-rotation extent and carries the transform separately.
        assert_eq!(
            region.physical_size(),
            PhysicalSize::new(1920, 1080).expect("physical size")
        );
        assert_eq!(region.transform(), OutputTransform::Rotate90);
        assert_eq!(
            region.expected_applied_size().expect("applied size"),
            AppliedSize::new(1080, 1920).expect("rotated size")
        );
        assert_eq!(
            applied.primary().applied_rect().size(),
            AppliedSize::new(1080, 1920).expect("applied footprint")
        );
    }

    #[test]
    fn differently_scaled_left_neighbor_stays_flush_against_the_primary() {
        // Primary is 2x scale (logical 960x540 -> physical 1920x1080). The
        // left neighbor is 1x scale (logical == physical 1280x720) and its
        // logical right edge exactly touches the primary's logical left edge
        // (left.x + 1280 == primary.x == 0).
        let primary = scaled_monitor(
            "primary",
            0,
            0,
            1920,
            1080,
            960,
            540,
            true,
            Rotation::Degrees0,
        );
        let left = scaled_monitor(
            "left",
            -1280,
            0,
            1280,
            720,
            1280,
            720,
            false,
            Rotation::Degrees0,
        );
        let requested = RequestedMonitorTopology::new(vec![primary, left]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");

        let left_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "left")
            .expect("left monitor");
        let primary_plan = plan.primary();
        // Own footprint (1280 wide), flush against the primary, no gap.
        assert_eq!((left_plan.x, left_plan.width), (0, 1280));
        assert_eq!(primary_plan.x, 1280);
        assert_eq!(plan.virtual_width, 1280 + 1920);
    }

    #[test]
    fn differently_scaled_above_neighbor_stays_flush_against_the_primary() {
        // Primary is 2x scale (logical 960x540 -> physical 1920x1080). The
        // monitor above is 1x scale (logical == physical 1280x720) and its
        // logical bottom edge exactly touches the primary's logical top edge
        // (above.y + 720 == primary.y == 0).
        let primary = scaled_monitor(
            "primary",
            0,
            0,
            1920,
            1080,
            960,
            540,
            true,
            Rotation::Degrees0,
        );
        let above = scaled_monitor(
            "above",
            0,
            -720,
            1280,
            720,
            1280,
            720,
            false,
            Rotation::Degrees0,
        );
        let requested = RequestedMonitorTopology::new(vec![primary, above]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");

        let above_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "above")
            .expect("above monitor");
        let primary_plan = plan.primary();
        assert_eq!((above_plan.y, above_plan.height), (0, 720));
        assert_eq!(primary_plan.y, 720);
        assert_eq!(plan.virtual_height, 720 + 1080);
    }

    #[test]
    fn differently_scaled_below_neighbor_stays_flush_against_the_primary() {
        // Primary is 2x scale (logical 960x540 -> physical 1920x1080). The
        // monitor below is 1x scale (logical == physical 1280x720) and its
        // logical top edge exactly touches the primary's logical bottom edge
        // (below.y == primary.y + 540 == 540).
        let primary = scaled_monitor(
            "primary",
            0,
            0,
            1920,
            1080,
            960,
            540,
            true,
            Rotation::Degrees0,
        );
        let below = scaled_monitor(
            "below",
            0,
            540,
            1280,
            720,
            1280,
            720,
            false,
            Rotation::Degrees0,
        );
        let requested = RequestedMonitorTopology::new(vec![primary, below]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");

        let below_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "below")
            .expect("below monitor");
        let primary_plan = plan.primary();
        // Flush directly beneath the primary's own physical footprint (1080
        // tall), not beneath a naively-scaled logical offset.
        assert_eq!(below_plan.y, primary_plan.height as i32);
        assert_eq!((below_plan.y, below_plan.height), (1080, 720));
        assert_eq!(plan.virtual_height, 1080 + 720);
    }

    #[test]
    fn a_logical_gap_with_no_touching_edge_falls_back_to_primary_scale_and_is_preserved() {
        // `second` does not touch the primary anywhere (there is a genuine
        // 500-logical-unit gap to the right of the 2x-scale primary's own
        // logical width of 960). No touching-edge chain reaches it, so it
        // must fall back to the primary's own scale applied to its absolute
        // logical offset -- preserving the intentional gap proportionally,
        // rather than erroring or silently snapping flush.
        let primary = scaled_monitor(
            "primary",
            0,
            0,
            1920,
            1080,
            960,
            540,
            true,
            Rotation::Degrees0,
        );
        let second = scaled_monitor(
            "second",
            1460, // 960 (primary logical width) + 500 (gap)
            0,
            1280,
            720,
            1280,
            720,
            false,
            Rotation::Degrees0,
        );
        let requested = RequestedMonitorTopology::new(vec![primary, second]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(2)).expect("plan");

        let second_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "second")
            .expect("second monitor");
        // Fallback: round(1460 * 2.0) == 2920, leaving an exact 1000px gap
        // past the primary's own 1920px-wide physical footprint.
        assert_eq!(second_plan.x, 2920);
        assert_eq!(plan.virtual_width, 2920 + 1280);
    }

    #[test]
    fn rotated_chain_of_differently_scaled_monitors_stays_flush() {
        // Primary is mounted rotated 90 degrees (native 1920x1080, apparent
        // rotated logical/physical footprint 1080x1920, 1:1 scale). `b`
        // touches primary's right logical edge at 2x scale (logical 960x540
        // -> physical 1920x1080). `c` touches `b`'s right logical edge at yet
        // another, 1x scale. Each hop must stay flush using its own anchor's
        // scale, regardless of the primary's own rotation or scale.
        let primary_monitor = arcen_media::Monitor {
            identity: MonitorIdentity {
                id: "rotated-primary".to_owned(),
                name: "Rotated Primary".to_owned(),
                ..MonitorIdentity::default()
            },
            x: 0,
            y: 0,
            width_px: 1920,
            height_px: 1080,
            scale: 1.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees90,
            primary: true,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        let primary = RequestedMonitor::new(primary_monitor, 1080, 1920).expect("primary");
        let b = scaled_monitor(
            "b",
            1080,
            0,
            1920,
            1080,
            960,
            540,
            false,
            Rotation::Degrees0,
        );
        let c = scaled_monitor(
            "c",
            2040,
            0,
            1920,
            1080,
            1920,
            1080,
            false,
            Rotation::Degrees0,
        );
        let requested = RequestedMonitorTopology::new(vec![primary, b, c]).expect("requested");
        let plan = plan_topology(&requested, generation(), &inventory(3)).expect("plan");

        let find = |id: &str| {
            plan.monitors
                .iter()
                .find(|monitor| monitor.client_display_id == id)
                .unwrap_or_else(|| panic!("{id} monitor"))
        };
        let plan_primary = find("rotated-primary");
        let plan_b = find("b");
        let plan_c = find("c");
        assert_eq!((plan_primary.x, plan_primary.width), (0, 1080));
        // `b` flush against the rotated primary's own (1x) footprint width.
        assert_eq!((plan_b.x, plan_b.width), (1080, 1920));
        // `c` flush against `b`'s own (2x) footprint, not a naive primary
        // (1x rotated) scale applied to the whole b+c logical span.
        assert_eq!(
            plan_b.x as i64 + i64::from(plan_b.width),
            i64::from(plan_c.x)
        );
        assert_eq!((plan_c.x, plan_c.width), (3000, 1920));
        assert_eq!(plan.virtual_width, 3000 + 1920);
    }
}
