//! Capability-gated Wayland output inventory for the Linux host.
//!
//! This module is intentionally host-local. It models `wl_output`,
//! `xdg-output`, fractional presentation scale, and compositor detection
//! reporting without linking a Wayland client library or selecting a Wayland
//! session at runtime. The dedicated-Xorg implementation of
//! [`arcen_outputs::OutputProvider`] remains the only wired Linux session
//! output path.
//!
//! Nothing here is an output *transaction*. [`WaylandOutputSource`] enumerates
//! what a compositor already has, and [`WaylandOutputCapabilities`] reports
//! detection facts. ADR 0010 renamed both out of their former generic names
//! precisely so this seam stops being read as an implementation of the shared
//! lifecycle, which stages, binds, verifies, commits, and rolls back a topology
//! and which this module deliberately does not do.
//!
//! Enabling the default-off `wayland-provider` Cargo feature exposes the
//! enumeration trait and makes the binary eligible for runtime selection. It
//! does not make output enumeration, portals, or Mutter virtual outputs
//! available. [`detect_output_capability`] stays fail-closed until a
//! separately reviewed native adapter supplies protocol facts and the
//! compile-time implementation constants change.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};

use arcen_media::{
    LogicalPoint, LogicalRect, LogicalSize, OutputIdentity, OutputTransform, PhysicalSize,
    RegionContractError, RegionDescriptor, RegionGeneration, RegionId, RegionSet, Scale120,
};
#[cfg(feature = "wayland-provider")]
use std::error::Error;
use thiserror::Error;

/// Whether this binary was compiled with the default-off provider seam.
pub const WAYLAND_PROVIDER_FEATURE_ENABLED: bool = cfg!(feature = "wayland-provider");

/// Native `wl_display`/registry/output enumeration is not part of this tranche.
pub const NATIVE_WAYLAND_OUTPUT_ADAPTER_IMPLEMENTED: bool = false;

/// Mutter virtual-output creation is modeled but not implemented.
pub const MUTTER_VIRTUAL_OUTPUT_ADAPTER_IMPLEMENTED: bool = false;

/// Three-state result used when a native protocol probe is unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProbeState {
    /// No native adapter supplied an authoritative answer.
    #[default]
    Unknown,
    /// The capability was authoritatively probed and is absent.
    Unavailable,
    /// The capability was authoritatively probed and is present.
    Available,
}

/// Result of resolving the process's Wayland display socket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DisplaySocketState {
    /// The socket was not probed.
    #[default]
    Unknown,
    /// `WAYLAND_DISPLAY` is absent.
    MissingDisplayName,
    /// A relative display name was supplied without `XDG_RUNTIME_DIR`.
    MissingRuntimeDirectory,
    /// A relative display name contained path traversal or extra components.
    InvalidDisplayName,
    /// The resolved path is absent or is not a Unix-domain socket.
    MissingOrNotSocket,
    /// The resolved path exists and is a Unix-domain socket.
    Available,
}

/// Runtime evidence consumed by the fail-closed output capability evaluator.
///
/// [`Self::from_process_environment`] can only prove the session marker and
/// socket. A native Wayland adapter must authoritatively populate the protocol
/// fields; leaving them [`ProbeState::Unknown`] keeps the provider unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WaylandRuntimeFacts {
    pub wayland_session: ProbeState,
    pub display_socket: DisplaySocketState,
    pub wl_output: ProbeState,
    pub xdg_output: ProbeState,
    pub fractional_scale: ProbeState,
    pub mutter_virtual_output: ProbeState,
}

impl WaylandRuntimeFacts {
    /// Collects only facts that can be proven without a native Wayland or
    /// D-Bus dependency.
    #[must_use]
    pub fn from_process_environment() -> Self {
        let wayland_session = match std::env::var("XDG_SESSION_TYPE") {
            Ok(value) if value.eq_ignore_ascii_case("wayland") => ProbeState::Available,
            Ok(_) => ProbeState::Unavailable,
            Err(_) => ProbeState::Unknown,
        };
        Self {
            wayland_session,
            display_socket: probe_display_socket(
                std::env::var_os("WAYLAND_DISPLAY").as_deref(),
                std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
            ),
            wl_output: ProbeState::Unknown,
            xdg_output: ProbeState::Unknown,
            fractional_scale: ProbeState::Unknown,
            mutter_virtual_output: ProbeState::Unknown,
        }
    }
}

/// Why the Wayland output path must not be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WaylandOutputUnavailableReason {
    #[error("the binary was not compiled with the `wayland-provider` feature")]
    FeatureDisabled,
    #[error("the Wayland provider is supported only on Linux targets")]
    UnsupportedTarget,
    #[error("the graphical session type could not be authoritatively probed")]
    SessionTypeUnknown,
    #[error("the graphical session is not Wayland")]
    NotWaylandSession,
    #[error("WAYLAND_DISPLAY is not set")]
    MissingDisplayName,
    #[error("XDG_RUNTIME_DIR is required for the relative Wayland display name")]
    MissingRuntimeDirectory,
    #[error("WAYLAND_DISPLAY is not a safe socket name or absolute path")]
    InvalidDisplayName,
    #[error("the resolved Wayland display path is absent or is not a socket")]
    MissingDisplaySocket,
    #[error("the Wayland display socket was not probed")]
    DisplaySocketUnknown,
    #[error("wl_output availability could not be authoritatively probed")]
    WlOutputUnknown,
    #[error("the compositor does not advertise wl_output")]
    WlOutputUnavailable,
    #[error("xdg-output availability could not be authoritatively probed")]
    XdgOutputUnknown,
    #[error("the compositor does not advertise xdg-output logical regions")]
    XdgOutputUnavailable,
    #[error("no native Wayland output adapter is implemented in this binary")]
    NativeAdapterUnavailable,
}

/// Mutter virtual-output status is separate from ordinary output enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutterVirtualOutputCapability {
    Unknown,
    Unavailable,
    /// Mutter advertised a relevant capability, but Arcen has no implementation.
    DetectedButUnimplemented,
    Implemented,
}

/// Compositor detection facts an actual Wayland output implementation may
/// expose.
///
/// These are *not* [`arcen_outputs::OutputCapabilities`] and must never become
/// them: `enumerate_outputs`, `xdg_output_logical_regions`, and
/// `mutter_virtual_output` describe what this compositor advertises, not what
/// a provider promises about the resulting desktop. ADR 0010 keeps the two
/// vocabularies apart, and section "Wayland capability contract" records how a
/// future native provider would map one onto the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaylandOutputCapabilities {
    pub enumerate_outputs: bool,
    pub xdg_output_logical_regions: bool,
    pub fractional_scale: bool,
    pub mutter_virtual_output: MutterVirtualOutputCapability,
}

/// Complete detection result, including optional evidence that does not
/// authorize selecting the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputCapabilityReport {
    availability: Result<WaylandOutputCapabilities, WaylandOutputUnavailableReason>,
    fractional_scale: ProbeState,
    mutter_virtual_output: MutterVirtualOutputCapability,
}

impl OutputCapabilityReport {
    pub const fn availability(
        &self,
    ) -> &Result<WaylandOutputCapabilities, WaylandOutputUnavailableReason> {
        &self.availability
    }

    #[must_use]
    pub const fn fractional_scale(&self) -> ProbeState {
        self.fractional_scale
    }

    #[must_use]
    pub const fn mutter_virtual_output(&self) -> MutterVirtualOutputCapability {
        self.mutter_virtual_output
    }
}

#[derive(Debug, Clone, Copy)]
struct OutputBuildFacts {
    feature_enabled: bool,
    linux_target: bool,
    native_adapter: bool,
    mutter_virtual_output_adapter: bool,
}

/// Evaluates the current binary and supplied runtime facts.
#[must_use]
pub fn detect_output_capability(facts: WaylandRuntimeFacts) -> OutputCapabilityReport {
    evaluate_output_capability(
        OutputBuildFacts {
            feature_enabled: WAYLAND_PROVIDER_FEATURE_ENABLED,
            linux_target: cfg!(target_os = "linux"),
            native_adapter: NATIVE_WAYLAND_OUTPUT_ADAPTER_IMPLEMENTED,
            mutter_virtual_output_adapter: MUTTER_VIRTUAL_OUTPUT_ADAPTER_IMPLEMENTED,
        },
        facts,
    )
}

fn evaluate_output_capability(
    build: OutputBuildFacts,
    facts: WaylandRuntimeFacts,
) -> OutputCapabilityReport {
    let mutter_virtual_output = match facts.mutter_virtual_output {
        ProbeState::Unknown => MutterVirtualOutputCapability::Unknown,
        ProbeState::Unavailable => MutterVirtualOutputCapability::Unavailable,
        ProbeState::Available if build.mutter_virtual_output_adapter => {
            MutterVirtualOutputCapability::Implemented
        }
        ProbeState::Available => MutterVirtualOutputCapability::DetectedButUnimplemented,
    };
    let availability = evaluate_output_availability(build, facts, mutter_virtual_output);
    OutputCapabilityReport {
        availability,
        fractional_scale: facts.fractional_scale,
        mutter_virtual_output,
    }
}

fn evaluate_output_availability(
    build: OutputBuildFacts,
    facts: WaylandRuntimeFacts,
    mutter_virtual_output: MutterVirtualOutputCapability,
) -> Result<WaylandOutputCapabilities, WaylandOutputUnavailableReason> {
    if !build.feature_enabled {
        return Err(WaylandOutputUnavailableReason::FeatureDisabled);
    }
    if !build.linux_target {
        return Err(WaylandOutputUnavailableReason::UnsupportedTarget);
    }
    match facts.wayland_session {
        ProbeState::Unknown => return Err(WaylandOutputUnavailableReason::SessionTypeUnknown),
        ProbeState::Unavailable => {
            return Err(WaylandOutputUnavailableReason::NotWaylandSession);
        }
        ProbeState::Available => {}
    }
    match facts.display_socket {
        DisplaySocketState::Unknown => {
            return Err(WaylandOutputUnavailableReason::DisplaySocketUnknown);
        }
        DisplaySocketState::MissingDisplayName => {
            return Err(WaylandOutputUnavailableReason::MissingDisplayName);
        }
        DisplaySocketState::MissingRuntimeDirectory => {
            return Err(WaylandOutputUnavailableReason::MissingRuntimeDirectory);
        }
        DisplaySocketState::InvalidDisplayName => {
            return Err(WaylandOutputUnavailableReason::InvalidDisplayName);
        }
        DisplaySocketState::MissingOrNotSocket => {
            return Err(WaylandOutputUnavailableReason::MissingDisplaySocket);
        }
        DisplaySocketState::Available => {}
    }
    match facts.wl_output {
        ProbeState::Unknown => return Err(WaylandOutputUnavailableReason::WlOutputUnknown),
        ProbeState::Unavailable => {
            return Err(WaylandOutputUnavailableReason::WlOutputUnavailable);
        }
        ProbeState::Available => {}
    }
    match facts.xdg_output {
        ProbeState::Unknown => return Err(WaylandOutputUnavailableReason::XdgOutputUnknown),
        ProbeState::Unavailable => {
            return Err(WaylandOutputUnavailableReason::XdgOutputUnavailable);
        }
        ProbeState::Available => {}
    }
    if !build.native_adapter {
        return Err(WaylandOutputUnavailableReason::NativeAdapterUnavailable);
    }
    Ok(WaylandOutputCapabilities {
        enumerate_outputs: true,
        xdg_output_logical_regions: true,
        fractional_scale: facts.fractional_scale == ProbeState::Available,
        mutter_virtual_output,
    })
}

fn probe_display_socket(
    display: Option<&OsStr>,
    runtime_directory: Option<&OsStr>,
) -> DisplaySocketState {
    let Some(display) = display else {
        return DisplaySocketState::MissingDisplayName;
    };
    let display = PathBuf::from(display);
    let socket = if display.is_absolute() {
        display
    } else {
        if display.components().count() != 1
            || !matches!(display.components().next(), Some(Component::Normal(_)))
        {
            return DisplaySocketState::InvalidDisplayName;
        }
        let Some(runtime_directory) = runtime_directory else {
            return DisplaySocketState::MissingRuntimeDirectory;
        };
        PathBuf::from(runtime_directory).join(display)
    };
    if path_is_socket(&socket) {
        DisplaySocketState::Available
    } else {
        DisplaySocketState::MissingOrNotSocket
    }
}

#[cfg(unix)]
fn path_is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;

    path.metadata()
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn path_is_socket(_path: &Path) -> bool {
    false
}

/// Host-local enumeration seam. No implementation is supplied by this tranche.
///
/// This is a *detection and selection* interface, not a transaction: it
/// enumerates what the compositor already has. It deliberately does not
/// implement [`arcen_outputs::OutputProvider`], which stages, binds, verifies,
/// commits, and rolls back a topology. ADR 0010 renamed it out of that
/// collision. When a native Wayland or Mutter output provider is separately
/// approved, it becomes a second Linux implementer of the shared lifecycle and
/// uses this seam to enumerate; it does not replace it.
#[cfg(feature = "wayland-provider")]
pub trait WaylandOutputSource {
    type Error: Error + Send + Sync + 'static;

    fn capabilities(&self) -> WaylandOutputCapabilities;
    fn snapshot(&mut self) -> Result<WaylandOutputSnapshot, Self::Error>;
}

/// Nonzero registry global name for one `wl_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WlOutputGlobalName(NonZeroU32);

impl WlOutputGlobalName {
    /// # Errors
    ///
    /// Returns [`WaylandModelError::ZeroGlobalName`] for zero.
    pub const fn new(value: u32) -> Result<Self, WaylandModelError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(WaylandModelError::ZeroGlobalName),
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// Positive integer scale reported by `wl_output.scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WlOutputScale(NonZeroU32);

impl WlOutputScale {
    /// # Errors
    ///
    /// Returns [`WaylandModelError::ZeroIntegerScale`] for zero.
    pub const fn new(value: u32) -> Result<Self, WaylandModelError> {
        match NonZeroU32::new(value) {
            Some(value) => Ok(Self(value)),
            None => Err(WaylandModelError::ZeroIntegerScale),
        }
    }

    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }

    fn as_scale120(self) -> Result<Scale120, WaylandModelError> {
        let value = self
            .get()
            .checked_mul(Scale120::denominator())
            .ok_or(WaylandModelError::ScaleOverflow(self.get()))?;
        Scale120::new(value).map_err(WaylandModelError::Region)
    }
}

/// Current pre-transform pixel mode reported for a `wl_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WlOutputMode {
    physical_size: PhysicalSize,
}

impl WlOutputMode {
    /// # Errors
    ///
    /// Returns a region validation error for a zero width or height.
    pub fn new(width: u32, height: u32) -> Result<Self, WaylandModelError> {
        match PhysicalSize::new(width, height) {
            Ok(physical_size) => Ok(Self { physical_size }),
            Err(error) => Err(WaylandModelError::Region(error)),
        }
    }

    #[must_use]
    pub const fn physical_size(self) -> PhysicalSize {
        self.physical_size
    }
}

/// Integer logical output region supplied by `xdg-output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XdgOutputLogicalRegion {
    rect: LogicalRect,
}

impl XdgOutputLogicalRegion {
    /// Converts whole compositor logical pixels to Arcen's 1/120 fixed-point
    /// logical coordinate domain.
    ///
    /// # Errors
    ///
    /// Returns a region validation error for an empty extent or overflow.
    pub fn from_pixels(x: i32, y: i32, width: u32, height: u32) -> Result<Self, WaylandModelError> {
        let origin = LogicalPoint::from_pixels(i64::from(x), i64::from(y))?;
        let size = LogicalSize::from_pixels(u64::from(width), u64::from(height))?;
        Ok(Self {
            rect: LogicalRect::new(origin, size)?,
        })
    }

    #[must_use]
    pub const fn rect(self) -> LogicalRect {
        self.rect
    }
}

/// Output-associated fractional preference represented in protocol units of
/// 1/120.
///
/// `fractional-scale-v1` preferences are surface-scoped. A future native
/// provider must not attach one to an output unless it has an authoritative
/// compositor-specific association for that surface/output pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FractionalScalePreference(Scale120);

impl FractionalScalePreference {
    #[must_use]
    pub const fn new(scale: Scale120) -> Self {
        Self(scale)
    }

    #[must_use]
    pub const fn scale(self) -> Scale120 {
        self.0
    }
}

/// The eight transforms from the `wl_output.transform` protocol enum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WlOutputTransform {
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

impl TryFrom<u32> for WlOutputTransform {
    type Error = WaylandModelError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Normal),
            1 => Ok(Self::Rotate90),
            2 => Ok(Self::Rotate180),
            3 => Ok(Self::Rotate270),
            4 => Ok(Self::Flipped),
            5 => Ok(Self::Flipped90),
            6 => Ok(Self::Flipped180),
            7 => Ok(Self::Flipped270),
            value => Err(WaylandModelError::UnknownTransform(value)),
        }
    }
}

impl From<WlOutputTransform> for OutputTransform {
    fn from(value: WlOutputTransform) -> Self {
        match value {
            WlOutputTransform::Normal => Self::Normal,
            WlOutputTransform::Rotate90 => Self::Rotate90,
            WlOutputTransform::Rotate180 => Self::Rotate180,
            WlOutputTransform::Rotate270 => Self::Rotate270,
            WlOutputTransform::Flipped => Self::Flipped,
            WlOutputTransform::Flipped90 => Self::Flipped90,
            WlOutputTransform::Flipped180 => Self::Flipped180,
            WlOutputTransform::Flipped270 => Self::Flipped270,
        }
    }
}

/// One coherent `wl_output` + `xdg-output` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandOutput {
    region_id: RegionId,
    global_name: WlOutputGlobalName,
    output_identity: OutputIdentity,
    mode: WlOutputMode,
    logical_region: XdgOutputLogicalRegion,
    integer_scale: WlOutputScale,
    fractional_scale: Option<FractionalScalePreference>,
    transform: WlOutputTransform,
    primary: bool,
}

impl WaylandOutput {
    /// # Errors
    ///
    /// Returns a region validation error for an invalid output identity.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        region_id: RegionId,
        global_name: WlOutputGlobalName,
        output_identity: impl Into<String>,
        mode: WlOutputMode,
        logical_region: XdgOutputLogicalRegion,
        integer_scale: WlOutputScale,
        fractional_scale: Option<FractionalScalePreference>,
        transform: WlOutputTransform,
        primary: bool,
    ) -> Result<Self, WaylandModelError> {
        Ok(Self {
            region_id,
            global_name,
            output_identity: OutputIdentity::new(output_identity)?,
            mode,
            logical_region,
            integer_scale,
            fractional_scale,
            transform,
            primary,
        })
    }

    #[must_use]
    pub const fn region_id(&self) -> RegionId {
        self.region_id
    }

    #[must_use]
    pub const fn global_name(&self) -> WlOutputGlobalName {
        self.global_name
    }

    #[must_use]
    pub const fn output_identity(&self) -> &OutputIdentity {
        &self.output_identity
    }

    #[must_use]
    pub const fn logical_region(&self) -> XdgOutputLogicalRegion {
        self.logical_region
    }

    /// Fractional preference wins only when a provider has authoritatively
    /// associated it with this output; otherwise the integer `wl_output`
    /// scale is converted to `Scale120`.
    ///
    /// # Errors
    ///
    /// Returns [`WaylandModelError::ScaleOverflow`] when an integer protocol
    /// scale cannot be represented in `Scale120`.
    pub fn effective_scale(&self) -> Result<Scale120, WaylandModelError> {
        match self.fractional_scale {
            Some(scale) => Ok(scale.scale()),
            None => self.integer_scale.as_scale120(),
        }
    }

    fn descriptor(&self) -> Result<RegionDescriptor, WaylandModelError> {
        Ok(RegionDescriptor::new(
            self.region_id,
            self.output_identity.clone(),
            self.logical_region.rect(),
            self.mode.physical_size(),
            self.effective_scale()?,
            self.transform.into(),
            self.primary,
        ))
    }
}

/// Complete coherent output generation from one provider roundtrip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandOutputSnapshot {
    outputs: Vec<WaylandOutput>,
    regions: RegionSet,
}

impl WaylandOutputSnapshot {
    /// Validates unique registry globals and the shared region-set invariants.
    ///
    /// # Errors
    ///
    /// Returns the first duplicate global or invalid region invariant.
    pub fn new(
        generation: RegionGeneration,
        outputs: Vec<WaylandOutput>,
    ) -> Result<Self, WaylandModelError> {
        let mut globals = BTreeSet::new();
        for output in &outputs {
            if !globals.insert(output.global_name) {
                return Err(WaylandModelError::DuplicateGlobalName(
                    output.global_name.get(),
                ));
            }
        }
        let descriptors = outputs
            .iter()
            .map(WaylandOutput::descriptor)
            .collect::<Result<Vec<_>, _>>()?;
        let regions = RegionSet::new(generation, descriptors)?;
        Ok(Self { outputs, regions })
    }

    #[must_use]
    pub fn outputs(&self) -> &[WaylandOutput] {
        &self.outputs
    }

    #[must_use]
    pub const fn regions(&self) -> &RegionSet {
        &self.regions
    }
}

/// Validation failure for the pure Wayland model.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WaylandModelError {
    #[error("wl_output registry global name must be nonzero")]
    ZeroGlobalName,
    #[error("wl_output integer scale must be nonzero")]
    ZeroIntegerScale,
    #[error("wl_output integer scale {0} overflows Scale120")]
    ScaleOverflow(u32),
    #[error("unknown wl_output transform value {0}")]
    UnknownTransform(u32),
    #[error("wl_output registry global {0} is duplicated in one snapshot")]
    DuplicateGlobalName(u32),
    #[error(transparent)]
    Region(#[from] RegionContractError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_facts() -> WaylandRuntimeFacts {
        WaylandRuntimeFacts {
            wayland_session: ProbeState::Available,
            display_socket: DisplaySocketState::Available,
            wl_output: ProbeState::Available,
            xdg_output: ProbeState::Available,
            fractional_scale: ProbeState::Available,
            mutter_virtual_output: ProbeState::Available,
        }
    }

    fn output(
        region_id: u32,
        global_name: u32,
        identity: &str,
        x: i32,
        primary: bool,
        transform: WlOutputTransform,
        fractional_scale: Option<u32>,
    ) -> WaylandOutput {
        WaylandOutput::new(
            RegionId::new(region_id).expect("region id"),
            WlOutputGlobalName::new(global_name).expect("global name"),
            identity,
            WlOutputMode::new(3840, 2160).expect("mode"),
            XdgOutputLogicalRegion::from_pixels(x, -20, 2560, 1440).expect("logical region"),
            WlOutputScale::new(2).expect("integer scale"),
            fractional_scale.map(|scale| {
                FractionalScalePreference::new(Scale120::new(scale).expect("fractional scale"))
            }),
            transform,
            primary,
        )
        .expect("output")
    }

    #[test]
    fn protocol_transform_values_map_to_shared_transform_values() {
        for (raw, expected) in OutputTransform::ALL.into_iter().enumerate() {
            let raw = u32::try_from(raw).expect("small transform index");
            let transform = WlOutputTransform::try_from(raw).expect("known transform");
            assert_eq!(OutputTransform::from(transform), expected);
        }
        assert_eq!(
            WlOutputTransform::try_from(8),
            Err(WaylandModelError::UnknownTransform(8))
        );
    }

    #[test]
    fn snapshot_preserves_xdg_logical_region_fractional_scale_and_transform() {
        let snapshot = WaylandOutputSnapshot::new(
            RegionGeneration::new(4).expect("generation"),
            vec![output(
                1,
                7,
                "DP-1",
                -2560,
                true,
                WlOutputTransform::Rotate90,
                Some(180),
            )],
        )
        .expect("snapshot");
        let region = snapshot.regions().primary();
        assert_eq!(
            region.logical_rect().origin(),
            LogicalPoint::from_pixels(-2560, -20).expect("origin")
        );
        assert_eq!(region.logical_rect().size().width(), 2560 * 120);
        assert_eq!(region.scale().get(), 180);
        assert_eq!(region.transform(), OutputTransform::Rotate90);
        let applied = region.expected_applied_size().expect("applied size");
        assert_eq!((applied.width(), applied.height()), (2160, 3840));
    }

    #[test]
    fn integer_scale_is_used_only_without_an_associated_fractional_preference() {
        let output = output(1, 7, "DP-1", 0, true, WlOutputTransform::Normal, None);
        assert_eq!(output.effective_scale().expect("scale").get(), 240);

        let overflow = WaylandOutput::new(
            RegionId::new(1).expect("region id"),
            WlOutputGlobalName::new(7).expect("global"),
            "DP-1",
            WlOutputMode::new(1, 1).expect("mode"),
            XdgOutputLogicalRegion::from_pixels(0, 0, 1, 1).expect("logical"),
            WlOutputScale::new(u32::MAX).expect("nonzero"),
            None,
            WlOutputTransform::Normal,
            true,
        )
        .expect("output");
        assert_eq!(
            overflow.effective_scale(),
            Err(WaylandModelError::ScaleOverflow(u32::MAX))
        );
    }

    #[test]
    fn snapshot_rejects_duplicate_registry_globals() {
        let outputs = vec![
            output(1, 7, "DP-1", 0, true, WlOutputTransform::Normal, None),
            output(2, 7, "DP-2", 2560, false, WlOutputTransform::Normal, None),
        ];
        assert_eq!(
            WaylandOutputSnapshot::new(RegionGeneration::new(1).expect("generation"), outputs),
            Err(WaylandModelError::DuplicateGlobalName(7))
        );
    }

    #[test]
    fn feature_and_runtime_detection_fail_closed_before_native_adapter_exists() {
        let disabled = evaluate_output_capability(
            OutputBuildFacts {
                feature_enabled: false,
                linux_target: true,
                native_adapter: false,
                mutter_virtual_output_adapter: false,
            },
            runtime_facts(),
        );
        assert_eq!(
            disabled.availability(),
            &Err(WaylandOutputUnavailableReason::FeatureDisabled)
        );

        let model_only = evaluate_output_capability(
            OutputBuildFacts {
                feature_enabled: true,
                linux_target: true,
                native_adapter: false,
                mutter_virtual_output_adapter: false,
            },
            runtime_facts(),
        );
        assert_eq!(
            model_only.availability(),
            &Err(WaylandOutputUnavailableReason::NativeAdapterUnavailable)
        );
        assert_eq!(
            model_only.mutter_virtual_output(),
            MutterVirtualOutputCapability::DetectedButUnimplemented
        );
        assert_eq!(model_only.fractional_scale(), ProbeState::Available);
    }

    #[test]
    fn current_binary_detection_obeys_compile_time_feature_and_target_gates() {
        let report = detect_output_capability(runtime_facts());
        #[cfg(not(feature = "wayland-provider"))]
        assert_eq!(
            report.availability(),
            &Err(WaylandOutputUnavailableReason::FeatureDisabled)
        );
        #[cfg(all(feature = "wayland-provider", not(target_os = "linux")))]
        assert_eq!(
            report.availability(),
            &Err(WaylandOutputUnavailableReason::UnsupportedTarget)
        );
        #[cfg(all(feature = "wayland-provider", target_os = "linux"))]
        assert_eq!(
            report.availability(),
            &Err(WaylandOutputUnavailableReason::NativeAdapterUnavailable)
        );
    }

    #[test]
    fn unknown_protocol_probe_never_becomes_runtime_support() {
        let report = evaluate_output_capability(
            OutputBuildFacts {
                feature_enabled: true,
                linux_target: true,
                native_adapter: true,
                mutter_virtual_output_adapter: false,
            },
            WaylandRuntimeFacts {
                wl_output: ProbeState::Unknown,
                ..runtime_facts()
            },
        );
        assert_eq!(
            report.availability(),
            &Err(WaylandOutputUnavailableReason::WlOutputUnknown)
        );
    }

    #[test]
    fn safe_display_name_probe_distinguishes_missing_runtime_and_traversal() {
        assert_eq!(
            probe_display_socket(Some(OsStr::new("wayland-0")), None),
            DisplaySocketState::MissingRuntimeDirectory
        );
        assert_eq!(
            probe_display_socket(
                Some(OsStr::new("../wayland-0")),
                Some(OsStr::new("/run/user/1"))
            ),
            DisplaySocketState::InvalidDisplayName
        );
        assert_eq!(
            probe_display_socket(None, Some(OsStr::new("/run/user/1"))),
            DisplaySocketState::MissingDisplayName
        );
    }

    #[test]
    fn snapshot_rejects_zero_protocol_values() {
        assert_eq!(
            WlOutputGlobalName::new(0),
            Err(WaylandModelError::ZeroGlobalName)
        );
        assert_eq!(
            WlOutputScale::new(0),
            Err(WaylandModelError::ZeroIntegerScale)
        );
    }
}
