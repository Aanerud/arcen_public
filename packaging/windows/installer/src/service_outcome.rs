//! What starting the service achieved, carried through the shared installer
//! transaction so the exit status reports a running service only when the
//! service control manager says one runs.

use arcen_session::install_lifecycle::{InstallEvent, InstallTransaction};

/// The service control manager's settled answer after a start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// Dry run or staging prefix: nothing was started.
    NotRequested,
    /// The service settled in `RUNNING`.
    Running,
    /// The service settled in any other state.
    NotRunning(String),
}

/// Applies `outcome` to a staged transaction and returns whether the install
/// succeeded, which only [`InstallTransaction::finish`] decides.
///
/// # Errors
///
/// A message naming the service's state when it is not running, or the
/// transaction error when the events arrive out of order.
pub fn finish_install(
    mut transaction: InstallTransaction,
    outcome: &ServiceOutcome,
    service_name: &str,
) -> Result<(), String> {
    let events: &[InstallEvent] = match outcome {
        ServiceOutcome::NotRequested => &[InstallEvent::ServiceNotRequested],
        ServiceOutcome::Running => &[
            InstallEvent::ServiceQuiesced,
            InstallEvent::ActivationCommitted,
            InstallEvent::SmokePassed,
        ],
        ServiceOutcome::NotRunning(_) => &[
            InstallEvent::ServiceQuiesced,
            InstallEvent::ActivationCommitted,
            InstallEvent::TransactionFailed,
        ],
    };
    for event in events {
        transaction
            .apply(*event)
            .map_err(|error| format!("installer transaction: {error}"))?;
    }
    transaction
        .finish()
        .map(|_| ())
        .map_err(|error| match outcome {
            ServiceOutcome::NotRunning(state) => format!(
                "{error}: service {service_name} is {state:?}, not running. The files are \
                 installed; see the Arcen Pier log under ProgramData\\Arcen\\logs for why it \
                 stopped"
            ),
            _ => error.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged() -> InstallTransaction {
        let mut transaction = InstallTransaction::new();
        transaction
            .apply(InstallEvent::PreflightPassed)
            .expect("preflight");
        transaction
            .apply(InstallEvent::PayloadStaged)
            .expect("staged");
        transaction
    }

    #[test]
    fn the_install_succeeds_only_when_the_service_is_proven_running() {
        assert_eq!(
            finish_install(staged(), &ServiceOutcome::Running, "ArcenPier"),
            Ok(())
        );
        assert_eq!(
            finish_install(staged(), &ServiceOutcome::NotRequested, "ArcenPier"),
            Ok(())
        );
        let error = finish_install(
            staged(),
            &ServiceOutcome::NotRunning("stopped".into()),
            "ArcenPier",
        )
        .expect_err("a stopped service fails the install");
        assert!(error.contains("ArcenPier"), "{error}");
        assert!(error.contains("\"stopped\""), "{error}");
    }
}
