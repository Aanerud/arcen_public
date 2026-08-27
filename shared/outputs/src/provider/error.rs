//! Typed lifecycle errors.
//!
//! Section 4 of ADR 0010. Two rules are structural, not stylistic:
//!
//! - A rollback failure is never flattened into a formatted string. Both the
//!   primary failure and the rollback failure survive as separate typed
//!   values; [`Display`](core::fmt::Display) may render both, but the variant
//!   keeps both.
//! - [`OutputStage::Admission`] and [`OutputStage::Preflight`] can never
//!   carry a rollback, because nothing is bound yet. The driver enforces this
//!   by construction.

use core::fmt;

use super::CapabilityMismatch;

/// Lifecycle stage that failed.
///
/// `#[non_exhaustive]` so new stages are additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum OutputStage {
    /// The shared capability gate, before any provider code runs.
    Admission,
    /// Synchronous preflight. Makes no operating-system-visible mutation.
    Preflight,
    /// Creating the binding that owns every operating-system-visible
    /// resource.
    Bind,
    /// Confirming the applied state matches the plan.
    Verify,
    /// Handing the verified binding to the session.
    Commit,
}

impl fmt::Display for OutputStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Admission => "admission",
            Self::Preflight => "preflight",
            Self::Bind => "bind",
            Self::Verify => "verify",
            Self::Commit => "commit",
        })
    }
}

/// A failed lifecycle transition, with the rollback outcome preserved.
///
/// Exhaustive on purpose: these three outcomes are the frozen shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputTransactionError<E> {
    /// The shared capability gate refused the plan. Nothing was bound, so no
    /// rollback exists.
    Admission(CapabilityMismatch),
    /// A stage failed and no rollback was needed, or rollback succeeded.
    Operation { stage: OutputStage, source: E },
    /// A stage failed and the rollback that followed also failed. Both
    /// failures survive.
    Rollback {
        stage: OutputStage,
        source: E,
        rollback: E,
    },
}

impl<E> OutputTransactionError<E> {
    /// The stage that failed.
    #[must_use]
    pub const fn stage(&self) -> OutputStage {
        match self {
            Self::Admission(_) => OutputStage::Admission,
            Self::Operation { stage, .. } | Self::Rollback { stage, .. } => *stage,
        }
    }

    /// The primary failure, when this is not an admission refusal.
    ///
    /// Named `failure` rather than `source` so it never shadows
    /// [`std::error::Error::source`] at a call site.
    #[must_use]
    pub const fn failure(&self) -> Option<&E> {
        match self {
            Self::Admission(_) => None,
            Self::Operation { source, .. } | Self::Rollback { source, .. } => Some(source),
        }
    }

    /// The rollback failure, when rollback itself failed.
    #[must_use]
    pub const fn rollback(&self) -> Option<&E> {
        match self {
            Self::Admission(_) | Self::Operation { .. } => None,
            Self::Rollback { rollback, .. } => Some(rollback),
        }
    }

    /// Whether the host still holds an unresolved teardown obligation
    /// because rollback failed.
    #[must_use]
    pub const fn rollback_failed(&self) -> bool {
        matches!(self, Self::Rollback { .. })
    }
}

impl<E: fmt::Display> fmt::Display for OutputTransactionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Admission(mismatch) => write!(formatter, "output admission refused: {mismatch}"),
            Self::Operation { stage, source } => {
                write!(formatter, "output {stage} failed: {source}")
            }
            Self::Rollback {
                stage,
                source,
                rollback,
            } => write!(
                formatter,
                "output {stage} failed: {source}; the rollback that followed also failed: \
                 {rollback}"
            ),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for OutputTransactionError<E> {}

/// A failed [`bind`](super::OutputProvider::bind), plus the outcome of the
/// provider's own internal undo.
///
/// `bind` produces the binding, so on failure there is no binding for the
/// driver to roll back. A provider that already mutated something must undo
/// it before returning `Err`, and must report the outcome of that undo here.
/// The driver maps `Some(rollback)` to
/// [`OutputTransactionError::Rollback`] at [`OutputStage::Bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindFailure<E> {
    /// Why binding failed.
    pub source: E,
    /// The failure of the provider's own undo, when that undo also failed.
    /// `None` means "nothing needed undoing, or the undo succeeded".
    pub rollback: Option<E>,
}

impl<E> BindFailure<E> {
    /// A bind failure that left nothing behind.
    #[must_use]
    pub const fn new(source: E) -> Self {
        Self {
            source,
            rollback: None,
        }
    }

    /// A bind failure whose own undo also failed.
    #[must_use]
    pub const fn with_rollback(source: E, rollback: E) -> Self {
        Self {
            source,
            rollback: Some(rollback),
        }
    }
}

impl<E> From<E> for BindFailure<E> {
    fn from(source: E) -> Self {
        Self::new(source)
    }
}

impl<E: fmt::Display> fmt::Display for BindFailure<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.rollback {
            None => write!(formatter, "output bind failed: {}", self.source),
            Some(rollback) => write!(
                formatter,
                "output bind failed: {}; the provider's own undo also failed: {rollback}",
                self.source
            ),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for BindFailure<E> {}

#[cfg(test)]
mod tests {
    use super::{BindFailure, OutputStage, OutputTransactionError};
    use crate::provider::CapabilityMismatch;

    #[test]
    fn stage_attribution_covers_every_variant() {
        assert_eq!(
            OutputTransactionError::<&str>::Admission(CapabilityMismatch::HeadlessUnsupported)
                .stage(),
            OutputStage::Admission
        );
        for stage in [
            OutputStage::Preflight,
            OutputStage::Bind,
            OutputStage::Verify,
            OutputStage::Commit,
        ] {
            assert_eq!(
                OutputTransactionError::Operation {
                    stage,
                    source: "boom"
                }
                .stage(),
                stage
            );
            assert_eq!(
                OutputTransactionError::Rollback {
                    stage,
                    source: "boom",
                    rollback: "undo",
                }
                .stage(),
                stage
            );
        }
    }

    #[test]
    fn a_rollback_failure_keeps_both_failures_separate() {
        let error = OutputTransactionError::Rollback {
            stage: OutputStage::Verify,
            source: "verify refused",
            rollback: "restore refused",
        };
        assert_eq!(error.failure(), Some(&"verify refused"));
        assert_eq!(error.rollback(), Some(&"restore refused"));
        assert!(error.rollback_failed());
        let rendered = error.to_string();
        assert!(rendered.contains("verify refused"), "{rendered}");
        assert!(rendered.contains("restore refused"), "{rendered}");
    }

    #[test]
    fn an_operation_failure_has_no_rollback_failure() {
        let error = OutputTransactionError::Operation {
            stage: OutputStage::Preflight,
            source: "no head available",
        };
        assert_eq!(error.failure(), Some(&"no head available"));
        assert_eq!(error.rollback(), None);
        assert!(!error.rollback_failed());
    }

    #[test]
    fn an_admission_refusal_carries_no_source_and_no_rollback() {
        let error = OutputTransactionError::<&str>::Admission(CapabilityMismatch::RegionCount {
            requested: 5,
            min: 1,
            max: 4,
        });
        assert_eq!(error.failure(), None);
        assert_eq!(error.rollback(), None);
        assert!(!error.rollback_failed());
        assert!(error.to_string().contains('5'), "{error}");
    }

    #[test]
    fn bind_failure_reports_its_own_undo() {
        assert_eq!(BindFailure::new("boom").rollback, None);
        assert_eq!(
            BindFailure::with_rollback("boom", "undo").rollback,
            Some("undo")
        );
        assert_eq!(BindFailure::from("boom"), BindFailure::new("boom"));
        let rendered = BindFailure::with_rollback("boom", "undo").to_string();
        assert!(rendered.contains("boom"), "{rendered}");
        assert!(rendered.contains("undo"), "{rendered}");
    }
}
