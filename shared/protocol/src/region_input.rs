use serde::{Deserialize, Serialize};

use crate::messages::PenToolMsg;

pub const REGION_POINTER_MOTION: &str = "region_pointer_motion";
pub const REGION_POINTER_BUTTON: &str = "region_pointer_button";
pub const REGION_POINTER_SCROLL: &str = "region_pointer_scroll";
pub const REGION_POINTER_ENTER: &str = "region_pointer_enter";
pub const REGION_POINTER_LEAVE: &str = "region_pointer_leave";
pub const REGION_PEN_EVENT: &str = "region_pen_event";

fn default_region_pointer_motion_type() -> String {
    REGION_POINTER_MOTION.to_owned()
}

fn default_region_pointer_button_type() -> String {
    REGION_POINTER_BUTTON.to_owned()
}

fn default_region_pointer_scroll_type() -> String {
    REGION_POINTER_SCROLL.to_owned()
}

fn default_region_pointer_enter_type() -> String {
    REGION_POINTER_ENTER.to_owned()
}

fn default_region_pointer_leave_type() -> String {
    REGION_POINTER_LEAVE.to_owned()
}

fn default_region_pen_event_type() -> String {
    REGION_PEN_EVENT.to_owned()
}

/// Region generation, identity, and region-local logical position.
///
/// `logical_x` and `logical_y` are signed fixed-point values in units of
/// 1/120 logical pixel. Products validate membership against the negotiated
/// `arcen-media` region generation before applying an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionInputPositionMsg {
    pub region_generation: u64,
    pub region_id: u32,
    pub logical_x: i64,
    pub logical_y: i64,
}

impl RegionInputPositionMsg {
    fn validate(self) -> Result<(), RegionInputValidationError> {
        if self.region_generation == 0 {
            return Err(RegionInputValidationError::ZeroRegionGeneration);
        }
        if self.region_id == 0 {
            return Err(RegionInputValidationError::ZeroRegionId);
        }
        Ok(())
    }
}

/// Metadata shared by every region-scoped input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionInputMetadataMsg {
    pub sequence: u64,
    #[serde(default)]
    pub timestamp_ns: u64,
    #[serde(default)]
    pub coalescable: bool,
}

impl RegionInputMetadataMsg {
    fn validate(self) -> Result<(), RegionInputValidationError> {
        if self.sequence == 0 {
            Err(RegionInputValidationError::ZeroSequence)
        } else {
            Ok(())
        }
    }
}

/// Absolute pointer motion scoped to one negotiated region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionPointerMotionMsg {
    #[serde(rename = "type", default = "default_region_pointer_motion_type")]
    pub msg_type: String,
    #[serde(flatten)]
    pub position: RegionInputPositionMsg,
    #[serde(flatten)]
    pub metadata: RegionInputMetadataMsg,
}

impl RegionPointerMotionMsg {
    /// Validates nonzero wire identities and sequence.
    ///
    /// # Errors
    ///
    /// Returns the first invalid common region-input field.
    pub fn validate(&self) -> Result<(), RegionInputValidationError> {
        self.position.validate()?;
        self.metadata.validate()
    }
}

/// Pointer button transition at the latest authoritative region position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionPointerButtonMsg {
    #[serde(rename = "type", default = "default_region_pointer_button_type")]
    pub msg_type: String,
    #[serde(flatten)]
    pub position: RegionInputPositionMsg,
    pub button: u8,
    pub pressed: bool,
    #[serde(flatten)]
    pub metadata: RegionInputMetadataMsg,
}

impl RegionPointerButtonMsg {
    /// Validates nonzero wire identities, button, and sequence.
    ///
    /// # Errors
    ///
    /// Returns the first invalid region-input field.
    pub fn validate(&self) -> Result<(), RegionInputValidationError> {
        self.position.validate()?;
        if self.button == 0 {
            return Err(RegionInputValidationError::ZeroButton);
        }
        self.metadata.validate()
    }
}

/// Fixed-point logical scroll delta at the latest region position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegionPointerScrollMsg {
    #[serde(rename = "type", default = "default_region_pointer_scroll_type")]
    pub msg_type: String,
    #[serde(flatten)]
    pub position: RegionInputPositionMsg,
    pub delta_x: i64,
    pub delta_y: i64,
    #[serde(flatten)]
    pub metadata: RegionInputMetadataMsg,
}

impl RegionPointerScrollMsg {
    /// Validates nonzero wire identities and sequence.
    ///
    /// # Errors
    ///
    /// Returns the first invalid common region-input field.
    pub fn validate(&self) -> Result<(), RegionInputValidationError> {
        self.position.validate()?;
        self.metadata.validate()
    }
}

macro_rules! region_focus_message {
    ($name:ident, $default_type:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct $name {
            #[serde(rename = "type", default = $default_type)]
            pub msg_type: String,
            #[serde(flatten)]
            pub position: RegionInputPositionMsg,
            #[serde(flatten)]
            pub metadata: RegionInputMetadataMsg,
        }

        impl $name {
            /// Validates nonzero wire identities and sequence.
            ///
            /// # Errors
            ///
            /// Returns the first invalid common region-input field.
            pub fn validate(&self) -> Result<(), RegionInputValidationError> {
                self.position.validate()?;
                self.metadata.validate()
            }
        }
    };
}

region_focus_message!(RegionPointerEnterMsg, "default_region_pointer_enter_type");
region_focus_message!(RegionPointerLeaveMsg, "default_region_pointer_leave_type");

/// Full pen sample scoped to one negotiated region.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegionPenEventMsg {
    #[serde(rename = "type", default = "default_region_pen_event_type")]
    pub msg_type: String,
    #[serde(flatten)]
    pub position: RegionInputPositionMsg,
    pub pressure: f32,
    #[serde(default)]
    pub tilt_x_degrees: f32,
    #[serde(default)]
    pub tilt_y_degrees: f32,
    #[serde(default)]
    pub rotation_degrees: f32,
    pub tool: PenToolMsg,
    #[serde(default)]
    pub in_proximity: bool,
    #[serde(default)]
    pub touching: bool,
    #[serde(default)]
    pub buttons: u16,
    #[serde(flatten)]
    pub metadata: RegionInputMetadataMsg,
}

impl RegionPenEventMsg {
    /// Validates common region fields and physical pen ranges.
    ///
    /// # Errors
    ///
    /// Returns the first invalid region-input or pen field.
    pub fn validate(&self) -> Result<(), RegionInputValidationError> {
        self.position.validate()?;
        self.metadata.validate()?;
        validate_f32("pressure", self.pressure, 0.0, 1.0)?;
        validate_f32("tilt_x_degrees", self.tilt_x_degrees, -90.0, 90.0)?;
        validate_f32("tilt_y_degrees", self.tilt_y_degrees, -90.0, 90.0)?;
        validate_f32("rotation_degrees", self.rotation_degrees, 0.0, 360.0)
    }
}

fn validate_f32(
    field: &'static str,
    value: f32,
    minimum: f32,
    maximum: f32,
) -> Result<(), RegionInputValidationError> {
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(RegionInputValidationError::PenFieldOutOfRange(field))
    }
}

/// Invalid region-scoped input wire field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionInputValidationError {
    ZeroRegionGeneration,
    ZeroRegionId,
    ZeroSequence,
    ZeroButton,
    PenFieldOutOfRange(&'static str),
}

impl std::fmt::Display for RegionInputValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroRegionGeneration => formatter.write_str("region generation must be nonzero"),
            Self::ZeroRegionId => formatter.write_str("region id must be nonzero"),
            Self::ZeroSequence => formatter.write_str("region input sequence must be nonzero"),
            Self::ZeroButton => formatter.write_str("pointer button must be nonzero"),
            Self::PenFieldOutOfRange(field) => {
                write!(formatter, "region pen {field} is outside its allowed range")
            }
        }
    }
}

impl std::error::Error for RegionInputValidationError {}
