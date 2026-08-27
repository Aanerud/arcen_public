use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::messages::{
    AuthRequest, AuthResponse, ClientHelloMsg, ClientMonitor, CursorMode, ServerHelloMsg,
    MAX_CLIENT_DISPLAY_ID_BYTES, MAX_MULTI_MONITOR_COUNT, MULTI_MONITOR_V1,
};

/// Bounded, control-character-free, non-empty opaque client display identifier
/// used by multi-monitor-v1 to correlate requested and applied monitors
/// without relying on the legacy numeric [`ClientMonitor::id`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(try_from = "String", into = "String")]
pub struct ClientDisplayId(String);

impl ClientDisplayId {
    /// Creates a validated opaque client display identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty, oversized, or contains a
    /// control character.
    pub fn new(value: impl Into<String>) -> Result<Self, ClientDisplayIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ClientDisplayIdError::Empty);
        }
        if value.len() > MAX_CLIENT_DISPLAY_ID_BYTES {
            return Err(ClientDisplayIdError::TooLong);
        }
        if value.chars().any(char::is_control) {
            return Err(ClientDisplayIdError::ControlCharacter);
        }
        Ok(Self(value))
    }

    /// Returns the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientDisplayId {
    type Error = ClientDisplayIdError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ClientDisplayId {
    type Error = ClientDisplayIdError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value.to_owned())
    }
}

impl From<ClientDisplayId> for String {
    fn from(value: ClientDisplayId) -> Self {
        value.0
    }
}

/// Invalid opaque client display identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientDisplayIdError {
    Empty,
    TooLong,
    ControlCharacter,
}

impl std::fmt::Display for ClientDisplayIdError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("client display id is empty"),
            Self::TooLong => formatter.write_str("client display id is too long"),
            Self::ControlCharacter => {
                formatter.write_str("client display id contains a control character")
            }
        }
    }
}

impl std::error::Error for ClientDisplayIdError {}

/// Clockwise monitor rotation for additive multi-monitor-v1 metadata.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "snake_case")]
pub enum RotationMsg {
    #[default]
    Degrees0,
    Degrees90,
    Degrees180,
    Degrees270,
}

impl RotationMsg {
    /// All wire rotations in their stable protocol order.
    pub const ALL: [Self; 4] = [
        Self::Degrees0,
        Self::Degrees90,
        Self::Degrees180,
        Self::Degrees270,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Degrees0 => "degrees0",
            Self::Degrees90 => "degrees90",
            Self::Degrees180 => "degrees180",
            Self::Degrees270 => "degrees270",
        }
    }
}

impl std::fmt::Display for RotationMsg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Local fullscreen policy used to derive the requested presentation size.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SafeAreaPolicyMsg {
    /// Standard native fullscreen respecting the system safe area.
    #[default]
    StandardFullscreen,
    /// Full-frame presentation over the whole display rectangle.
    FullFrame,
}

/// Client quality intent for one requested monitor. This never selects a GPU
/// or encoder; the host remains authoritative for resource assignment.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorQualityIntentMsg {
    /// Preserve the connection's ordinary profile and let host policy decide.
    #[default]
    HostDefault,
    /// The complete plan must provide full-color 4:4:4 for this monitor.
    FullColorRequired,
    /// This monitor may use a host-approved 4:2:0 hardware or software plan.
    BandwidthOptimized,
}

/// Rich requested monitor descriptor for additive multi-monitor-v1 setup.
///
/// `AuthResponse.monitors` remains the legacy primary-compatible roster. This
/// sidecar repeats those fields and adds the rotation/logical/safe-area facts
/// needed by future multi-monitor admission without changing old peers.
///
/// `x/y/logical_width/logical_height` are one requested logical desktop space.
/// `width_px/height_px` are the requested physical/backing-pixel stream extent
/// for that same monitor and must not be mixed into aggregate logical bounds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestedMonitorDescriptorMsg {
    /// Opaque requested monitor identity, stable enough to correlate a layout
    /// across reconnect and applied host mappings.
    pub client_display_id: ClientDisplayId,
    /// Legacy numeric [`ClientMonitor::id`] retained only to synthesize the
    /// compatibility roster in `AuthResponse.monitors`.
    pub client_monitor_id: u32,
    /// Requested logical desktop horizontal origin.
    pub x: i32,
    /// Requested logical desktop vertical origin.
    pub y: i32,
    /// Requested physical/backing-pixel stream width.
    pub width_px: u32,
    /// Requested physical/backing-pixel stream height.
    pub height_px: u32,
    /// Requested logical arrangement width matching `x/y`.
    pub logical_width: u32,
    /// Requested logical arrangement height matching `x/y`.
    pub logical_height: u32,
    pub scale: f32,
    #[serde(default)]
    pub refresh_hz: u32,
    #[serde(default)]
    pub rotation: RotationMsg,
    pub is_primary: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub width_mm: f32,
    #[serde(default)]
    pub height_mm: f32,
    #[serde(default)]
    pub vendor: u32,
    #[serde(default)]
    pub model: u32,
    #[serde(default)]
    pub serial: u32,
    #[serde(default)]
    pub edid: String,
    #[serde(default)]
    pub safe_area_policy: SafeAreaPolicyMsg,
    #[serde(default)]
    pub quality_intent: MonitorQualityIntentMsg,
}

impl RequestedMonitorDescriptorMsg {
    #[must_use]
    pub fn legacy_client_monitor(&self) -> ClientMonitor {
        ClientMonitor {
            id: self.client_monitor_id,
            x: self.x,
            y: self.y,
            width_px: self.width_px,
            height_px: self.height_px,
            scale: self.scale,
            refresh_hz: self.refresh_hz,
            is_primary: self.is_primary,
            name: self.name.clone(),
            width_mm: self.width_mm,
            height_mm: self.height_mm,
            vendor: self.vendor,
            model: self.model,
            serial: self.serial,
            edid: self.edid.clone(),
        }
    }
}

/// Reliable QUIC carrier shape under multi-monitor-v1 evaluation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MultiMonitorCarrierMsg {
    MuxedReliableStream,
    PerMonitorReliableStream,
}

impl MultiMonitorCarrierMsg {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MuxedReliableStream => "muxed_reliable_stream",
            Self::PerMonitorReliableStream => "per_monitor_reliable_stream",
        }
    }
}

impl std::fmt::Display for MultiMonitorCarrierMsg {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host topology backend kind for multi-monitor-v1 negotiation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TopologyBackendKindMsg {
    DedicatedXorg,
    PhysicalOutputs,
    VirtualOutputs,
}

/// Invalid multi-monitor-v1 requested/applied topology or capability metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiMonitorValidationError {
    UnsupportedMonitorCount(usize),
    InvalidMaxMonitors(u8),
    EmptySupportedRotations,
    EmptyAdvertisedCarriers,
    EmptyClientSupportedCarriers,
    DuplicateSupportedRotation(RotationMsg),
    DuplicateAdvertisedCarrier(MultiMonitorCarrierMsg),
    DuplicateClientSupportedCarrier(MultiMonitorCarrierMsg),
    NoCommonAuthCarrier,
    RequestedMonitorCountExceedsMax {
        count: usize,
        max_monitors: u8,
    },
    AppliedMonitorCountExceedsMax {
        count: usize,
        max_monitors: u8,
    },
    UnsupportedRequestedRotation {
        client_display_id: String,
        rotation: RotationMsg,
    },
    UnsupportedAppliedRotation {
        client_display_id: String,
        rotation: RotationMsg,
    },
    UnadvertisedSelectedCarrier(MultiMonitorCarrierMsg),
    PrimaryMonitorCount(usize),
    DuplicateClientDisplayId(String),
    DuplicateClientMonitorId(u32),
    ZeroSessionMonitorId,
    DuplicateSessionMonitorId(u16),
    InvalidRequestedPhysicalDimensions(String),
    InvalidRequestedLogicalDimensions(String),
    InvalidAppliedPixelDimensions(String),
    InvalidAppliedMediaDimensions(String),
    InvalidAppliedFps(String),
    InvalidAppliedBitrateKbps(String),
    ZeroAppliedStreamEpoch(String),
    InvalidAppliedEncoderBackend(String),
    InvalidAppliedEncoderClass(String),
    InvalidAppliedCodec(String, String),
    InvalidAppliedChroma(String, String),
    InvalidMonitorScale(String),
    InvalidDeclaredDesktopDimensions,
    ZeroTopologyGeneration,
    CoordinateOverflow(&'static str),
    InconsistentAppliedDesktopBounds,
    InconsistentAppliedDesktopTranslation,
}

impl std::fmt::Display for MultiMonitorValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedMonitorCount(count) => {
                write!(formatter, "expected 1..=4 monitors, found {count}")
            }
            Self::InvalidMaxMonitors(max_monitors) => {
                write!(
                    formatter,
                    "max_monitors must be 1..=4, found {max_monitors}"
                )
            }
            Self::EmptySupportedRotations => {
                formatter.write_str("supported_rotations must advertise at least one rotation")
            }
            Self::EmptyAdvertisedCarriers => {
                formatter.write_str("carriers must advertise at least one carrier")
            }
            Self::EmptyClientSupportedCarriers => formatter.write_str(
                "auth-time request carriers must include at least one client-supported carrier",
            ),
            Self::DuplicateSupportedRotation(rotation) => {
                write!(formatter, "duplicate supported rotation: {rotation}")
            }
            Self::DuplicateAdvertisedCarrier(carrier) => {
                write!(formatter, "duplicate advertised carrier: {carrier}")
            }
            Self::DuplicateClientSupportedCarrier(carrier) => {
                write!(formatter, "duplicate client supported carrier: {carrier}")
            }
            Self::NoCommonAuthCarrier => formatter.write_str(
                "auth-time request carriers share no common carrier with the advertised host offer",
            ),
            Self::RequestedMonitorCountExceedsMax {
                count,
                max_monitors,
            } => write!(
                formatter,
                "requested topology has {count} monitors but max_monitors advertises {max_monitors}"
            ),
            Self::AppliedMonitorCountExceedsMax {
                count,
                max_monitors,
            } => write!(
                formatter,
                "applied topology has {count} monitors but max_monitors advertises {max_monitors}"
            ),
            Self::UnsupportedRequestedRotation {
                client_display_id,
                rotation,
            } => write!(
                formatter,
                "requested monitor {client_display_id} uses unsupported rotation {rotation}"
            ),
            Self::UnsupportedAppliedRotation {
                client_display_id,
                rotation,
            } => write!(
                formatter,
                "applied monitor {client_display_id} uses unsupported rotation {rotation}"
            ),
            Self::UnadvertisedSelectedCarrier(carrier) => write!(
                formatter,
                "applied topology selected_carrier {carrier} was not advertised"
            ),
            Self::PrimaryMonitorCount(count) => {
                write!(formatter, "expected one primary monitor, found {count}")
            }
            Self::DuplicateClientDisplayId(id) => {
                write!(formatter, "duplicate client display id: {id}")
            }
            Self::DuplicateClientMonitorId(id) => {
                write!(formatter, "duplicate client monitor id: {id}")
            }
            Self::ZeroSessionMonitorId => formatter.write_str(
                "session_monitor_id must be 1..=65535; 0 is reserved for legacy single-monitor framing",
            ),
            Self::DuplicateSessionMonitorId(id) => {
                write!(formatter, "duplicate session monitor id: {id}")
            }
            Self::InvalidRequestedPhysicalDimensions(id) => {
                write!(
                    formatter,
                    "requested monitor {id} has zero physical dimensions"
                )
            }
            Self::InvalidRequestedLogicalDimensions(id) => {
                write!(
                    formatter,
                    "requested monitor {id} has zero logical dimensions"
                )
            }
            Self::InvalidAppliedPixelDimensions(id) => {
                write!(formatter, "applied monitor {id} has zero dimensions")
            }
            Self::InvalidAppliedMediaDimensions(id) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan has zero dimensions"
                )
            }
            Self::InvalidAppliedFps(id) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan fps must be nonzero"
                )
            }
            Self::InvalidAppliedBitrateKbps(id) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan bitrate_kbps must be nonzero"
                )
            }
            Self::ZeroAppliedStreamEpoch(id) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan stream_epoch must be nonzero"
                )
            }
            Self::InvalidAppliedEncoderBackend(id) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan encoder_backend must be nonempty"
                )
            }
            Self::InvalidAppliedEncoderClass(id) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan encoder_class must be hardware or software"
                )
            }
            Self::InvalidAppliedCodec(id, codec) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan codec {codec:?} is unsupported"
                )
            }
            Self::InvalidAppliedChroma(id, chroma) => {
                write!(
                    formatter,
                    "applied monitor {id} media plan chroma {chroma:?} is unsupported"
                )
            }
            Self::InvalidMonitorScale(id) => {
                write!(formatter, "requested monitor {id} has invalid scale")
            }
            Self::InvalidDeclaredDesktopDimensions => {
                formatter.write_str("declared desktop bounds have zero dimensions")
            }
            Self::ZeroTopologyGeneration => {
                formatter.write_str("topology generation must be nonzero")
            }
            Self::CoordinateOverflow(context) => {
                write!(formatter, "{context} overflowed the signed desktop domain")
            }
            Self::InconsistentAppliedDesktopBounds => formatter
                .write_str("applied monitor rectangles do not match declared desktop bounds"),
            Self::InconsistentAppliedDesktopTranslation => formatter.write_str(
                "declared desktop translation is inconsistent with the declared desktop origin",
            ),
        }
    }
}

impl std::error::Error for MultiMonitorValidationError {}

/// Failure while attaching multi-monitor-v1 capability sidecars.
#[derive(Debug)]
pub enum MultiMonitorCapabilityError {
    Validation(MultiMonitorValidationError),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for MultiMonitorCapabilityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(error) => {
                write!(formatter, "invalid multi-monitor capability: {error}")
            }
            Self::Serialize(error) => {
                write!(
                    formatter,
                    "failed to serialize multi-monitor capability: {error}"
                )
            }
        }
    }
}

impl std::error::Error for MultiMonitorCapabilityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            Self::Serialize(error) => Some(error),
        }
    }
}

impl From<MultiMonitorValidationError> for MultiMonitorCapabilityError {
    fn from(error: MultiMonitorValidationError) -> Self {
        Self::Validation(error)
    }
}

impl From<serde_json::Error> for MultiMonitorCapabilityError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

fn validate_multi_monitor_count(count: usize) -> Result<(), MultiMonitorValidationError> {
    if count == 0 || count > MAX_MULTI_MONITOR_COUNT {
        return Err(MultiMonitorValidationError::UnsupportedMonitorCount(count));
    }
    Ok(())
}

fn validate_session_monitor_id(session_monitor_id: u16) -> Result<(), MultiMonitorValidationError> {
    if session_monitor_id == 0 {
        return Err(MultiMonitorValidationError::ZeroSessionMonitorId);
    }
    Ok(())
}

fn validate_applied_monitor_descriptor(
    monitor: &AppliedMonitorDescriptorMsg,
) -> Result<CheckedLayoutRect, MultiMonitorValidationError> {
    let display_id = monitor.client_display_id.as_str();
    validate_session_monitor_id(monitor.session_monitor_id)?;
    let pixel_rect = CheckedLayoutRect::new(
        monitor.x,
        monitor.y,
        monitor.width_px,
        monitor.height_px,
        MultiMonitorValidationError::InvalidAppliedPixelDimensions(display_id.to_owned()),
        "applied monitor pixel bounds",
    )?;
    if monitor.media_plan.width_px == 0 || monitor.media_plan.height_px == 0 {
        return Err(MultiMonitorValidationError::InvalidAppliedMediaDimensions(
            display_id.to_owned(),
        ));
    }
    if monitor.media_plan.fps == 0 {
        return Err(MultiMonitorValidationError::InvalidAppliedFps(
            display_id.to_owned(),
        ));
    }
    if monitor.media_plan.bitrate_kbps == 0 {
        return Err(MultiMonitorValidationError::InvalidAppliedBitrateKbps(
            display_id.to_owned(),
        ));
    }
    if monitor.media_plan.stream_epoch == 0 {
        return Err(MultiMonitorValidationError::ZeroAppliedStreamEpoch(
            display_id.to_owned(),
        ));
    }
    if monitor.media_plan.encoder_backend.is_empty() {
        return Err(MultiMonitorValidationError::InvalidAppliedEncoderBackend(
            display_id.to_owned(),
        ));
    }
    if !matches!(
        monitor.media_plan.encoder_class.as_str(),
        "hardware" | "software"
    ) {
        return Err(MultiMonitorValidationError::InvalidAppliedEncoderClass(
            display_id.to_owned(),
        ));
    }
    if !matches!(
        monitor.media_plan.codec.as_str(),
        "jpeg" | "h264" | "h265" | "vp9" | "av1"
    ) {
        return Err(MultiMonitorValidationError::InvalidAppliedCodec(
            display_id.to_owned(),
            monitor.media_plan.codec.clone(),
        ));
    }
    if !matches!(
        monitor.media_plan.chroma.as_str(),
        "yuv420" | "yuv422" | "yuv444"
    ) {
        return Err(MultiMonitorValidationError::InvalidAppliedChroma(
            display_id.to_owned(),
            monitor.media_plan.chroma.clone(),
        ));
    }
    Ok(pixel_rect)
}

fn validate_max_monitors(max_monitors: u8) -> Result<(), MultiMonitorValidationError> {
    if !(1..=MAX_MULTI_MONITOR_COUNT as u8).contains(&max_monitors) {
        return Err(MultiMonitorValidationError::InvalidMaxMonitors(
            max_monitors,
        ));
    }
    Ok(())
}

fn validate_supported_rotations(
    supported_rotations: &[RotationMsg],
) -> Result<(), MultiMonitorValidationError> {
    if supported_rotations.is_empty() {
        return Err(MultiMonitorValidationError::EmptySupportedRotations);
    }
    let mut seen = std::collections::BTreeSet::new();
    for rotation in supported_rotations {
        if !seen.insert(*rotation) {
            return Err(MultiMonitorValidationError::DuplicateSupportedRotation(
                *rotation,
            ));
        }
    }
    Ok(())
}

fn validate_advertised_carriers(
    carriers: &[MultiMonitorCarrierMsg],
) -> Result<(), MultiMonitorValidationError> {
    if carriers.is_empty() {
        return Err(MultiMonitorValidationError::EmptyAdvertisedCarriers);
    }
    let mut seen = std::collections::BTreeSet::new();
    for carrier in carriers {
        if !seen.insert(*carrier) {
            return Err(MultiMonitorValidationError::DuplicateAdvertisedCarrier(
                *carrier,
            ));
        }
    }
    Ok(())
}

fn validate_client_supported_carriers(
    carriers: &[MultiMonitorCarrierMsg],
) -> Result<(), MultiMonitorValidationError> {
    if carriers.is_empty() {
        return Err(MultiMonitorValidationError::EmptyClientSupportedCarriers);
    }
    let mut seen = std::collections::BTreeSet::new();
    for carrier in carriers {
        if !seen.insert(*carrier) {
            return Err(MultiMonitorValidationError::DuplicateClientSupportedCarrier(*carrier));
        }
    }
    Ok(())
}

fn validate_multi_monitor_advertisement(
    max_monitors: u8,
    supported_rotations: &[RotationMsg],
    carriers: &[MultiMonitorCarrierMsg],
) -> Result<(), MultiMonitorValidationError> {
    validate_max_monitors(max_monitors)?;
    validate_supported_rotations(supported_rotations)?;
    validate_advertised_carriers(carriers)?;
    Ok(())
}

fn validate_requested_topology_against_advertisement(
    max_monitors: u8,
    supported_rotations: &[RotationMsg],
    topology: &RequestedMonitorTopologyMsg,
) -> Result<(), MultiMonitorValidationError> {
    let _ = topology.validate()?;
    let count = topology.monitors().len();
    if count > usize::from(max_monitors) {
        return Err(
            MultiMonitorValidationError::RequestedMonitorCountExceedsMax {
                count,
                max_monitors,
            },
        );
    }
    for monitor in topology.monitors() {
        if !supported_rotations.contains(&monitor.rotation) {
            return Err(MultiMonitorValidationError::UnsupportedRequestedRotation {
                client_display_id: monitor.client_display_id.as_str().to_owned(),
                rotation: monitor.rotation,
            });
        }
    }
    Ok(())
}

fn validate_applied_topology_against_advertisement(
    max_monitors: u8,
    supported_rotations: &[RotationMsg],
    carriers: &[MultiMonitorCarrierMsg],
    topology: &AppliedMonitorTopologyMsg,
) -> Result<(), MultiMonitorValidationError> {
    let _ = topology.validate()?;
    let count = topology.monitors().len();
    if count > usize::from(max_monitors) {
        return Err(MultiMonitorValidationError::AppliedMonitorCountExceedsMax {
            count,
            max_monitors,
        });
    }
    if !carriers.contains(&topology.selected_carrier()) {
        return Err(MultiMonitorValidationError::UnadvertisedSelectedCarrier(
            topology.selected_carrier(),
        ));
    }
    for monitor in topology.monitors() {
        if !supported_rotations.contains(&monitor.rotation) {
            return Err(MultiMonitorValidationError::UnsupportedAppliedRotation {
                client_display_id: monitor.client_display_id.as_str().to_owned(),
                rotation: monitor.rotation,
            });
        }
    }
    Ok(())
}

fn validate_layout_end(
    origin: i32,
    extent: u32,
    context: &'static str,
) -> Result<(), MultiMonitorValidationError> {
    let end = i64::from(origin)
        .checked_add(i64::from(extent))
        .ok_or(MultiMonitorValidationError::CoordinateOverflow(context))?;
    if end > i64::from(i32::MAX) + 1 {
        return Err(MultiMonitorValidationError::CoordinateOverflow(context));
    }
    Ok(())
}

fn checked_i64_to_i32(
    value: i64,
    context: &'static str,
) -> Result<i32, MultiMonitorValidationError> {
    i32::try_from(value).map_err(|_| MultiMonitorValidationError::CoordinateOverflow(context))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckedLayoutRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl CheckedLayoutRect {
    fn new(
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        zero_dimensions: MultiMonitorValidationError,
        overflow_context: &'static str,
    ) -> Result<Self, MultiMonitorValidationError> {
        if width == 0 || height == 0 {
            return Err(zero_dimensions);
        }
        validate_layout_end(x, width, overflow_context)?;
        validate_layout_end(y, height, overflow_context)?;
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    fn right_exclusive(self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    fn bottom_exclusive(self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CheckedLayoutBounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl CheckedLayoutBounds {
    fn from_rects(
        rectangles: &[CheckedLayoutRect],
        overflow_context: &'static str,
    ) -> Result<Self, MultiMonitorValidationError> {
        let Some(first) = rectangles.first().copied() else {
            return Err(MultiMonitorValidationError::UnsupportedMonitorCount(0));
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
        let width = u32::try_from(max_right.checked_sub(min_x).ok_or(
            MultiMonitorValidationError::CoordinateOverflow(overflow_context),
        )?)
        .map_err(|_| MultiMonitorValidationError::CoordinateOverflow(overflow_context))?;
        let height = u32::try_from(max_bottom.checked_sub(min_y).ok_or(
            MultiMonitorValidationError::CoordinateOverflow(overflow_context),
        )?)
        .map_err(|_| MultiMonitorValidationError::CoordinateOverflow(overflow_context))?;
        Ok(Self {
            x: checked_i64_to_i32(min_x, overflow_context)?,
            y: checked_i64_to_i32(min_y, overflow_context)?,
            width,
            height,
        })
    }

    fn translated(
        self,
        dx: i64,
        dy: i64,
        overflow_context: &'static str,
    ) -> Result<Self, MultiMonitorValidationError> {
        Ok(Self {
            x: checked_i64_to_i32(
                i64::from(self.x).checked_add(dx).ok_or(
                    MultiMonitorValidationError::CoordinateOverflow(overflow_context),
                )?,
                overflow_context,
            )?,
            y: checked_i64_to_i32(
                i64::from(self.y).checked_add(dy).ok_or(
                    MultiMonitorValidationError::CoordinateOverflow(overflow_context),
                )?,
                overflow_context,
            )?,
            width: self.width,
            height: self.height,
        })
    }

    fn translation_to_origin(self) -> (i64, i64) {
        (
            if self.x < 0 { -i64::from(self.x) } else { 0 },
            if self.y < 0 { -i64::from(self.y) } else { 0 },
        )
    }
}

fn validate_requested_topology(
    monitors: &[RequestedMonitorDescriptorMsg],
) -> Result<usize, MultiMonitorValidationError> {
    validate_multi_monitor_count(monitors.len())?;
    let mut client_display_ids = std::collections::BTreeSet::new();
    let mut client_monitor_ids = std::collections::BTreeSet::new();
    let mut logical_rects = Vec::with_capacity(monitors.len());
    let mut primary_count = 0usize;
    let mut primary_index = None;
    for (index, monitor) in monitors.iter().enumerate() {
        let display_id = monitor.client_display_id.as_str();
        if !client_display_ids.insert(display_id) {
            return Err(MultiMonitorValidationError::DuplicateClientDisplayId(
                display_id.to_owned(),
            ));
        }
        if !client_monitor_ids.insert(monitor.client_monitor_id) {
            return Err(MultiMonitorValidationError::DuplicateClientMonitorId(
                monitor.client_monitor_id,
            ));
        }
        if !monitor.scale.is_finite() || monitor.scale <= 0.0 {
            return Err(MultiMonitorValidationError::InvalidMonitorScale(
                display_id.to_owned(),
            ));
        }
        logical_rects.push(CheckedLayoutRect::new(
            monitor.x,
            monitor.y,
            monitor.logical_width,
            monitor.logical_height,
            MultiMonitorValidationError::InvalidRequestedLogicalDimensions(display_id.to_owned()),
            "requested monitor logical bounds",
        )?);
        if monitor.width_px == 0 || monitor.height_px == 0 {
            return Err(
                MultiMonitorValidationError::InvalidRequestedPhysicalDimensions(
                    display_id.to_owned(),
                ),
            );
        }
        if monitor.is_primary {
            primary_count += 1;
            primary_index = Some(index);
        }
    }
    let Some(primary_index) = primary_index else {
        return Err(MultiMonitorValidationError::PrimaryMonitorCount(0));
    };
    if primary_count != 1 {
        return Err(MultiMonitorValidationError::PrimaryMonitorCount(
            primary_count,
        ));
    }
    let _ = CheckedLayoutBounds::from_rects(&logical_rects, "requested monitor logical bounds")?;
    Ok(primary_index)
}

fn validate_declared_desktop_bounds(
    desktop_x: i32,
    desktop_y: i32,
    desktop_width_px: u32,
    desktop_height_px: u32,
) -> Result<CheckedLayoutBounds, MultiMonitorValidationError> {
    let _ = CheckedLayoutRect::new(
        desktop_x,
        desktop_y,
        desktop_width_px,
        desktop_height_px,
        MultiMonitorValidationError::InvalidDeclaredDesktopDimensions,
        "declared desktop bounds",
    )?;
    Ok(CheckedLayoutBounds {
        x: desktop_x,
        y: desktop_y,
        width: desktop_width_px,
        height: desktop_height_px,
    })
}

fn validate_declared_desktop_translation(
    desktop_bounds: CheckedLayoutBounds,
    translation_x: i64,
    translation_y: i64,
) -> Result<(), MultiMonitorValidationError> {
    let source_bounds = desktop_bounds.translated(
        translation_x
            .checked_neg()
            .ok_or(MultiMonitorValidationError::CoordinateOverflow(
                "declared desktop translation",
            ))?,
        translation_y
            .checked_neg()
            .ok_or(MultiMonitorValidationError::CoordinateOverflow(
                "declared desktop translation",
            ))?,
        "declared desktop translation",
    )?;
    if source_bounds.translation_to_origin() != (translation_x, translation_y) {
        return Err(MultiMonitorValidationError::InconsistentAppliedDesktopTranslation);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_applied_topology(
    topology_generation: u64,
    desktop_x: i32,
    desktop_y: i32,
    desktop_width_px: u32,
    desktop_height_px: u32,
    translation_x: i64,
    translation_y: i64,
    monitors: &[AppliedMonitorDescriptorMsg],
) -> Result<usize, MultiMonitorValidationError> {
    if topology_generation == 0 {
        return Err(MultiMonitorValidationError::ZeroTopologyGeneration);
    }
    validate_multi_monitor_count(monitors.len())?;
    let mut client_display_ids = std::collections::BTreeSet::new();
    let mut session_monitor_ids = std::collections::BTreeSet::new();
    let mut pixel_rects = Vec::with_capacity(monitors.len());
    let mut primary_count = 0usize;
    let mut primary_index = None;
    for (index, monitor) in monitors.iter().enumerate() {
        let display_id = monitor.client_display_id.as_str();
        if !client_display_ids.insert(display_id) {
            return Err(MultiMonitorValidationError::DuplicateClientDisplayId(
                display_id.to_owned(),
            ));
        }
        let pixel_rect = validate_applied_monitor_descriptor(monitor)?;
        if !session_monitor_ids.insert(monitor.session_monitor_id) {
            return Err(MultiMonitorValidationError::DuplicateSessionMonitorId(
                monitor.session_monitor_id,
            ));
        }
        pixel_rects.push(pixel_rect);
        if monitor.is_primary {
            primary_count += 1;
            primary_index = Some(index);
        }
    }
    let Some(primary_index) = primary_index else {
        return Err(MultiMonitorValidationError::PrimaryMonitorCount(0));
    };
    if primary_count != 1 {
        return Err(MultiMonitorValidationError::PrimaryMonitorCount(
            primary_count,
        ));
    }
    let declared_bounds = validate_declared_desktop_bounds(
        desktop_x,
        desktop_y,
        desktop_width_px,
        desktop_height_px,
    )?;
    if CheckedLayoutBounds::from_rects(&pixel_rects, "applied monitor pixel bounds")?
        != declared_bounds
    {
        return Err(MultiMonitorValidationError::InconsistentAppliedDesktopBounds);
    }
    validate_declared_desktop_translation(declared_bounds, translation_x, translation_y)?;
    Ok(primary_index)
}

/// Full requested multi-monitor-v1 roster carried inside the auth-time request
/// wrapper and as an additive `client_hello.device_capabilities.multi_monitor_v1`
/// echo.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    try_from = "RequestedMonitorTopologyWire",
    into = "RequestedMonitorTopologyWire"
)]
pub struct RequestedMonitorTopologyMsg {
    monitors: Vec<RequestedMonitorDescriptorMsg>,
    primary_index: usize,
}

impl RequestedMonitorTopologyMsg {
    /// Creates a validated requested multi-monitor-v1 roster.
    ///
    /// # Errors
    ///
    /// Returns an error when the roster violates the shared multi-monitor-v1
    /// invariants.
    pub fn new(
        monitors: Vec<RequestedMonitorDescriptorMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        let primary_index = validate_requested_topology(&monitors)?;
        Ok(Self {
            monitors,
            primary_index,
        })
    }

    fn validate(&self) -> Result<usize, MultiMonitorValidationError> {
        validate_requested_topology(&self.monitors)
    }

    fn validated_primary(
        &self,
    ) -> Result<&RequestedMonitorDescriptorMsg, MultiMonitorValidationError> {
        let primary_index = self.validate()?;
        Ok(&self.monitors[primary_index])
    }

    #[must_use]
    pub fn monitors(&self) -> &[RequestedMonitorDescriptorMsg] {
        &self.monitors
    }

    #[must_use]
    pub fn primary(&self) -> &RequestedMonitorDescriptorMsg {
        &self.monitors[self.primary_index]
    }

    #[must_use]
    pub fn legacy_monitors(&self) -> Vec<ClientMonitor> {
        self.monitors
            .iter()
            .map(RequestedMonitorDescriptorMsg::legacy_client_monitor)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RequestedMonitorTopologyWire {
    #[serde(default)]
    monitors: Vec<RequestedMonitorDescriptorMsg>,
}

impl TryFrom<RequestedMonitorTopologyWire> for RequestedMonitorTopologyMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: RequestedMonitorTopologyWire) -> Result<Self, Self::Error> {
        Self::new(wire.monitors)
    }
}

impl From<RequestedMonitorTopologyMsg> for RequestedMonitorTopologyWire {
    fn from(value: RequestedMonitorTopologyMsg) -> Self {
        Self {
            monitors: value.monitors,
        }
    }
}

/// Concrete applied per-monitor media truth mirrored in `server_hello`.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppliedMonitorMediaPlanMsg {
    /// Nonzero epoch fencing this monitor's encoded stream. A restarted
    /// pipeline must advertise a new epoch before sending frames.
    pub stream_epoch: u64,
    pub encoder_backend: String,
    pub encoder_class: String,
    pub codec: String,
    pub chroma: String,
    pub width_px: u32,
    pub height_px: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    #[serde(default, skip_serializing_if = "is_local_cursor_mode")]
    pub cursor_mode: CursorMode,
    #[serde(default, skip_serializing_if = "is_false")]
    pub degraded: bool,
}

/// Exact applied monitor descriptor carried inside
/// `server_hello.device_capabilities.multi_monitor_v1`.
///
/// Unlike [`RequestedMonitorDescriptorMsg`], `x/y/width_px/height_px` here are
/// one coherent applied host-pixel rectangle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    try_from = "AppliedMonitorDescriptorWire",
    into = "AppliedMonitorDescriptorWire"
)]
pub struct AppliedMonitorDescriptorMsg {
    pub client_display_id: ClientDisplayId,
    /// Nonzero host-assigned session monitor id in the range `1..=65535`.
    /// `0` stays reserved for legacy single-monitor video headers.
    pub session_monitor_id: u16,
    /// Applied host desktop horizontal origin in pixels.
    pub x: i32,
    /// Applied host desktop vertical origin in pixels.
    pub y: i32,
    /// Applied host rectangle width in pixels.
    pub width_px: u32,
    /// Applied host rectangle height in pixels.
    pub height_px: u32,
    #[serde(default)]
    pub refresh_hz: u32,
    #[serde(default)]
    pub rotation: RotationMsg,
    pub is_primary: bool,
    pub media_plan: AppliedMonitorMediaPlanMsg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AppliedMonitorDescriptorWire {
    client_display_id: ClientDisplayId,
    session_monitor_id: u16,
    x: i32,
    y: i32,
    width_px: u32,
    height_px: u32,
    #[serde(default)]
    refresh_hz: u32,
    #[serde(default)]
    rotation: RotationMsg,
    is_primary: bool,
    media_plan: AppliedMonitorMediaPlanMsg,
}

impl TryFrom<AppliedMonitorDescriptorWire> for AppliedMonitorDescriptorMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: AppliedMonitorDescriptorWire) -> Result<Self, Self::Error> {
        let descriptor = Self {
            client_display_id: wire.client_display_id,
            session_monitor_id: wire.session_monitor_id,
            x: wire.x,
            y: wire.y,
            width_px: wire.width_px,
            height_px: wire.height_px,
            refresh_hz: wire.refresh_hz,
            rotation: wire.rotation,
            is_primary: wire.is_primary,
            media_plan: wire.media_plan,
        };
        let _ = validate_applied_monitor_descriptor(&descriptor)?;
        Ok(descriptor)
    }
}

impl From<AppliedMonitorDescriptorMsg> for AppliedMonitorDescriptorWire {
    fn from(value: AppliedMonitorDescriptorMsg) -> Self {
        Self {
            client_display_id: value.client_display_id,
            session_monitor_id: value.session_monitor_id,
            x: value.x,
            y: value.y,
            width_px: value.width_px,
            height_px: value.height_px,
            refresh_hz: value.refresh_hz,
            rotation: value.rotation,
            is_primary: value.is_primary,
            media_plan: value.media_plan,
        }
    }
}

/// Applied multi-monitor-v1 topology metadata echoed inside
/// `server_hello.device_capabilities.multi_monitor_v1`.
///
/// `desktop_*`, `translation_*`, and every mirrored
/// [`AppliedMonitorDescriptorMsg`] rectangle stay in host pixel coordinates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    try_from = "AppliedMonitorTopologyWire",
    into = "AppliedMonitorTopologyWire"
)]
pub struct AppliedMonitorTopologyMsg {
    topology_generation: u64,
    /// Applied host desktop horizontal origin in pixels.
    desktop_x: i32,
    /// Applied host desktop vertical origin in pixels.
    desktop_y: i32,
    /// Applied host desktop width in pixels.
    desktop_width_px: u32,
    /// Applied host desktop height in pixels.
    desktop_height_px: u32,
    /// Host-pixel translation used to move the applied rectangles to the
    /// echoed desktop origin.
    translation_x: i64,
    /// Host-pixel translation used to move the applied rectangles to the
    /// echoed desktop origin.
    translation_y: i64,
    selected_carrier: MultiMonitorCarrierMsg,
    monitors: Vec<AppliedMonitorDescriptorMsg>,
    primary_index: usize,
}

impl AppliedMonitorTopologyMsg {
    /// Creates a validated applied multi-monitor-v1 topology sidecar.
    ///
    /// # Errors
    ///
    /// Returns an error when the roster violates the shared multi-monitor-v1
    /// wire invariants.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        topology_generation: u64,
        desktop_x: i32,
        desktop_y: i32,
        desktop_width_px: u32,
        desktop_height_px: u32,
        translation_x: i64,
        translation_y: i64,
        selected_carrier: MultiMonitorCarrierMsg,
        monitors: Vec<AppliedMonitorDescriptorMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        let primary_index = validate_applied_topology(
            topology_generation,
            desktop_x,
            desktop_y,
            desktop_width_px,
            desktop_height_px,
            translation_x,
            translation_y,
            &monitors,
        )?;
        Ok(Self {
            topology_generation,
            desktop_x,
            desktop_y,
            desktop_width_px,
            desktop_height_px,
            translation_x,
            translation_y,
            selected_carrier,
            monitors,
            primary_index,
        })
    }

    fn validate(&self) -> Result<usize, MultiMonitorValidationError> {
        validate_applied_topology(
            self.topology_generation,
            self.desktop_x,
            self.desktop_y,
            self.desktop_width_px,
            self.desktop_height_px,
            self.translation_x,
            self.translation_y,
            &self.monitors,
        )
    }

    #[must_use]
    pub const fn topology_generation(&self) -> u64 {
        self.topology_generation
    }

    #[must_use]
    pub const fn desktop_x(&self) -> i32 {
        self.desktop_x
    }

    #[must_use]
    pub const fn desktop_y(&self) -> i32 {
        self.desktop_y
    }

    #[must_use]
    pub const fn desktop_width_px(&self) -> u32 {
        self.desktop_width_px
    }

    #[must_use]
    pub const fn desktop_height_px(&self) -> u32 {
        self.desktop_height_px
    }

    #[must_use]
    pub const fn translation_x(&self) -> i64 {
        self.translation_x
    }

    #[must_use]
    pub const fn translation_y(&self) -> i64 {
        self.translation_y
    }

    #[must_use]
    pub const fn selected_carrier(&self) -> MultiMonitorCarrierMsg {
        self.selected_carrier
    }

    #[must_use]
    pub fn monitors(&self) -> &[AppliedMonitorDescriptorMsg] {
        &self.monitors
    }

    #[must_use]
    pub fn primary(&self) -> &AppliedMonitorDescriptorMsg {
        &self.monitors[self.primary_index]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AppliedMonitorTopologyWire {
    topology_generation: u64,
    /// Applied host desktop horizontal origin in pixels.
    desktop_x: i32,
    /// Applied host desktop vertical origin in pixels.
    desktop_y: i32,
    /// Applied host desktop width in pixels.
    desktop_width_px: u32,
    /// Applied host desktop height in pixels.
    desktop_height_px: u32,
    /// Host-pixel translation used to move the applied rectangles to the
    /// echoed desktop origin.
    #[serde(default)]
    translation_x: i64,
    /// Host-pixel translation used to move the applied rectangles to the
    /// echoed desktop origin.
    #[serde(default)]
    translation_y: i64,
    selected_carrier: MultiMonitorCarrierMsg,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    monitors: Vec<AppliedMonitorDescriptorMsg>,
}

impl TryFrom<AppliedMonitorTopologyWire> for AppliedMonitorTopologyMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: AppliedMonitorTopologyWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.topology_generation,
            wire.desktop_x,
            wire.desktop_y,
            wire.desktop_width_px,
            wire.desktop_height_px,
            wire.translation_x,
            wire.translation_y,
            wire.selected_carrier,
            wire.monitors,
        )
    }
}

impl From<AppliedMonitorTopologyMsg> for AppliedMonitorTopologyWire {
    fn from(value: AppliedMonitorTopologyMsg) -> Self {
        Self {
            topology_generation: value.topology_generation,
            desktop_x: value.desktop_x,
            desktop_y: value.desktop_y,
            desktop_width_px: value.desktop_width_px,
            desktop_height_px: value.desktop_height_px,
            translation_x: value.translation_x,
            translation_y: value.translation_y,
            selected_carrier: value.selected_carrier,
            monitors: value.monitors,
        }
    }
}

/// Pre-auth host multi-monitor-v1 offer required before a client may send a
/// requested multi-monitor topology during authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    try_from = "AuthMultiMonitorOfferWire",
    into = "AuthMultiMonitorOfferWire"
)]
pub struct AuthMultiMonitorOfferMsg {
    max_monitors: u8,
    supported_rotations: Vec<RotationMsg>,
    carriers: Vec<MultiMonitorCarrierMsg>,
}

impl AuthMultiMonitorOfferMsg {
    /// Creates a validated pre-auth host multi-monitor-v1 offer.
    ///
    /// # Errors
    ///
    /// Returns an error when the advertised offer is internally inconsistent.
    pub fn new(
        max_monitors: u8,
        supported_rotations: Vec<RotationMsg>,
        carriers: Vec<MultiMonitorCarrierMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        validate_multi_monitor_advertisement(max_monitors, &supported_rotations, &carriers)?;
        Ok(Self {
            max_monitors,
            supported_rotations,
            carriers,
        })
    }

    fn validate(&self) -> Result<(), MultiMonitorValidationError> {
        validate_multi_monitor_advertisement(
            self.max_monitors,
            &self.supported_rotations,
            &self.carriers,
        )
    }

    #[must_use]
    pub const fn max_monitors(&self) -> u8 {
        self.max_monitors
    }

    #[must_use]
    pub fn supported_rotations(&self) -> &[RotationMsg] {
        &self.supported_rotations
    }

    #[must_use]
    pub fn carriers(&self) -> &[MultiMonitorCarrierMsg] {
        &self.carriers
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct AuthMultiMonitorOfferWire {
    max_monitors: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_rotations: Vec<RotationMsg>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carriers: Vec<MultiMonitorCarrierMsg>,
}

impl TryFrom<AuthMultiMonitorOfferWire> for AuthMultiMonitorOfferMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: AuthMultiMonitorOfferWire) -> Result<Self, Self::Error> {
        Self::new(wire.max_monitors, wire.supported_rotations, wire.carriers)
    }
}

impl From<AuthMultiMonitorOfferMsg> for AuthMultiMonitorOfferWire {
    fn from(value: AuthMultiMonitorOfferMsg) -> Self {
        Self {
            max_monitors: value.max_monitors,
            supported_rotations: value.supported_rotations,
            carriers: value.carriers,
        }
    }
}

/// Auth-time client multi-monitor-v1 request admitted only after an explicit
/// pre-auth host offer. Carries the requested topology plus the client's
/// ordered carrier preferences so the host can choose one before `ServerHello`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    try_from = "AuthMultiMonitorRequestWire",
    into = "AuthMultiMonitorRequestWire"
)]
pub struct AuthMultiMonitorRequestMsg {
    requested_topology: RequestedMonitorTopologyMsg,
    carriers: Vec<MultiMonitorCarrierMsg>,
}

impl AuthMultiMonitorRequestMsg {
    /// Creates a validated auth-time multi-monitor-v1 request wrapper.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested topology is invalid or the ordered
    /// client-supported carrier list is empty or duplicated.
    pub fn new(
        requested_topology: RequestedMonitorTopologyMsg,
        carriers: Vec<MultiMonitorCarrierMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        let _ = requested_topology.validate()?;
        validate_client_supported_carriers(&carriers)?;
        Ok(Self {
            requested_topology,
            carriers,
        })
    }

    fn validate(&self) -> Result<(), MultiMonitorValidationError> {
        let _ = self.requested_topology.validate()?;
        validate_client_supported_carriers(&self.carriers)?;
        Ok(())
    }

    #[must_use]
    pub fn requested_topology(&self) -> &RequestedMonitorTopologyMsg {
        &self.requested_topology
    }

    #[must_use]
    pub fn carriers(&self) -> &[MultiMonitorCarrierMsg] {
        &self.carriers
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AuthMultiMonitorRequestWire {
    requested_topology: RequestedMonitorTopologyMsg,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carriers: Vec<MultiMonitorCarrierMsg>,
}

impl TryFrom<AuthMultiMonitorRequestWire> for AuthMultiMonitorRequestMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: AuthMultiMonitorRequestWire) -> Result<Self, Self::Error> {
        Self::new(wire.requested_topology, wire.carriers)
    }
}

impl From<AuthMultiMonitorRequestMsg> for AuthMultiMonitorRequestWire {
    fn from(value: AuthMultiMonitorRequestMsg) -> Self {
        Self {
            requested_topology: value.requested_topology,
            carriers: value.carriers,
        }
    }
}

/// Validated evidence that the host advertised multi-monitor-v1 support in
/// `AuthRequest` before authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvertisedMultiMonitorOffer<'a>(&'a AuthMultiMonitorOfferMsg);

impl<'a> AdvertisedMultiMonitorOffer<'a> {
    #[must_use]
    pub const fn max_monitors(self) -> u8 {
        self.0.max_monitors()
    }

    #[must_use]
    pub fn supported_rotations(self) -> &'a [RotationMsg] {
        self.0.supported_rotations()
    }

    #[must_use]
    pub fn carriers(self) -> &'a [MultiMonitorCarrierMsg] {
        self.0.carriers()
    }

    fn common_request_carriers(
        self,
        request: &AuthMultiMonitorRequestMsg,
    ) -> Vec<MultiMonitorCarrierMsg> {
        request
            .carriers()
            .iter()
            .copied()
            .filter(|carrier| self.carriers().contains(carrier))
            .collect()
    }

    /// Validates that an auth-time request stays within this advertised offer.
    ///
    /// # Errors
    ///
    /// Returns an error when the request topology exceeds the advertised
    /// monitor count/rotation support, the request carrier list is invalid, or
    /// the client and host share no common carrier.
    pub fn validate_request(
        self,
        request: &AuthMultiMonitorRequestMsg,
    ) -> Result<(), MultiMonitorValidationError> {
        request.validate()?;
        validate_requested_topology_against_advertisement(
            self.max_monitors(),
            self.supported_rotations(),
            request.requested_topology(),
        )?;
        if self.common_request_carriers(request).is_empty() {
            return Err(MultiMonitorValidationError::NoCommonAuthCarrier);
        }
        Ok(())
    }

    /// Returns the client-ordered carrier intersection for this auth-time
    /// request.
    ///
    /// # Errors
    ///
    /// Returns an error when the request does not fit inside this advertised
    /// offer.
    pub fn common_carriers(
        self,
        request: &AuthMultiMonitorRequestMsg,
    ) -> Result<Vec<MultiMonitorCarrierMsg>, MultiMonitorValidationError> {
        self.validate_request(request)?;
        Ok(self.common_request_carriers(request))
    }

    /// Selects the carrier the host will apply for this auth-time request.
    ///
    /// `preferred_order` lets host policy choose among the common carriers.
    /// When it is empty or has no match, the first client-ordered common
    /// carrier is returned.
    ///
    /// # Errors
    ///
    /// Returns an error when the request does not fit inside this advertised
    /// offer.
    pub fn select_carrier(
        self,
        request: &AuthMultiMonitorRequestMsg,
        preferred_order: &[MultiMonitorCarrierMsg],
    ) -> Result<MultiMonitorCarrierMsg, MultiMonitorValidationError> {
        let common = self.common_carriers(request)?;
        for carrier in preferred_order {
            if common.contains(carrier) {
                return Ok(*carrier);
            }
        }
        Ok(common[0])
    }
}

/// Failure to obtain a validated pre-auth multi-monitor-v1 offer from an
/// `AuthRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRequestMultiMonitorOfferError {
    Missing,
    Invalid(MultiMonitorValidationError),
}

impl std::fmt::Display for AuthRequestMultiMonitorOfferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => formatter.write_str(
                "auth_request.multi_monitor_v1 offer is missing; host did not advertise multi-monitor support",
            ),
            Self::Invalid(error) => write!(
                formatter,
                "auth_request.multi_monitor_v1 offer is invalid: {error}"
            ),
        }
    }
}

impl std::error::Error for AuthRequestMultiMonitorOfferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Missing => None,
            Self::Invalid(error) => Some(error),
        }
    }
}

/// Additive client multi-monitor-v1 capabilities and diagnostic requested-roster
/// echo. Auth-time carrier authority lives on [`AuthMultiMonitorRequestMsg`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(try_from = "ClientMultiMonitorWire", into = "ClientMultiMonitorWire")]
pub struct ClientMultiMonitorMsg {
    max_monitors: u8,
    supported_rotations: Vec<RotationMsg>,
    carriers: Vec<MultiMonitorCarrierMsg>,
    requested_topology: Option<RequestedMonitorTopologyMsg>,
}

/// Additive host multi-monitor-v1 capability and applied-topology metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "ServerMultiMonitorWire", into = "ServerMultiMonitorWire")]
pub struct ServerMultiMonitorMsg {
    max_monitors: u8,
    supported_rotations: Vec<RotationMsg>,
    fixed_topology: bool,
    topology_backend: TopologyBackendKindMsg,
    carriers: Vec<MultiMonitorCarrierMsg>,
    applied_topology: Option<AppliedMonitorTopologyMsg>,
}

impl ClientMultiMonitorMsg {
    /// Creates validated client multi-monitor-v1 capability metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_monitors` is outside `1..=4`, the advertised
    /// rotations/carriers contain duplicates, or the requested topology is
    /// invalid or exceeds the advertised support.
    pub fn new(
        max_monitors: u8,
        supported_rotations: Vec<RotationMsg>,
        carriers: Vec<MultiMonitorCarrierMsg>,
        requested_topology: Option<RequestedMonitorTopologyMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        validate_multi_monitor_advertisement(max_monitors, &supported_rotations, &carriers)?;
        if let Some(topology) = &requested_topology {
            validate_requested_topology_against_advertisement(
                max_monitors,
                &supported_rotations,
                topology,
            )?;
        }
        Ok(Self {
            max_monitors,
            supported_rotations,
            carriers,
            requested_topology,
        })
    }

    fn validate(&self) -> Result<(), MultiMonitorValidationError> {
        validate_multi_monitor_advertisement(
            self.max_monitors,
            &self.supported_rotations,
            &self.carriers,
        )?;
        if let Some(topology) = &self.requested_topology {
            validate_requested_topology_against_advertisement(
                self.max_monitors,
                &self.supported_rotations,
                topology,
            )?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn max_monitors(&self) -> u8 {
        self.max_monitors
    }

    #[must_use]
    pub fn supported_rotations(&self) -> &[RotationMsg] {
        &self.supported_rotations
    }

    #[must_use]
    pub fn carriers(&self) -> &[MultiMonitorCarrierMsg] {
        &self.carriers
    }

    #[must_use]
    pub fn requested_topology(&self) -> Option<&RequestedMonitorTopologyMsg> {
        self.requested_topology.as_ref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ClientMultiMonitorWire {
    max_monitors: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_rotations: Vec<RotationMsg>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carriers: Vec<MultiMonitorCarrierMsg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_topology: Option<RequestedMonitorTopologyMsg>,
}

impl TryFrom<ClientMultiMonitorWire> for ClientMultiMonitorMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: ClientMultiMonitorWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.max_monitors,
            wire.supported_rotations,
            wire.carriers,
            wire.requested_topology,
        )
    }
}

impl From<ClientMultiMonitorMsg> for ClientMultiMonitorWire {
    fn from(value: ClientMultiMonitorMsg) -> Self {
        Self {
            max_monitors: value.max_monitors,
            supported_rotations: value.supported_rotations,
            carriers: value.carriers,
            requested_topology: value.requested_topology,
        }
    }
}

impl ServerMultiMonitorMsg {
    /// Creates validated host multi-monitor-v1 capability metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_monitors` is outside `1..=4`, the advertised
    /// rotations/carriers contain duplicates, or the applied topology is
    /// invalid or exceeds the advertised support.
    pub fn new(
        max_monitors: u8,
        supported_rotations: Vec<RotationMsg>,
        fixed_topology: bool,
        topology_backend: TopologyBackendKindMsg,
        carriers: Vec<MultiMonitorCarrierMsg>,
        applied_topology: Option<AppliedMonitorTopologyMsg>,
    ) -> Result<Self, MultiMonitorValidationError> {
        validate_multi_monitor_advertisement(max_monitors, &supported_rotations, &carriers)?;
        if let Some(topology) = &applied_topology {
            validate_applied_topology_against_advertisement(
                max_monitors,
                &supported_rotations,
                &carriers,
                topology,
            )?;
        }
        Ok(Self {
            max_monitors,
            supported_rotations,
            fixed_topology,
            topology_backend,
            carriers,
            applied_topology,
        })
    }

    fn validate(&self) -> Result<(), MultiMonitorValidationError> {
        validate_multi_monitor_advertisement(
            self.max_monitors,
            &self.supported_rotations,
            &self.carriers,
        )?;
        if let Some(topology) = &self.applied_topology {
            validate_applied_topology_against_advertisement(
                self.max_monitors,
                &self.supported_rotations,
                &self.carriers,
                topology,
            )?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn max_monitors(&self) -> u8 {
        self.max_monitors
    }

    #[must_use]
    pub fn supported_rotations(&self) -> &[RotationMsg] {
        &self.supported_rotations
    }

    #[must_use]
    pub const fn fixed_topology(&self) -> bool {
        self.fixed_topology
    }

    #[must_use]
    pub const fn topology_backend(&self) -> TopologyBackendKindMsg {
        self.topology_backend
    }

    #[must_use]
    pub fn carriers(&self) -> &[MultiMonitorCarrierMsg] {
        &self.carriers
    }

    #[must_use]
    pub fn applied_topology(&self) -> Option<&AppliedMonitorTopologyMsg> {
        self.applied_topology.as_ref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ServerMultiMonitorWire {
    max_monitors: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    supported_rotations: Vec<RotationMsg>,
    #[serde(default)]
    fixed_topology: bool,
    topology_backend: TopologyBackendKindMsg,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    carriers: Vec<MultiMonitorCarrierMsg>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied_topology: Option<AppliedMonitorTopologyMsg>,
}

impl TryFrom<ServerMultiMonitorWire> for ServerMultiMonitorMsg {
    type Error = MultiMonitorValidationError;

    fn try_from(wire: ServerMultiMonitorWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.max_monitors,
            wire.supported_rotations,
            wire.fixed_topology,
            wire.topology_backend,
            wire.carriers,
            wire.applied_topology,
        )
    }
}

impl From<ServerMultiMonitorMsg> for ServerMultiMonitorWire {
    fn from(value: ServerMultiMonitorMsg) -> Self {
        Self {
            max_monitors: value.max_monitors,
            supported_rotations: value.supported_rotations,
            fixed_topology: value.fixed_topology,
            topology_backend: value.topology_backend,
            carriers: value.carriers,
            applied_topology: value.applied_topology,
        }
    }
}

const fn is_false(value: &bool) -> bool {
    !*value
}

const fn is_local_cursor_mode(mode: &CursorMode) -> bool {
    matches!(mode, CursorMode::Local)
}

fn encode_device_capability<T: Serialize>(
    capabilities: &mut BTreeMap<String, Value>,
    key: &str,
    value: &T,
) -> Result<(), serde_json::Error> {
    capabilities.insert(key.to_owned(), serde_json::to_value(value)?);
    Ok(())
}

fn decode_device_capability<T: DeserializeOwned>(
    capabilities: &BTreeMap<String, Value>,
    key: &str,
) -> Result<Option<T>, serde_json::Error> {
    capabilities
        .get(key)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
}

pub(crate) fn attach_auth_request_multi_monitor_v1_offer(
    request: &mut AuthRequest,
    offer: AuthMultiMonitorOfferMsg,
) -> Result<(), MultiMonitorValidationError> {
    offer.validate()?;
    request.multi_monitor_v1 = Some(offer);
    Ok(())
}

pub(crate) fn required_auth_request_multi_monitor_v1_offer(
    request: &AuthRequest,
) -> Result<AdvertisedMultiMonitorOffer<'_>, AuthRequestMultiMonitorOfferError> {
    let offer = request
        .multi_monitor_v1
        .as_ref()
        .ok_or(AuthRequestMultiMonitorOfferError::Missing)?;
    offer
        .validate()
        .map_err(AuthRequestMultiMonitorOfferError::Invalid)?;
    Ok(AdvertisedMultiMonitorOffer(offer))
}

pub(crate) fn attach_auth_response_multi_monitor_v1(
    response: &mut AuthResponse,
    offer: AdvertisedMultiMonitorOffer<'_>,
    requested_topology: RequestedMonitorTopologyMsg,
    carriers: Vec<MultiMonitorCarrierMsg>,
) -> Result<(), MultiMonitorValidationError> {
    let request = AuthMultiMonitorRequestMsg::new(requested_topology, carriers)?;
    offer.validate_request(&request)?;
    let primary = request.requested_topology().validated_primary()?;
    response.screen_width = primary.width_px;
    response.screen_height = primary.height_px;
    response.displays_mode = "match_layout".to_string();
    response.monitors = request.requested_topology().legacy_monitors();
    response.multi_monitor_v1 = Some(request);
    Ok(())
}

pub(crate) fn attach_client_hello_multi_monitor_v1(
    hello: &mut ClientHelloMsg,
    capability: &ClientMultiMonitorMsg,
) -> Result<(), MultiMonitorCapabilityError> {
    capability.validate()?;
    if let Some(primary) = capability
        .requested_topology()
        .map(RequestedMonitorTopologyMsg::validated_primary)
        .transpose()?
    {
        hello.screen_width = primary.width_px;
        hello.screen_height = primary.height_px;
    }
    encode_device_capability(&mut hello.device_capabilities, MULTI_MONITOR_V1, capability)?;
    Ok(())
}

pub(crate) fn decode_client_hello_multi_monitor_v1(
    hello: &ClientHelloMsg,
) -> Result<Option<ClientMultiMonitorMsg>, serde_json::Error> {
    decode_device_capability(&hello.device_capabilities, MULTI_MONITOR_V1)
}

pub(crate) fn attach_server_hello_multi_monitor_v1(
    hello: &mut ServerHelloMsg,
    capability: &ServerMultiMonitorMsg,
) -> Result<(), MultiMonitorCapabilityError> {
    capability.validate()?;
    encode_device_capability(&mut hello.device_capabilities, MULTI_MONITOR_V1, capability)?;
    Ok(())
}

pub(crate) fn decode_server_hello_multi_monitor_v1(
    hello: &ServerHelloMsg,
) -> Result<Option<ServerMultiMonitorMsg>, serde_json::Error> {
    decode_device_capability(&hello.device_capabilities, MULTI_MONITOR_V1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{
        AuthRequest, AuthResponse, ClientHelloMsg, CursorMode, ServerHelloMsg, AUTH_REQUEST,
        MAX_CLIENT_DISPLAY_ID_BYTES, MULTI_MONITOR_V1,
    };
    use serde_json::Value;

    fn sample_client_display_id(value: &str) -> ClientDisplayId {
        ClientDisplayId::try_from(value).expect("client display id")
    }

    fn sample_legacy_server_monitors() -> Vec<Value> {
        vec![serde_json::json!({
            "id": 42,
            "x": 1920,
            "y": 0,
            "width_px": 2560,
            "height_px": 1600,
            "refresh_hz": 60,
            "scale": 2.0,
            "is_primary": true,
            "name": "Built-in Display",
            "capture_output_index": 1
        })]
    }

    fn sample_requested_monitors() -> Vec<RequestedMonitorDescriptorMsg> {
        vec![
            RequestedMonitorDescriptorMsg {
                client_display_id: sample_client_display_id("display-primary"),
                client_monitor_id: 7,
                x: 0,
                y: 0,
                width_px: 3600,
                height_px: 2338,
                logical_width: 1800,
                logical_height: 1169,
                scale: 2.0,
                refresh_hz: 120,
                rotation: RotationMsg::Degrees0,
                is_primary: true,
                name: "Built-in Display".to_string(),
                width_mm: 301.8,
                height_mm: 196.0,
                vendor: 0x610,
                model: 0xa05e,
                serial: 0xfd626d62,
                edid: String::new(),
                safe_area_policy: SafeAreaPolicyMsg::StandardFullscreen,
                quality_intent: MonitorQualityIntentMsg::HostDefault,
            },
            RequestedMonitorDescriptorMsg {
                client_display_id: sample_client_display_id("display-left"),
                client_monitor_id: 11,
                x: -2560,
                y: 160,
                width_px: 2560,
                height_px: 1440,
                logical_width: 2560,
                logical_height: 1440,
                scale: 1.0,
                refresh_hz: 60,
                rotation: RotationMsg::Degrees90,
                is_primary: false,
                name: "Display 2".to_string(),
                width_mm: 600.0,
                height_mm: 340.0,
                vendor: 0x2222,
                model: 0x3333,
                serial: 0x4444,
                edid: "base64-edid".to_string(),
                safe_area_policy: SafeAreaPolicyMsg::FullFrame,
                quality_intent: MonitorQualityIntentMsg::FullColorRequired,
            },
        ]
    }

    fn sample_requested_topology() -> RequestedMonitorTopologyMsg {
        RequestedMonitorTopologyMsg::new(sample_requested_monitors()).expect("requested topology")
    }

    fn sample_supported_rotations() -> Vec<RotationMsg> {
        vec![
            RotationMsg::Degrees0,
            RotationMsg::Degrees90,
            RotationMsg::Degrees180,
            RotationMsg::Degrees270,
        ]
    }

    fn sample_multi_monitor_carriers() -> Vec<MultiMonitorCarrierMsg> {
        vec![
            MultiMonitorCarrierMsg::MuxedReliableStream,
            MultiMonitorCarrierMsg::PerMonitorReliableStream,
        ]
    }

    fn sample_client_supported_carriers() -> Vec<MultiMonitorCarrierMsg> {
        vec![
            MultiMonitorCarrierMsg::PerMonitorReliableStream,
            MultiMonitorCarrierMsg::MuxedReliableStream,
        ]
    }

    fn sample_auth_multi_monitor_offer() -> AuthMultiMonitorOfferMsg {
        AuthMultiMonitorOfferMsg::new(
            4,
            sample_supported_rotations(),
            sample_multi_monitor_carriers(),
        )
        .expect("auth offer")
    }

    fn sample_auth_request_with_offer(offer: AuthMultiMonitorOfferMsg) -> AuthRequest {
        AuthRequest {
            msg_type: AUTH_REQUEST.to_string(),
            auth_methods: vec!["pam".to_string()],
            challenge: "c".to_string(),
            salt: "s".to_string(),
            auth_mode: Some("pam".to_string()),
            disclaimer: None,
            multi_monitor_v1: Some(offer),
        }
    }

    fn sample_applied_monitors() -> Vec<AppliedMonitorDescriptorMsg> {
        vec![
            AppliedMonitorDescriptorMsg {
                client_display_id: sample_client_display_id("display-primary"),
                session_monitor_id: 41,
                x: 2560,
                y: 0,
                width_px: 3600,
                height_px: 2338,
                refresh_hz: 120,
                rotation: RotationMsg::Degrees0,
                is_primary: true,
                media_plan: AppliedMonitorMediaPlanMsg {
                    stream_epoch: 9,
                    encoder_backend: "native-nvenc".to_string(),
                    encoder_class: "hardware".to_string(),
                    codec: "h264".to_string(),
                    chroma: "yuv420".to_string(),
                    width_px: 3600,
                    height_px: 2338,
                    fps: 60,
                    bitrate_kbps: 18_000,
                    cursor_mode: CursorMode::Local,
                    degraded: false,
                },
            },
            AppliedMonitorDescriptorMsg {
                client_display_id: sample_client_display_id("display-left"),
                session_monitor_id: 42,
                x: 0,
                y: 160,
                width_px: 2560,
                height_px: 1440,
                refresh_hz: 60,
                rotation: RotationMsg::Degrees90,
                is_primary: false,
                media_plan: AppliedMonitorMediaPlanMsg {
                    stream_epoch: 10,
                    encoder_backend: "openh264-sw-h264".to_string(),
                    encoder_class: "software".to_string(),
                    codec: "h264".to_string(),
                    chroma: "yuv420".to_string(),
                    width_px: 1920,
                    height_px: 1080,
                    fps: 30,
                    bitrate_kbps: 4_000,
                    cursor_mode: CursorMode::Local,
                    degraded: true,
                },
            },
        ]
    }

    #[test]
    fn applied_media_roster_round_trips_mixed_h265_and_h264_profiles() {
        let mut monitors = sample_applied_monitors();
        monitors[0].media_plan.codec = "h265".to_string();
        monitors[0].media_plan.chroma = "yuv420".to_string();
        monitors[1].media_plan.codec = "h264".to_string();
        monitors[1].media_plan.chroma = "yuv420".to_string();
        let topology = AppliedMonitorTopologyMsg::new(
            3,
            0,
            0,
            6160,
            2338,
            0,
            0,
            MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors,
        )
        .expect("mixed roster");
        let json = serde_json::to_value(&topology).expect("serialize roster");
        let decoded: AppliedMonitorTopologyMsg =
            serde_json::from_value(json).expect("deserialize roster");
        assert_eq!(decoded.monitors()[0].media_plan.codec, "h265");
        assert_eq!(decoded.monitors()[0].media_plan.stream_epoch, 9);
        assert_eq!(decoded.monitors()[1].media_plan.codec, "h264");
        assert_eq!(decoded.monitors()[1].media_plan.stream_epoch, 10);
    }

    #[test]
    fn applied_media_roster_rejects_legacy_entries_without_stream_epoch() {
        let descriptor = sample_applied_monitors().remove(0);
        let mut json = serde_json::to_value(descriptor).expect("serialize descriptor");
        json["media_plan"]
            .as_object_mut()
            .expect("media plan object")
            .remove("stream_epoch");
        let error = serde_json::from_value::<AppliedMonitorDescriptorMsg>(json)
            .expect_err("legacy media plan must be rejected");
        assert!(error.to_string().contains("stream_epoch"));
    }

    fn sample_client_multi_monitor() -> ClientMultiMonitorMsg {
        ClientMultiMonitorMsg::new(
            4,
            sample_supported_rotations(),
            sample_multi_monitor_carriers(),
            Some(sample_requested_topology()),
        )
        .expect("client capability")
    }

    fn sample_server_multi_monitor() -> ServerMultiMonitorMsg {
        ServerMultiMonitorMsg::new(
            4,
            sample_supported_rotations(),
            true,
            TopologyBackendKindMsg::PhysicalOutputs,
            sample_multi_monitor_carriers(),
            Some(
                AppliedMonitorTopologyMsg::new(
                    9,
                    0,
                    0,
                    6160,
                    2338,
                    2560,
                    0,
                    MultiMonitorCarrierMsg::PerMonitorReliableStream,
                    sample_applied_monitors(),
                )
                .expect("applied topology"),
            ),
        )
        .expect("server capability")
    }

    fn sample_auth_multi_monitor_request() -> AuthMultiMonitorRequestMsg {
        AuthMultiMonitorRequestMsg::new(
            sample_requested_topology(),
            sample_client_supported_carriers(),
        )
        .expect("auth request sidecar")
    }

    #[test]
    fn client_display_id_is_bounded_nonempty_and_control_free() {
        let id = ClientDisplayId::new("display-primary").unwrap();
        assert_eq!(id.as_str(), "display-primary");
        assert_eq!(
            serde_json::from_str::<ClientDisplayId>("\"display-primary\"").unwrap(),
            id
        );
        assert_eq!(
            ClientDisplayId::new(String::new()),
            Err(ClientDisplayIdError::Empty)
        );
        assert_eq!(
            ClientDisplayId::new("x".repeat(MAX_CLIENT_DISPLAY_ID_BYTES + 1)),
            Err(ClientDisplayIdError::TooLong)
        );
        assert_eq!(
            ClientDisplayId::new("display\nprimary"),
            Err(ClientDisplayIdError::ControlCharacter)
        );
        assert!(serde_json::from_str::<ClientDisplayId>("\"display\\nprimary\"").is_err());
    }

    #[test]
    fn auth_multi_monitor_request_round_trips_and_preserves_carrier_order() {
        let request = sample_auth_multi_monitor_request();
        let json = serde_json::to_value(&request).expect("auth request json");
        assert_eq!(json["carriers"][0], "per_monitor_reliable_stream");
        assert_eq!(
            json["requested_topology"]["monitors"][0]["client_display_id"],
            "display-primary"
        );
        assert_eq!(
            serde_json::from_value::<AuthMultiMonitorRequestMsg>(json).expect("auth request"),
            request
        );
    }

    #[test]
    fn auth_response_multi_monitor_v1_offer_keeps_legacy_fields_coherent() {
        let legacy: AuthResponse = serde_json::from_str(
            r#"{"type":"auth_response","method":"pam","username":"artist","credential":"secret","screen_width":1920,"screen_height":1080}"#,
        )
        .unwrap();
        assert_eq!(legacy.multi_monitor_v1, None);

        let requested = sample_requested_topology();
        let expected_carriers = sample_client_supported_carriers();
        let request = sample_auth_request_with_offer(sample_auth_multi_monitor_offer());
        let response = AuthResponse::password("artist", "secret")
            .with_multi_monitor_v1(
                request
                    .required_multi_monitor_v1_offer()
                    .expect("host offer"),
                requested.clone(),
                expected_carriers.clone(),
            )
            .expect("topology");
        let validated_offer = request
            .required_multi_monitor_v1_offer()
            .expect("validated host offer");
        let auth_request = response
            .multi_monitor_v1
            .as_ref()
            .expect("auth-time request sidecar");
        assert_eq!(response.displays_mode, "match_layout");
        assert_eq!(response.screen_width, 3600);
        assert_eq!(response.screen_height, 2338);
        assert_eq!(response.monitors[1].id, 11);
        assert_eq!(auth_request.requested_topology(), &requested);
        assert_eq!(auth_request.carriers(), expected_carriers.as_slice());
        assert_eq!(
            validated_offer
                .common_carriers(auth_request)
                .expect("common carriers"),
            expected_carriers
        );
        assert_eq!(
            validated_offer
                .select_carrier(auth_request, &[MultiMonitorCarrierMsg::MuxedReliableStream])
                .expect("host-selected carrier"),
            MultiMonitorCarrierMsg::MuxedReliableStream
        );
        assert_eq!(
            validated_offer
                .select_carrier(auth_request, &[])
                .expect("first common carrier"),
            MultiMonitorCarrierMsg::PerMonitorReliableStream
        );

        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(
            json["multi_monitor_v1"]["requested_topology"]["monitors"][1]["rotation"],
            "degrees90"
        );
        assert_eq!(
            json["multi_monitor_v1"]["requested_topology"]["monitors"][1]["client_display_id"],
            "display-left"
        );
        assert_eq!(
            json["multi_monitor_v1"]["requested_topology"]["monitors"][1]["client_monitor_id"],
            11
        );
        assert_eq!(
            json["multi_monitor_v1"]["carriers"][0],
            "per_monitor_reliable_stream"
        );
        assert_eq!(
            serde_json::from_value::<AuthResponse>(json).unwrap(),
            response
        );
        assert_eq!(requested.legacy_monitors()[0].width_px, 3600);
    }

    #[test]
    fn client_hello_multi_monitor_v1_keeps_primary_screen_size_coherent() {
        let capability = sample_client_multi_monitor();
        let requested = capability
            .requested_topology()
            .cloned()
            .expect("requested topology");
        let primary = requested.primary();

        let hello = ClientHelloMsg::default()
            .with_multi_monitor_v1(&capability)
            .expect("capability");
        assert_eq!(hello.screen_width, primary.width_px);
        assert_eq!(hello.screen_height, primary.height_px);

        let request = sample_auth_request_with_offer(sample_auth_multi_monitor_offer());
        let auth = AuthResponse::password("artist", "secret")
            .with_multi_monitor_v1(
                request
                    .required_multi_monitor_v1_offer()
                    .expect("host offer"),
                requested,
                capability.carriers().to_vec(),
            )
            .expect("topology");
        assert_eq!(hello.screen_width, auth.screen_width);
        assert_eq!(hello.screen_height, auth.screen_height);
    }

    #[test]
    fn auth_response_multi_monitor_v1_requires_explicit_supported_offer() {
        let legacy_request: AuthRequest = serde_json::from_str(
            r#"{"type":"auth_request","auth_methods":["pam"],"challenge":"c","salt":"s","auth_mode":"pam"}"#,
        )
        .unwrap();
        assert_eq!(
            legacy_request.required_multi_monitor_v1_offer(),
            Err(AuthRequestMultiMonitorOfferError::Missing)
        );

        let requested = sample_requested_topology();
        let count_limited_offer = sample_auth_request_with_offer(
            AuthMultiMonitorOfferMsg::new(
                1,
                sample_supported_rotations(),
                sample_multi_monitor_carriers(),
            )
            .expect("count-limited offer"),
        );
        assert_eq!(
            AuthResponse::password("artist", "secret")
                .with_multi_monitor_v1(
                    count_limited_offer
                        .required_multi_monitor_v1_offer()
                        .expect("host offer"),
                    requested.clone(),
                    sample_client_supported_carriers(),
                )
                .unwrap_err(),
            MultiMonitorValidationError::RequestedMonitorCountExceedsMax {
                count: 2,
                max_monitors: 1,
            }
        );

        let rotation_limited_offer = sample_auth_request_with_offer(
            AuthMultiMonitorOfferMsg::new(
                4,
                vec![RotationMsg::Degrees0],
                sample_multi_monitor_carriers(),
            )
            .expect("rotation-limited offer"),
        );
        assert_eq!(
            AuthResponse::password("artist", "secret")
                .with_multi_monitor_v1(
                    rotation_limited_offer
                        .required_multi_monitor_v1_offer()
                        .expect("host offer"),
                    requested,
                    sample_client_supported_carriers(),
                )
                .unwrap_err(),
            MultiMonitorValidationError::UnsupportedRequestedRotation {
                client_display_id: "display-left".to_string(),
                rotation: RotationMsg::Degrees90,
            }
        );

        let carrier_limited_offer = sample_auth_request_with_offer(
            AuthMultiMonitorOfferMsg::new(
                4,
                sample_supported_rotations(),
                vec![MultiMonitorCarrierMsg::MuxedReliableStream],
            )
            .expect("carrier-limited offer"),
        );
        assert_eq!(
            AuthResponse::password("artist", "secret")
                .with_multi_monitor_v1(
                    carrier_limited_offer
                        .required_multi_monitor_v1_offer()
                        .expect("host offer"),
                    sample_requested_topology(),
                    vec![MultiMonitorCarrierMsg::PerMonitorReliableStream],
                )
                .unwrap_err(),
            MultiMonitorValidationError::NoCommonAuthCarrier
        );
    }

    #[test]
    fn client_hello_multi_monitor_v1_round_trips_and_legacy_shape_still_parses() {
        let legacy: ClientHelloMsg = serde_json::from_str(
            r#"{"type":"client_hello","client_name":"old","version":"3","screen_width":1,"screen_height":1,"supports_h264":false,"supports_h265":false,"supports_av1":false,"supports_yuv444":false,"supports_audio":false,"supports_pen":false,"decoder_backend":"","capture_mode":"","picked_monitor_id":-1,"picked_monitor_name":"","device_capabilities":{}}"#,
        )
        .unwrap();
        assert_eq!(legacy.multi_monitor_v1().unwrap(), None);

        let capability = sample_client_multi_monitor();
        let hello = ClientHelloMsg::default()
            .with_multi_monitor_v1(&capability)
            .expect("capability");
        assert_eq!(hello.screen_width, 3600);
        assert_eq!(hello.screen_height, 2338);
        assert_eq!(hello.multi_monitor_v1().unwrap(), Some(capability.clone()));
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(
            json["device_capabilities"][MULTI_MONITOR_V1]["carriers"][1],
            "per_monitor_reliable_stream"
        );
        assert_eq!(
            json["device_capabilities"][MULTI_MONITOR_V1]["requested_topology"]["monitors"][0]
                ["client_display_id"],
            "display-primary"
        );
        assert_eq!(
            serde_json::from_value::<ClientHelloMsg>(json)
                .unwrap()
                .multi_monitor_v1()
                .unwrap(),
            Some(capability)
        );
    }

    #[test]
    fn legacy_server_hello_monitors_round_trip_unchanged() {
        let legacy_monitors = sample_legacy_server_monitors();
        let legacy: ServerHelloMsg = serde_json::from_value(serde_json::json!({
            "type": "server_hello",
            "monitors": legacy_monitors.clone(),
        }))
        .unwrap();
        assert_eq!(legacy.multi_monitor_v1().unwrap(), None);
        let json = serde_json::to_value(&legacy).unwrap();
        assert_eq!(json["monitors"], Value::Array(legacy_monitors));
    }

    #[test]
    fn server_hello_multi_monitor_v1_uses_capability_sidecar_without_mutating_legacy_shape() {
        let capability = sample_server_multi_monitor();
        let legacy_monitors = sample_legacy_server_monitors();
        let hello = serde_json::from_value::<ServerHelloMsg>(serde_json::json!({
            "type": "server_hello",
            "monitors": legacy_monitors.clone(),
        }))
        .unwrap()
        .with_multi_monitor_v1(&capability)
        .expect("capability");
        assert_eq!(hello.monitors, legacy_monitors);
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(
            json["device_capabilities"][MULTI_MONITOR_V1]["applied_topology"]["monitors"][0]
                ["client_display_id"],
            "display-primary"
        );
        assert_eq!(
            json["device_capabilities"][MULTI_MONITOR_V1]["applied_topology"]
                ["topology_generation"],
            9
        );
        assert_eq!(
            json["monitors"],
            serde_json::json!([{
                "id": 42,
                "x": 1920,
                "y": 0,
                "width_px": 2560,
                "height_px": 1600,
                "refresh_hz": 60,
                "scale": 2.0,
                "is_primary": true,
                "name": "Built-in Display",
                "capture_output_index": 1
            }])
        );
        assert_eq!(
            serde_json::from_value::<ServerHelloMsg>(json)
                .unwrap()
                .multi_monitor_v1()
                .unwrap(),
            Some(capability)
        );
    }

    #[test]
    fn requested_monitor_topology_rejects_invalid_rosters() {
        assert_eq!(
            RequestedMonitorTopologyMsg::new(Vec::new()),
            Err(MultiMonitorValidationError::UnsupportedMonitorCount(0))
        );

        let mut too_many = sample_requested_monitors();
        for (index, client_monitor_id) in [21_u32, 22, 23].into_iter().enumerate() {
            let mut extra = too_many[1].clone();
            extra.client_display_id = sample_client_display_id(&format!("display-extra-{index}"));
            extra.client_monitor_id = client_monitor_id;
            extra.x = -5120 - (index as i32 * 2560);
            too_many.push(extra);
        }
        assert_eq!(
            RequestedMonitorTopologyMsg::new(too_many),
            Err(MultiMonitorValidationError::UnsupportedMonitorCount(5))
        );

        let mut no_primary = sample_requested_monitors();
        no_primary[0].is_primary = false;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(no_primary),
            Err(MultiMonitorValidationError::PrimaryMonitorCount(0))
        );

        let mut two_primary = sample_requested_monitors();
        two_primary[1].is_primary = true;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(two_primary),
            Err(MultiMonitorValidationError::PrimaryMonitorCount(2))
        );

        let mut duplicate_display = sample_requested_monitors();
        duplicate_display[1].client_display_id = duplicate_display[0].client_display_id.clone();
        assert_eq!(
            RequestedMonitorTopologyMsg::new(duplicate_display),
            Err(MultiMonitorValidationError::DuplicateClientDisplayId(
                "display-primary".to_string()
            ))
        );

        let mut duplicate_legacy_id = sample_requested_monitors();
        duplicate_legacy_id[1].client_monitor_id = duplicate_legacy_id[0].client_monitor_id;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(duplicate_legacy_id),
            Err(MultiMonitorValidationError::DuplicateClientMonitorId(7))
        );
    }

    #[test]
    fn requested_monitor_topology_rejects_bad_dimensions_scale_and_deserialization() {
        let mut zero_physical = sample_requested_monitors();
        zero_physical[0].width_px = 0;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(zero_physical),
            Err(
                MultiMonitorValidationError::InvalidRequestedPhysicalDimensions(
                    "display-primary".to_string()
                )
            )
        );

        let mut zero_logical = sample_requested_monitors();
        zero_logical[0].logical_height = 0;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(zero_logical),
            Err(
                MultiMonitorValidationError::InvalidRequestedLogicalDimensions(
                    "display-primary".to_string()
                )
            )
        );

        let mut bad_scale = sample_requested_monitors();
        bad_scale[0].scale = f32::INFINITY;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(bad_scale),
            Err(MultiMonitorValidationError::InvalidMonitorScale(
                "display-primary".to_string()
            ))
        );

        let mut overflow = sample_requested_monitors();
        overflow[0].x = i32::MAX;
        overflow[0].logical_width = 2;
        assert_eq!(
            RequestedMonitorTopologyMsg::new(overflow),
            Err(MultiMonitorValidationError::CoordinateOverflow(
                "requested monitor logical bounds"
            ))
        );

        let mut json = serde_json::to_value(sample_requested_topology()).unwrap();
        json["monitors"][0]["logical_width"] = 0.into();
        let error = serde_json::from_value::<RequestedMonitorTopologyMsg>(json).unwrap_err();
        assert!(error
            .to_string()
            .contains("requested monitor display-primary has zero logical dimensions"));
    }

    #[test]
    fn applied_monitor_topology_rejects_invalid_rosters_bounds_and_media_plan() {
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                0,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                sample_applied_monitors(),
            ),
            Err(MultiMonitorValidationError::ZeroTopologyGeneration)
        );
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                1,
                1,
                0,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                Vec::new(),
            ),
            Err(MultiMonitorValidationError::UnsupportedMonitorCount(0))
        );

        let mut too_many = sample_applied_monitors();
        for (index, session_monitor_id) in [51_u16, 52, 53].into_iter().enumerate() {
            let mut extra = too_many[1].clone();
            extra.client_display_id = sample_client_display_id(&format!("applied-extra-{index}"));
            extra.session_monitor_id = session_monitor_id;
            too_many.push(extra);
        }
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                too_many,
            ),
            Err(MultiMonitorValidationError::UnsupportedMonitorCount(5))
        );

        let mut no_primary = sample_applied_monitors();
        no_primary[0].is_primary = false;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                no_primary,
            ),
            Err(MultiMonitorValidationError::PrimaryMonitorCount(0))
        );

        let mut two_primary = sample_applied_monitors();
        two_primary[1].is_primary = true;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                two_primary,
            ),
            Err(MultiMonitorValidationError::PrimaryMonitorCount(2))
        );

        let mut duplicate_display = sample_applied_monitors();
        duplicate_display[1].client_display_id = duplicate_display[0].client_display_id.clone();
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                duplicate_display,
            ),
            Err(MultiMonitorValidationError::DuplicateClientDisplayId(
                "display-primary".to_string()
            ))
        );

        let mut duplicate_session_id = sample_applied_monitors();
        duplicate_session_id[1].session_monitor_id = duplicate_session_id[0].session_monitor_id;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                duplicate_session_id,
            ),
            Err(MultiMonitorValidationError::DuplicateSessionMonitorId(41))
        );

        let mut zero_dimensions = sample_applied_monitors();
        zero_dimensions[1].height_px = 0;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                zero_dimensions,
            ),
            Err(MultiMonitorValidationError::InvalidAppliedPixelDimensions(
                "display-left".to_string()
            ))
        );

        let mut zero_media_dimensions = sample_applied_monitors();
        zero_media_dimensions[1].media_plan.width_px = 0;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                zero_media_dimensions,
            ),
            Err(MultiMonitorValidationError::InvalidAppliedMediaDimensions(
                "display-left".to_string()
            ))
        );

        let mut zero_fps = sample_applied_monitors();
        zero_fps[1].media_plan.fps = 0;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                zero_fps,
            ),
            Err(MultiMonitorValidationError::InvalidAppliedFps(
                "display-left".to_string()
            ))
        );

        let mut zero_bitrate = sample_applied_monitors();
        zero_bitrate[1].media_plan.bitrate_kbps = 0;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                zero_bitrate,
            ),
            Err(MultiMonitorValidationError::InvalidAppliedBitrateKbps(
                "display-left".to_string()
            ))
        );

        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                0,
                0,
                6000,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                sample_applied_monitors(),
            ),
            Err(MultiMonitorValidationError::InconsistentAppliedDesktopBounds)
        );

        let mut shifted = sample_applied_monitors();
        for monitor in &mut shifted {
            monitor.x += 1;
        }
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                1,
                1,
                0,
                6160,
                2338,
                1,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                shifted,
            ),
            Err(MultiMonitorValidationError::InconsistentAppliedDesktopTranslation)
        );
    }

    #[test]
    fn applied_monitor_topology_deserialization_rejects_invalid_json() {
        let mut json = serde_json::to_value(
            sample_server_multi_monitor()
                .applied_topology()
                .cloned()
                .expect("applied topology"),
        )
        .unwrap();
        json["topology_generation"] = 0.into();
        let error = serde_json::from_value::<AppliedMonitorTopologyMsg>(json).unwrap_err();
        assert!(error
            .to_string()
            .contains("topology generation must be nonzero"));
    }

    #[test]
    fn multi_monitor_capabilities_reject_invalid_max_monitors_and_helpers_refuse_invalid_state() {
        assert_eq!(
            ClientMultiMonitorMsg::new(0, Vec::new(), Vec::new(), None),
            Err(MultiMonitorValidationError::InvalidMaxMonitors(0))
        );
        assert_eq!(
            ServerMultiMonitorMsg::new(
                5,
                Vec::new(),
                false,
                TopologyBackendKindMsg::PhysicalOutputs,
                Vec::new(),
                None,
            ),
            Err(MultiMonitorValidationError::InvalidMaxMonitors(5))
        );

        let mut client_json = serde_json::to_value(sample_client_multi_monitor()).unwrap();
        client_json["max_monitors"] = 0.into();
        let client_error =
            serde_json::from_value::<ClientMultiMonitorMsg>(client_json).unwrap_err();
        assert!(client_error
            .to_string()
            .contains("max_monitors must be 1..=4"));

        let mut invalid_monitors = sample_requested_monitors();
        invalid_monitors[0].is_primary = false;
        let invalid_topology = RequestedMonitorTopologyMsg {
            monitors: invalid_monitors.clone(),
            primary_index: 0,
        };
        let base_auth = AuthResponse::password("artist", "secret");
        let request = sample_auth_request_with_offer(sample_auth_multi_monitor_offer());
        assert_eq!(
            base_auth
                .clone()
                .with_multi_monitor_v1(
                    request
                        .required_multi_monitor_v1_offer()
                        .expect("host offer"),
                    invalid_topology,
                    sample_client_supported_carriers(),
                )
                .unwrap_err(),
            MultiMonitorValidationError::PrimaryMonitorCount(0)
        );
        assert_eq!(base_auth.screen_width, 0);
        assert_eq!(base_auth.screen_height, 0);

        let invalid_capability = ClientMultiMonitorMsg {
            max_monitors: 4,
            supported_rotations: sample_supported_rotations(),
            carriers: sample_multi_monitor_carriers(),
            requested_topology: Some(RequestedMonitorTopologyMsg {
                monitors: invalid_monitors,
                primary_index: 0,
            }),
        };
        let base_hello = ClientHelloMsg::default();
        let error = base_hello
            .clone()
            .with_multi_monitor_v1(&invalid_capability)
            .unwrap_err();
        assert!(matches!(
            error,
            MultiMonitorCapabilityError::Validation(
                MultiMonitorValidationError::PrimaryMonitorCount(0)
            )
        ));
        assert_eq!(base_hello.screen_width, 1920);
        assert_eq!(base_hello.screen_height, 1080);
    }

    #[test]
    fn multi_monitor_advertisements_require_nonempty_support_vectors_in_constructors() {
        assert_eq!(
            AuthMultiMonitorOfferMsg::new(4, Vec::new(), sample_multi_monitor_carriers()),
            Err(MultiMonitorValidationError::EmptySupportedRotations)
        );
        assert_eq!(
            AuthMultiMonitorOfferMsg::new(4, vec![RotationMsg::Degrees0], Vec::new()),
            Err(MultiMonitorValidationError::EmptyAdvertisedCarriers)
        );

        assert_eq!(
            ClientMultiMonitorMsg::new(4, Vec::new(), sample_multi_monitor_carriers(), None),
            Err(MultiMonitorValidationError::EmptySupportedRotations)
        );
        assert_eq!(
            ClientMultiMonitorMsg::new(4, vec![RotationMsg::Degrees0], Vec::new(), None),
            Err(MultiMonitorValidationError::EmptyAdvertisedCarriers)
        );

        assert_eq!(
            ServerMultiMonitorMsg::new(
                4,
                Vec::new(),
                true,
                TopologyBackendKindMsg::PhysicalOutputs,
                sample_multi_monitor_carriers(),
                None,
            ),
            Err(MultiMonitorValidationError::EmptySupportedRotations)
        );
        assert_eq!(
            ServerMultiMonitorMsg::new(
                4,
                vec![RotationMsg::Degrees0],
                true,
                TopologyBackendKindMsg::PhysicalOutputs,
                Vec::new(),
                None,
            ),
            Err(MultiMonitorValidationError::EmptyAdvertisedCarriers)
        );

        assert_eq!(
            AuthMultiMonitorRequestMsg::new(sample_requested_topology(), Vec::new()),
            Err(MultiMonitorValidationError::EmptyClientSupportedCarriers)
        );
    }

    #[test]
    fn multi_monitor_capabilities_reject_duplicate_advertisements_and_cross_field_mismatches() {
        assert_eq!(
            AuthMultiMonitorOfferMsg::new(
                4,
                vec![RotationMsg::Degrees0, RotationMsg::Degrees0],
                sample_multi_monitor_carriers(),
            ),
            Err(MultiMonitorValidationError::DuplicateSupportedRotation(
                RotationMsg::Degrees0
            ))
        );
        assert_eq!(
            ServerMultiMonitorMsg::new(
                4,
                sample_supported_rotations(),
                true,
                TopologyBackendKindMsg::PhysicalOutputs,
                vec![
                    MultiMonitorCarrierMsg::MuxedReliableStream,
                    MultiMonitorCarrierMsg::MuxedReliableStream,
                ],
                None,
            ),
            Err(MultiMonitorValidationError::DuplicateAdvertisedCarrier(
                MultiMonitorCarrierMsg::MuxedReliableStream
            ))
        );
        assert_eq!(
            AuthMultiMonitorRequestMsg::new(
                sample_requested_topology(),
                vec![
                    MultiMonitorCarrierMsg::MuxedReliableStream,
                    MultiMonitorCarrierMsg::MuxedReliableStream,
                ],
            ),
            Err(
                MultiMonitorValidationError::DuplicateClientSupportedCarrier(
                    MultiMonitorCarrierMsg::MuxedReliableStream
                )
            )
        );
        assert_eq!(
            ClientMultiMonitorMsg::new(
                1,
                sample_supported_rotations(),
                sample_multi_monitor_carriers(),
                Some(sample_requested_topology()),
            ),
            Err(
                MultiMonitorValidationError::RequestedMonitorCountExceedsMax {
                    count: 2,
                    max_monitors: 1,
                }
            )
        );
        assert_eq!(
            ServerMultiMonitorMsg::new(
                4,
                vec![RotationMsg::Degrees0],
                true,
                TopologyBackendKindMsg::PhysicalOutputs,
                sample_multi_monitor_carriers(),
                sample_server_multi_monitor().applied_topology().cloned(),
            ),
            Err(MultiMonitorValidationError::UnsupportedAppliedRotation {
                client_display_id: "display-left".to_string(),
                rotation: RotationMsg::Degrees90,
            })
        );
        assert_eq!(
            ServerMultiMonitorMsg::new(
                4,
                sample_supported_rotations(),
                true,
                TopologyBackendKindMsg::PhysicalOutputs,
                vec![MultiMonitorCarrierMsg::MuxedReliableStream],
                sample_server_multi_monitor().applied_topology().cloned(),
            ),
            Err(MultiMonitorValidationError::UnadvertisedSelectedCarrier(
                MultiMonitorCarrierMsg::PerMonitorReliableStream
            ))
        );
    }

    #[test]
    fn multi_monitor_advertisements_require_nonempty_support_vectors_in_serde() {
        let offer_error = serde_json::from_value::<AuthMultiMonitorOfferMsg>(serde_json::json!({
            "max_monitors": 4,
            "supported_rotations": [],
            "carriers": ["muxed_reliable_stream"]
        }))
        .unwrap_err();
        assert!(offer_error
            .to_string()
            .contains("supported_rotations must advertise at least one rotation"));

        let client_error = serde_json::from_value::<ClientMultiMonitorMsg>(serde_json::json!({
            "max_monitors": 4,
            "supported_rotations": ["degrees0"],
            "carriers": []
        }))
        .unwrap_err();
        assert!(client_error
            .to_string()
            .contains("carriers must advertise at least one carrier"));

        let server_error = serde_json::from_value::<ServerMultiMonitorMsg>(serde_json::json!({
            "max_monitors": 4,
            "supported_rotations": [],
            "fixed_topology": true,
            "topology_backend": "physical_outputs",
            "carriers": ["muxed_reliable_stream"]
        }))
        .unwrap_err();
        assert!(server_error
            .to_string()
            .contains("supported_rotations must advertise at least one rotation"));

        let auth_request_error =
            serde_json::from_value::<AuthMultiMonitorRequestMsg>(serde_json::json!({
                "requested_topology": serde_json::to_value(sample_requested_topology()).unwrap(),
                "carriers": []
            }))
            .unwrap_err();
        assert!(auth_request_error.to_string().contains(
            "auth-time request carriers must include at least one client-supported carrier"
        ));
    }

    #[test]
    fn multi_monitor_capability_deserialization_rejects_invalid_json() {
        let mut offer_json = serde_json::to_value(sample_auth_request_with_offer(
            sample_auth_multi_monitor_offer(),
        ))
        .unwrap();
        offer_json["multi_monitor_v1"]["supported_rotations"] =
            serde_json::json!(["degrees0", "degrees0"]);
        let offer_error = serde_json::from_value::<AuthRequest>(offer_json).unwrap_err();
        assert!(offer_error
            .to_string()
            .contains("duplicate supported rotation: degrees0"));

        let mut client_json = serde_json::to_value(sample_client_multi_monitor()).unwrap();
        client_json["max_monitors"] = 1.into();
        let client_error =
            serde_json::from_value::<ClientMultiMonitorMsg>(client_json).unwrap_err();
        assert!(client_error
            .to_string()
            .contains("requested topology has 2 monitors but max_monitors advertises 1"));

        let mut server_json = serde_json::to_value(sample_server_multi_monitor()).unwrap();
        server_json["carriers"] = serde_json::json!(["muxed_reliable_stream"]);
        let server_error =
            serde_json::from_value::<ServerMultiMonitorMsg>(server_json).unwrap_err();
        assert!(server_error
            .to_string()
            .contains("selected_carrier per_monitor_reliable_stream was not advertised"));

        let mut descriptors = sample_applied_monitors();
        let mut descriptor_json =
            serde_json::to_value(descriptors.remove(0)).expect("descriptor json");
        descriptor_json["session_monitor_id"] = 0.into();
        let descriptor_error =
            serde_json::from_value::<AppliedMonitorDescriptorMsg>(descriptor_json).unwrap_err();
        assert!(descriptor_error
            .to_string()
            .contains("session_monitor_id must be 1..=65535"));
    }

    #[test]
    fn applied_topology_rejects_zero_session_monitor_id() {
        let mut monitors = sample_applied_monitors();
        monitors[0].session_monitor_id = 0;
        assert_eq!(
            AppliedMonitorTopologyMsg::new(
                9,
                0,
                0,
                6160,
                2338,
                2560,
                0,
                MultiMonitorCarrierMsg::PerMonitorReliableStream,
                monitors,
            ),
            Err(MultiMonitorValidationError::ZeroSessionMonitorId)
        );

        let mut server_json = serde_json::to_value(sample_server_multi_monitor()).unwrap();
        server_json["applied_topology"]["monitors"][0]["session_monitor_id"] = 0.into();
        let error = serde_json::from_value::<ServerMultiMonitorMsg>(server_json).unwrap_err();
        assert!(error
            .to_string()
            .contains("session_monitor_id must be 1..=65535"));
    }

    #[test]
    fn auth_response_deserialization_rejects_invalid_multi_monitor_v1_sidecar() {
        let request = sample_auth_request_with_offer(sample_auth_multi_monitor_offer());
        let mut json = serde_json::to_value(
            AuthResponse::password("artist", "secret")
                .with_multi_monitor_v1(
                    request
                        .required_multi_monitor_v1_offer()
                        .expect("host offer"),
                    sample_requested_topology(),
                    sample_client_supported_carriers(),
                )
                .expect("topology"),
        )
        .unwrap();
        json["multi_monitor_v1"]["requested_topology"]["monitors"][0]["is_primary"] = false.into();
        let error = serde_json::from_value::<AuthResponse>(json).unwrap_err();
        assert!(error
            .to_string()
            .contains("expected one primary monitor, found 0"));
    }
}
