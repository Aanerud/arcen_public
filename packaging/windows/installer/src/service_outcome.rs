// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! What starting the service achieved, carried through the shared installer
//! transaction so the exit status reports a running service only when the
//! service control manager says one runs.

use arcen_session::install_lifecycle::{
    InstallEvent, InstallTransaction, ServiceStartFailure, ServiceStartStanding,
};

/// The service control manager's settled answer after a start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// Dry run or staging prefix: nothing was started.
    NotRequested,
    /// The service settled in `RUNNING`.
    Running,
    /// The service is not proven to run: its start failed, it is still
    /// starting, or its state could not be read
    /// ([`ServiceStartFailure::standing`]).
    NotRunning(ServiceStartFailure),
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
    let activated = [
        InstallEvent::ServiceQuiesced,
        InstallEvent::ActivationCommitted,
    ];
    let events: Vec<InstallEvent> = match outcome {
        ServiceOutcome::NotRequested => vec![InstallEvent::ServiceNotRequested],
        ServiceOutcome::Running => activated
            .into_iter()
            .chain([InstallEvent::SmokePassed])
            .collect(),
        // A failed start fails the transaction; a start still under way or
        // unreadable leaves it activated and unproven.
        ServiceOutcome::NotRunning(failure) => activated
            .into_iter()
            .chain(failure.standing().concluding_event())
            .collect(),
    };
    for event in events {
        transaction
            .apply(event)
            .map_err(|error| format!("installer transaction: {error}"))?;
    }
    transaction
        .finish()
        .map(|_| ())
        .map_err(|error| match outcome {
            ServiceOutcome::NotRunning(failure) => match failure.standing() {
                ServiceStartStanding::Failed => format!(
                    "{error}: service {service_name} failed to start ({failure}). The files \
                     are installed; see the Arcen Pier log under ProgramData\\Arcen\\logs \
                     for why"
                ),
                ServiceStartStanding::StillStarting => format!(
                    "{error}: service {service_name} is {failure}, so it is not yet proven to \
                     run. The files are installed; check it with Get-Service {service_name}"
                ),
                ServiceStartStanding::Unknown => format!(
                    "{error}: the state of service {service_name} is not known ({failure}). \
                     The files are installed; check it with Get-Service {service_name}"
                ),
            },
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
            &ServiceOutcome::NotRunning(ServiceStartFailure::Stopped {
                exit_code: 1066,
                service_exit_code: 6,
            }),
            "ArcenPier",
        )
        .expect_err("a stopped service fails the install");
        assert!(error.contains("at Failed"), "{error}");
        assert!(
            error.contains("ArcenPier failed to start (stopped (exit code 1066"),
            "{error}"
        );
    }

    #[test]
    fn a_start_still_under_way_or_unreadable_is_unproven_not_failed() {
        let error = finish_install(
            staged(),
            &ServiceOutcome::NotRunning(ServiceStartFailure::StillStarting {
                state: "start_pending",
                waited: std::time::Duration::from_secs(120),
            }),
            "ArcenPier",
        )
        .expect_err("a service still starting is not proven to run");
        assert!(error.contains("at Activated"), "{error}");
        assert!(
            error.contains("ArcenPier is still start_pending after 120 s"),
            "{error}"
        );
        assert!(!error.contains("failed to start"), "{error}");
        let error = finish_install(
            staged(),
            &ServiceOutcome::NotRunning(ServiceStartFailure::Unreadable("access denied".into())),
            "ArcenPier",
        )
        .expect_err("an unreadable status is not a running service");
        assert!(error.contains("at Activated"), "{error}");
        assert!(
            error.contains("not known (status unreadable: access denied)"),
            "{error}"
        );
    }
}
