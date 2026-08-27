use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_media::{AppliedRegionSet, LogicalPoint, RegionGeneration, RegionId};

use crate::PenTool;

/// Region identity and region-local fixed-point logical position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionLogicalPosition {
    pub region_id: RegionId,
    pub point: LogicalPoint,
}

/// Canonical region-scoped pen sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionPenSample {
    pub position: RegionLogicalPosition,
    pub pressure: f32,
    pub tilt_x_degrees: f32,
    pub tilt_y_degrees: f32,
    pub rotation_degrees: f32,
    pub tool: PenTool,
    pub in_proximity: bool,
    pub touching: bool,
    pub buttons: u16,
}

impl RegionPenSample {
    fn validate(self) -> Result<(), RegionInputStateError> {
        validate_f32("pressure", self.pressure, 0.0, 1.0)?;
        validate_f32("tilt_x_degrees", self.tilt_x_degrees, -90.0, 90.0)?;
        validate_f32("tilt_y_degrees", self.tilt_y_degrees, -90.0, 90.0)?;
        validate_f32("rotation_degrees", self.rotation_degrees, 0.0, 360.0)
    }
}

/// Last accepted region-scoped pen state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionPenState {
    pub sample: RegionPenSample,
}

/// Semantic region input event after checked wire-to-domain conversion.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum RegionInputEvent {
    PointerEnter {
        generation: RegionGeneration,
        position: RegionLogicalPosition,
        sequence: u64,
    },
    PointerLeave {
        generation: RegionGeneration,
        position: RegionLogicalPosition,
        sequence: u64,
    },
    PointerMotion {
        generation: RegionGeneration,
        position: RegionLogicalPosition,
        sequence: u64,
    },
    PointerButton {
        generation: RegionGeneration,
        position: RegionLogicalPosition,
        button: u8,
        pressed: bool,
        sequence: u64,
    },
    PointerScroll {
        generation: RegionGeneration,
        position: RegionLogicalPosition,
        delta_x: i64,
        delta_y: i64,
        sequence: u64,
    },
    Pen {
        generation: RegionGeneration,
        sample: RegionPenSample,
        sequence: u64,
    },
}

impl RegionInputEvent {
    /// The region generation this event was produced against.
    #[must_use]
    pub const fn generation(self) -> RegionGeneration {
        match self {
            Self::PointerEnter { generation, .. }
            | Self::PointerLeave { generation, .. }
            | Self::PointerMotion { generation, .. }
            | Self::PointerButton { generation, .. }
            | Self::PointerScroll { generation, .. }
            | Self::Pen { generation, .. } => generation,
        }
    }

    /// The strictly increasing input sequence of this event.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        match self {
            Self::PointerEnter { sequence, .. }
            | Self::PointerLeave { sequence, .. }
            | Self::PointerMotion { sequence, .. }
            | Self::PointerButton { sequence, .. }
            | Self::PointerScroll { sequence, .. }
            | Self::Pen { sequence, .. } => sequence,
        }
    }

    /// The region-local logical position carried by this event.
    #[must_use]
    pub const fn position(self) -> RegionLogicalPosition {
        match self {
            Self::PointerEnter { position, .. }
            | Self::PointerLeave { position, .. }
            | Self::PointerMotion { position, .. }
            | Self::PointerButton { position, .. }
            | Self::PointerScroll { position, .. } => position,
            Self::Pen { sample, .. } => sample.position,
        }
    }
}

/// Releases required when focus or the connection is lost.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleasedRegionInput {
    pub pointer_position: Option<RegionLogicalPosition>,
    pub buttons: Vec<u8>,
    pub pen: Option<RegionPenState>,
    pub had_focus: bool,
}

/// Dependency-safe semantic state for one ordered region input stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegionInputState {
    active_pointer_region: Option<RegionId>,
    latest_pointer_position: Option<RegionLogicalPosition>,
    held_buttons: BTreeSet<u8>,
    pen: Option<RegionPenState>,
    focused: bool,
    last_sequence: u64,
}

impl RegionInputState {
    /// Creates the empty, unfocused state at sequence zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active_pointer_region: None,
            latest_pointer_position: None,
            held_buttons: BTreeSet::new(),
            pen: None,
            focused: false,
            last_sequence: 0,
        }
    }

    #[must_use]
    pub const fn active_pointer_region(&self) -> Option<RegionId> {
        self.active_pointer_region
    }

    #[must_use]
    pub const fn latest_pointer_position(&self) -> Option<RegionLogicalPosition> {
        self.latest_pointer_position
    }

    #[must_use]
    pub fn held_buttons(&self) -> impl ExactSizeIterator<Item = u8> + '_ {
        self.held_buttons.iter().copied()
    }

    #[must_use]
    pub const fn pen(&self) -> Option<RegionPenState> {
        self.pen
    }

    #[must_use]
    pub const fn is_focused(&self) -> bool {
        self.focused
    }

    #[must_use]
    pub const fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    /// Validates and applies one event atomically.
    ///
    /// # Errors
    ///
    /// Rejects stale generations, unknown regions, out-of-region positions,
    /// non-increasing sequences, inconsistent pointer focus/positions, invalid
    /// button edges, and invalid pen ranges without changing state.
    pub fn apply(
        &mut self,
        regions: &AppliedRegionSet,
        event: RegionInputEvent,
    ) -> Result<(), RegionInputStateError> {
        self.validate_common(regions, event)?;

        match event {
            RegionInputEvent::PointerEnter { position, .. } => {
                if self.focused {
                    return Err(RegionInputStateError::PointerAlreadyFocused);
                }
                self.active_pointer_region = Some(position.region_id);
                self.latest_pointer_position = Some(position);
                self.focused = true;
            }
            RegionInputEvent::PointerLeave { position, .. } => {
                self.require_active(position.region_id)?;
                self.latest_pointer_position = Some(position);
                self.active_pointer_region = None;
                self.focused = false;
            }
            RegionInputEvent::PointerMotion { position, .. } => {
                self.require_active(position.region_id)?;
                self.latest_pointer_position = Some(position);
            }
            RegionInputEvent::PointerButton {
                position,
                button,
                pressed,
                ..
            } => {
                self.require_latest(position)?;
                if button == 0 {
                    return Err(RegionInputStateError::ZeroButton);
                }
                if pressed {
                    if self.held_buttons.contains(&button) {
                        return Err(RegionInputStateError::ButtonAlreadyHeld(button));
                    }
                    self.held_buttons.insert(button);
                } else if !self.held_buttons.remove(&button) {
                    return Err(RegionInputStateError::ButtonNotHeld(button));
                }
            }
            RegionInputEvent::PointerScroll { position, .. } => {
                self.require_latest(position)?;
            }
            RegionInputEvent::Pen { sample, .. } => {
                sample.validate()?;
                self.pen = if sample.in_proximity || sample.touching || sample.buttons != 0 {
                    Some(RegionPenState { sample })
                } else {
                    None
                };
            }
        }

        self.last_sequence = event.sequence();
        Ok(())
    }

    /// Clears all held/transient state and returns the native releases a
    /// product adapter must emit. The accepted sequence remains monotonic.
    #[must_use]
    pub fn release_all(&mut self) -> ReleasedRegionInput {
        let released = ReleasedRegionInput {
            pointer_position: self.latest_pointer_position,
            buttons: self.held_buttons.iter().copied().collect(),
            pen: self.pen,
            had_focus: self.focused,
        };
        self.held_buttons.clear();
        self.pen = None;
        self.active_pointer_region = None;
        self.focused = false;
        released
    }

    fn validate_common(
        &self,
        regions: &AppliedRegionSet,
        event: RegionInputEvent,
    ) -> Result<(), RegionInputStateError> {
        if event.generation() != regions.generation() {
            return Err(RegionInputStateError::StaleGeneration {
                expected: regions.generation(),
                received: event.generation(),
            });
        }
        let sequence = event.sequence();
        if sequence == 0 || sequence <= self.last_sequence {
            return Err(RegionInputStateError::InvalidSequence {
                last: self.last_sequence,
                received: sequence,
            });
        }
        match event {
            RegionInputEvent::PointerEnter { position, .. }
            | RegionInputEvent::PointerLeave { position, .. }
            | RegionInputEvent::PointerMotion { position, .. }
            | RegionInputEvent::PointerButton { position, .. }
            | RegionInputEvent::PointerScroll { position, .. } => {
                validate_position(regions, position)
            }
            RegionInputEvent::Pen { sample, .. } => validate_position(regions, sample.position),
        }
    }

    fn require_active(&self, region_id: RegionId) -> Result<(), RegionInputStateError> {
        if self.focused && self.active_pointer_region == Some(region_id) {
            Ok(())
        } else {
            Err(RegionInputStateError::PointerNotFocused(region_id))
        }
    }

    fn require_latest(&self, position: RegionLogicalPosition) -> Result<(), RegionInputStateError> {
        self.require_active(position.region_id)?;
        if self.latest_pointer_position == Some(position) {
            Ok(())
        } else {
            Err(RegionInputStateError::ButtonPositionMismatch {
                expected: self.latest_pointer_position,
                received: position,
            })
        }
    }
}

fn validate_position(
    regions: &AppliedRegionSet,
    position: RegionLogicalPosition,
) -> Result<(), RegionInputStateError> {
    let region = regions
        .get(position.region_id)
        .ok_or(RegionInputStateError::UnknownRegion(position.region_id))?;
    let size = region.descriptor().logical_rect().size();
    let x = u64::try_from(position.point.x)
        .map_err(|_| RegionInputStateError::PositionOutsideRegion(position))?;
    let y = u64::try_from(position.point.y)
        .map_err(|_| RegionInputStateError::PositionOutsideRegion(position))?;
    if x < size.width() && y < size.height() {
        Ok(())
    } else {
        Err(RegionInputStateError::PositionOutsideRegion(position))
    }
}

fn validate_f32(
    field: &'static str,
    value: f32,
    minimum: f32,
    maximum: f32,
) -> Result<(), RegionInputStateError> {
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(RegionInputStateError::PenFieldOutOfRange(field))
    }
}

/// Rejected semantic region input transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionInputStateError {
    StaleGeneration {
        expected: RegionGeneration,
        received: RegionGeneration,
    },
    UnknownRegion(RegionId),
    PositionOutsideRegion(RegionLogicalPosition),
    InvalidSequence {
        last: u64,
        received: u64,
    },
    PointerAlreadyFocused,
    PointerNotFocused(RegionId),
    ButtonPositionMismatch {
        expected: Option<RegionLogicalPosition>,
        received: RegionLogicalPosition,
    },
    ZeroButton,
    ButtonAlreadyHeld(u8),
    ButtonNotHeld(u8),
    PenFieldOutOfRange(&'static str),
}

impl Display for RegionInputStateError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration { expected, received } => write!(
                formatter,
                "stale region generation {} (expected {})",
                received.get(),
                expected.get()
            ),
            Self::UnknownRegion(id) => write!(formatter, "unknown region {}", id.get()),
            Self::PositionOutsideRegion(_) => {
                formatter.write_str("logical position is outside the region")
            }
            Self::InvalidSequence { last, received } => {
                write!(
                    formatter,
                    "input sequence {received} does not follow {last}"
                )
            }
            Self::PointerAlreadyFocused => formatter.write_str("pointer is already focused"),
            Self::PointerNotFocused(id) => {
                write!(formatter, "pointer is not focused in region {}", id.get())
            }
            Self::ButtonPositionMismatch { .. } => {
                formatter.write_str("button or scroll position is not the latest pointer position")
            }
            Self::ZeroButton => formatter.write_str("pointer button must be nonzero"),
            Self::ButtonAlreadyHeld(button) => {
                write!(formatter, "pointer button {button} is already held")
            }
            Self::ButtonNotHeld(button) => {
                write!(formatter, "pointer button {button} is not held")
            }
            Self::PenFieldOutOfRange(field) => {
                write!(formatter, "pen {field} is outside its allowed range")
            }
        }
    }
}

impl Error for RegionInputStateError {}
