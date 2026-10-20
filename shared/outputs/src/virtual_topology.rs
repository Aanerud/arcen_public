//! Placement of a requested multi-monitor topology onto compositor virtual
//! outputs.
//!
//! A host whose outputs are virtual monitors it creates on demand (a
//! headless Wayland compositor's `RecordVirtual` monitors, a macOS virtual
//! display) has no head inventory to assign: every requested monitor gets
//! one virtual output of exactly its requested size and refresh, placed by
//! the client's logical arrangement. This module is that placement and
//! nothing else: no compositor call, no I/O.
//!
//! Conventions are fixed and stated, never inferred: a virtual output is
//! created at its already-oriented extent
//! ([`TransformConvention::AlreadyCompositorOriented`]) and the desktop is
//! translated so its bounding box starts at `(0, 0)`
//! ([`OriginPolicy::TranslateToNonNegative`]), which is what a compositor
//! layout of adjacent monitors anchored at the origin needs. Rotation is
//! served only when the host says it can rotate a virtual output.

use std::fmt::{Display, Formatter};

use arcen_media::{
    LayoutRect, LogicalRect, MediaContractError, OriginPolicy, PhysicalSize,
    RequestedMonitorTopology, Rotation, Scale120, SessionMonitorId, TopologyGeneration,
    TopologyPlacementError, TransformConvention,
};

/// Convention under which virtual-output plans are made.
pub const VIRTUAL_TRANSFORM_CONVENTION: TransformConvention =
    TransformConvention::AlreadyCompositorOriented;

/// Origin policy under which virtual-output plans are made.
pub const VIRTUAL_ORIGIN_POLICY: OriginPolicy = OriginPolicy::TranslateToNonNegative;

/// What a host can serve, stated by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualOutputLimits {
    /// Most outputs one desktop serves.
    pub max_outputs: usize,
    /// Smallest edge of one output, in pixels.
    pub min_edge: u32,
    /// Largest edge of one output, in pixels.
    pub max_edge: u32,
    /// Largest edge of the whole desktop, in pixels.
    pub max_desktop_edge: u32,
    /// Highest refresh rate one output may run at.
    pub max_refresh_hz: u32,
    /// Whether an output may be rotated.
    pub rotation: bool,
}

/// One planned virtual output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualOutputPlan {
    /// Wire identity of the region, 1-based in requested-roster order.
    pub session_monitor_id: SessionMonitorId,
    /// The client's opaque display identity.
    pub client_display_id: String,
    /// Left edge in desktop pixels (non-negative).
    pub x: i32,
    /// Top edge in desktop pixels (non-negative).
    pub y: i32,
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Requested refresh rate.
    pub refresh_hz: u32,
    /// Requested logical arrangement rectangle.
    pub logical_rect: LogicalRect,
    /// Stream extent.
    pub physical_size: PhysicalSize,
    /// Requested presentation (backing) scale.
    pub scale: Scale120,
    /// Physical size the client reported, in millimetres (zero when unknown).
    pub width_mm: u32,
    /// Physical size the client reported, in millimetres (zero when unknown).
    pub height_mm: u32,
    /// Rotation (always [`Rotation::Degrees0`] unless the limits allow it).
    pub rotation: Rotation,
    /// Whether this output anchors the desktop.
    pub primary: bool,
}

impl VirtualOutputPlan {
    /// The output's desktop rectangle.
    ///
    /// # Errors
    ///
    /// When the rectangle overflows the signed desktop domain.
    pub fn rect(&self) -> Result<LayoutRect, MediaContractError> {
        LayoutRect::new(self.x, self.y, self.width, self.height)
    }
}

/// A complete virtual-output plan for one topology generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualTopologyPlan {
    /// Generation of the requested topology.
    pub generation: TopologyGeneration,
    /// Width of the desktop's bounding box.
    pub desktop_width: u32,
    /// Height of the desktop's bounding box.
    pub desktop_height: u32,
    /// Planned outputs, in requested-roster order.
    pub outputs: Vec<VirtualOutputPlan>,
}

impl VirtualTopologyPlan {
    /// The primary output.
    #[must_use]
    pub fn primary(&self) -> Option<&VirtualOutputPlan> {
        self.outputs.iter().find(|output| output.primary)
    }

    /// Output indices with the primary first and the rest in roster order:
    /// the order a compositor that treats its first output as primary must
    /// be given.
    #[must_use]
    pub fn primary_first_order(&self) -> Vec<usize> {
        let primary = self
            .outputs
            .iter()
            .position(|output| output.primary)
            .unwrap_or(0);
        std::iter::once(primary)
            .chain((0..self.outputs.len()).filter(|index| *index != primary))
            .collect()
    }
}

/// Why a requested topology cannot be placed on virtual outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VirtualTopologyError {
    /// More monitors than the host serves.
    TooManyOutputs {
        /// Requested.
        requested: usize,
        /// Served.
        max: usize,
    },
    /// An output edge is outside the host's bounds.
    OutputSize {
        /// Client display identity.
        display: String,
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// An odd pixel dimension (encoders need even sizes).
    OddSize {
        /// Client display identity.
        display: String,
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// A refresh rate of zero or above the host's ceiling.
    Refresh {
        /// Client display identity.
        display: String,
        /// Requested refresh.
        refresh_hz: u32,
    },
    /// A rotation the host cannot apply to a virtual output.
    Rotation {
        /// Client display identity.
        display: String,
        /// Requested rotation.
        rotation: Rotation,
    },
    /// The desktop's bounding box exceeds the host's bound.
    DesktopTooLarge {
        /// Bounding width.
        width: u32,
        /// Bounding height.
        height: u32,
    },
    /// The shared placement rejected the layout.
    Placement(TopologyPlacementError),
    /// A shared contract rejected the topology.
    Contract(MediaContractError),
}

impl Display for VirtualTopologyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyOutputs { requested, max } => write!(
                formatter,
                "{requested} monitors requested, this host serves at most {max} virtual outputs"
            ),
            Self::OutputSize {
                display,
                width,
                height,
            } => write!(
                formatter,
                "monitor {display:?} requests {width}x{height}, outside this host's virtual output size"
            ),
            Self::OddSize {
                display,
                width,
                height,
            } => write!(
                formatter,
                "monitor {display:?} requests an odd size {width}x{height}; encoders need even sizes"
            ),
            Self::Refresh {
                display,
                refresh_hz,
            } => write!(
                formatter,
                "monitor {display:?} requests {refresh_hz} Hz, outside this host's virtual output refresh"
            ),
            Self::Rotation { display, rotation } => write!(
                formatter,
                "monitor {display:?} requests rotation {rotation:?}, which this host does not apply to a virtual output"
            ),
            Self::DesktopTooLarge { width, height } => write!(
                formatter,
                "the planned desktop {width}x{height} exceeds this host's bound"
            ),
            Self::Placement(error) => write!(formatter, "placement failed: {error}"),
            Self::Contract(error) => write!(formatter, "invalid topology: {error}"),
        }
    }
}

impl std::error::Error for VirtualTopologyError {}

impl From<TopologyPlacementError> for VirtualTopologyError {
    fn from(error: TopologyPlacementError) -> Self {
        match error {
            TopologyPlacementError::Contract(error) => Self::Contract(error),
            other => Self::Placement(other),
        }
    }
}

impl From<MediaContractError> for VirtualTopologyError {
    fn from(error: MediaContractError) -> Self {
        Self::Contract(error)
    }
}

fn millimetres(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 && value < 100_000.0 {
        // Range-checked above; rounding to whole millimetres is the
        // precision clients report.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = value.round() as u32;
        rounded
    } else {
        0
    }
}

/// Plans `requested` onto one virtual output per monitor, as `generation`.
///
/// Atomic: either every monitor is planned or the whole topology is refused.
///
/// # Errors
///
/// [`VirtualTopologyError`] naming the first refusal.
pub fn plan_virtual_topology(
    requested: &RequestedMonitorTopology,
    generation: TopologyGeneration,
    limits: VirtualOutputLimits,
) -> Result<VirtualTopologyPlan, VirtualTopologyError> {
    let monitors = requested.monitors();
    if monitors.is_empty() || monitors.len() > limits.max_outputs {
        return Err(VirtualTopologyError::TooManyOutputs {
            requested: monitors.len(),
            max: limits.max_outputs,
        });
    }
    for monitor in monitors {
        let display = monitor.client_display_id()?.as_str().to_owned();
        let raw = monitor.monitor();
        let (width, height) = (raw.width_px, raw.height_px);
        if width < limits.min_edge
            || height < limits.min_edge
            || width > limits.max_edge
            || height > limits.max_edge
        {
            return Err(VirtualTopologyError::OutputSize {
                display,
                width,
                height,
            });
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(VirtualTopologyError::OddSize {
                display,
                width,
                height,
            });
        }
        if raw.refresh_hz == 0 || raw.refresh_hz > limits.max_refresh_hz {
            return Err(VirtualTopologyError::Refresh {
                display,
                refresh_hz: raw.refresh_hz,
            });
        }
        if raw.rotation != Rotation::Degrees0 && !limits.rotation {
            return Err(VirtualTopologyError::Rotation {
                display,
                rotation: raw.rotation,
            });
        }
    }
    let primary_index = monitors
        .iter()
        .position(|monitor| monitor.monitor().primary)
        .ok_or(MediaContractError::PrimaryMonitorCount(0))?;
    let placed = arcen_media::place_monitors(
        monitors,
        primary_index,
        VIRTUAL_TRANSFORM_CONVENTION,
        VIRTUAL_ORIGIN_POLICY,
    )?;
    let bounds = placed.bounds();
    if bounds.width > limits.max_desktop_edge || bounds.height > limits.max_desktop_edge {
        return Err(VirtualTopologyError::DesktopTooLarge {
            width: bounds.width,
            height: bounds.height,
        });
    }
    let mut outputs = Vec::with_capacity(monitors.len());
    for (index, (monitor, rect)) in monitors.iter().zip(placed.rects()).enumerate() {
        let raw = monitor.monitor();
        let session_monitor_id = u16::try_from(index + 1)
            .map_err(|_| MediaContractError::CoordinateOverflow)
            .and_then(SessionMonitorId::new)?;
        let physical_size = PhysicalSize::new(raw.width_px, raw.height_px)
            .map_err(TopologyPlacementError::Region)?;
        outputs.push(VirtualOutputPlan {
            session_monitor_id,
            client_display_id: monitor.client_display_id()?.as_str().to_owned(),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            refresh_hz: raw.refresh_hz,
            logical_rect: arcen_media::logical_rect_from_layout(
                monitor.logical_arrangement_rect()?,
            )?,
            physical_size,
            scale: arcen_media::scale120_from_scale(raw.scale)?,
            width_mm: millimetres(raw.width_mm),
            height_mm: millimetres(raw.height_mm),
            rotation: raw.rotation,
            primary: raw.primary,
        });
    }
    Ok(VirtualTopologyPlan {
        generation,
        desktop_width: bounds.width,
        desktop_height: bounds.height,
        outputs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{Monitor, MonitorIdentity, RequestedMonitor};

    const LIMITS: VirtualOutputLimits = VirtualOutputLimits {
        max_outputs: 4,
        min_edge: 320,
        max_edge: 8192,
        max_desktop_edge: 16_384,
        max_refresh_hz: 240,
        rotation: false,
    };

    fn monitor(
        id: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        primary: bool,
    ) -> RequestedMonitor {
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                ..MonitorIdentity::default()
            },
            x,
            y,
            width_px: width,
            height_px: height,
            scale: 1.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
            color: Default::default(),
        };
        RequestedMonitor::new(monitor, width, height).expect("requested monitor")
    }

    fn topology(monitors: Vec<RequestedMonitor>) -> RequestedMonitorTopology {
        RequestedMonitorTopology::new(monitors).expect("topology")
    }

    fn generation() -> TopologyGeneration {
        TopologyGeneration::new(1).expect("generation")
    }

    #[test]
    fn side_by_side_outputs_keep_roster_order_and_sizes() {
        let plan = plan_virtual_topology(
            &topology(vec![
                monitor("a", 0, 0, 1920, 1080, true),
                monitor("b", 1920, 0, 1280, 1024, false),
            ]),
            generation(),
            LIMITS,
        )
        .expect("plan");
        assert_eq!((plan.desktop_width, plan.desktop_height), (3200, 1080));
        let rects: Vec<_> = plan
            .outputs
            .iter()
            .map(|output| (output.x, output.y, output.width, output.height))
            .collect();
        assert_eq!(rects, vec![(0, 0, 1920, 1080), (1920, 0, 1280, 1024)]);
        assert_eq!(plan.outputs[1].session_monitor_id.get(), 2);
        assert_eq!(plan.primary_first_order(), vec![0, 1]);
    }

    #[test]
    fn a_monitor_left_of_the_primary_translates_the_desktop_to_the_origin() {
        let plan = plan_virtual_topology(
            &topology(vec![
                monitor("main", 0, 0, 1920, 1080, true),
                monitor("left", -1280, 0, 1280, 800, false),
            ]),
            generation(),
            LIMITS,
        )
        .expect("plan");
        assert_eq!((plan.outputs[0].x, plan.outputs[0].y), (1280, 0));
        assert_eq!((plan.outputs[1].x, plan.outputs[1].y), (0, 0));
        assert_eq!(plan.primary_first_order(), vec![0, 1]);
    }

    #[test]
    fn a_stacked_layout_places_the_second_output_below() {
        let plan = plan_virtual_topology(
            &topology(vec![
                monitor("top", 0, -1080, 1920, 1080, false),
                monitor("bottom", 0, 0, 1920, 1080, true),
            ]),
            generation(),
            LIMITS,
        )
        .expect("plan");
        assert_eq!((plan.outputs[0].x, plan.outputs[0].y), (0, 0));
        assert_eq!((plan.outputs[1].x, plan.outputs[1].y), (0, 1080));
        assert_eq!(plan.primary_first_order(), vec![1, 0]);
        assert_eq!(plan.primary().map(|output| output.y), Some(1080));
    }

    #[test]
    fn four_outputs_are_served_and_five_never_reach_the_planner() {
        let plan = plan_virtual_topology(
            &topology(vec![
                monitor("1", 0, 0, 1920, 1080, true),
                monitor("2", 1920, 0, 1920, 1080, false),
                monitor("3", 0, 1080, 1920, 1080, false),
                monitor("4", 1920, 1080, 1920, 1080, false),
            ]),
            generation(),
            LIMITS,
        )
        .expect("plan");
        assert_eq!((plan.desktop_width, plan.desktop_height), (3840, 2160));
        let two = VirtualOutputLimits {
            max_outputs: 2,
            ..LIMITS
        };
        let refused = plan_virtual_topology(
            &topology(vec![
                monitor("1", 0, 0, 1920, 1080, true),
                monitor("2", 1920, 0, 1920, 1080, false),
                monitor("3", 3840, 0, 1920, 1080, false),
            ]),
            generation(),
            two,
        );
        assert!(matches!(
            refused,
            Err(VirtualTopologyError::TooManyOutputs {
                requested: 3,
                max: 2
            })
        ));
    }

    #[test]
    fn odd_small_and_fast_outputs_are_refused_whole() {
        let odd = plan_virtual_topology(
            &topology(vec![
                monitor("a", 0, 0, 1920, 1080, true),
                monitor("b", 1920, 0, 1281, 1024, false),
            ]),
            generation(),
            LIMITS,
        );
        assert!(matches!(odd, Err(VirtualTopologyError::OddSize { .. })));
        let small = plan_virtual_topology(
            &topology(vec![monitor("a", 0, 0, 200, 200, true)]),
            generation(),
            LIMITS,
        );
        assert!(matches!(
            small,
            Err(VirtualTopologyError::OutputSize { .. })
        ));
        let mut fast = monitor("a", 0, 0, 1920, 1080, true);
        fast.monitor.refresh_hz = 480;
        let fast = plan_virtual_topology(&topology(vec![fast]), generation(), LIMITS);
        assert!(matches!(fast, Err(VirtualTopologyError::Refresh { .. })));
    }

    #[test]
    fn rotation_is_refused_unless_the_host_rotates_virtual_outputs() {
        let mut rotated = monitor("a", 0, 0, 1080, 1920, true);
        rotated.monitor.rotation = Rotation::Degrees90;
        let refused = plan_virtual_topology(&topology(vec![rotated.clone()]), generation(), LIMITS);
        assert!(matches!(
            refused,
            Err(VirtualTopologyError::Rotation { .. })
        ));
        let allowed = VirtualOutputLimits {
            rotation: true,
            ..LIMITS
        };
        let plan = plan_virtual_topology(&topology(vec![rotated]), generation(), allowed)
            .expect("rotation allowed");
        assert_eq!(
            (plan.outputs[0].width, plan.outputs[0].height),
            (1080, 1920)
        );
    }

    #[test]
    fn refresh_scale_and_physical_size_are_kept() {
        let mut hidpi = monitor("retina", 0, 0, 3024, 1964, true);
        hidpi.monitor.scale = 2.0;
        hidpi.monitor.refresh_hz = 120;
        hidpi.monitor.width_mm = 302.4;
        hidpi.monitor.height_mm = 196.4;
        hidpi.logical_width = 1512;
        hidpi.logical_height = 982;
        let plan =
            plan_virtual_topology(&topology(vec![hidpi]), generation(), LIMITS).expect("plan");
        let output = &plan.outputs[0];
        assert_eq!(output.refresh_hz, 120);
        assert_eq!(output.scale.get(), 240);
        assert_eq!((output.width_mm, output.height_mm), (302, 196));
        assert_eq!((output.width, output.height), (3024, 1964));
    }

    #[test]
    fn the_desktop_bound_is_enforced() {
        let tight = VirtualOutputLimits {
            max_desktop_edge: 3000,
            ..LIMITS
        };
        let refused = plan_virtual_topology(
            &topology(vec![
                monitor("a", 0, 0, 1920, 1080, true),
                monitor("b", 1920, 0, 1920, 1080, false),
            ]),
            generation(),
            tight,
        );
        assert!(matches!(
            refused,
            Err(VirtualTopologyError::DesktopTooLarge { width: 3840, .. })
        ));
    }
}
