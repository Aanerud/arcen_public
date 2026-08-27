//! Shared client-side region input encoder and state emitter.
//!
//! [`RegionInputEmitter`] is the exact mirror of [`crate::RegionInputPipeline`]
//! for the sending endpoint. It owns the ordered [`RegionInputState`], derives
//! the enter/leave/motion transitions a region change implies, allocates the
//! strictly increasing input sequence, and encodes each accepted transition
//! into a validated [`RegionInputWireMessage`].
//!
//! It deliberately borrows the applied aggregate per call instead of owning it,
//! so a client can keep its own viewport bookkeeping and swap the negotiated
//! aggregate on renegotiation without rebuilding the emitter.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_media::{AppliedRegionSet, RegionContractError};
use arcen_protocol::messages::RegionInputValidationError;

use crate::region_state::{
    RegionInputEvent, RegionInputState, RegionInputStateError, RegionLogicalPosition,
    RegionPenSample, ReleasedRegionInput,
};
use crate::region_wire::RegionInputWireMessage;

/// Ordered region input encoder for one client input stream.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RegionInputEmitter {
    state: RegionInputState,
    sequence: u64,
}

impl RegionInputEmitter {
    /// Creates an emitter whose first emitted sequence is `1`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: RegionInputState::new(),
            sequence: 0,
        }
    }

    /// Creates an emitter that continues an existing sequence.
    #[must_use]
    pub const fn with_sequence(sequence: u64) -> Self {
        Self {
            state: RegionInputState::new(),
            sequence,
        }
    }

    #[must_use]
    pub const fn state(&self) -> &RegionInputState {
        &self.state
    }

    /// The most recently allocated input sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Raises the allocation floor to a sequence allocated elsewhere.
    ///
    /// A client whose input sequence is one session-global counter shared
    /// with non-region input (keyboard, pen, legacy compatibility messages)
    /// calls this before emitting so the next allocated region sequence
    /// continues that counter. The floor never moves backwards, so an already
    /// accepted region sequence can never be reissued.
    pub const fn advance_sequence_to(&mut self, sequence: u64) {
        if sequence > self.sequence {
            self.sequence = sequence;
        }
    }

    /// Emits the enter/leave/motion transitions required to place the pointer
    /// at `position`.
    ///
    /// # Errors
    ///
    /// Returns an ordered-state or wire validation error.
    pub fn pointer_motion(
        &mut self,
        regions: &AppliedRegionSet,
        position: RegionLogicalPosition,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, RegionInputEmitError> {
        self.ensure_pointer_position(regions, position, timestamp_ns)
    }

    /// Emits the transitions required to place the pointer at `position`, then
    /// one button edge for every button whose state differs from the held set.
    ///
    /// # Errors
    ///
    /// Returns an ordered-state or wire validation error.
    pub fn pointer_sample(
        &mut self,
        regions: &AppliedRegionSet,
        position: RegionLogicalPosition,
        buttons: &[(u8, bool)],
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, RegionInputEmitError> {
        let mut messages = self.ensure_pointer_position(regions, position, timestamp_ns)?;
        let held = self.state.held_buttons().collect::<BTreeSet<_>>();
        for &(button, pressed) in buttons {
            if held.contains(&button) != pressed {
                messages.push(self.emit_button(
                    regions,
                    position,
                    button,
                    pressed,
                    timestamp_ns,
                )?);
            }
        }
        Ok(messages)
    }

    /// Emits the transitions required to place the pointer at `position`, then
    /// one button edge when that button's state actually changes.
    ///
    /// # Errors
    ///
    /// Returns an ordered-state or wire validation error.
    pub fn pointer_button(
        &mut self,
        regions: &AppliedRegionSet,
        position: RegionLogicalPosition,
        button: u8,
        pressed: bool,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, RegionInputEmitError> {
        let mut messages = self.ensure_pointer_position(regions, position, timestamp_ns)?;
        if self.state.held_buttons().any(|held| held == button) != pressed {
            messages.push(self.emit_button(regions, position, button, pressed, timestamp_ns)?);
        }
        Ok(messages)
    }

    /// Emits a button edge at the latest accepted pointer position without
    /// moving the pointer.
    ///
    /// # Errors
    ///
    /// Returns [`RegionInputEmitError::MissingPointerPosition`] when no
    /// position has been accepted yet, or an ordered-state or wire validation
    /// error.
    pub fn pointer_button_at_latest(
        &mut self,
        regions: &AppliedRegionSet,
        button: u8,
        pressed: bool,
        timestamp_ns: u64,
    ) -> Result<Option<RegionInputWireMessage>, RegionInputEmitError> {
        let position = self
            .state
            .latest_pointer_position()
            .ok_or(RegionInputEmitError::MissingPointerPosition)?;
        if self.state.held_buttons().any(|held| held == button) == pressed {
            return Ok(None);
        }
        self.emit_button(regions, position, button, pressed, timestamp_ns)
            .map(Some)
    }

    /// Emits the transitions required to place the pointer at `position`, then
    /// one scroll sample when either fixed-point delta is nonzero.
    ///
    /// # Errors
    ///
    /// Returns an ordered-state or wire validation error.
    pub fn pointer_scroll(
        &mut self,
        regions: &AppliedRegionSet,
        position: RegionLogicalPosition,
        delta_x: i64,
        delta_y: i64,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, RegionInputEmitError> {
        let mut messages = self.ensure_pointer_position(regions, position, timestamp_ns)?;
        if delta_x != 0 || delta_y != 0 {
            let sequence = self.next_sequence();
            messages.push(self.emit(
                regions,
                RegionInputEvent::PointerScroll {
                    generation: regions.generation(),
                    position,
                    delta_x,
                    delta_y,
                    sequence,
                },
                timestamp_ns,
                false,
            )?);
        }
        Ok(messages)
    }

    /// Emits one pen sample using the next emitter sequence.
    ///
    /// # Errors
    ///
    /// Returns an ordered-state or wire validation error.
    pub fn pen(
        &mut self,
        regions: &AppliedRegionSet,
        sample: RegionPenSample,
        timestamp_ns: u64,
        coalescable: bool,
    ) -> Result<RegionInputWireMessage, RegionInputEmitError> {
        let sequence = self.next_sequence();
        self.pen_with_sequence(regions, sample, sequence, timestamp_ns, coalescable)
    }

    /// Emits one pen sample using a device-supplied sequence.
    ///
    /// The emitter's own counter is advanced to `sequence` so later pointer
    /// transitions stay strictly increasing.
    ///
    /// # Errors
    ///
    /// Returns an ordered-state or wire validation error.
    pub fn pen_with_sequence(
        &mut self,
        regions: &AppliedRegionSet,
        sample: RegionPenSample,
        sequence: u64,
        timestamp_ns: u64,
        coalescable: bool,
    ) -> Result<RegionInputWireMessage, RegionInputEmitError> {
        let message = self.emit(
            regions,
            RegionInputEvent::Pen {
                generation: regions.generation(),
                sample,
                sequence,
            },
            timestamp_ns,
            coalescable,
        )?;
        self.sequence = self.sequence.max(sequence);
        Ok(message)
    }

    /// Clears held focus, button, and pen state and returns the releases the
    /// client must mirror locally. The accepted sequence stays monotonic.
    #[must_use]
    pub fn release_all(&mut self) -> ReleasedRegionInput {
        self.state.release_all()
    }

    fn ensure_pointer_position(
        &mut self,
        regions: &AppliedRegionSet,
        position: RegionLogicalPosition,
        timestamp_ns: u64,
    ) -> Result<Vec<RegionInputWireMessage>, RegionInputEmitError> {
        let mut messages = Vec::with_capacity(2);
        if self.state.active_pointer_region() == Some(position.region_id) {
            if self.state.latest_pointer_position() != Some(position) {
                let sequence = self.next_sequence();
                messages.push(self.emit(
                    regions,
                    RegionInputEvent::PointerMotion {
                        generation: regions.generation(),
                        position,
                        sequence,
                    },
                    timestamp_ns,
                    true,
                )?);
            }
            return Ok(messages);
        }
        if self.state.is_focused() {
            let previous = self
                .state
                .latest_pointer_position()
                .ok_or(RegionInputEmitError::MissingPointerPosition)?;
            let sequence = self.next_sequence();
            messages.push(self.emit(
                regions,
                RegionInputEvent::PointerLeave {
                    generation: regions.generation(),
                    position: previous,
                    sequence,
                },
                timestamp_ns,
                false,
            )?);
        }
        let sequence = self.next_sequence();
        messages.push(self.emit(
            regions,
            RegionInputEvent::PointerEnter {
                generation: regions.generation(),
                position,
                sequence,
            },
            timestamp_ns,
            false,
        )?);
        Ok(messages)
    }

    fn emit_button(
        &mut self,
        regions: &AppliedRegionSet,
        position: RegionLogicalPosition,
        button: u8,
        pressed: bool,
        timestamp_ns: u64,
    ) -> Result<RegionInputWireMessage, RegionInputEmitError> {
        let sequence = self.next_sequence();
        self.emit(
            regions,
            RegionInputEvent::PointerButton {
                generation: regions.generation(),
                position,
                button,
                pressed,
                sequence,
            },
            timestamp_ns,
            false,
        )
    }

    fn emit(
        &mut self,
        regions: &AppliedRegionSet,
        event: RegionInputEvent,
        timestamp_ns: u64,
        coalescable: bool,
    ) -> Result<RegionInputWireMessage, RegionInputEmitError> {
        self.state.apply(regions, event)?;
        let message = RegionInputWireMessage::encode(event, timestamp_ns, coalescable);
        message.validate()?;
        Ok(message)
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }
}

/// Rejected client-side region input emission.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionInputEmitError {
    State(RegionInputStateError),
    Wire(RegionInputValidationError),
    Contract(RegionContractError),
    MissingPointerPosition,
}

impl Display for RegionInputEmitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::State(error) => Display::fmt(error, formatter),
            Self::Wire(error) => Display::fmt(error, formatter),
            Self::Contract(error) => Display::fmt(error, formatter),
            Self::MissingPointerPosition => {
                formatter.write_str("no region pointer position has been accepted yet")
            }
        }
    }
}

impl Error for RegionInputEmitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::State(error) => Some(error),
            Self::Wire(error) => Some(error),
            Self::Contract(error) => Some(error),
            Self::MissingPointerPosition => None,
        }
    }
}

impl From<RegionInputStateError> for RegionInputEmitError {
    fn from(value: RegionInputStateError) -> Self {
        Self::State(value)
    }
}

impl From<RegionInputValidationError> for RegionInputEmitError {
    fn from(value: RegionInputValidationError) -> Self {
        Self::Wire(value)
    }
}

impl From<RegionContractError> for RegionInputEmitError {
    fn from(value: RegionContractError) -> Self {
        Self::Contract(value)
    }
}
