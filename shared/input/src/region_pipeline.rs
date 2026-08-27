//! Generic, OS-free region input pipeline.
//!
//! [`RegionInputPipeline`] owns everything a host must do identically on every
//! platform: wire validation, checked wire-to-domain conversion, the ordered
//! [`RegionInputState`] transition, and the region coordinate transform. The
//! only platform-specific step is the final [`RegionPointMapper::map_applied`]
//! call that turns one applied pixel index into an OS-native injection point.
//!
//! State is advanced only after mapping succeeds, so a point that the platform
//! cannot represent never desynchronizes the shared stream.

use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_media::{AppliedPoint, AppliedRegionSet, RegionSet};
use arcen_protocol::messages::{
    RegionInputValidationError, RegionPenEventMsg, RegionPointerButtonMsg, RegionPointerEnterMsg,
    RegionPointerLeaveMsg, RegionPointerMotionMsg, RegionPointerScrollMsg,
};

use crate::region::{CoordinateTransformError, RegionCoordinateTransformer};
use crate::region_state::{
    RegionInputEvent, RegionInputState, RegionInputStateError, RegionLogicalPosition,
    RegionPenSample, ReleasedRegionInput,
};
use crate::region_wire::{RegionInputDecodeError, RegionInputWireRef};

/// Final OS-specific step of the shared region input pipeline.
///
/// Implementors receive an applied pixel index inside the negotiated region
/// aggregate and return the native point their injection API consumes. They
/// must not re-derive region membership, scaling, or rotation.
pub trait RegionPointMapper {
    /// Native injection point produced for one applied pixel index.
    type Point;
    /// Platform-specific mapping failure.
    type Error;

    /// Maps one applied pixel index to the OS-native injection point.
    ///
    /// # Errors
    ///
    /// Returns a platform error when the point lies outside the native
    /// injection surface or cannot be represented by its axes.
    fn map_applied(&self, point: AppliedPoint) -> Result<Self::Point, Self::Error>;
}

/// Checked region-scoped pointer button transition ready for injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRegionButton<P> {
    pub position: P,
    pub button: u8,
    pub pressed: bool,
}

/// Checked region-scoped scroll sample ready for injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappedRegionScroll<P> {
    pub position: P,
    pub delta_x: i64,
    pub delta_y: i64,
}

/// Checked region-scoped pen sample ready for injection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MappedRegionPen<P> {
    pub position: P,
    pub sample: RegionPenSample,
}

/// One accepted region input event mapped to native coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MappedRegionInput<P> {
    PointerEnter(P),
    PointerLeave(P),
    PointerMotion(P),
    PointerButton(MappedRegionButton<P>),
    PointerScroll(MappedRegionScroll<P>),
    Pen(MappedRegionPen<P>),
}

/// Stateful shared pipeline from region input DTOs to native points.
#[derive(Debug, Clone)]
pub struct RegionInputPipeline<M: RegionPointMapper> {
    applied_regions: AppliedRegionSet,
    state: RegionInputState,
    mapper: M,
}

impl<M: RegionPointMapper> RegionInputPipeline<M> {
    /// Builds a pipeline over one negotiated applied aggregate.
    #[must_use]
    pub fn new(applied_regions: AppliedRegionSet, mapper: M) -> Self {
        Self {
            applied_regions,
            state: RegionInputState::default(),
            mapper,
        }
    }

    /// Builds a pipeline after proving the requested and applied aggregates
    /// describe exactly the same regions.
    ///
    /// # Errors
    ///
    /// Returns [`RegionAggregateParityError`] when the two aggregates disagree
    /// on generation, region count, or any descriptor.
    pub fn with_aggregate_parity(
        requested: &RegionSet,
        applied: AppliedRegionSet,
        mapper: M,
    ) -> Result<Self, RegionAggregateParityError> {
        validate_aggregate_parity(requested, &applied)?;
        Ok(Self::new(applied, mapper))
    }

    #[must_use]
    pub const fn applied_regions(&self) -> &AppliedRegionSet {
        &self.applied_regions
    }

    #[must_use]
    pub const fn state(&self) -> &RegionInputState {
        &self.state
    }

    #[must_use]
    pub const fn mapper(&self) -> &M {
        &self.mapper
    }

    /// Clears shared focus, button, and pen state and returns the native
    /// releases the platform adapter must mirror.
    #[must_use]
    pub fn release_all(&mut self) -> ReleasedRegionInput {
        self.state.release_all()
    }

    /// Applies and maps a region pointer-enter transition.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn pointer_enter(
        &mut self,
        message: &RegionPointerEnterMsg,
    ) -> Result<M::Point, RegionInputPipelineError<M::Error>> {
        self.accept(RegionInputWireRef::PointerEnter(message))
            .map(|(point, _)| point)
    }

    /// Applies and maps a region pointer-leave transition.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn pointer_leave(
        &mut self,
        message: &RegionPointerLeaveMsg,
    ) -> Result<M::Point, RegionInputPipelineError<M::Error>> {
        self.accept(RegionInputWireRef::PointerLeave(message))
            .map(|(point, _)| point)
    }

    /// Applies and maps region-local absolute pointer motion.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn pointer_motion(
        &mut self,
        message: &RegionPointerMotionMsg,
    ) -> Result<M::Point, RegionInputPipelineError<M::Error>> {
        self.accept(RegionInputWireRef::PointerMotion(message))
            .map(|(point, _)| point)
    }

    /// Applies a button edge only when its logical position is exactly the
    /// latest accepted pointer position, then maps that position.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn pointer_button(
        &mut self,
        message: &RegionPointerButtonMsg,
    ) -> Result<MappedRegionButton<M::Point>, RegionInputPipelineError<M::Error>> {
        let (position, _) = self.accept(RegionInputWireRef::PointerButton(message))?;
        Ok(MappedRegionButton {
            position,
            button: message.button,
            pressed: message.pressed,
        })
    }

    /// Applies a scroll sample at the exact latest accepted pointer position.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn pointer_scroll(
        &mut self,
        message: &RegionPointerScrollMsg,
    ) -> Result<MappedRegionScroll<M::Point>, RegionInputPipelineError<M::Error>> {
        let (position, _) = self.accept(RegionInputWireRef::PointerScroll(message))?;
        Ok(MappedRegionScroll {
            position,
            delta_x: message.delta_x,
            delta_y: message.delta_y,
        })
    }

    /// Applies and maps a full region-scoped pen sample.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn pen(
        &mut self,
        message: &RegionPenEventMsg,
    ) -> Result<MappedRegionPen<M::Point>, RegionInputPipelineError<M::Error>> {
        let (position, event) = self.accept(RegionInputWireRef::Pen(message))?;
        let RegionInputEvent::Pen { sample, .. } = event else {
            unreachable!("a pen DTO decodes to a pen event")
        };
        Ok(MappedRegionPen { position, sample })
    }

    /// Applies and maps any `Region*` DTO through one dispatch point.
    ///
    /// # Errors
    ///
    /// Returns a wire, region-domain, ordered-state, transform, or platform
    /// mapping error.
    pub fn apply(
        &mut self,
        message: RegionInputWireRef<'_>,
    ) -> Result<MappedRegionInput<M::Point>, RegionInputPipelineError<M::Error>> {
        let (position, event) = self.accept(message)?;
        Ok(match event {
            RegionInputEvent::PointerEnter { .. } => MappedRegionInput::PointerEnter(position),
            RegionInputEvent::PointerLeave { .. } => MappedRegionInput::PointerLeave(position),
            RegionInputEvent::PointerMotion { .. } => MappedRegionInput::PointerMotion(position),
            RegionInputEvent::PointerButton {
                button, pressed, ..
            } => MappedRegionInput::PointerButton(MappedRegionButton {
                position,
                button,
                pressed,
            }),
            RegionInputEvent::PointerScroll {
                delta_x, delta_y, ..
            } => MappedRegionInput::PointerScroll(MappedRegionScroll {
                position,
                delta_x,
                delta_y,
            }),
            RegionInputEvent::Pen { sample, .. } => {
                MappedRegionInput::Pen(MappedRegionPen { position, sample })
            }
        })
    }

    /// Maps one already-validated logical position without touching state.
    ///
    /// # Errors
    ///
    /// Returns a transform or platform mapping error.
    pub fn map(
        &self,
        position: RegionLogicalPosition,
    ) -> Result<M::Point, RegionInputPipelineError<M::Error>> {
        let point = RegionCoordinateTransformer::new(&self.applied_regions)
            .logical_to_applied(position.region_id, position.point)?;
        self.mapper
            .map_applied(point)
            .map_err(RegionInputPipelineError::Mapping)
    }

    fn accept(
        &mut self,
        message: RegionInputWireRef<'_>,
    ) -> Result<(M::Point, RegionInputEvent), RegionInputPipelineError<M::Error>> {
        let event = message.decode()?;
        let mapped = self.map(event.position())?;
        self.state.apply(&self.applied_regions, event)?;
        Ok((mapped, event))
    }
}

/// Proves the requested and applied region aggregates describe exactly the
/// same regions at the same generation.
///
/// # Errors
///
/// Returns [`RegionAggregateParityError`] when the generation, region count,
/// or any descriptor differs.
pub fn validate_aggregate_parity(
    requested: &RegionSet,
    applied: &AppliedRegionSet,
) -> Result<(), RegionAggregateParityError> {
    if requested.generation() != applied.generation()
        || requested.regions().len() != applied.regions().len()
        || requested.regions().iter().any(|descriptor| {
            applied
                .get(descriptor.id())
                .is_none_or(|region| region.descriptor() != descriptor)
        })
    {
        return Err(RegionAggregateParityError);
    }
    Ok(())
}

/// The requested and applied region aggregates do not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionAggregateParityError;

impl Display for RegionAggregateParityError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("requested and applied shared region aggregates do not match")
    }
}

impl Error for RegionAggregateParityError {}

/// Rejected region input pipeline step.
///
/// Deliberately exhaustive: platform adapters map every variant onto their own
/// error surface, so a new failure mode must be a compile error for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionInputPipelineError<E> {
    Wire(RegionInputValidationError),
    Contract(arcen_media::RegionContractError),
    State(RegionInputStateError),
    Transform(CoordinateTransformError),
    Mapping(E),
}

impl<E> Display for RegionInputPipelineError<E>
where
    E: Display,
{
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wire(error) => Display::fmt(error, formatter),
            Self::Contract(error) => Display::fmt(error, formatter),
            Self::State(error) => Display::fmt(error, formatter),
            Self::Transform(error) => Display::fmt(error, formatter),
            Self::Mapping(error) => Display::fmt(error, formatter),
        }
    }
}

impl<E> Error for RegionInputPipelineError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Wire(error) => Some(error),
            Self::Contract(error) => Some(error),
            Self::State(error) => Some(error),
            Self::Transform(error) => Some(error),
            Self::Mapping(error) => Some(error),
        }
    }
}

impl<E> From<RegionInputDecodeError> for RegionInputPipelineError<E> {
    fn from(value: RegionInputDecodeError) -> Self {
        match value {
            RegionInputDecodeError::Wire(error) => Self::Wire(error),
            RegionInputDecodeError::Contract(error) => Self::Contract(error),
        }
    }
}

impl<E> From<RegionInputValidationError> for RegionInputPipelineError<E> {
    fn from(value: RegionInputValidationError) -> Self {
        Self::Wire(value)
    }
}

impl<E> From<arcen_media::RegionContractError> for RegionInputPipelineError<E> {
    fn from(value: arcen_media::RegionContractError) -> Self {
        Self::Contract(value)
    }
}

impl<E> From<RegionInputStateError> for RegionInputPipelineError<E> {
    fn from(value: RegionInputStateError) -> Self {
        Self::State(value)
    }
}

impl<E> From<CoordinateTransformError> for RegionInputPipelineError<E> {
    fn from(value: CoordinateTransformError) -> Self {
        Self::Transform(value)
    }
}
