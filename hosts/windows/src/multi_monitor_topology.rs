//! Pure Windows physical multi-monitor topology planner.
//!
//! Maps a validated [`RequestedMonitorTopology`] onto this host's *physical*
//! Windows outputs: exactly one applied output rectangle per requested
//! monitor, drawn from an operator/probe-supplied [`PhysicalOutputInventory`].
//! This module is pure planning logic only — no CCD/`SetDisplayConfig`/NVAPI
//! I/O happens here, and it never mutates the live display. Real output
//! enumeration lives in `crate::gpu_probe`/`crate::display`; production
//! callers translate that enumeration into an [`AvailableOutput`] roster and
//! feed it in here.
//!
//! # Stable identity
//!
//! Every output this planner reasons about is identified by its
//! [`AdapterLuid`] (the adapter that owns it) plus its CCD/`DISPLAYCONFIG`
//! target id (`target_id`) — stable for the lifetime of one Windows session
//! as long as the monitor stays connected. [`Self::adapter_output_index`]
//! (the adapter-local `EnumOutputs`/DXGI ordinal), [`Self::global_index`]
//! (the whole-desktop DXGI enumeration ordinal — matches `display::
//! ResolvedOutput::global_index`), and `adapter_name` are all *not* used as
//! identity here: `adapter_output_index`/`global_index` can be reassigned by
//! the driver whenever the attached output set changes anywhere on the host
//! — exactly the instability `display::OutputSelector::GlobalIndex` already
//! documents for this host's existing single-output path — and `adapter_name`
//! alone is ambiguous whenever two adapters share a model string (identical
//! GPUs). No rejection/assignment decision in this module ever keys on any of
//! them.
//!
//! `global_index`/`adapter_output_index`/`adapter_name`/`device_name` are
//! still carried on [`AvailableOutput`]/[`WindowsMonitorPlan`] as the
//! *current-at-probe-time* concrete selector `crate::capenc::CapencConfig`
//! needs (it cannot select by LUID directly), but they are snapshots, not
//! identity: [`resolve_capture_selector`] must be called against a **freshly
//! re-probed** [`PhysicalOutputInventory`] immediately before every capture
//! start or restart to obtain a current [`CaptureSelector`], never reused
//! across an enumeration boundary.
//!
//! # Multi-GPU
//!
//! Outputs across different adapters (different [`AdapterLuid`]s) are
//! ordinary inventory entries; target ids are only unique *within* one
//! adapter, so [`PhysicalOutputInventory`] validates uniqueness on the full
//! `(adapter_luid, target_id)` pair, never on `target_id` alone. Assignment
//! itself never assumes a single adapter or a contiguous target id range.
//!
//! # Edge-aware mixed-scale placement
//!
//! Where each monitor lands on the virtual desktop is not this module's own
//! math: it is the shared [`arcen_media::plan_edge_aware_offsets`] primitive,
//! the same one the Linux planner uses. Starting from the primary, a monitor
//! that logically touches an already-placed neighbor (a shared full edge with
//! perpendicular-axis overlap) is placed flush against that neighbor's own
//! already-computed host-pixel footprint, using the *neighbor's* scale for the
//! shared edge's cross-axis offset. A chain of differently scaled monitors
//! therefore stays gap-free and overlap-free no matter how many hops separate
//! it from the primary.
//!
//! This replaces the earlier global-primary-scale placement, which converted
//! every monitor's *absolute* logical offset from the primary with a single
//! scale derived from the primary alone. That stayed exact only while every
//! monitor shared the primary's scale; from the second hop onwards a
//! mixed-scale chain accumulated either a gap (a high-scale primary
//! over-converting a lower-scale neighbor's offset) or an overlap (the
//! inverse), and a differently scaled monitor placed directly left of or above
//! the primary landed on top of it.
//!
//! A monitor with no touching path back to the primary — a genuine gap in the
//! client's logical layout, or a deliberately disconnected cluster — still
//! falls back to converting its absolute logical offset at the primary's own
//! scale, so intentionally separated layouts are preserved rather than
//! collapsed together.
//!
//! # Signed Windows virtual-desktop coordinates
//!
//! Unlike an Xorg/RandR screen (which must start at a non-negative origin),
//! the Windows virtual desktop is natively signed and the OS itself always
//! anchors the *primary* display's own origin at `(0, 0)` — `GetSystemMetrics
//! (SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN)` can be negative for a monitor above
//! or left of the primary, but the primary's own desktop rectangle is never
//! moved off `(0, 0)`. [`plan_topology`] never translates a computed layout
//! to a non-negative origin (that would drag the primary away from `(0, 0)`
//! whenever any monitor sits above/left of it) and instead requires the
//! client's declared primary to already be at logical `(0, 0)`, rejecting the
//! request otherwise (see [`WindowsTopologyError::PrimaryNotAtLogicalOrigin`]).
//! [`WindowsTopologyPlan::desktop_x`]/[`WindowsTopologyPlan::desktop_y`] may
//! therefore be negative.

use arcen_media::{
    AppliedMonitor, AppliedMonitorTopology, AppliedPoint, AppliedRect, AppliedRegionSet,
    AppliedSize, LayoutBounds, LayoutRect, LogicalRect, MediaContractError, OriginPolicy,
    OutputIdentity, PhysicalSize, RegionContractError, RegionGeneration, RegionId, RegionPlacement,
    RegionSet, RequestedMonitor, RequestedMonitorTopology, Rotation, Scale120, SessionMonitorId,
    TopologyGeneration, TopologyPlacementError, TransformConvention,
};

use crate::nvapi::AdapterLuid;

/// Explicit shared transform convention for this planner: physical outputs are
/// driven at their native pre-rotation mode and Windows applies the rotation,
/// so region descriptors carry the native stream extent plus a separate output
/// transform.
const TRANSFORM_CONVENTION: TransformConvention = TransformConvention::NativeNeedsTransform;
/// Explicit shared origin policy for this planner: the Windows virtual desktop
/// is natively signed and the OS anchors the primary's own origin at `(0, 0)`,
/// so a computed layout is never translated to a non-negative origin.
const ORIGIN_POLICY: OriginPolicy = OriginPolicy::PreserveSigned;

/// Matches the shared encoder dimension ceiling
/// (`shared/media/src/video/plan.rs`'s `BackendLimits::max_width/max_height`
/// of 8192), so a planned desktop never exceeds what this host can encode.
pub const MAX_VIRTUAL_DESKTOP_DIMENSION_PX: u32 = 8_192;

/// One exact mode a physical output can be driven at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// How a physical output's drivable modes are bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputModeCapability {
    /// A GPU/driver path (matching this host's existing NVAPI custom-timing
    /// path in `nvapi.rs`/`display.rs`'s `DisplayPolicy::ExactIsolated`) that
    /// can synthesize any mode inside these inclusive bounds.
    CustomTimingCapable {
        min_width: u32,
        max_width: u32,
        min_height: u32,
        max_height: u32,
        min_refresh_hz: u32,
        max_refresh_hz: u32,
    },
    /// A fixed enumerated mode list (matching this host's existing
    /// `DisplayPolicy::Negotiated`-family non-NVIDIA path): this output can
    /// only be driven at exactly one of these modes.
    FixedModes(Vec<OutputMode>),
}

impl OutputModeCapability {
    /// Whether driving this output at `mode` needs a *synthesized* timing
    /// (this host's NVAPI custom-timing path) rather than a mode the display
    /// driver already enumerates.
    ///
    /// Only [`Self::CustomTimingCapable`] outputs are ever driven at a mode
    /// the driver does not enumerate, so they always keep this host's
    /// existing NVAPI path. A [`Self::FixedModes`] output is by construction
    /// only ever planned at one of its own enumerated modes, which the
    /// vendor-neutral CCD apply can set on its own.
    #[must_use]
    pub const fn requires_custom_timing(&self) -> bool {
        matches!(self, Self::CustomTimingCapable { .. })
    }

    fn supports(&self, mode: OutputMode) -> bool {
        match self {
            Self::CustomTimingCapable {
                min_width,
                max_width,
                min_height,
                max_height,
                min_refresh_hz,
                max_refresh_hz,
            } => {
                mode.width >= *min_width
                    && mode.width <= *max_width
                    && mode.height >= *min_height
                    && mode.height <= *max_height
                    && mode.refresh_hz >= *min_refresh_hz
                    && mode.refresh_hz <= *max_refresh_hz
            }
            Self::FixedModes(modes) => modes.contains(&mode),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::FixedModes(modes) if modes.is_empty())
    }
}

/// One physical Windows output available to plan against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableOutput {
    /// Stable identity component: the adapter that owns this output.
    pub adapter_luid: AdapterLuid,
    /// Stable identity component: this output's CCD/`DISPLAYCONFIG` target id
    /// on `adapter_luid`. Unique only within one adapter.
    pub target_id: u32,
    /// Adapter-local `EnumOutputs`/DXGI ordinal, as of this inventory's probe
    /// time. Current-selector use only — never stable identity, and never
    /// used as a re-resolution key (see module documentation).
    pub adapter_output_index: u32,
    /// Exact, case-insensitive DXGI adapter description, matching
    /// `config::DesktopConfig::adapter`. Diagnostics use only: two adapters
    /// of the same model share this string, so it is never sufficient to
    /// disambiguate an output on its own.
    pub adapter_name: String,
    /// Whole-desktop DXGI enumeration ordinal, as of this inventory's probe
    /// time — matches `display::ResolvedOutput::global_index`, the actual
    /// value `crate::capenc::CapencConfig::output_index` needs. Current
    /// selector use only, same re-resolution requirement as
    /// `adapter_output_index`.
    pub global_index: u32,
    /// Current DXGI device name, matching `display::ResolvedOutput::
    /// device_name`, as of this inventory's probe time. Current-selector use
    /// only.
    pub device_name: String,
    pub mode_capability: OutputModeCapability,
    /// Non-empty set of rotations this output's assigned pipe/CRTC can apply.
    pub supported_rotations: Vec<Rotation>,
    /// Current attached desktop rectangle and refresh, captured by the same
    /// interactive-session probe that supplied the current DXGI selectors.
    pub current_x: i32,
    pub current_y: i32,
    pub current_width: u32,
    pub current_height: u32,
    pub current_refresh_hz: u32,
    pub primary: bool,
}

/// Typed rejection building/using a [`PhysicalOutputInventory`] or planning a
/// topology onto one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsTopologyError {
    /// No physical outputs were supplied to plan against.
    EmptyInventory,
    /// An inventory entry declared no drivable modes.
    OutputHasNoModes {
        adapter_luid: AdapterLuid,
        target_id: u32,
    },
    /// An inventory entry declared no supported rotations.
    OutputHasNoRotations {
        adapter_luid: AdapterLuid,
        target_id: u32,
    },
    /// Two inventory entries share the same `(adapter_luid, target_id)`.
    DuplicateOutputInInventory {
        adapter_luid: AdapterLuid,
        target_id: u32,
    },
    /// The requested monitor count exceeds the available physical output
    /// count.
    InsufficientOutputs { requested: usize, available: usize },
    /// The client's declared primary monitor is not at logical `(0, 0)`.
    /// Windows always anchors the primary display's own desktop origin at
    /// `(0, 0)` (see the module documentation's "Signed Windows
    /// virtual-desktop coordinates" section); this host requires the
    /// client's request to already agree, rather than silently
    /// re-centering it.
    PrimaryNotAtLogicalOrigin { x: i32, y: i32 },
    /// A monitor's requested exact mode matches no supported mode on its
    /// assigned output.
    NoMatchingMode {
        client_display_id: String,
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    /// A monitor's requested rotation exceeds its assigned output's
    /// capability.
    UnsupportedRotation {
        client_display_id: String,
        rotation: Rotation,
    },
    /// The planned bounding desktop exceeds this tranche's encode ceiling.
    DesktopTooLarge { width: u32, height: u32 },
    /// The requested/applied topology violated a shared media contract
    /// invariant.
    InvalidTopology(MediaContractError),
    /// A requested presentation scale could not be represented in the shared
    /// 1/120 scale domain.
    RegionScaleOutOfRange(String),
    /// The committed Windows topology could not be represented by the shared
    /// region aggregate.
    InvalidRegion(RegionContractError),
    /// The shared topology placement primitives rejected this layout.
    Placement(TopologyPlacementError),
}

impl From<TopologyPlacementError> for WindowsTopologyError {
    /// Preserves this planner's existing typed rejections: shared contract and
    /// region failures keep mapping to [`Self::InvalidTopology`] and
    /// [`Self::InvalidRegion`] exactly as they did before the shared
    /// primitives were adopted.
    fn from(value: TopologyPlacementError) -> Self {
        match value {
            TopologyPlacementError::Contract(error) => Self::InvalidTopology(error),
            TopologyPlacementError::Region(error) => Self::InvalidRegion(error),
            other => Self::Placement(other),
        }
    }
}

impl std::fmt::Display for WindowsTopologyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyInventory => {
                formatter.write_str("no physical outputs are available to plan against")
            }
            Self::OutputHasNoModes {
                adapter_luid,
                target_id,
            } => write!(
                formatter,
                "output target {target_id} on adapter {adapter_luid:?} declares no supported modes"
            ),
            Self::OutputHasNoRotations {
                adapter_luid,
                target_id,
            } => write!(
                formatter,
                "output target {target_id} on adapter {adapter_luid:?} declares no supported rotations"
            ),
            Self::DuplicateOutputInInventory {
                adapter_luid,
                target_id,
            } => write!(
                formatter,
                "duplicate output target {target_id} on adapter {adapter_luid:?} in inventory"
            ),
            Self::InsufficientOutputs {
                requested,
                available,
            } => write!(
                formatter,
                "requested {requested} monitors but only {available} physical outputs are available"
            ),
            Self::PrimaryNotAtLogicalOrigin { x, y } => write!(
                formatter,
                "declared primary monitor is at logical ({x}, {y}), not (0, 0), which this host requires"
            ),
            Self::NoMatchingMode {
                client_display_id,
                width,
                height,
                refresh_hz,
            } => write!(
                formatter,
                "monitor {client_display_id:?} requests {width}x{height}@{refresh_hz}hz, which matches no supported mode on its assigned output"
            ),
            Self::UnsupportedRotation {
                client_display_id,
                rotation,
            } => write!(
                formatter,
                "monitor {client_display_id:?} requests rotation {rotation:?}, which its assigned output does not support"
            ),
            Self::DesktopTooLarge { width, height } => write!(
                formatter,
                "planned desktop {width}x{height} exceeds the maximum {MAX_VIRTUAL_DESKTOP_DIMENSION_PX}x{MAX_VIRTUAL_DESKTOP_DIMENSION_PX}"
            ),
            Self::InvalidTopology(error) => {
                write!(formatter, "requested/applied topology is invalid: {error}")
            }
            Self::RegionScaleOutOfRange(id) => {
                write!(
                    formatter,
                    "monitor {id:?} scale cannot be represented in shared Scale120 units"
                )
            }
            Self::InvalidRegion(error) => {
                write!(
                    formatter,
                    "shared region contract rejected the Windows topology: {error}"
                )
            }
            Self::Placement(error) => {
                write!(
                    formatter,
                    "shared topology placement rejected the Windows layout: {error}"
                )
            }
        }
    }
}

impl std::error::Error for WindowsTopologyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidTopology(error) => Some(error),
            Self::InvalidRegion(error) => Some(error),
            Self::Placement(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MediaContractError> for WindowsTopologyError {
    fn from(error: MediaContractError) -> Self {
        Self::InvalidTopology(error)
    }
}

impl From<RegionContractError> for WindowsTopologyError {
    fn from(error: RegionContractError) -> Self {
        Self::InvalidRegion(error)
    }
}

/// Validated, ordered inventory of physical Windows outputs available to
/// plan against. Order is significant: outputs are assigned to requested
/// monitors in inventory order (primary first), so callers list preferred
/// outputs first (e.g. the operator-configured/probed adapter order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalOutputInventory {
    outputs: Vec<AvailableOutput>,
}

impl PhysicalOutputInventory {
    /// Validates a physical output roster.
    ///
    /// # Errors
    ///
    /// Returns an error when the roster is empty, an entry declares no
    /// modes/rotations, or two entries share the same
    /// `(adapter_luid, target_id)`.
    pub fn new(outputs: Vec<AvailableOutput>) -> Result<Self, WindowsTopologyError> {
        if outputs.is_empty() {
            return Err(WindowsTopologyError::EmptyInventory);
        }
        let mut seen = std::collections::BTreeSet::new();
        for output in &outputs {
            if output.mode_capability.is_empty() {
                return Err(WindowsTopologyError::OutputHasNoModes {
                    adapter_luid: output.adapter_luid,
                    target_id: output.target_id,
                });
            }
            if output.supported_rotations.is_empty() {
                return Err(WindowsTopologyError::OutputHasNoRotations {
                    adapter_luid: output.adapter_luid,
                    target_id: output.target_id,
                });
            }
            let key = (
                output.adapter_luid.low_part,
                output.adapter_luid.high_part,
                output.target_id,
            );
            if !seen.insert(key) {
                return Err(WindowsTopologyError::DuplicateOutputInInventory {
                    adapter_luid: output.adapter_luid,
                    target_id: output.target_id,
                });
            }
        }
        Ok(Self { outputs })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    #[must_use]
    pub fn outputs(&self) -> &[AvailableOutput] {
        &self.outputs
    }

    /// Resolves a stable `(adapter_luid, target_id)` binding against this
    /// inventory's current entries.
    ///
    /// Callers performing an actual capture start/restart must pass a
    /// **freshly re-probed** inventory here — never the one a topology plan
    /// was originally built against — since [`AvailableOutput::global_index`]
    /// /`adapter_output_index`/`device_name` are only valid as of one probe.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureSelectorError::MissingBinding`] when no entry matches,
    /// or [`CaptureSelectorError::AmbiguousBinding`] when more than one does
    /// (a defense-in-depth guard; a validated inventory's `(adapter_luid,
    /// target_id)` uniqueness already prevents this in practice).
    pub fn resolve(
        &self,
        adapter_luid: AdapterLuid,
        target_id: u32,
    ) -> Result<CaptureSelector, CaptureSelectorError> {
        resolve_capture_selector(&self.outputs, adapter_luid, target_id)
    }
}

/// A validated, freshly re-resolved mapping from one stable
/// `(adapter_luid, target_id)` binding to today's concrete capenc selectors.
///
/// Because `crate::capenc::CapencConfig` cannot select an output by LUID
/// directly, this must be re-derived from a **freshly re-probed**
/// [`PhysicalOutputInventory`] immediately before every capture start or
/// restart attempt — never cached across an enumeration boundary — via
/// [`resolve_capture_selector`]/[`PhysicalOutputInventory::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSelector {
    pub adapter_luid: AdapterLuid,
    pub target_id: u32,
    /// Current whole-desktop DXGI enumeration ordinal — the actual value
    /// `CapencConfig::output_index` must be set to.
    pub global_index: u32,
    pub adapter_name: String,
    pub adapter_output_index: u32,
    pub device_name: String,
}

/// Typed rejection resolving a stable `(adapter_luid, target_id)` binding
/// against a freshly probed inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSelectorError {
    /// No output in the fresh inventory matches this binding — e.g. the
    /// monitor was unplugged, or its owning adapter is gone. Fail closed:
    /// never fall back to a stale or positional selector.
    MissingBinding {
        adapter_luid: AdapterLuid,
        target_id: u32,
    },
    /// More than one output in the fresh inventory matches this binding.
    /// `(adapter_luid, target_id)` is unique by construction in any
    /// [`PhysicalOutputInventory`] built via [`PhysicalOutputInventory::new`],
    /// so this is a defense-in-depth guard, not a case normal production
    /// input can reach — and it is never resolved by falling back to
    /// `adapter_name`, which identical-model adapters can share.
    AmbiguousBinding {
        adapter_luid: AdapterLuid,
        target_id: u32,
        matches: usize,
    },
}

impl std::fmt::Display for CaptureSelectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBinding {
                adapter_luid,
                target_id,
            } => write!(
                formatter,
                "output target {target_id} on adapter {adapter_luid:?} is no longer present in the fresh output inventory"
            ),
            Self::AmbiguousBinding {
                adapter_luid,
                target_id,
                matches,
            } => write!(
                formatter,
                "output target {target_id} on adapter {adapter_luid:?} matched {matches} entries in the fresh output inventory"
            ),
        }
    }
}

impl std::error::Error for CaptureSelectorError {}

/// Resolves a stable `(adapter_luid, target_id)` binding against `outputs` —
/// a fresh probe result, not necessarily the inventory a plan was originally
/// built against. Matches strictly on the stable identity pair; `adapter_name`
/// alone is never sufficient (identical-model adapters can share it) and is
/// never consulted here.
///
/// # Errors
///
/// Returns [`CaptureSelectorError::MissingBinding`] when no entry matches, or
/// [`CaptureSelectorError::AmbiguousBinding`] when more than one does.
pub fn resolve_capture_selector(
    outputs: &[AvailableOutput],
    adapter_luid: AdapterLuid,
    target_id: u32,
) -> Result<CaptureSelector, CaptureSelectorError> {
    let mut matches = outputs.iter().filter(|candidate| {
        candidate.adapter_luid == adapter_luid && candidate.target_id == target_id
    });
    let Some(first) = matches.next() else {
        return Err(CaptureSelectorError::MissingBinding {
            adapter_luid,
            target_id,
        });
    };
    let extra = matches.count();
    if extra > 0 {
        return Err(CaptureSelectorError::AmbiguousBinding {
            adapter_luid,
            target_id,
            matches: extra + 1,
        });
    }
    Ok(CaptureSelector {
        adapter_luid,
        target_id,
        global_index: first.global_index,
        adapter_name: first.adapter_name.clone(),
        adapter_output_index: first.adapter_output_index,
        device_name: first.device_name.clone(),
    })
}

/// One applied physical output binding, in host virtual-desktop pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsMonitorPlan {
    pub session_monitor_id: SessionMonitorId,
    pub client_display_id: String,
    /// Stable identity: see the module documentation.
    pub adapter_luid: AdapterLuid,
    pub target_id: u32,
    /// Current-selector snapshot as of planning time only; never stable
    /// identity, and never used directly for a capture start/restart without
    /// first re-resolving via [`resolve_capture_selector`].
    pub adapter_output_index: u32,
    pub adapter_name: String,
    /// Current whole-desktop DXGI enumeration ordinal, as of planning time
    /// only — see [`AvailableOutput::global_index`].
    pub global_index: u32,
    /// Current DXGI device name, as of planning time only — see
    /// [`AvailableOutput::device_name`].
    pub device_name: String,
    /// Host virtual-desktop horizontal origin. Signed: may be negative for a
    /// monitor placed above/left of the primary (see the module
    /// documentation's "Signed Windows virtual-desktop coordinates"
    /// section). Always exactly `0` for the primary monitor.
    pub x: i32,
    /// Host virtual-desktop vertical origin. Signed; same primary-at-`0`
    /// guarantee as `x`.
    pub y: i32,
    /// Rotation-aware on-desktop footprint (swapped from `mode_width`/
    /// `mode_height` at 90/270 degrees).
    pub width: u32,
    pub height: u32,
    /// Exact native (pre-rotation) mode this output must be set to.
    pub mode_width: u32,
    pub mode_height: u32,
    /// Requested logical desktop rectangle retained in shared fixed-point
    /// units so region input never re-derives it from applied pixels.
    pub logical_rect: LogicalRect,
    /// Requested presentation scale in the shared 1/120 representation.
    pub scale: Scale120,
    pub refresh_hz: u32,
    pub rotation: Rotation,
    pub primary: bool,
}

/// Complete Windows physical topology plan for one committed
/// [`TopologyGeneration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsTopologyPlan {
    pub generation: TopologyGeneration,
    /// Bounding virtual-desktop horizontal origin. Signed: negative whenever
    /// any monitor sits left of the primary. Never translated to `0`/
    /// non-negative (see the module documentation).
    pub desktop_x: i32,
    /// Bounding virtual-desktop vertical origin. Signed; same convention as
    /// `desktop_x`.
    pub desktop_y: i32,
    /// Bounding virtual-desktop width every applied monitor fits inside.
    pub desktop_width: u32,
    /// Bounding virtual-desktop height every applied monitor fits inside.
    pub desktop_height: u32,
    /// Applied monitors, in requested-roster order (not output-assignment
    /// order).
    pub monitors: Vec<WindowsMonitorPlan>,
    /// Whether applying this plan needs the NVAPI synthesized-timing path.
    ///
    /// True when any assigned output is [`OutputModeCapability::CustomTimingCapable`],
    /// i.e. it was planned at a mode it does not itself enumerate. Outputs
    /// that only enumerate fixed modes are by construction planned at one of
    /// their own enumerated modes, which the vendor-neutral CCD apply sets
    /// without NVAPI — the only path available on a host with no NVIDIA
    /// driver at all.
    pub requires_custom_timing: bool,
}

impl WindowsTopologyPlan {
    #[must_use]
    pub fn primary(&self) -> &WindowsMonitorPlan {
        self.monitors
            .iter()
            .find(|monitor| monitor.primary)
            .unwrap_or(&self.monitors[0])
    }

    /// Builds the shared requested and applied region aggregates represented
    /// by this committed Windows topology, through the shared
    /// [`arcen_media::build_region_sets`] constructor under this planner's
    /// explicit [`TransformConvention::NativeNeedsTransform`] convention.
    ///
    /// # Errors
    ///
    /// Returns a shared region-contract error when a malformed recovery or
    /// provider plan has inconsistent identities, geometry, or transformed
    /// extents.
    pub fn region_sets(&self) -> Result<(RegionSet, AppliedRegionSet), WindowsTopologyError> {
        let generation = RegionGeneration::new(self.generation.get())?;
        let placements = self
            .monitors
            .iter()
            .map(WindowsMonitorPlan::region_placement)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(arcen_media::build_region_sets(
            generation,
            TRANSFORM_CONVENTION,
            &placements,
        )?)
    }
}

impl WindowsMonitorPlan {
    fn region_placement(&self) -> Result<RegionPlacement, WindowsTopologyError> {
        Ok(RegionPlacement {
            region_id: RegionId::new(u32::from(self.session_monitor_id.get()))?,
            output: OutputIdentity::new(format!(
                "luid:{}:{}:target:{}",
                self.adapter_luid.high_part, self.adapter_luid.low_part, self.target_id
            ))?,
            logical_rect: self.logical_rect,
            stream_size: PhysicalSize::new(self.mode_width, self.mode_height)?,
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

fn region_logical_rect(monitor: &RequestedMonitor) -> Result<LogicalRect, WindowsTopologyError> {
    Ok(arcen_media::logical_rect_from_layout(
        monitor.logical_arrangement_rect()?,
    )?)
}

fn region_scale(client_display_id: &str, scale: f32) -> Result<Scale120, WindowsTopologyError> {
    arcen_media::scale120_from_scale(scale).map_err(|error| match error {
        TopologyPlacementError::ScaleOutOfRange => {
            WindowsTopologyError::RegionScaleOutOfRange(client_display_id.to_owned())
        }
        other => WindowsTopologyError::from(other),
    })
}

/// Returns the checked aggregate desktop bounds of already-placed host-pixel
/// footprints under this planner's explicit
/// [`OriginPolicy::PreserveSigned`] policy, which never translates a layout
/// (so the primary stays anchored at `(0, 0)` and monitors above/left of it
/// keep their negative coordinates).
fn signed_desktop_bounds(rects: Vec<LayoutRect>) -> Result<LayoutBounds, WindowsTopologyError> {
    Ok(arcen_media::apply_origin_policy(rects, ORIGIN_POLICY)?.bounds())
}

/// Derives every requested monitor's host-pixel desktop origin from the
/// client's logical arrangement, anchored at the primary, through the shared
/// edge-aware placement primitive (see the module documentation's "Edge-aware
/// mixed-scale placement" section).
///
/// # Errors
///
/// Returns an error when `primary_index` is out of range, a requested
/// monitor's logical rectangle is invalid, or a coordinate conversion
/// overflows the signed Windows desktop domain.
fn plan_monitor_offsets(
    monitors: &[RequestedMonitor],
    primary_index: usize,
) -> Result<Vec<(i32, i32)>, WindowsTopologyError> {
    Ok(arcen_media::plan_edge_aware_offsets(
        monitors,
        primary_index,
        TRANSFORM_CONVENTION,
    )?)
}

fn client_display_id_of(applied: &AppliedMonitor) -> Result<String, WindowsTopologyError> {
    Ok(applied
        .client_display_id()
        .map_err(WindowsTopologyError::InvalidTopology)?
        .as_str()
        .to_owned())
}

/// Builds a no-mutation physical-output plan from the desktop Windows has
/// already applied. Every selected output is captured at its current
/// rectangle/mode while the requested client-display roster is preserved.
pub fn plan_current_topology(
    requested: &RequestedMonitorTopology,
    generation: TopologyGeneration,
    inventory: &PhysicalOutputInventory,
) -> Result<WindowsTopologyPlan, WindowsTopologyError> {
    let requested_monitors = requested.monitors();
    if requested_monitors.len() > inventory.len() {
        return Err(WindowsTopologyError::InsufficientOutputs {
            requested: requested_monitors.len(),
            available: inventory.len(),
        });
    }

    let primary_index = requested_monitors
        .iter()
        .position(|monitor| monitor.monitor().primary)
        .unwrap_or(0);
    let mut assignment_order = Vec::with_capacity(requested_monitors.len());
    assignment_order.push(primary_index);
    for index in 0..requested_monitors.len() {
        if index != primary_index {
            assignment_order.push(index);
        }
    }

    let mut plans: Vec<Option<WindowsMonitorPlan>> = vec![None; requested_monitors.len()];
    let mut rects = Vec::with_capacity(requested_monitors.len());
    let mut requires_custom_timing = false;
    for (output_slot, monitor_index) in assignment_order.into_iter().enumerate() {
        let monitor = requested_monitors[monitor_index].monitor();
        let output = &inventory.outputs()[output_slot];
        if !output.supported_rotations.contains(&monitor.rotation) {
            return Err(WindowsTopologyError::UnsupportedRotation {
                client_display_id: monitor.identity.id.clone(),
                rotation: monitor.rotation,
            });
        }
        let raw_id =
            u16::try_from(monitor_index + 1).map_err(|_| MediaContractError::CoordinateOverflow)?;
        let session_monitor_id =
            SessionMonitorId::new(raw_id).map_err(WindowsTopologyError::from)?;
        let rect = LayoutRect::new(
            output.current_x,
            output.current_y,
            output.current_width,
            output.current_height,
        )?;
        let (mode_width, mode_height) = match monitor.rotation {
            Rotation::Degrees0 | Rotation::Degrees180 => (rect.width, rect.height),
            Rotation::Degrees90 | Rotation::Degrees270 => (rect.height, rect.width),
        };
        rects.push(rect);
        requires_custom_timing |= output.mode_capability.requires_custom_timing();
        plans[monitor_index] = Some(WindowsMonitorPlan {
            session_monitor_id,
            client_display_id: monitor.identity.id.clone(),
            adapter_luid: output.adapter_luid,
            target_id: output.target_id,
            adapter_output_index: output.adapter_output_index,
            adapter_name: output.adapter_name.clone(),
            global_index: output.global_index,
            device_name: output.device_name.clone(),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            mode_width,
            mode_height,
            logical_rect: region_logical_rect(&requested_monitors[monitor_index])?,
            scale: region_scale(&monitor.identity.id, monitor.scale)?,
            refresh_hz: output.current_refresh_hz.max(1),
            rotation: monitor.rotation,
            primary: monitor.primary,
        });
    }
    let bounds = signed_desktop_bounds(rects)?;
    if bounds.width > MAX_VIRTUAL_DESKTOP_DIMENSION_PX
        || bounds.height > MAX_VIRTUAL_DESKTOP_DIMENSION_PX
    {
        return Err(WindowsTopologyError::DesktopTooLarge {
            width: bounds.width,
            height: bounds.height,
        });
    }
    Ok(WindowsTopologyPlan {
        generation,
        desktop_x: bounds.x,
        desktop_y: bounds.y,
        desktop_width: bounds.width,
        desktop_height: bounds.height,
        monitors: plans
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .expect("every requested monitor receives one current output"),
        requires_custom_timing,
    })
}

/// Plans a validated 1..=4 monitor [`RequestedMonitorTopology`] onto
/// `inventory`'s physical outputs (primary first, remaining monitors
/// assigned in roster order to remaining outputs in inventory order).
///
/// # Errors
///
/// Returns a typed rejection when the requested monitor count exceeds the
/// available output count, an assigned output has no mode matching a
/// monitor's exact requested resolution/refresh, a monitor's requested
/// rotation exceeds its assigned output's capability, the planned bounding
/// desktop exceeds this tranche's ceiling, or an applied-topology invariant is
/// violated. No partial plan is ever returned: the caller must treat any
/// `Err` as a whole-topology rejection (exact all-or-nothing mapping).
pub fn plan_topology(
    requested: &RequestedMonitorTopology,
    generation: TopologyGeneration,
    inventory: &PhysicalOutputInventory,
) -> Result<WindowsTopologyPlan, WindowsTopologyError> {
    let monitors = requested.monitors();
    if monitors.len() > inventory.len() {
        return Err(WindowsTopologyError::InsufficientOutputs {
            requested: monitors.len(),
            available: inventory.len(),
        });
    }

    // Windows always anchors the primary display's own desktop origin at
    // `(0, 0)` (see the module documentation) and this planner never
    // translates a layout to force that after the fact, so the client's
    // declared primary must already agree.
    let primary_monitor = requested.primary().monitor();
    if primary_monitor.x != 0 || primary_monitor.y != 0 {
        return Err(WindowsTopologyError::PrimaryNotAtLogicalOrigin {
            x: primary_monitor.x,
            y: primary_monitor.y,
        });
    }
    let primary_index = monitors
        .iter()
        .position(|monitor| monitor.monitor().primary)
        .unwrap_or(0);
    // Edge-aware: every monitor's host-pixel origin is derived by walking the
    // touching-edge graph out from the primary, placing each monitor flush
    // against its first-reached already-placed neighbor using that neighbor's
    // own scale, so a mixed-scale chain never accumulates a gap or an overlap
    // -- see `plan_monitor_offsets` and the module documentation.
    let offsets = plan_monitor_offsets(monitors, primary_index)?;

    let mut applied_monitors = Vec::with_capacity(monitors.len());
    for (index, requested_monitor) in monitors.iter().enumerate() {
        let (desktop_x, desktop_y) = offsets[index];
        // Deterministic 1-based session monitor ids, in requested-roster
        // order. `index + 1` is always in `1..=4` (the roster is already
        // bounded by `RequestedMonitorTopology::new`), so only the `u16`
        // conversion below can realistically fail.
        let raw_id =
            u16::try_from(index + 1).map_err(|_| MediaContractError::CoordinateOverflow)?;
        let session_monitor_id =
            SessionMonitorId::new(raw_id).map_err(WindowsTopologyError::from)?;
        applied_monitors.push(AppliedMonitor::new(
            session_monitor_id,
            requested_monitor.clone(),
            desktop_x,
            desktop_y,
        )?);
    }

    let topology = AppliedMonitorTopology::new(generation, applied_monitors)?;
    // `AppliedMonitor::desktop_rect_px` already reports the rotation-aware
    // on-desktop footprint (native dimensions swapped at 90/270 degrees), so
    // every bounds/placement calculation below can use it directly.
    //
    // Unlike an Xorg/RandR screen, the Windows virtual desktop is natively
    // signed and never translated to a non-negative origin here: shared
    // edge-aware placement already anchors the primary at desktop `(0, 0)`,
    // and any monitor above/left of it legitimately carries negative
    // coordinates, exactly like `GetSystemMetrics(SM_XVIRTUALSCREEN)` can be
    // negative. Translating to a non-negative origin (as the Linux/Xorg
    // planner does) would drag the primary away from `(0, 0)` whenever any
    // monitor sits above/left of it, which this host must never do. That is
    // the explicit shared `OriginPolicy::PreserveSigned` policy.
    let footprint_rects = topology
        .monitors()
        .iter()
        .map(AppliedMonitor::desktop_rect_px)
        .collect::<Result<Vec<LayoutRect>, MediaContractError>>()?;
    let bounds = signed_desktop_bounds(footprint_rects.clone())?;

    if bounds.width > MAX_VIRTUAL_DESKTOP_DIMENSION_PX
        || bounds.height > MAX_VIRTUAL_DESKTOP_DIMENSION_PX
    {
        return Err(WindowsTopologyError::DesktopTooLarge {
            width: bounds.width,
            height: bounds.height,
        });
    }

    // Assign outputs: the primary monitor gets the first inventory output;
    // the remaining monitors (in original roster order, primary excluded)
    // get the remaining outputs in inventory order. Deterministic and stable
    // across identical requests, and correct regardless of how outputs are
    // distributed across adapters (multi-GPU aware): assignment only ever
    // consumes inventory slots by position, never by adapter or a global
    // ordinal. `topology` preserves requested-roster order, so `primary_index`
    // (resolved above for placement) indexes it unchanged.
    let mut assignment_order = Vec::with_capacity(topology.monitors().len());
    assignment_order.push(primary_index);
    for index in 0..topology.monitors().len() {
        if index != primary_index {
            assignment_order.push(index);
        }
    }

    let mut plans: Vec<Option<WindowsMonitorPlan>> = vec![None; topology.monitors().len()];
    let mut requires_custom_timing = false;
    for (output_slot, monitor_index) in assignment_order.into_iter().enumerate() {
        let applied_monitor = &topology.monitors()[monitor_index];
        let output = &inventory.outputs()[output_slot];
        let client_display_id = client_display_id_of(applied_monitor)?;
        let rotation = applied_monitor.monitor().rotation;
        if !output.supported_rotations.contains(&rotation) {
            return Err(WindowsTopologyError::UnsupportedRotation {
                client_display_id,
                rotation,
            });
        }
        let mode_width = applied_monitor.monitor().width_px;
        let mode_height = applied_monitor.monitor().height_px;
        let refresh_hz = applied_monitor.monitor().refresh_hz;
        let scale = region_scale(&client_display_id, applied_monitor.monitor().scale)?;
        let requested_mode = OutputMode {
            width: mode_width,
            height: mode_height,
            refresh_hz,
        };
        if !output.mode_capability.supports(requested_mode) {
            return Err(WindowsTopologyError::NoMatchingMode {
                client_display_id,
                width: mode_width,
                height: mode_height,
                refresh_hz,
            });
        }
        let rect = footprint_rects[monitor_index];
        requires_custom_timing |= output.mode_capability.requires_custom_timing();
        plans[monitor_index] = Some(WindowsMonitorPlan {
            session_monitor_id: applied_monitor.session_monitor_id,
            client_display_id,
            adapter_luid: output.adapter_luid,
            target_id: output.target_id,
            adapter_output_index: output.adapter_output_index,
            adapter_name: output.adapter_name.clone(),
            global_index: output.global_index,
            device_name: output.device_name.clone(),
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            mode_width,
            mode_height,
            logical_rect: region_logical_rect(applied_monitor.requested_monitor())?,
            scale,
            refresh_hz,
            rotation,
            primary: applied_monitor.monitor().primary,
        });
    }

    let monitors = plans
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .expect("every monitor index was assigned exactly one output slot above");

    Ok(WindowsTopologyPlan {
        generation,
        desktop_x: bounds.x,
        desktop_y: bounds.y,
        desktop_width: bounds.width,
        desktop_height: bounds.height,
        monitors,
        requires_custom_timing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{
        LogicalPoint, LogicalSize, Monitor, MonitorIdentity, OutputTransform, RequestedMonitor,
    };

    fn generation() -> TopologyGeneration {
        TopologyGeneration::new(1).expect("nonzero generation")
    }

    fn luid(low_part: u32) -> AdapterLuid {
        AdapterLuid {
            low_part,
            high_part: 0,
        }
    }

    /// A generous custom-timing-capable output (matches this host's NVAPI
    /// custom-timing path): accepts any requested mode inside the bounds
    /// below, so tests can request arbitrary sizes without also listing an
    /// exact mode. `width`/`height` are unused (kept for call-site clarity
    /// about the fixture's intended resolution) since custom-timing bounds
    /// already cover every size these tests request.
    fn output(
        adapter_luid: AdapterLuid,
        target_id: u32,
        width: u32,
        height: u32,
    ) -> AvailableOutput {
        AvailableOutput {
            adapter_luid,
            target_id,
            adapter_output_index: target_id,
            adapter_name: format!("adapter-{}", adapter_luid.low_part),
            // Fixtures default the ephemeral current-selector fields to the
            // same value as `adapter_output_index`/`target_id`; tests that
            // specifically exercise non-contiguous/stale global indices
            // override these via struct-update syntax.
            global_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", target_id + 1),
            mode_capability: OutputModeCapability::CustomTimingCapable {
                min_width: 320,
                max_width: 7_680,
                min_height: 240,
                max_height: 4_320,
                min_refresh_hz: 30,
                max_refresh_hz: 240,
            },
            supported_rotations: vec![
                Rotation::Degrees0,
                Rotation::Degrees90,
                Rotation::Degrees180,
                Rotation::Degrees270,
            ],
            current_x: if target_id == 0 {
                0
            } else {
                i32::try_from(width).unwrap_or(i32::MAX)
            },
            current_y: 0,
            current_width: width,
            current_height: height,
            current_refresh_hz: 60,
            primary: target_id == 0,
        }
    }

    fn fixed_mode_output(
        adapter_luid: AdapterLuid,
        target_id: u32,
        modes: Vec<OutputMode>,
    ) -> AvailableOutput {
        let current = modes.first().copied().unwrap_or(OutputMode {
            width: 1_920,
            height: 1_080,
            refresh_hz: 60,
        });
        AvailableOutput {
            adapter_luid,
            target_id,
            adapter_output_index: target_id,
            adapter_name: format!("adapter-{}", adapter_luid.low_part),
            global_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", target_id + 1),
            mode_capability: OutputModeCapability::FixedModes(modes),
            supported_rotations: vec![Rotation::Degrees0],
            current_x: if target_id == 0 {
                0
            } else {
                i32::try_from(current.width).unwrap_or(i32::MAX)
            },
            current_y: 0,
            current_width: current.width,
            current_height: current.height,
            current_refresh_hz: current.refresh_hz,
            primary: target_id == 0,
        }
    }

    fn monitor(
        id: &str,
        position: (i32, i32),
        size_px: (u32, u32),
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        monitor_scaled(id, position, size_px, size_px, 1.0, primary, rotation)
    }

    fn monitor_scaled(
        id: &str,
        position: (i32, i32),
        size_px: (u32, u32),
        logical_size: (u32, u32),
        scale: f32,
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        let (x, y) = position;
        let (width_px, height_px) = size_px;
        let (logical_width, logical_height) = logical_size;
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                vendor: 0,
                model: 0,
                serial: 0,
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
        };
        RequestedMonitor::new(monitor, logical_width, logical_height).expect("requested monitor")
    }

    fn horizontal_separation_px(left: &WindowsMonitorPlan, right: &WindowsMonitorPlan) -> i64 {
        i64::from(right.x) - (i64::from(left.x) + i64::from(left.width))
    }

    fn vertical_separation_px(top: &WindowsMonitorPlan, bottom: &WindowsMonitorPlan) -> i64 {
        i64::from(bottom.y) - (i64::from(top.y) + i64::from(top.height))
    }

    fn monitor_plan<'a>(plan: &'a WindowsTopologyPlan, id: &str) -> &'a WindowsMonitorPlan {
        plan.monitors
            .iter()
            .find(|monitor| monitor.client_display_id == id)
            .unwrap_or_else(|| panic!("{id} monitor plan"))
    }

    fn placed_rect(monitor: &WindowsMonitorPlan) -> (i32, i32, u32, u32) {
        (monitor.x, monitor.y, monitor.width, monitor.height)
    }

    /// Asserts the two invariants every planned Windows desktop must hold
    /// regardless of scale mix: no two monitors share a pixel, the primary is
    /// anchored at `(0, 0)`, and the reported bounding desktop covers exactly
    /// the union of the planned footprints (signed origins included).
    fn assert_no_overlap_and_exact_signed_bounds(plan: &WindowsTopologyPlan) {
        for (index, left) in plan.monitors.iter().enumerate() {
            for right in plan.monitors.iter().skip(index + 1) {
                let overlaps_horizontally = i64::from(left.x)
                    < i64::from(right.x) + i64::from(right.width)
                    && i64::from(right.x) < i64::from(left.x) + i64::from(left.width);
                let overlaps_vertically = i64::from(left.y)
                    < i64::from(right.y) + i64::from(right.height)
                    && i64::from(right.y) < i64::from(left.y) + i64::from(left.height);
                assert!(
                    !(overlaps_horizontally && overlaps_vertically),
                    "{:?} and {:?} overlap on the virtual desktop",
                    placed_rect(left),
                    placed_rect(right)
                );
            }
        }
        let primary = plan.primary();
        assert_eq!(
            (primary.x, primary.y),
            (0, 0),
            "Windows always anchors the primary at (0, 0)"
        );
        let min_x = plan
            .monitors
            .iter()
            .map(|monitor| i64::from(monitor.x))
            .min()
            .expect("at least one monitor");
        let min_y = plan
            .monitors
            .iter()
            .map(|monitor| i64::from(monitor.y))
            .min()
            .expect("at least one monitor");
        let max_right = plan
            .monitors
            .iter()
            .map(|monitor| i64::from(monitor.x) + i64::from(monitor.width))
            .max()
            .expect("at least one monitor");
        let max_bottom = plan
            .monitors
            .iter()
            .map(|monitor| i64::from(monitor.y) + i64::from(monitor.height))
            .max()
            .expect("at least one monitor");
        assert_eq!(i64::from(plan.desktop_x), min_x);
        assert_eq!(i64::from(plan.desktop_y), min_y);
        assert_eq!(i64::from(plan.desktop_width), max_right - min_x);
        assert_eq!(i64::from(plan.desktop_height), max_bottom - min_y);
    }

    /// Asserts a left-to-right chain is flush: every neighbor pair touches
    /// with neither a gap nor an overlap.
    fn assert_horizontal_chain_is_flush(chain: &[&WindowsMonitorPlan]) {
        for pair in chain.windows(2) {
            assert_eq!(
                horizontal_separation_px(pair[0], pair[1]),
                0,
                "{:?} and {:?} must touch exactly",
                placed_rect(pair[0]),
                placed_rect(pair[1])
            );
        }
    }

    /// Asserts a top-to-bottom chain is flush, same contract as
    /// [`assert_horizontal_chain_is_flush`] on the vertical axis.
    fn assert_vertical_chain_is_flush(chain: &[&WindowsMonitorPlan]) {
        for pair in chain.windows(2) {
            assert_eq!(
                vertical_separation_px(pair[0], pair[1]),
                0,
                "{:?} and {:?} must touch exactly",
                placed_rect(pair[0]),
                placed_rect(pair[1])
            );
        }
    }

    #[test]
    fn plans_a_single_monitor() {
        let inventory = PhysicalOutputInventory::new(vec![output(luid(1), 0, 1_920, 1_080)])
            .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![monitor(
            "a",
            (0, 0),
            (1_920, 1_080),
            true,
            Rotation::Degrees0,
        )])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        assert_eq!(plan.monitors.len(), 1);
        assert_eq!(plan.desktop_width, 1_920);
        assert_eq!(plan.desktop_height, 1_080);
        assert!(plan.primary().primary);
    }

    #[test]
    fn plans_two_monitors_side_by_side() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        assert_eq!(plan.desktop_width, 3_840);
        assert_eq!(plan.desktop_height, 1_080);
        let second = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "b")
            .expect("second monitor");
        assert_eq!(second.x, 1_920);
        assert_eq!(second.y, 0);
    }

    #[test]
    fn three_monitor_mixed_scale_chain_has_no_gap_or_overlap() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
            output(luid(1), 2, 1_920, 1_080),
        ])
        .expect("inventory");

        // Regression for the former global-primary-scale defect: the 2x
        // primary used to convert the second (1x -> 1.5x) hop's absolute
        // logical offset with its own scale, leaving a +1280px gap between
        // the middle and tail monitors. Edge-aware placement converts each
        // hop against its own neighbor, so every hop is flush.
        let gap_requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "gap-primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "gap-middle",
                (960, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "gap-tail",
                (2_240, 0),
                (1_200, 900),
                (800, 600),
                1.5,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("gap topology");
        let gap_plan = plan_topology(&gap_requested, generation(), &inventory).expect("gap plan");
        let gap_primary = monitor_plan(&gap_plan, "gap-primary");
        let gap_middle = monitor_plan(&gap_plan, "gap-middle");
        let gap_tail = monitor_plan(&gap_plan, "gap-tail");
        assert_eq!(placed_rect(gap_primary), (0, 0, 1_920, 1_080));
        assert_eq!(placed_rect(gap_middle), (1_920, 0, 1_280, 720));
        assert_eq!(placed_rect(gap_tail), (3_200, 0, 1_200, 900));
        assert_horizontal_chain_is_flush(&[gap_primary, gap_middle, gap_tail]);
        assert_no_overlap_and_exact_signed_bounds(&gap_plan);
        assert_eq!(gap_plan.desktop_width, 4_400);
        assert_eq!(gap_plan.desktop_height, 1_080);

        // The inverse fixture covers the matching overlap defect: a 1x
        // primary used to under-convert the hop after a 2x middle monitor,
        // producing a -960px separation (a real overlap of two physical
        // outputs). It is now flush as well.
        let overlap_requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "overlap-primary",
                (0, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "overlap-middle",
                (1_280, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "overlap-tail",
                (2_240, 0),
                (1_280, 960),
                (1_024, 768),
                1.25,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("overlap topology");
        let overlap_plan =
            plan_topology(&overlap_requested, generation(), &inventory).expect("overlap plan");
        let overlap_primary = monitor_plan(&overlap_plan, "overlap-primary");
        let overlap_middle = monitor_plan(&overlap_plan, "overlap-middle");
        let overlap_tail = monitor_plan(&overlap_plan, "overlap-tail");
        assert_eq!(placed_rect(overlap_primary), (0, 0, 1_280, 720));
        assert_eq!(placed_rect(overlap_middle), (1_280, 0, 1_920, 1_080));
        assert_eq!(placed_rect(overlap_tail), (3_200, 0, 1_280, 960));
        assert_horizontal_chain_is_flush(&[overlap_primary, overlap_middle, overlap_tail]);
        assert_no_overlap_and_exact_signed_bounds(&overlap_plan);
        assert_eq!(overlap_plan.desktop_width, 4_480);
        assert_eq!(overlap_plan.desktop_height, 1_080);
    }

    #[test]
    fn four_monitor_mixed_scale_chain_has_no_gap_or_overlap() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_280, 720),
            output(luid(1), 1, 1_920, 1_080),
            output(luid(2), 0, 1_280, 960),
            output(luid(2), 1, 1_200, 900),
        ])
        .expect("inventory");
        // Four distinct scales (1x, 2x, 1.25x, 1.5x) chained left to right,
        // so every hop is converted against a differently scaled neighbor.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "a",
                (0, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "b",
                (1_280, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "c",
                (2_240, 0),
                (1_280, 960),
                (1_024, 768),
                1.25,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "d",
                (3_264, 0),
                (1_200, 900),
                (800, 600),
                1.5,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let a = monitor_plan(&plan, "a");
        let b = monitor_plan(&plan, "b");
        let c = monitor_plan(&plan, "c");
        let d = monitor_plan(&plan, "d");
        assert_eq!(placed_rect(a), (0, 0, 1_280, 720));
        assert_eq!(placed_rect(b), (1_280, 0, 1_920, 1_080));
        assert_eq!(placed_rect(c), (3_200, 0, 1_280, 960));
        assert_eq!(placed_rect(d), (4_480, 0, 1_200, 900));
        assert_horizontal_chain_is_flush(&[a, b, c, d]);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!(plan.desktop_width, 5_680);
        assert_eq!(plan.desktop_height, 1_080);
        // Exact modes stay the native requested ones, unaffected by placement.
        assert_eq!((b.mode_width, b.mode_height), (1_920, 1_080));
        assert_eq!((d.mode_width, d.mode_height), (1_200, 900));
    }

    #[test]
    fn mixed_scale_chain_above_the_primary_stays_flush_and_signed() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_280, 720),
            output(luid(1), 2, 1_200, 1_080),
        ])
        .expect("inventory");
        // A vertical mixed-scale stack growing upwards from the primary: both
        // hops carry negative host coordinates and both must stay flush.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "above",
                (0, -720),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "top",
                (0, -1_440),
                (1_200, 1_080),
                (800, 720),
                1.5,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let primary = monitor_plan(&plan, "primary");
        let above = monitor_plan(&plan, "above");
        let top = monitor_plan(&plan, "top");
        assert_eq!(placed_rect(primary), (0, 0, 1_920, 1_080));
        assert_eq!(placed_rect(above), (0, -720, 1_280, 720));
        assert_eq!(placed_rect(top), (0, -1_800, 1_200, 1_080));
        assert_vertical_chain_is_flush(&[top, above, primary]);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_x, plan.desktop_y), (0, -1_800));
        assert_eq!((plan.desktop_width, plan.desktop_height), (1_920, 2_880));
    }

    #[test]
    fn mixed_scale_chain_across_a_rotated_hop_stays_flush() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
            output(luid(1), 2, 1_280, 720),
        ])
        .expect("inventory");
        // The middle monitor is driven at its native 1920x1080 mode but
        // rotated 90 degrees, so it occupies a 1080x1920 desktop footprint
        // and the chain must continue from that rotated width.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "portrait",
                (960, 0),
                (1_920, 1_080),
                (540, 960),
                2.0,
                false,
                Rotation::Degrees90,
            ),
            monitor_scaled(
                "tail",
                (1_500, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let primary = monitor_plan(&plan, "primary");
        let portrait = monitor_plan(&plan, "portrait");
        let tail = monitor_plan(&plan, "tail");
        assert_eq!(placed_rect(primary), (0, 0, 1_920, 1_080));
        // Native mode stays unswapped; only the desktop footprint rotates.
        assert_eq!((portrait.mode_width, portrait.mode_height), (1_920, 1_080));
        assert_eq!(placed_rect(portrait), (1_920, 0, 1_080, 1_920));
        assert_eq!(placed_rect(tail), (3_000, 0, 1_280, 720));
        assert_horizontal_chain_is_flush(&[primary, portrait, tail]);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_width, plan.desktop_height), (4_280, 1_920));
        let (regions, applied_regions) = plan.region_sets().expect("regions");
        let portrait_region = regions
            .get(RegionId::new(2).expect("portrait id"))
            .expect("portrait region");
        assert_eq!(
            portrait_region.physical_size(),
            PhysicalSize::new(1_920, 1_080).unwrap()
        );
        assert_eq!(portrait_region.transform(), OutputTransform::Rotate90);
        let applied_portrait = applied_regions
            .get(portrait_region.id())
            .expect("applied portrait");
        assert_eq!(
            applied_portrait.applied_rect().origin(),
            AppliedPoint::new(1_920, 0)
        );
        assert_eq!(
            applied_portrait.applied_rect().size(),
            AppliedSize::new(1_080, 1_920).unwrap()
        );
    }

    #[test]
    fn generates_explicit_shared_regions_for_mixed_scale_signed_layout() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_280, 960),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "main",
                (0, 0),
                (1_920, 1_080),
                (1_920, 1_080),
                1.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "left",
                (-1_024, -96),
                (1_280, 960),
                (1_024, 768),
                1.25,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let (regions, applied) = plan.region_sets().expect("shared regions");
        assert_eq!(regions.generation().get(), plan.generation.get());
        assert_eq!(applied.generation(), regions.generation());
        let left = regions
            .get(RegionId::new(2).expect("left id"))
            .expect("left region");
        let left_plan = plan
            .monitors
            .iter()
            .find(|monitor| monitor.session_monitor_id.get() == 2)
            .expect("left monitor plan");
        assert_eq!(
            left.output_identity().as_str(),
            format!(
                "luid:{}:{}:target:{}",
                left_plan.adapter_luid.high_part,
                left_plan.adapter_luid.low_part,
                left_plan.target_id
            )
        );
        assert_eq!(
            left.logical_rect().origin(),
            LogicalPoint::from_pixels(-1_024, -96).expect("logical origin")
        );
        assert_eq!(
            left.logical_rect().size(),
            LogicalSize::from_pixels(1_024, 768).expect("logical size")
        );
        assert_eq!(left.physical_size(), PhysicalSize::new(1_280, 960).unwrap());
        assert_eq!(left.scale().get(), 150);
        let applied_left = applied.get(left.id()).expect("applied left");
        // The 1.25x monitor is placed flush against the primary's left edge
        // by its own 1280px footprint width, not by its 1024 logical width:
        // the former global-primary-scale placement produced -1024, which
        // overlapped the primary by 256px.
        assert_eq!(
            applied_left.applied_rect().origin(),
            AppliedPoint::new(-1_280, -96)
        );
        assert_eq!(
            applied_left.applied_rect().size(),
            AppliedSize::new(1_280, 960).unwrap()
        );
        assert_eq!(horizontal_separation_px(left_plan, plan.primary()), 0);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_x, plan.desktop_y), (-1_280, -96));
        assert_eq!((plan.desktop_width, plan.desktop_height), (3_200, 1_176));
    }

    #[test]
    fn plans_four_monitors() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
            output(luid(2), 0, 1_920, 1_080),
            output(luid(2), 1, 1_920, 1_080),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
            monitor("c", (3_840, 0), (1_920, 1_080), false, Rotation::Degrees0),
            monitor("d", (5_760, 0), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        assert_eq!(plan.monitors.len(), 4);
        assert_eq!(plan.desktop_width, 7_680);
    }

    #[test]
    fn assigns_outputs_across_non_contiguous_multi_gpu_targets_by_stable_identity() {
        // Two adapters, each exposing target ids 0 and 2 (non-contiguous, and
        // colliding numerically across adapters) — the plan must key
        // assignment on `(adapter_luid, target_id)`, never a global ordinal.
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(10), 0, 1_920, 1_080),
            output(luid(10), 2, 1_920, 1_080),
            output(luid(20), 0, 1_920, 1_080),
            output(luid(20), 2, 1_920, 1_080),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
            monitor("c", (3_840, 0), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        // The primary is assigned the first inventory slot: adapter 10 / target 0.
        assert_eq!(plan.primary().adapter_luid, luid(10));
        assert_eq!(plan.primary().target_id, 0);
        // Every assigned (adapter_luid, target_id) pair must be unique.
        let mut seen = std::collections::BTreeSet::new();
        for monitor_plan in &plan.monitors {
            assert!(seen.insert((
                monitor_plan.adapter_luid.low_part,
                monitor_plan.adapter_luid.high_part,
                monitor_plan.target_id
            )));
        }
    }

    #[test]
    fn primary_is_assigned_first_inventory_output_even_when_listed_second_in_the_roster() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 2_560, 1_440),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (-1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
            monitor("b", (0, 0), (2_560, 1_440), true, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let primary = plan.primary();
        assert_eq!(primary.client_display_id, "b");
        assert_eq!(primary.target_id, 0);
    }

    #[test]
    fn host_regions_use_native_needs_transform_convention() {
        let inventory = PhysicalOutputInventory::new(vec![output(luid(1), 0, 1_080, 1_920)])
            .expect("inventory");
        // Native (pre-rotation) mode is 1080x1920 portrait; the client's
        // logical arrangement describes the already-rotated 1920x1080
        // apparent footprint, matching `RequestedMonitor`'s documented
        // contract.
        let rotated = Monitor {
            identity: MonitorIdentity {
                id: "a".to_owned(),
                name: "Display a".to_owned(),
                vendor: 0,
                model: 0,
                serial: 0,
            },
            x: 0,
            y: 0,
            width_px: 1_080,
            height_px: 1_920,
            scale: 1.0,
            refresh_hz: 60,
            rotation: Rotation::Degrees90,
            primary: true,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        let requested_monitor =
            RequestedMonitor::new(rotated, 1_920, 1_080).expect("requested monitor");
        let requested =
            RequestedMonitorTopology::new(vec![requested_monitor]).expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let applied = &plan.monitors[0];
        assert_eq!(applied.mode_width, 1_080);
        assert_eq!(applied.mode_height, 1_920);
        // Host NativeNeedsTransform keeps the native stream extent and
        // carries the 90-degree output transform separately.
        assert_eq!(applied.width, 1_920);
        assert_eq!(applied.height, 1_080);
        let (regions, applied_regions) = plan.region_sets().expect("rotated regions");
        let region = regions.primary();
        assert_eq!(
            region.physical_size(),
            PhysicalSize::new(1_080, 1_920).unwrap()
        );
        assert_eq!(region.transform(), OutputTransform::Rotate90);
        assert_eq!(
            applied_regions.primary().applied_rect().size(),
            AppliedSize::new(1_920, 1_080).unwrap()
        );
    }

    #[test]
    fn rejects_rotation_the_assigned_output_does_not_support() {
        let mut fixed = fixed_mode_output(
            luid(1),
            0,
            vec![OutputMode {
                width: 1_920,
                height: 1_080,
                refresh_hz: 60,
            }],
        );
        fixed.supported_rotations = vec![Rotation::Degrees0];
        let inventory = PhysicalOutputInventory::new(vec![fixed]).expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![monitor(
            "a",
            (0, 0),
            (1_920, 1_080),
            true,
            Rotation::Degrees90,
        )])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert!(matches!(
            error,
            WindowsTopologyError::UnsupportedRotation { .. }
        ));
    }

    #[test]
    fn preserves_signed_negative_origins_without_translating_to_zero() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
        ])
        .expect("inventory");
        // "b" sits to the left of the primary at a negative logical origin.
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (-1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        // Unlike the Linux/Xorg planner, Windows never translates a
        // negative-origin layout to a non-negative one: the primary stays
        // at desktop `(0, 0)` and "b" keeps its true, negative offset.
        let a = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "a")
            .expect("a");
        assert_eq!(a.x, 0);
        assert_eq!(a.y, 0);
        let b = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "b")
            .expect("b");
        assert_eq!(b.x, -1_920);
        assert_eq!(b.y, 0);
        assert_eq!(plan.desktop_x, -1_920);
        assert_eq!(plan.desktop_y, 0);
        assert_eq!(plan.desktop_width, 3_840);
        assert_eq!(plan.desktop_height, 1_080);
    }

    #[test]
    fn mixed_scale_negative_origin_stays_signed_and_flush() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_440, 900),
            output(luid(1), 1, 1_280, 720),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_440, 900),
                (960, 600),
                1.5,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "left",
                (-1_280, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let primary = plan.primary();
        let left = monitor_plan(&plan, "left");

        // Windows never translates the primary away from (0, 0), and the
        // 1x monitor left of a 1.5x primary is placed flush against the
        // primary's own left edge using its own footprint width. The former
        // global-primary-scale placement converted -1280 logical units at the
        // primary's 1.5x scale to -1920, leaving a 640px gap.
        assert_eq!((primary.x, primary.y), (0, 0));
        assert_eq!(placed_rect(left), (-1_280, 0, 1_280, 720));
        assert_eq!(horizontal_separation_px(left, primary), 0);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_x, plan.desktop_y), (-1_280, 0));
        assert_eq!((plan.desktop_width, plan.desktop_height), (2_720, 900));
    }

    #[test]
    fn preserves_signed_negative_origin_for_a_monitor_placed_above_the_primary() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
        ])
        .expect("inventory");
        // "b" sits directly above the primary at a negative logical origin.
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (0, -1_080), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let a = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "a")
            .expect("a");
        assert_eq!((a.x, a.y), (0, 0));
        let b = plan
            .monitors
            .iter()
            .find(|monitor| monitor.client_display_id == "b")
            .expect("b");
        assert_eq!((b.x, b.y), (0, -1_080));
        assert_eq!(plan.desktop_x, 0);
        assert_eq!(plan.desktop_y, -1_080);
        assert_eq!(plan.desktop_height, 2_160);
    }

    #[test]
    fn a_diagonal_corner_touch_is_not_an_edge_and_uses_the_primary_scale() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_280, 720),
        ])
        .expect("inventory");
        // Corner contact shares exactly one point and has no
        // perpendicular-axis overlap, so it is a genuine layout gap rather
        // than an ambiguous edge to snap to: the monitor keeps its absolute
        // logical offset converted at the primary's own scale.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "diagonal",
                (960, 540),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        assert_eq!(
            placed_rect(monitor_plan(&plan, "primary")),
            (0, 0, 1_920, 1_080)
        );
        assert_eq!(
            placed_rect(monitor_plan(&plan, "diagonal")),
            (1_920, 1_080, 1_280, 720)
        );
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_width, plan.desktop_height), (3_200, 1_800));
    }

    #[test]
    fn a_monitor_touching_two_neighbors_anchors_deterministically_on_the_first_reached() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_280, 720),
            output(luid(1), 2, 1_440, 810),
        ])
        .expect("inventory");
        // `corner` touches two logical edges at once: the primary's bottom
        // edge and `right`'s left edge. Under mixed scales those two
        // adjacencies cannot both be honored (each neighbor converts the
        // shared edge at its own scale), so the walk is ambiguous by
        // construction. It must still resolve deterministically -- the
        // breadth-first spanning tree anchors on the first-reached neighbor,
        // the primary -- and must never resolve into an overlap.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "right",
                (960, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "corner",
                (0, 540),
                (1_440, 810),
                (960, 540),
                1.5,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let primary = monitor_plan(&plan, "primary");
        let right = monitor_plan(&plan, "right");
        let corner = monitor_plan(&plan, "corner");
        assert_eq!(placed_rect(primary), (0, 0, 1_920, 1_080));
        assert_eq!(placed_rect(right), (1_920, 0, 1_280, 720));
        assert_eq!(placed_rect(corner), (0, 1_080, 1_440, 810));
        assert_horizontal_chain_is_flush(&[primary, right]);
        assert_vertical_chain_is_flush(&[primary, corner]);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_width, plan.desktop_height), (3_200, 1_890));

        // Same roster, `corner` listed before `right`: the primary is still
        // reached first for both, so placement is identical regardless of
        // roster order.
        let reordered = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "corner",
                (0, 540),
                (1_440, 810),
                (960, 540),
                1.5,
                false,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "right",
                (960, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("reordered topology");
        let reordered_plan =
            plan_topology(&reordered, generation(), &inventory).expect("reordered plan");
        assert_eq!(
            placed_rect(monitor_plan(&reordered_plan, "corner")),
            (0, 1_080, 1_440, 810)
        );
        assert_eq!(
            placed_rect(monitor_plan(&reordered_plan, "right")),
            (1_920, 0, 1_280, 720)
        );
        assert_no_overlap_and_exact_signed_bounds(&reordered_plan);
    }

    #[test]
    fn a_disconnected_monitor_keeps_its_intentional_separation() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_280, 720),
        ])
        .expect("inventory");
        // No touching edge anywhere: the island is not dragged flush against
        // the primary, it keeps its absolute logical offset at the primary's
        // own 2x scale.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "island",
                (2_000, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plan");
        let primary = monitor_plan(&plan, "primary");
        let island = monitor_plan(&plan, "island");
        assert_eq!(placed_rect(island), (4_000, 0, 1_280, 720));
        assert_eq!(horizontal_separation_px(primary, island), 2_080);
        assert_no_overlap_and_exact_signed_bounds(&plan);
        assert_eq!((plan.desktop_width, plan.desktop_height), (5_280, 1_080));
    }

    #[test]
    fn rejects_a_disconnected_layout_that_exceeds_the_desktop_ceiling() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_280, 720),
        ])
        .expect("inventory");
        // A disconnected island 4000 logical units out at the primary's 2x
        // scale lands at 8000px, so the bounding desktop exceeds the encode
        // ceiling: the whole topology is rejected, never partially planned.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (1_920, 1_080),
                (960, 540),
                2.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "island",
                (4_000, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert_eq!(
            error,
            WindowsTopologyError::DesktopTooLarge {
                width: 9_280,
                height: 1_080,
            }
        );
    }

    #[test]
    fn rejects_a_placement_offset_that_overflows_the_signed_desktop() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 3_840, 2_160),
            output(luid(1), 1, 1_280, 720),
        ])
        .expect("inventory");
        // An extreme logical offset multiplied by a huge primary scale
        // cannot be represented on the signed Windows desktop: placement
        // fails closed instead of wrapping into a plausible coordinate.
        let requested = RequestedMonitorTopology::new(vec![
            monitor_scaled(
                "primary",
                (0, 0),
                (3_840, 2_160),
                (2, 2),
                1.0,
                true,
                Rotation::Degrees0,
            ),
            monitor_scaled(
                "far",
                (2_000_000_000, 0),
                (1_280, 720),
                (1_280, 720),
                1.0,
                false,
                Rotation::Degrees0,
            ),
        ])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert_eq!(
            error,
            WindowsTopologyError::InvalidTopology(MediaContractError::CoordinateOverflow)
        );
    }

    #[test]
    fn rejects_a_declared_primary_not_at_logical_origin() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 1, 1_920, 1_080),
        ])
        .expect("inventory");
        // The primary is declared at a nonzero logical origin: Windows must
        // reject this rather than silently re-centering the layout.
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (100, 50), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (2_020, 50), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert_eq!(
            error,
            WindowsTopologyError::PrimaryNotAtLogicalOrigin { x: 100, y: 50 }
        );
    }

    #[test]
    fn rejects_insufficient_outputs_atomically() {
        let inventory = PhysicalOutputInventory::new(vec![output(luid(1), 0, 1_920, 1_080)])
            .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert_eq!(
            error,
            WindowsTopologyError::InsufficientOutputs {
                requested: 2,
                available: 1,
            }
        );
    }

    #[test]
    fn rejects_a_request_with_no_matching_mode_even_when_one_monitor_would_have_fit() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            fixed_mode_output(
                luid(1),
                1,
                vec![OutputMode {
                    width: 1_280,
                    height: 720,
                    refresh_hz: 60,
                }],
            ),
        ])
        .expect("inventory");
        // "b" requests 4K, but its assigned fixed-mode output only has 720p.
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (1_920, 0), (3_840, 2_160), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert!(matches!(error, WindowsTopologyError::NoMatchingMode { .. }));
    }

    #[test]
    fn rejects_duplicate_outputs_in_inventory() {
        let error = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            output(luid(1), 0, 1_920, 1_080),
        ])
        .expect_err("rejected");
        assert!(matches!(
            error,
            WindowsTopologyError::DuplicateOutputInInventory { .. }
        ));
    }

    #[test]
    fn rejects_an_empty_inventory() {
        let error = PhysicalOutputInventory::new(Vec::new()).expect_err("rejected");
        assert_eq!(error, WindowsTopologyError::EmptyInventory);
    }

    #[test]
    fn resolves_a_stable_binding_to_its_current_non_contiguous_global_index() {
        // Global indices are deliberately non-contiguous/out of inventory
        // order, as a real DXGI re-enumeration could produce.
        let mut first = output(luid(1), 0, 1_920, 1_080);
        first.global_index = 3;
        first.adapter_output_index = 1;
        first.device_name = r"\\.\DISPLAY4".to_owned();
        let mut second = output(luid(1), 1, 1_920, 1_080);
        second.global_index = 0;
        second.adapter_output_index = 0;
        second.device_name = r"\\.\DISPLAY1".to_owned();
        let inventory = PhysicalOutputInventory::new(vec![first, second]).expect("inventory");

        let selector = inventory.resolve(luid(1), 1).expect("resolves");
        assert_eq!(selector.adapter_luid, luid(1));
        assert_eq!(selector.target_id, 1);
        assert_eq!(selector.global_index, 0);
        assert_eq!(selector.adapter_output_index, 0);
        assert_eq!(selector.device_name, r"\\.\DISPLAY1");
    }

    #[test]
    fn resolves_correctly_across_identically_named_adapters_by_luid_not_name() {
        // Two physically distinct adapters that happen to share the exact
        // same DXGI description string (e.g. two identical GPU models).
        // Resolution must key strictly on `(adapter_luid, target_id)` and
        // must never fall back to `adapter_name`.
        let mut first = output(luid(1), 0, 1_920, 1_080);
        first.adapter_name = "NVIDIA GeForce RTX 4090".to_owned();
        first.global_index = 0;
        let mut second = output(luid(2), 0, 1_920, 1_080);
        second.adapter_name = "NVIDIA GeForce RTX 4090".to_owned();
        second.global_index = 1;
        let inventory = PhysicalOutputInventory::new(vec![first, second]).expect("inventory");

        let resolved_first = inventory.resolve(luid(1), 0).expect("resolves first");
        assert_eq!(resolved_first.global_index, 0);
        let resolved_second = inventory.resolve(luid(2), 0).expect("resolves second");
        assert_eq!(resolved_second.global_index, 1);
    }

    #[test]
    fn fails_closed_when_a_stable_binding_is_missing_from_a_fresh_inventory() {
        let inventory = PhysicalOutputInventory::new(vec![output(luid(1), 0, 1_920, 1_080)])
            .expect("inventory");
        // Target 1 was never present (e.g. unplugged before re-enumeration).
        let error = inventory.resolve(luid(1), 1).expect_err("missing");
        assert_eq!(
            error,
            CaptureSelectorError::MissingBinding {
                adapter_luid: luid(1),
                target_id: 1,
            }
        );
    }

    #[test]
    fn fails_closed_on_an_ambiguous_binding_rather_than_falling_back_to_adapter_name() {
        // A validated `PhysicalOutputInventory` can never contain a
        // duplicate `(adapter_luid, target_id)` pair, so this calls the raw
        // resolver directly against a hand-built slice to prove the
        // defense-in-depth guard exists and is never bypassed by matching on
        // `adapter_name` alone.
        let mut duplicate = output(luid(1), 0, 1_920, 1_080);
        duplicate.device_name = r"\\.\DISPLAY9".to_owned();
        let outputs = vec![output(luid(1), 0, 1_920, 1_080), duplicate];
        let error = resolve_capture_selector(&outputs, luid(1), 0).expect_err("ambiguous binding");
        assert_eq!(
            error,
            CaptureSelectorError::AmbiguousBinding {
                adapter_luid: luid(1),
                target_id: 0,
                matches: 2,
            }
        );
    }
    /// The exact attached-output inventory probed inside the interactive
    /// session of the `pier-windows-software.example.internal` Windows host: a QEMU guest whose only
    /// display device is std-VGA driven by the inbox Microsoft Basic Display
    /// Adapter. One output, no NVIDIA driver, and a fixed 64 Hz mode list.
    fn co_maintenance_output() -> AvailableOutput {
        let modes = [
            (640, 480),
            (800, 600),
            (1_024, 768),
            (1_152, 864),
            (1_280, 720),
            (1_280, 768),
            (1_280, 800),
            (1_280, 960),
            (1_280, 1_024),
            (1_360, 768),
            (1_366, 768),
            (1_600, 900),
            (1_600, 1_200),
            (1_680, 1_050),
            (1_920, 1_080),
            (1_920, 1_200),
            (2_560, 1_440),
            (2_560, 1_600),
            (3_840, 2_160),
        ]
        .into_iter()
        .map(|(width, height)| OutputMode {
            width,
            height,
            refresh_hz: 64,
        })
        .collect::<Vec<_>>();
        AvailableOutput {
            adapter_luid: AdapterLuid {
                low_part: 0x0000_66d5,
                high_part: 0,
            },
            target_id: 0,
            adapter_output_index: 0,
            adapter_name: "Microsoft Basic Render Driver".to_owned(),
            global_index: 0,
            device_name: r"\\.\DISPLAY1".to_owned(),
            mode_capability: OutputModeCapability::FixedModes(modes),
            supported_rotations: vec![Rotation::Degrees0],
            current_x: 0,
            current_y: 0,
            current_width: 1_920,
            current_height: 1_200,
            current_refresh_hz: 64,
            primary: true,
        }
    }

    fn monitor_at_refresh(
        id: &str,
        position: (i32, i32),
        size_px: (u32, u32),
        refresh_hz: u32,
        primary: bool,
    ) -> RequestedMonitor {
        let (x, y) = position;
        let (width_px, height_px) = size_px;
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: id.to_owned(),
                name: format!("Display {id}"),
                vendor: 0,
                model: 0,
                serial: 0,
            },
            x,
            y,
            width_px,
            height_px,
            scale: 1.0,
            refresh_hz,
            rotation: Rotation::Degrees0,
            primary,
            width_mm: 0.0,
            height_mm: 0.0,
        };
        RequestedMonitor::new(monitor, width_px, height_px).expect("requested monitor")
    }

    #[test]
    fn only_custom_timing_capable_outputs_require_a_synthesized_timing() {
        assert!(!OutputModeCapability::FixedModes(vec![OutputMode {
            width: 1_920,
            height: 1_200,
            refresh_hz: 64,
        }])
        .requires_custom_timing());
        assert!(OutputModeCapability::CustomTimingCapable {
            min_width: 320,
            max_width: 7_680,
            min_height: 240,
            max_height: 4_320,
            min_refresh_hz: 30,
            max_refresh_hz: 240,
        }
        .requires_custom_timing());
    }

    /// Regression for `regress-comaintenance-multimon`: a software/OpenH264 host
    /// with no NVIDIA driver plans its own single attached output at one of
    /// its enumerated modes, and the plan says so — nothing on the apply path
    /// may reach for NVAPI to set a mode this output already advertises.
    #[test]
    fn software_host_plans_its_single_fixed_mode_output_without_custom_timing() {
        let inventory =
            PhysicalOutputInventory::new(vec![co_maintenance_output()]).expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![monitor_at_refresh(
            "deck-1",
            (0, 0),
            (1_920, 1_200),
            64,
            true,
        )])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plans");
        assert_eq!(plan.monitors.len(), 1);
        assert!(!plan.requires_custom_timing);
        let monitor = &plan.monitors[0];
        assert_eq!(monitor.adapter_name, "Microsoft Basic Render Driver");
        assert_eq!(monitor.mode_width, 1_920);
        assert_eq!(monitor.mode_height, 1_200);
        assert_eq!(monitor.refresh_hz, 64);
        assert!(monitor.primary);
    }

    /// The same host must still refuse a two-monitor layout: it physically
    /// has one output, and no gate above may paper over that.
    #[test]
    fn software_host_refuses_a_second_monitor_it_has_no_output_for() {
        let inventory =
            PhysicalOutputInventory::new(vec![co_maintenance_output()]).expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor_at_refresh("deck-1", (0, 0), (1_920, 1_200), 64, true),
            monitor_at_refresh("deck-2", (1_920, 0), (1_920, 1_200), 64, false),
        ])
        .expect("requested topology");
        let error = plan_topology(&requested, generation(), &inventory).expect_err("rejected");
        assert_eq!(
            error,
            WindowsTopologyError::InsufficientOutputs {
                requested: 2,
                available: 1,
            }
        );
    }

    /// A mixed host keeps today's behavior: one custom-timing-capable output
    /// anywhere in the plan still routes the whole apply through the
    /// synthesized-timing path.
    #[test]
    fn any_custom_timing_capable_output_marks_the_whole_plan() {
        let inventory = PhysicalOutputInventory::new(vec![
            output(luid(1), 0, 1_920, 1_080),
            fixed_mode_output(
                luid(2),
                1,
                vec![OutputMode {
                    width: 1_920,
                    height: 1_080,
                    refresh_hz: 60,
                }],
            ),
        ])
        .expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![
            monitor("a", (0, 0), (1_920, 1_080), true, Rotation::Degrees0),
            monitor("b", (1_920, 0), (1_920, 1_080), false, Rotation::Degrees0),
        ])
        .expect("requested topology");
        let plan = plan_topology(&requested, generation(), &inventory).expect("plans");
        assert!(plan.requires_custom_timing);
    }

    /// The no-mutation current-topology planner reports the same requirement
    /// from the same capability, so a caller that captures today's rectangles
    /// on a software host never reaches for NVAPI either.
    #[test]
    fn current_topology_planner_reports_the_same_custom_timing_requirement() {
        let inventory =
            PhysicalOutputInventory::new(vec![co_maintenance_output()]).expect("inventory");
        let requested = RequestedMonitorTopology::new(vec![monitor_at_refresh(
            "deck-1",
            (0, 0),
            (1_920, 1_200),
            64,
            true,
        )])
        .expect("requested topology");
        let plan = plan_current_topology(&requested, generation(), &inventory).expect("plans");
        assert!(!plan.requires_custom_timing);
        assert_eq!(plan.monitors[0].refresh_hz, 64);
    }
}
