// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! OS-free installer transaction lifecycle, and the decision of when a
//! service an installer started has run, failed, or is still starting.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::Duration;

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

/// One reading of the service manager's status for a service an installer
/// has just started.
///
/// `progress` is the service's own progress counter while it is starting or
/// stopping (the Windows checkpoint), and `wait_hint` the time it asked for
/// before it reports progress again.
///
/// `process_id` is the service's process as the manager reports it, or 0
/// when it reports none; a different process during one watch means the
/// service crashed and was started again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStatusReading {
    Starting {
        progress: u32,
        wait_hint: Duration,
        process_id: u32,
    },
    Running {
        process_id: u32,
    },
    Stopping {
        progress: u32,
        wait_hint: Duration,
    },
    Stopped {
        /// The manager's exit code for the service.
        exit_code: u32,
        /// The service's own code, when it reported one.
        service_exit_code: u32,
    },
    /// Any other state, by the manager's name for it.
    Other(&'static str),
    /// The status could not be read, and why.
    Unreadable(String),
}

/// Why a started service is not running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStartFailure {
    /// The manager refused the start request.
    Refused(String),
    /// The service stopped, during its start or soon after it reported
    /// running.
    Stopped {
        exit_code: u32,
        service_exit_code: u32,
    },
    /// The service stayed in `state` without progress for longer than it
    /// asked for.
    Stalled {
        state: &'static str,
        progress: u32,
        silent_for: Duration,
        wait_hint: Duration,
    },
    /// The service's process changed during the watch: it crashed after it
    /// started and something (such as a recovery action) started it again.
    Restarted { from_process: u32, to_process: u32 },
    /// The service kept reporting progress but did not finish within the
    /// installer's ceiling. It may still come up; it is not proven to run.
    StillStarting {
        state: &'static str,
        waited: Duration,
    },
    /// The service went to a state other than running.
    Settled(&'static str),
    /// The status could not be read, so the service is not known to run.
    Unreadable(String),
}

/// What an installer may say about a service whose start was not proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStartStanding {
    /// The start failed: refused, stopped, restarted, hung or another state.
    Failed,
    /// Still starting, and reporting progress, when the installer stopped
    /// waiting.
    StillStarting,
    /// The status could not be read.
    Unknown,
}

impl ServiceStartStanding {
    /// The transaction event that concludes an install whose service start
    /// ended here, after `ActivationCommitted`.
    ///
    /// Only a failed start fails the transaction. A service still starting
    /// or unreadable leaves it `Activated`: the files are in place and the
    /// service is not proven to run, which [`InstallTransaction::finish`]
    /// reports as incomplete rather than as a failed start.
    #[must_use]
    pub const fn concluding_event(self) -> Option<InstallEvent> {
        match self {
            Self::Failed => Some(InstallEvent::TransactionFailed),
            Self::StillStarting | Self::Unknown => None,
        }
    }
}

impl ServiceStartFailure {
    /// The manager's name for the state the service was left in.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Refused(_) | Self::Stopped { .. } => "stopped",
            Self::Restarted { .. } => "restarted",
            Self::Stalled { state, .. }
            | Self::StillStarting { state, .. }
            | Self::Settled(state) => state,
            Self::Unreadable(_) => "unknown",
        }
    }

    /// Whether the start failed, is still under way, or is unknown.
    #[must_use]
    pub const fn standing(&self) -> ServiceStartStanding {
        match self {
            Self::Refused(_)
            | Self::Stopped { .. }
            | Self::Stalled { .. }
            | Self::Restarted { .. }
            | Self::Settled(_) => ServiceStartStanding::Failed,
            Self::StillStarting { .. } => ServiceStartStanding::StillStarting,
            Self::Unreadable(_) => ServiceStartStanding::Unknown,
        }
    }
}

impl Display for ServiceStartFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(reason) => write!(formatter, "the start was refused: {reason}"),
            Self::Stopped {
                exit_code,
                service_exit_code,
            } => write!(
                formatter,
                "stopped (exit code {exit_code}, service exit code {service_exit_code})"
            ),
            Self::Stalled {
                state,
                progress,
                silent_for,
                wait_hint,
            } => write!(
                formatter,
                "{state} at checkpoint {progress} with no progress for {} s (it asked for {} s)",
                silent_for.as_secs(),
                wait_hint.as_secs()
            ),
            Self::Restarted {
                from_process,
                to_process,
            } => write!(
                formatter,
                "its process {from_process} ended and process {to_process} replaced it during the \
                 start"
            ),
            Self::StillStarting { state, waited } => write!(
                formatter,
                "still {state} after {} s and reporting progress",
                waited.as_secs()
            ),
            Self::Settled(state) => write!(formatter, "{state}"),
            Self::Unreadable(reason) => write!(formatter, "status unreadable: {reason}"),
        }
    }
}

impl Error for ServiceStartFailure {}

/// What an installer does after one reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStartVerdict {
    /// Read the status again after this long.
    Wait(Duration),
    /// The service runs and stayed running for the confirmation interval.
    Running,
    Failed(ServiceStartFailure),
}

/// The limits a [`ServiceStartWatch`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceStartPolicy {
    /// The longest an install waits, even for a service that reports
    /// progress.
    pub ceiling: Duration,
    /// How long the manager itself allows a service process to connect
    /// before it has reported any progress (the Windows SCM's 30 s).
    pub launch_grace: Duration,
    /// Added to every wait hint for polling and scheduling delay.
    pub slack: Duration,
    /// How long a running service must stay running before it counts, so a
    /// service that fails straight after reporting running is caught.
    pub confirm: Duration,
    /// Consecutive unreadable readings tolerated before the status is
    /// reported unreadable.
    pub unreadable_limit: u32,
    pub poll: Duration,
}

impl ServiceStartPolicy {
    pub const DEFAULT: Self = Self {
        ceiling: Duration::from_secs(120),
        launch_grace: Duration::from_secs(30),
        slack: Duration::from_secs(2),
        confirm: Duration::from_secs(2),
        unreadable_limit: 3,
        poll: Duration::from_millis(250),
    };
}

impl Default for ServiceStartPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Decides, reading by reading, whether a service an installer started is
/// running, has failed, or is still making progress.
///
/// A service is waited for while it reports progress within its own wait
/// hint, up to the policy's ceiling. Pending is never success, and an
/// unreadable status is never mistaken for a running service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStartWatch {
    policy: ServiceStartPolicy,
    last_progress: Option<(&'static str, u32)>,
    progress_at: Duration,
    running_since: Option<Duration>,
    unreadable: u32,
    process_id: Option<u32>,
}

impl ServiceStartWatch {
    #[must_use]
    pub const fn new(policy: ServiceStartPolicy) -> Self {
        Self {
            policy,
            last_progress: None,
            progress_at: Duration::ZERO,
            running_since: None,
            unreadable: 0,
            process_id: None,
        }
    }

    /// Judges `reading`, taken `elapsed` after the start was requested.
    pub fn observe(
        &mut self,
        elapsed: Duration,
        reading: ServiceStatusReading,
    ) -> ServiceStartVerdict {
        let policy = self.policy;
        if !matches!(reading, ServiceStatusReading::Unreadable(_)) {
            self.unreadable = 0;
        }
        if let ServiceStatusReading::Starting { process_id, .. }
        | ServiceStatusReading::Running { process_id } = reading
        {
            if let Some(failure) = self.same_process(process_id) {
                return ServiceStartVerdict::Failed(failure);
            }
        }
        let (state, progress, wait_hint) = match reading {
            ServiceStatusReading::Running { .. } => {
                let since = *self.running_since.get_or_insert(elapsed);
                let held = elapsed.saturating_sub(since);
                return if held >= policy.confirm {
                    ServiceStartVerdict::Running
                } else {
                    ServiceStartVerdict::Wait(policy.poll.min(policy.confirm.saturating_sub(held)))
                };
            }
            ServiceStatusReading::Stopped {
                exit_code,
                service_exit_code,
            } => {
                return ServiceStartVerdict::Failed(ServiceStartFailure::Stopped {
                    exit_code,
                    service_exit_code,
                });
            }
            ServiceStatusReading::Other(state) => {
                return ServiceStartVerdict::Failed(ServiceStartFailure::Settled(state));
            }
            ServiceStatusReading::Unreadable(reason) => {
                self.unreadable += 1;
                if self.unreadable >= policy.unreadable_limit || elapsed >= policy.ceiling {
                    return ServiceStartVerdict::Failed(ServiceStartFailure::Unreadable(reason));
                }
                return ServiceStartVerdict::Wait(policy.poll);
            }
            ServiceStatusReading::Starting {
                progress,
                wait_hint,
                ..
            } => ("start_pending", progress, wait_hint),
            // A service stopping during its start has failed it; it is
            // waited for only to read its exit codes once it has stopped.
            ServiceStatusReading::Stopping {
                progress,
                wait_hint,
            } => ("stop_pending", progress, wait_hint),
        };
        self.running_since = None;
        if self.last_progress != Some((state, progress)) {
            self.last_progress = Some((state, progress));
            self.progress_at = elapsed;
        }
        let silent_for = elapsed.saturating_sub(self.progress_at);
        let allowed = if progress == 0 {
            wait_hint.max(policy.launch_grace)
        } else {
            wait_hint
        } + policy.slack;
        if silent_for > allowed {
            ServiceStartVerdict::Failed(ServiceStartFailure::Stalled {
                state,
                progress,
                silent_for,
                wait_hint,
            })
        } else if elapsed < policy.ceiling {
            ServiceStartVerdict::Wait(policy.poll)
        } else if state == "start_pending" {
            ServiceStartVerdict::Failed(ServiceStartFailure::StillStarting {
                state,
                waited: elapsed,
            })
        } else {
            ServiceStartVerdict::Failed(ServiceStartFailure::Settled(state))
        }
    }

    /// The failure when `process_id` is not the process the watch saw
    /// before (0 is no process and proves nothing).
    fn same_process(&mut self, process_id: u32) -> Option<ServiceStartFailure> {
        if process_id == 0 {
            return None;
        }
        match self.process_id.replace(process_id) {
            Some(from_process) if from_process != process_id => {
                Some(ServiceStartFailure::Restarted {
                    from_process,
                    to_process: process_id,
                })
            }
            _ => None,
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

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    const PID: u32 = 4242;

    fn starting(progress: u32, hint: u64) -> ServiceStatusReading {
        ServiceStatusReading::Starting {
            progress,
            wait_hint: secs(hint),
            process_id: PID,
        }
    }

    fn running() -> ServiceStatusReading {
        ServiceStatusReading::Running { process_id: PID }
    }

    fn watch() -> ServiceStartWatch {
        ServiceStartWatch::new(ServiceStartPolicy::DEFAULT)
    }

    #[test]
    fn a_slow_start_that_reports_progress_is_waited_for() {
        // A Pier that spent 30 s in start-up reporting progress every 5 s
        // failed the install under the fixed 30 s poll.
        let mut watch = watch();
        for (second, progress) in (0..=45).step_by(5).zip(1..) {
            let verdict = watch.observe(secs(second), starting(progress, 10));
            assert!(
                matches!(verdict, ServiceStartVerdict::Wait(_)),
                "{verdict:?}"
            );
        }
        assert!(matches!(
            watch.observe(secs(46), running()),
            ServiceStartVerdict::Wait(_)
        ));
        assert_eq!(
            watch.observe(secs(48), running()),
            ServiceStartVerdict::Running
        );
    }

    #[test]
    fn a_start_without_progress_beyond_its_wait_hint_stalls() {
        let mut watch = watch();
        assert!(matches!(
            watch.observe(secs(1), starting(1, 10)),
            ServiceStartVerdict::Wait(_)
        ));
        assert!(matches!(
            watch.observe(secs(13), starting(1, 10)),
            ServiceStartVerdict::Wait(_)
        ));
        let verdict = watch.observe(secs(14), starting(1, 10));
        let ServiceStartVerdict::Failed(failure) = verdict else {
            panic!("a stalled start must fail: {verdict:?}");
        };
        assert_eq!(failure.state(), "start_pending");
        assert!(
            failure
                .to_string()
                .contains("start_pending at checkpoint 1"),
            "{failure}"
        );
    }

    #[test]
    fn a_service_that_has_not_reported_yet_gets_the_launch_grace() {
        // Before the process reports, the manager shows checkpoint 0 with a
        // short hint of its own; a cold executable can take longer.
        let mut watch = watch();
        assert!(matches!(
            watch.observe(Duration::ZERO, starting(0, 2)),
            ServiceStartVerdict::Wait(_)
        ));
        assert!(matches!(
            watch.observe(secs(20), starting(0, 2)),
            ServiceStartVerdict::Wait(_)
        ));
        assert!(matches!(
            watch.observe(secs(33), starting(0, 2)),
            ServiceStartVerdict::Failed(ServiceStartFailure::Stalled { .. })
        ));
    }

    #[test]
    fn progress_past_the_ceiling_is_still_starting_not_a_failed_start() {
        let mut watch = watch();
        let mut progress = 0;
        let mut second = 0;
        let verdict = loop {
            progress += 1;
            let verdict = watch.observe(secs(second), starting(progress, 10));
            if !matches!(verdict, ServiceStartVerdict::Wait(_)) {
                break verdict;
            }
            second += 5;
        };
        assert_eq!(
            verdict,
            ServiceStartVerdict::Failed(ServiceStartFailure::StillStarting {
                state: "start_pending",
                waited: secs(120),
            })
        );
        let ServiceStartVerdict::Failed(failure) = verdict else {
            unreachable!()
        };
        assert_eq!(failure.standing(), ServiceStartStanding::StillStarting);
        assert_eq!(failure.standing().concluding_event(), None);
    }

    #[test]
    fn a_service_stopping_during_its_start_never_counts_as_still_starting() {
        let mut watch = watch();
        let mut verdict = ServiceStartVerdict::Wait(Duration::ZERO);
        for (second, progress) in (0..=120).step_by(5).zip(1..) {
            verdict = watch.observe(
                secs(second),
                ServiceStatusReading::Stopping {
                    progress,
                    wait_hint: secs(10),
                },
            );
        }
        assert_eq!(
            verdict,
            ServiceStartVerdict::Failed(ServiceStartFailure::Settled("stop_pending"))
        );
    }

    #[test]
    fn a_crash_after_starting_is_caught_when_the_process_is_replaced() {
        // A recovery action can restart a crashed service before a reading
        // sees it stopped; the new process gives it away.
        let mut watch = watch();
        let _ = watch.observe(secs(1), starting(1, 10));
        assert!(matches!(
            watch.observe(secs(2), running()),
            ServiceStartVerdict::Wait(_)
        ));
        let verdict = watch.observe(
            secs(3),
            ServiceStatusReading::Starting {
                progress: 1,
                wait_hint: secs(10),
                process_id: PID + 8,
            },
        );
        let ServiceStartVerdict::Failed(failure) = verdict else {
            panic!("a replaced process must fail the start: {verdict:?}");
        };
        assert_eq!(
            failure,
            ServiceStartFailure::Restarted {
                from_process: PID,
                to_process: PID + 8,
            }
        );
        assert_eq!(failure.standing(), ServiceStartStanding::Failed);
        assert_eq!(
            failure.standing().concluding_event(),
            Some(InstallEvent::TransactionFailed)
        );
    }

    #[test]
    fn a_crash_after_reporting_running_fails_with_its_exit_code() {
        let mut watch = watch();
        let _ = watch.observe(secs(1), starting(1, 10));
        assert!(matches!(
            watch.observe(secs(2), running()),
            ServiceStartVerdict::Wait(_)
        ));
        assert_eq!(
            watch.observe(
                secs(3),
                ServiceStatusReading::Stopped {
                    exit_code: 1066,
                    service_exit_code: 6,
                }
            ),
            ServiceStartVerdict::Failed(ServiceStartFailure::Stopped {
                exit_code: 1066,
                service_exit_code: 6,
            })
        );
    }

    #[test]
    fn an_unreadable_status_is_never_success() {
        let mut watch = watch();
        let unreadable = || ServiceStatusReading::Unreadable("access denied".into());
        assert!(matches!(
            watch.observe(secs(1), unreadable()),
            ServiceStartVerdict::Wait(_)
        ));
        // A good reading between failures restarts the count.
        let _ = watch.observe(secs(2), starting(1, 10));
        let _ = watch.observe(secs(3), unreadable());
        let _ = watch.observe(secs(4), unreadable());
        let verdict = watch.observe(secs(5), unreadable());
        assert_eq!(
            verdict,
            ServiceStartVerdict::Failed(ServiceStartFailure::Unreadable("access denied".into()))
        );
        let ServiceStartVerdict::Failed(failure) = verdict else {
            unreachable!()
        };
        assert_eq!(failure.state(), "unknown");
        assert_eq!(failure.standing(), ServiceStartStanding::Unknown);
        assert_eq!(failure.standing().concluding_event(), None);
    }

    #[test]
    fn an_immediately_running_service_is_confirmed_then_reported() {
        let mut watch = watch();
        assert!(matches!(
            watch.observe(Duration::ZERO, running()),
            ServiceStartVerdict::Wait(_)
        ));
        assert_eq!(
            watch.observe(secs(2), running()),
            ServiceStartVerdict::Running
        );
        assert_eq!(
            ServiceStartWatch::new(ServiceStartPolicy::DEFAULT)
                .observe(Duration::ZERO, ServiceStatusReading::Other("paused")),
            ServiceStartVerdict::Failed(ServiceStartFailure::Settled("paused"))
        );
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
