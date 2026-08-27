use crate::AttachmentGeneration;
use std::fmt::{Display, Formatter};

/// Attachment lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentState {
    Offered,
    Accepted,
    ExporterReady,
    Enumerating,
    Active,
    Draining,
    Detached,
}

/// Pure attachment lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttachmentMachine {
    generation: AttachmentGeneration,
    state: AttachmentState,
}

impl AttachmentMachine {
    /// Creates a new offered attachment.
    #[must_use]
    pub const fn offered(generation: AttachmentGeneration) -> Self {
        Self {
            generation,
            state: AttachmentState::Offered,
        }
    }

    #[must_use]
    pub const fn generation(self) -> AttachmentGeneration {
        self.generation
    }

    #[must_use]
    pub const fn state(self) -> AttachmentState {
        self.state
    }

    /// Accepts a host-authorized offer.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] unless the attachment is offered.
    pub fn accept(&mut self) -> Result<(), StateError> {
        self.transition(AttachmentState::Offered, AttachmentState::Accepted)
    }

    /// Records that the exporter is ready.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] unless the offer is accepted.
    pub fn exporter_ready(&mut self) -> Result<(), StateError> {
        self.transition(AttachmentState::Accepted, AttachmentState::ExporterReady)
    }

    /// Starts host-side enumeration.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] unless the exporter is ready.
    pub fn begin_enumeration(&mut self) -> Result<(), StateError> {
        self.transition(AttachmentState::ExporterReady, AttachmentState::Enumerating)
    }

    /// Marks enumeration complete.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] unless enumeration is in progress.
    pub fn activate(&mut self) -> Result<(), StateError> {
        self.transition(AttachmentState::Enumerating, AttachmentState::Active)
    }

    /// Stops accepting new work and starts teardown.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] if the attachment is not in a live accepted
    /// state.
    pub fn begin_drain(&mut self) -> Result<(), StateError> {
        match self.state {
            AttachmentState::Accepted
            | AttachmentState::ExporterReady
            | AttachmentState::Enumerating
            | AttachmentState::Active => {
                self.state = AttachmentState::Draining;
                Ok(())
            }
            actual => Err(StateError::InvalidTransition {
                actual,
                expected: AttachmentState::Active,
                target: AttachmentState::Draining,
            }),
        }
    }

    /// Completes teardown.
    ///
    /// # Errors
    ///
    /// Returns [`StateError`] unless the attachment is draining.
    pub fn detach(&mut self) -> Result<(), StateError> {
        self.transition(AttachmentState::Draining, AttachmentState::Detached)
    }

    fn transition(
        &mut self,
        expected: AttachmentState,
        target: AttachmentState,
    ) -> Result<(), StateError> {
        if self.state != expected {
            return Err(StateError::InvalidTransition {
                actual: self.state,
                expected,
                target,
            });
        }
        self.state = target;
        Ok(())
    }
}

/// Invalid lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateError {
    InvalidTransition {
        actual: AttachmentState,
        expected: AttachmentState,
        target: AttachmentState,
    },
}

impl Display for StateError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTransition {
                actual,
                expected,
                target,
            } => write!(
                formatter,
                "cannot transition from {actual:?} to {target:?}; expected {expected:?}"
            ),
        }
    }
}

impl std::error::Error for StateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU64;

    #[test]
    fn normal_lifecycle_is_strict() {
        let mut machine = AttachmentMachine::offered(AttachmentGeneration::new(NonZeroU64::MIN));
        machine.accept().unwrap();
        machine.exporter_ready().unwrap();
        machine.begin_enumeration().unwrap();
        machine.activate().unwrap();
        machine.begin_drain().unwrap();
        machine.detach().unwrap();
        assert_eq!(machine.state(), AttachmentState::Detached);
    }

    #[test]
    fn activation_cannot_skip_enumeration() {
        let mut machine = AttachmentMachine::offered(AttachmentGeneration::new(NonZeroU64::MIN));
        machine.accept().unwrap();
        assert!(machine.activate().is_err());
    }
}
