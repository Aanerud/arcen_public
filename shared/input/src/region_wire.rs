//! Paired encode/decode between the input-v4 `Region*` wire DTOs and the
//! canonical [`RegionInputEvent`] domain.
//!
//! Every product used to carry its own copy of this conversion. The encoder
//! (Deck) and the decoders (Linux and Windows Piers) now share one
//! implementation, so a message produced by [`RegionInputWireMessage::encode`]
//! is byte-identical everywhere and [`RegionInputWireMessage::decode`] is its
//! exact inverse.
//!
//! [`RegionInputWireRef`] is the borrowed form used on the decode-heavy host
//! path: it dispatches over an already-owned DTO without cloning its `type`
//! string.

use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_media::{LogicalPoint, RegionContractError, RegionGeneration, RegionId};
use arcen_protocol::messages::{
    PenToolMsg, REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER,
    REGION_POINTER_LEAVE, REGION_POINTER_MOTION, REGION_POINTER_SCROLL, RegionInputMetadataMsg,
    RegionInputPositionMsg, RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg,
    RegionPointerEnterMsg, RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};

use crate::PenTool;
use crate::region_state::{RegionInputEvent, RegionLogicalPosition, RegionPenSample};

/// Converts a wire region generation into the checked domain generation.
///
/// # Errors
///
/// Returns [`RegionContractError::ZeroGeneration`] for zero.
pub const fn domain_generation(value: u64) -> Result<RegionGeneration, RegionContractError> {
    RegionGeneration::new(value)
}

/// Converts a wire region position into the checked domain position.
///
/// # Errors
///
/// Returns [`RegionContractError::ZeroRegionId`] for a zero region identity.
pub fn domain_position(
    position: RegionInputPositionMsg,
) -> Result<RegionLogicalPosition, RegionContractError> {
    Ok(RegionLogicalPosition {
        region_id: RegionId::new(position.region_id)?,
        point: LogicalPoint::new(position.logical_x, position.logical_y),
    })
}

/// Converts a wire pen tool into the canonical domain tool.
#[must_use]
pub const fn domain_pen_tool(tool: PenToolMsg) -> PenTool {
    match tool {
        PenToolMsg::Tip => PenTool::Tip,
        PenToolMsg::Eraser => PenTool::Eraser,
    }
}

/// Converts a canonical domain pen tool into its wire identifier.
#[must_use]
pub const fn wire_pen_tool(tool: PenTool) -> PenToolMsg {
    match tool {
        PenTool::Tip => PenToolMsg::Tip,
        PenTool::Eraser => PenToolMsg::Eraser,
    }
}

/// Converts a domain generation and position into the wire position DTO.
#[must_use]
pub const fn wire_position(
    generation: RegionGeneration,
    position: RegionLogicalPosition,
) -> RegionInputPositionMsg {
    RegionInputPositionMsg {
        region_generation: generation.get(),
        region_id: position.region_id.get(),
        logical_x: position.point.x,
        logical_y: position.point.y,
    }
}

/// One borrowed input-v4 `Region*` protocol DTO.
///
/// Hosts decode into an owned DTO and then dispatch through this reference so
/// the shared pipeline never clones a wire message.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RegionInputWireRef<'a> {
    PointerEnter(&'a RegionPointerEnterMsg),
    PointerLeave(&'a RegionPointerLeaveMsg),
    PointerMotion(&'a RegionPointerMotionMsg),
    PointerButton(&'a RegionPointerButtonMsg),
    PointerScroll(&'a RegionPointerScrollMsg),
    Pen(&'a RegionPenEventMsg),
}

impl RegionInputWireRef<'_> {
    /// Validates the wire identities, sequence, and physical ranges.
    ///
    /// # Errors
    ///
    /// Returns the first invalid region-input field.
    pub fn validate(self) -> Result<(), RegionInputValidationError> {
        match self {
            Self::PointerEnter(message) => message.validate(),
            Self::PointerLeave(message) => message.validate(),
            Self::PointerMotion(message) => message.validate(),
            Self::PointerButton(message) => message.validate(),
            Self::PointerScroll(message) => message.validate(),
            Self::Pen(message) => message.validate(),
        }
    }

    /// Region generation, identity, and region-local logical position.
    #[must_use]
    pub const fn position(self) -> RegionInputPositionMsg {
        match self {
            Self::PointerEnter(message) => message.position,
            Self::PointerLeave(message) => message.position,
            Self::PointerMotion(message) => message.position,
            Self::PointerButton(message) => message.position,
            Self::PointerScroll(message) => message.position,
            Self::Pen(message) => message.position,
        }
    }

    /// Sequence and transport metadata carried by the DTO.
    #[must_use]
    pub const fn metadata(self) -> RegionInputMetadataMsg {
        match self {
            Self::PointerEnter(message) => message.metadata,
            Self::PointerLeave(message) => message.metadata,
            Self::PointerMotion(message) => message.metadata,
            Self::PointerButton(message) => message.metadata,
            Self::PointerScroll(message) => message.metadata,
            Self::Pen(message) => message.metadata,
        }
    }

    /// The canonical `"type"` discriminator of the DTO.
    #[must_use]
    pub const fn input_type(self) -> &'static str {
        match self {
            Self::PointerEnter(_) => REGION_POINTER_ENTER,
            Self::PointerLeave(_) => REGION_POINTER_LEAVE,
            Self::PointerMotion(_) => REGION_POINTER_MOTION,
            Self::PointerButton(_) => REGION_POINTER_BUTTON,
            Self::PointerScroll(_) => REGION_POINTER_SCROLL,
            Self::Pen(_) => REGION_PEN_EVENT,
        }
    }

    /// The button edge carried by a `region_pointer_button` DTO.
    #[must_use]
    pub const fn button_state(self) -> Option<(u8, bool)> {
        match self {
            Self::PointerButton(message) => Some((message.button, message.pressed)),
            _ => None,
        }
    }

    /// Validates the DTO and decodes it into the canonical domain event.
    ///
    /// This is the exact inverse of [`RegionInputWireMessage::encode`]; the
    /// transport metadata carried by the DTO is not part of the domain event.
    ///
    /// # Errors
    ///
    /// Returns the first invalid wire field, or a region-domain error when a
    /// validated identity still cannot be represented.
    pub fn decode(self) -> Result<RegionInputEvent, RegionInputDecodeError> {
        self.validate()?;
        let generation = domain_generation(self.position().region_generation)?;
        let position = domain_position(self.position())?;
        let sequence = self.metadata().sequence;
        Ok(match self {
            Self::PointerEnter(_) => RegionInputEvent::PointerEnter {
                generation,
                position,
                sequence,
            },
            Self::PointerLeave(_) => RegionInputEvent::PointerLeave {
                generation,
                position,
                sequence,
            },
            Self::PointerMotion(_) => RegionInputEvent::PointerMotion {
                generation,
                position,
                sequence,
            },
            Self::PointerButton(message) => RegionInputEvent::PointerButton {
                generation,
                position,
                button: message.button,
                pressed: message.pressed,
                sequence,
            },
            Self::PointerScroll(message) => RegionInputEvent::PointerScroll {
                generation,
                position,
                delta_x: message.delta_x,
                delta_y: message.delta_y,
                sequence,
            },
            Self::Pen(message) => RegionInputEvent::Pen {
                generation,
                sample: RegionPenSample {
                    position,
                    pressure: message.pressure,
                    tilt_x_degrees: message.tilt_x_degrees,
                    tilt_y_degrees: message.tilt_y_degrees,
                    rotation_degrees: message.rotation_degrees,
                    tool: domain_pen_tool(message.tool),
                    in_proximity: message.in_proximity,
                    touching: message.touching,
                    buttons: message.buttons,
                },
                sequence,
            },
        })
    }

    /// Serializes the concrete `Region*` DTO without adding any legacy
    /// desktop-coordinate field.
    ///
    /// # Errors
    ///
    /// Returns the underlying serialization failure.
    pub fn to_json_value(self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            Self::PointerEnter(message) => serde_json::to_value(message),
            Self::PointerLeave(message) => serde_json::to_value(message),
            Self::PointerMotion(message) => serde_json::to_value(message),
            Self::PointerButton(message) => serde_json::to_value(message),
            Self::PointerScroll(message) => serde_json::to_value(message),
            Self::Pen(message) => serde_json::to_value(message),
        }
    }

    /// Clones the borrowed DTO into its owned form.
    #[must_use]
    pub fn to_owned_message(self) -> RegionInputWireMessage {
        match self {
            Self::PointerEnter(message) => RegionInputWireMessage::PointerEnter(message.clone()),
            Self::PointerLeave(message) => RegionInputWireMessage::PointerLeave(message.clone()),
            Self::PointerMotion(message) => RegionInputWireMessage::PointerMotion(message.clone()),
            Self::PointerButton(message) => RegionInputWireMessage::PointerButton(message.clone()),
            Self::PointerScroll(message) => RegionInputWireMessage::PointerScroll(message.clone()),
            Self::Pen(message) => RegionInputWireMessage::Pen(message.clone()),
        }
    }
}

/// One validated, owned input-v4 `Region*` protocol DTO.
///
/// This is the single wire representation shared by the Deck encoder and the
/// Pier decoders; no product builds or interprets the individual DTOs on its
/// own.
#[derive(Debug, Clone, PartialEq)]
pub enum RegionInputWireMessage {
    PointerEnter(RegionPointerEnterMsg),
    PointerLeave(RegionPointerLeaveMsg),
    PointerMotion(RegionPointerMotionMsg),
    PointerButton(RegionPointerButtonMsg),
    PointerScroll(RegionPointerScrollMsg),
    Pen(RegionPenEventMsg),
}

impl RegionInputWireMessage {
    /// Encodes one accepted domain event into its wire DTO.
    ///
    /// The wire sequence always mirrors the event sequence; only the transport
    /// metadata is supplied by the caller.
    #[must_use]
    pub fn encode(event: RegionInputEvent, timestamp_ns: u64, coalescable: bool) -> Self {
        let generation = event.generation();
        let metadata = RegionInputMetadataMsg {
            sequence: event.sequence(),
            timestamp_ns,
            coalescable,
        };
        match event {
            RegionInputEvent::PointerEnter { position, .. } => {
                Self::PointerEnter(RegionPointerEnterMsg {
                    msg_type: REGION_POINTER_ENTER.to_owned(),
                    position: wire_position(generation, position),
                    metadata,
                })
            }
            RegionInputEvent::PointerLeave { position, .. } => {
                Self::PointerLeave(RegionPointerLeaveMsg {
                    msg_type: REGION_POINTER_LEAVE.to_owned(),
                    position: wire_position(generation, position),
                    metadata,
                })
            }
            RegionInputEvent::PointerMotion { position, .. } => {
                Self::PointerMotion(RegionPointerMotionMsg {
                    msg_type: REGION_POINTER_MOTION.to_owned(),
                    position: wire_position(generation, position),
                    metadata,
                })
            }
            RegionInputEvent::PointerButton {
                position,
                button,
                pressed,
                ..
            } => Self::PointerButton(RegionPointerButtonMsg {
                msg_type: REGION_POINTER_BUTTON.to_owned(),
                position: wire_position(generation, position),
                button,
                pressed,
                metadata,
            }),
            RegionInputEvent::PointerScroll {
                position,
                delta_x,
                delta_y,
                ..
            } => Self::PointerScroll(RegionPointerScrollMsg {
                msg_type: REGION_POINTER_SCROLL.to_owned(),
                position: wire_position(generation, position),
                delta_x,
                delta_y,
                metadata,
            }),
            RegionInputEvent::Pen { sample, .. } => Self::Pen(RegionPenEventMsg {
                msg_type: REGION_PEN_EVENT.to_owned(),
                position: wire_position(generation, sample.position),
                pressure: sample.pressure,
                tilt_x_degrees: sample.tilt_x_degrees,
                tilt_y_degrees: sample.tilt_y_degrees,
                rotation_degrees: sample.rotation_degrees,
                tool: wire_pen_tool(sample.tool),
                in_proximity: sample.in_proximity,
                touching: sample.touching,
                buttons: sample.buttons,
                metadata,
            }),
        }
    }

    /// Borrows the owned DTO for shared dispatch.
    #[must_use]
    pub const fn as_ref(&self) -> RegionInputWireRef<'_> {
        match self {
            Self::PointerEnter(message) => RegionInputWireRef::PointerEnter(message),
            Self::PointerLeave(message) => RegionInputWireRef::PointerLeave(message),
            Self::PointerMotion(message) => RegionInputWireRef::PointerMotion(message),
            Self::PointerButton(message) => RegionInputWireRef::PointerButton(message),
            Self::PointerScroll(message) => RegionInputWireRef::PointerScroll(message),
            Self::Pen(message) => RegionInputWireRef::Pen(message),
        }
    }

    /// Validates the wire identities, sequence, and physical ranges.
    ///
    /// # Errors
    ///
    /// Returns the first invalid region-input field.
    pub fn validate(&self) -> Result<(), RegionInputValidationError> {
        self.as_ref().validate()
    }

    /// Validates the DTO and decodes it into the canonical domain event.
    ///
    /// # Errors
    ///
    /// Returns the first invalid wire field, or a region-domain error when a
    /// validated identity still cannot be represented.
    pub fn decode(&self) -> Result<RegionInputEvent, RegionInputDecodeError> {
        self.as_ref().decode()
    }

    /// Region generation, identity, and region-local logical position.
    #[must_use]
    pub const fn position(&self) -> RegionInputPositionMsg {
        self.as_ref().position()
    }

    /// Sequence and transport metadata carried by the DTO.
    #[must_use]
    pub const fn metadata(&self) -> RegionInputMetadataMsg {
        self.as_ref().metadata()
    }

    /// The canonical `"type"` discriminator of the DTO.
    #[must_use]
    pub const fn input_type(&self) -> &'static str {
        self.as_ref().input_type()
    }

    /// The button edge carried by a `region_pointer_button` DTO.
    #[must_use]
    pub const fn button_state(&self) -> Option<(u8, bool)> {
        self.as_ref().button_state()
    }

    /// Serializes the concrete `Region*` DTO without adding any legacy
    /// desktop-coordinate field.
    ///
    /// # Errors
    ///
    /// Returns the underlying serialization failure.
    pub fn to_json_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        self.as_ref().to_json_value()
    }

    /// Parses a `Region*` DTO from its `"type"`-tagged JSON form.
    ///
    /// Framing only: the caller still validates or decodes the result before
    /// advancing any state.
    ///
    /// # Errors
    ///
    /// Returns [`RegionInputWireError::UnknownMessageType`] for a non-region
    /// or untagged payload and [`RegionInputWireError::Json`] for a malformed
    /// body.
    pub fn from_json_value(value: &serde_json::Value) -> Result<Self, RegionInputWireError> {
        let message_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| RegionInputWireError::UnknownMessageType(String::new()))?;
        let parsed = match message_type {
            REGION_POINTER_ENTER => Self::PointerEnter(serde_json::from_value(value.clone())?),
            REGION_POINTER_LEAVE => Self::PointerLeave(serde_json::from_value(value.clone())?),
            REGION_POINTER_MOTION => Self::PointerMotion(serde_json::from_value(value.clone())?),
            REGION_POINTER_BUTTON => Self::PointerButton(serde_json::from_value(value.clone())?),
            REGION_POINTER_SCROLL => Self::PointerScroll(serde_json::from_value(value.clone())?),
            REGION_PEN_EVENT => Self::Pen(serde_json::from_value(value.clone())?),
            other => return Err(RegionInputWireError::UnknownMessageType(other.to_owned())),
        };
        Ok(parsed)
    }

    /// Parses a `Region*` DTO from its JSON text form.
    ///
    /// # Errors
    ///
    /// Returns [`RegionInputWireError::Json`] for malformed JSON and
    /// [`RegionInputWireError::UnknownMessageType`] for a non-region payload.
    pub fn from_json_str(json: &str) -> Result<Self, RegionInputWireError> {
        Self::from_json_value(&serde_json::from_str::<serde_json::Value>(json)?)
    }

    /// Parses and decodes one `Region*` JSON payload in a single step.
    ///
    /// # Errors
    ///
    /// Returns [`RegionInputWireError::UnknownMessageType`] for a non-region
    /// payload, [`RegionInputWireError::Json`] for a malformed body, and
    /// [`RegionInputWireError::Decode`] for an invalid region-input field.
    pub fn decode_json_value(
        value: &serde_json::Value,
    ) -> Result<RegionInputEvent, RegionInputWireError> {
        Ok(Self::from_json_value(value)?.decode()?)
    }

    /// Parses and decodes one `Region*` JSON text payload in a single step.
    ///
    /// # Errors
    ///
    /// Returns [`RegionInputWireError::UnknownMessageType`] for a non-region
    /// payload, [`RegionInputWireError::Json`] for a malformed body, and
    /// [`RegionInputWireError::Decode`] for an invalid region-input field.
    pub fn decode_json_str(json: &str) -> Result<RegionInputEvent, RegionInputWireError> {
        Ok(Self::from_json_str(json)?.decode()?)
    }
}

/// Rejected wire-to-domain region input conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionInputDecodeError {
    Wire(RegionInputValidationError),
    Contract(RegionContractError),
}

impl Display for RegionInputDecodeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(error) => Display::fmt(error, formatter),
            Self::Contract(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for RegionInputDecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Wire(error) => Some(error),
            Self::Contract(error) => Some(error),
        }
    }
}

impl From<RegionInputValidationError> for RegionInputDecodeError {
    fn from(value: RegionInputValidationError) -> Self {
        Self::Wire(value)
    }
}

impl From<RegionContractError> for RegionInputDecodeError {
    fn from(value: RegionContractError) -> Self {
        Self::Contract(value)
    }
}

/// Rejected `Region*` JSON framing.
#[derive(Debug)]
#[non_exhaustive]
pub enum RegionInputWireError {
    UnknownMessageType(String),
    Json(serde_json::Error),
    Decode(RegionInputDecodeError),
}

impl Display for RegionInputWireError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownMessageType(message_type) => {
                write!(formatter, "unknown region input type {message_type:?}")
            }
            Self::Json(error) => Display::fmt(error, formatter),
            Self::Decode(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for RegionInputWireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::UnknownMessageType(_) => None,
            Self::Json(error) => Some(error),
            Self::Decode(error) => Some(error),
        }
    }
}

impl From<serde_json::Error> for RegionInputWireError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<RegionInputDecodeError> for RegionInputWireError {
    fn from(value: RegionInputDecodeError) -> Self {
        Self::Decode(value)
    }
}
