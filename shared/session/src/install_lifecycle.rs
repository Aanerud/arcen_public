//! OS-free installer transaction lifecycle.

use std::error::Error;
use std::fmt::{Display, Formatter};

/// Transaction phase shared by Linux, Windows, and macOS installers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    New,
    Preflighted,
    Staged,
    Quiesced,
    Activated,
    SmokeValidated,
    /// Files are in place and the operator asked for no service (a staging
    /// prefix, `--no-service`, or a dry run): nothing was started, so
    /// nothing is claimed to run.
    InstalledWithoutService,
    RolledBack,
    Uninstalled,
    Failed,
}

/// Installer operation reported by a native package adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallEvent {
    PreflightPassed,
    PayloadStaged,
    ServiceQuiesced,
    ActivationCommitted,
    SmokePassed,
    /// The operator asked for the payload without a running service.
    ServiceNotRequested,
    RollbackCompleted,
    UninstallCompleted,
    TransactionFailed,
}

/// An installer event invalid for the current phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidInstallTransition {
    pub phase: InstallPhase,
    pub event: InstallEvent,
}

impl Display for InvalidInstallTransition {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "installer event {:?} is invalid in phase {:?}",
            self.event, self.phase
        )
    }
}

impl Error for InvalidInstallTransition {}

/// Pure installer transaction coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallTransaction {
    phase: InstallPhase,
}

impl Default for InstallTransaction {
    fn default() -> Self {
        Self::new()
    }
}

impl InstallTransaction {
    /// Creates a new, unmodified transaction.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: InstallPhase::New,
        }
    }

    /// Returns the current transaction phase.
    #[must_use]
    pub const fn phase(self) -> InstallPhase {
        self.phase
    }

    /// Applies one native installer operation.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidInstallTransition`] if the native adapter reports an
    /// operation out of order.
    pub fn apply(&mut self, event: InstallEvent) -> Result<InstallPhase, InvalidInstallTransition> {
        let next = match (self.phase, event) {
            (InstallPhase::New, InstallEvent::PreflightPassed) => InstallPhase::Preflighted,
            (InstallPhase::Preflighted, InstallEvent::PayloadStaged) => InstallPhase::Staged,
            (InstallPhase::Staged, InstallEvent::ServiceQuiesced) => InstallPhase::Quiesced,
            (InstallPhase::Quiesced, InstallEvent::ActivationCommitted) => InstallPhase::Activated,
            (InstallPhase::Activated, InstallEvent::SmokePassed) => InstallPhase::SmokeValidated,
            (InstallPhase::Staged, InstallEvent::ServiceNotRequested) => {
                InstallPhase::InstalledWithoutService
            }
            (
                InstallPhase::Staged
                | InstallPhase::Quiesced
                | InstallPhase::Activated
                | InstallPhase::SmokeValidated
                | InstallPhase::Failed,
                InstallEvent::RollbackCompleted,
            ) => InstallPhase::RolledBack,
            (
                InstallPhase::SmokeValidated | InstallPhase::RolledBack,
                InstallEvent::UninstallCompleted,
            ) => InstallPhase::Uninstalled,
            (
                InstallPhase::New
                | InstallPhase::Preflighted
                | InstallPhase::Staged
                | InstallPhase::Quiesced
                | InstallPhase::Activated
                | InstallPhase::SmokeValidated,
                InstallEvent::TransactionFailed,
            ) => InstallPhase::Failed,
            _ => {
                return Err(InvalidInstallTransition {
                    phase: self.phase,
                    event,
                });
            }
        };
        self.phase = next;
        Ok(next)
    }
}

/// An install that ended without reaching a successful terminal phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncompleteInstall {
    pub phase: InstallPhase,
}

impl Display for IncompleteInstall {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the install did not complete: it stopped at {:?}, before the service was \
             proven to run",
            self.phase
        )
    }
}

impl Error for IncompleteInstall {}

impl InstallTransaction {
    /// Whether the install succeeded, which only a smoke-validated service
    /// or an explicitly service-less install is.
    ///
    /// This is what an installer's exit status comes from, so a service that
    /// was started but never proven to run cannot be reported as installed.
    ///
    /// # Errors
    ///
    /// [`IncompleteInstall`] for every other phase.
    pub const fn finish(self) -> Result<InstallPhase, IncompleteInstall> {
        match self.phase {
            InstallPhase::SmokeValidated | InstallPhase::InstalledWithoutService => Ok(self.phase),
            phase => Err(IncompleteInstall { phase }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_proven_service_or_an_explicit_no_service_install_finishes() {
        let mut started_not_proven = InstallTransaction::new();
        for event in [
            InstallEvent::PreflightPassed,
            InstallEvent::PayloadStaged,
            InstallEvent::ServiceQuiesced,
            InstallEvent::ActivationCommitted,
        ] {
            started_not_proven.apply(event).expect("valid");
        }
        assert_eq!(
            started_not_proven.finish(),
            Err(IncompleteInstall {
                phase: InstallPhase::Activated
            })
        );
        started_not_proven
            .apply(InstallEvent::SmokePassed)
            .expect("smoke");
        assert_eq!(
            started_not_proven.finish(),
            Ok(InstallPhase::SmokeValidated)
        );

        let mut staging = InstallTransaction::new();
        staging
            .apply(InstallEvent::PreflightPassed)
            .expect("preflight");
        staging.apply(InstallEvent::PayloadStaged).expect("stage");
        staging
            .apply(InstallEvent::ServiceNotRequested)
            .expect("no service");
        assert_eq!(staging.finish(), Ok(InstallPhase::InstalledWithoutService));

        let mut failed = InstallTransaction::new();
        failed.apply(InstallEvent::TransactionFailed).expect("fail");
        assert!(failed.finish().is_err());
        assert!(
            InstallTransaction::new()
                .apply(InstallEvent::ServiceNotRequested)
                .is_err(),
            "nothing is installed before staging"
        );
    }

    #[test]
    fn install_requires_smoke_validation_before_completion() {
        let mut transaction = InstallTransaction::new();
        for event in [
            InstallEvent::PreflightPassed,
            InstallEvent::PayloadStaged,
            InstallEvent::ServiceQuiesced,
            InstallEvent::ActivationCommitted,
            InstallEvent::SmokePassed,
        ] {
            transaction.apply(event).expect("valid install event");
        }
        assert_eq!(transaction.phase(), InstallPhase::SmokeValidated);
    }

    #[test]
    fn failed_install_can_roll_back_and_uninstall() {
        let mut transaction = InstallTransaction::new();
        transaction
            .apply(InstallEvent::PreflightPassed)
            .expect("preflight");
        transaction
            .apply(InstallEvent::PayloadStaged)
            .expect("stage");
        transaction
            .apply(InstallEvent::TransactionFailed)
            .expect("failure");
        assert_eq!(transaction.phase(), InstallPhase::Failed);
        transaction
            .apply(InstallEvent::RollbackCompleted)
            .expect("rollback after failure");
        assert_eq!(transaction.phase(), InstallPhase::RolledBack);
    }

    #[test]
    fn rollback_is_allowed_before_smoke_proof() {
        let mut transaction = InstallTransaction {
            phase: InstallPhase::Activated,
        };
        transaction
            .apply(InstallEvent::RollbackCompleted)
            .expect("rollback");
        transaction
            .apply(InstallEvent::UninstallCompleted)
            .expect("uninstall");
        assert_eq!(transaction.phase(), InstallPhase::Uninstalled);
    }

    #[test]
    fn invalid_operation_does_not_change_phase() {
        let mut transaction = InstallTransaction::new();
        let error = transaction
            .apply(InstallEvent::SmokePassed)
            .expect_err("smoke cannot precede preflight");
        assert_eq!(
            error,
            InvalidInstallTransition {
                phase: InstallPhase::New,
                event: InstallEvent::SmokePassed,
            }
        );
        assert_eq!(transaction.phase(), InstallPhase::New);
    }
}
