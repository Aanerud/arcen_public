// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Automatic re-qualification at Pier start.
//!
//! A routine update of the compositor, `PipeWire` or GPU driver (or of the
//! Pier itself) makes the host's record stale, and a stale record refuses
//! Wayland. Rather than leave an unattended host down until an operator
//! re-qualifies it, the Pier service re-runs the qualification itself at
//! start — but only when the record is refused for a version reason
//! (stale, or another schema), never when it is missing (the installer
//! qualifies a new host) or cannot be trusted (not root-only, malformed),
//! and never when a live fact the record does not supply is missing (no
//! compositor, no `PipeWire`, no render node). A failed attempt for the same
//! stack is not repeated for [`REQUALIFY_RETRY_AFTER_S`], so systemd's
//! restart policy cannot turn it into a loop.
//!
//! An attempt that ends without an outcome is not a failure of the stack:
//! the marker is written [`RequalifyAttempt::started`] before the run, so the
//! next start sees that a crash, a kill or a power loss interrupted it and
//! tries again at once. Only [`AUTO_REQUALIFY_MAX_INTERRUPTED`] such
//! attempts in a row on one stack hold the next one off, so a run that
//! kills the Pier every time still cannot loop. An orderly stop (SIGTERM)
//! during the run leaves no marker at all.
//!
//! A second trigger runs the same automatic re-qualification for a record
//! that is valid: one whose X11 proof was skipped because a host
//! precondition (an unsafe `/tmp/.X11-unix`) failed at qualification, when
//! start-up finds that precondition holding again
//! ([`RequalifyTrigger::PreconditionCleared`],
//! [`precondition_requalify_decision`]). Wayland is served either way, so
//! this attempt never refuses start; it is tried once per boot and stack
//! (a finished attempt, passed or not, holds off another until the host
//! reboots, a failed one also for [`REQUALIFY_RETRY_AFTER_S`]), with the
//! same bounded retries after an interruption. Its marker is a separate
//! file, so the two triggers never hold each other off.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{HostIdentity, QualificationError, WaylandQualificationRecord};
use crate::desktop_provider::{ProviderRefusal, WaylandBlocker};
use crate::wayland_x11::{X11AppsDisabledReason, XwaylandProof};

/// Why an automatic re-qualification runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequalifyTrigger {
    /// An update made the record stale ([`auto_requalify_decision`]).
    #[default]
    RecordVersion,
    /// The host precondition the record's X11 proof was skipped for holds
    /// again ([`precondition_requalify_decision`]).
    PreconditionCleared,
    /// A trigger a newer build wrote: read, never acted on, so a record or
    /// marker carrying it stays readable.
    #[serde(other)]
    Unknown,
}

impl RequalifyTrigger {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::RecordVersion => "record_version",
            Self::PreconditionCleared => "precondition_cleared",
            Self::Unknown => "unknown",
        }
    }

    // serde's `skip_serializing_if` passes a reference.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    const fn is_record_version(&self) -> bool {
        matches!(self, Self::RecordVersion)
    }
}

/// What an automatic re-qualification for
/// [`RequalifyTrigger::PreconditionCleared`] found about X11 applications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum X11RequalifyOutcome {
    /// Xwayland proven: sessions serve X11 applications.
    X11AppsProven,
    /// Xwayland tried and not proven: the record says why
    /// (`qualification_failed` at start).
    X11AppsFailed,
    /// Xwayland not tried (the setting or the installed Xwayland changed).
    X11AppsNotTried,
    /// The precondition failed again during the run: the record keeps it.
    PreconditionReturned,
    /// The run did not pass: no record was written, the previous one
    /// stands.
    NotRecorded,
    /// An outcome a newer build wrote: read, never produced here.
    #[serde(other)]
    Unknown,
}

impl X11RequalifyOutcome {
    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::X11AppsProven => "x11_apps_proven",
            Self::X11AppsFailed => "x11_apps_failed",
            Self::X11AppsNotTried => "x11_apps_not_tried",
            Self::PreconditionReturned => "precondition_returned",
            Self::NotRecorded => "not_recorded",
            Self::Unknown => "unknown",
        }
    }
}

/// The X11 outcome of a run that `passed` (wrote a record) with `proof`.
#[must_use]
pub fn x11_requalify_outcome(passed: bool, proof: Option<&XwaylandProof>) -> X11RequalifyOutcome {
    match proof {
        _ if !passed => X11RequalifyOutcome::NotRecorded,
        None => X11RequalifyOutcome::X11AppsNotTried,
        Some(proof) if proof.host_precondition.is_some() => {
            X11RequalifyOutcome::PreconditionReturned
        }
        Some(proof) if proof.proven() => X11RequalifyOutcome::X11AppsProven,
        Some(_) => X11RequalifyOutcome::X11AppsFailed,
    }
}

/// Written into a record an automatic re-qualification produced
/// (`QualifiedFacts::automatic_requalification`): why it ran and, for the
/// X11 trigger, what it found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomaticRequalification {
    /// Why it ran.
    pub trigger: RequalifyTrigger,
    /// The host precondition the previous record skipped X11 for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared_precondition: Option<String>,
    /// What it found about X11 applications (X11 trigger only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x11_outcome: Option<X11RequalifyOutcome>,
    /// When it ran, seconds since the Unix epoch. For the X11 trigger the
    /// record's `qualified_at_unix_s` stays that of the facts it kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_unix_s: Option<u64>,
}

/// What a [`RequalifyTrigger::PreconditionCleared`] re-proof does to the
/// record it ran for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum X11Reproof {
    /// Write this record: the previous one with only `x11_apps` and
    /// `automatic_requalification` replaced; every other fact is kept.
    Write(Box<WaylandQualificationRecord>),
    /// Leave the record as it is: nothing new was proven or refuted about
    /// X11 (the precondition failed again), or the record no longer skips
    /// X11 for a precondition (requalified meanwhile).
    Unchanged,
}

/// Applies an X11 re-proof (`proof`, from a run that passed at `at_unix_s`)
/// to `previous`, the record it ran for. Only a proven or a failed proof
/// replaces the record's skipped one; the run's other measurements never
/// replace the record's (they are bounded, and the record is often an
/// operator's fuller qualification).
#[must_use]
pub fn x11_reproof_record(
    previous: &WaylandQualificationRecord,
    proof: Option<&XwaylandProof>,
    at_unix_s: u64,
) -> (X11RequalifyOutcome, X11Reproof) {
    let outcome = x11_requalify_outcome(true, proof);
    let Some(cleared) = previous
        .facts
        .x11_apps
        .as_ref()
        .and_then(|skipped| skipped.host_precondition.clone())
    else {
        return (outcome, X11Reproof::Unchanged);
    };
    let (Some(proof), X11RequalifyOutcome::X11AppsProven | X11RequalifyOutcome::X11AppsFailed) =
        (proof, outcome)
    else {
        return (outcome, X11Reproof::Unchanged);
    };
    let mut record = previous.clone();
    record.facts.x11_apps = Some(proof.clone());
    record.facts.automatic_requalification = Some(AutomaticRequalification {
        trigger: RequalifyTrigger::PreconditionCleared,
        cleared_precondition: Some(cleared),
        x11_outcome: Some(outcome),
        at_unix_s: Some(at_unix_s),
    });
    (outcome, X11Reproof::Write(Box::new(record)))
}

/// Schema of the attempt marker.
pub const REQUALIFY_MARKER_SCHEMA_VERSION: u32 = 1;

/// How long a failed automatic attempt for one stack holds off another.
pub const REQUALIFY_RETRY_AFTER_S: u64 = 6 * 60 * 60;

/// Consecutive attempts on one stack that may end without an outcome
/// (the Pier killed or crashed during the run) before the next is held off
/// like a failure.
pub const AUTO_REQUALIFY_MAX_INTERRUPTED: u32 = 3;

/// Most displays an automatic re-qualification measures; the rest are kept
/// in the requested-displays list for the next full qualification.
pub const AUTO_REQUALIFY_MAX_OUTPUTS: usize = 5;

/// No further display is started once this much time has passed.
pub const AUTO_REQUALIFY_DEADLINE_S: u64 = 10 * 60;

/// Seconds of motion per Speed measurement in an automatic run.
pub const AUTO_REQUALIFY_MEASURE_SECONDS: u32 = 6;

/// The last automatic attempt, kept beside the record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequalifyAttempt {
    /// [`REQUALIFY_MARKER_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// [`identity_digest`] of the stack the attempt ran on.
    pub identity_sha256: String,
    /// When, seconds since the Unix epoch.
    pub attempted_at_unix_s: u64,
    /// Whether it produced a record that serves Wayland.
    pub passed: bool,
    /// The failing steps, when it did not.
    pub failures: Vec<String>,
    /// The attempt was running when this marker was written: a marker
    /// still saying so at the next start was interrupted.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_progress: bool,
    /// Attempts on this stack that ended without an outcome just before
    /// this one.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub interrupted_before: u32,
    /// Why it ran; absent for [`RequalifyTrigger::RecordVersion`], so those
    /// markers are written as before.
    #[serde(default, skip_serializing_if = "RequalifyTrigger::is_record_version")]
    pub trigger: RequalifyTrigger,
    /// The kernel's boot id when it ran (X11 trigger: one attempt per boot).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// The host precondition the record skipped X11 for (X11 trigger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precondition: Option<String>,
    /// What it found about X11 applications (X11 trigger, once finished).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x11_outcome: Option<X11RequalifyOutcome>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl RequalifyAttempt {
    /// The marker written before an attempt runs: if the next start still
    /// finds it, the attempt was interrupted.
    #[must_use]
    pub const fn started(
        identity_sha256: String,
        attempted_at_unix_s: u64,
        interrupted_before: u32,
    ) -> Self {
        Self {
            schema_version: REQUALIFY_MARKER_SCHEMA_VERSION,
            identity_sha256,
            attempted_at_unix_s,
            passed: false,
            failures: Vec::new(),
            in_progress: true,
            interrupted_before,
            trigger: RequalifyTrigger::RecordVersion,
            boot_id: None,
            precondition: None,
            x11_outcome: None,
        }
    }

    /// The same marker for [`RequalifyTrigger::PreconditionCleared`]:
    /// `precondition` cleared on boot `boot_id`.
    #[must_use]
    pub fn for_precondition(self, precondition: String, boot_id: Option<String>) -> Self {
        Self {
            trigger: RequalifyTrigger::PreconditionCleared,
            boot_id,
            precondition: Some(precondition),
            ..self
        }
    }

    /// The marker of the same attempt once it has an outcome.
    #[must_use]
    pub fn finished(self, passed: bool, failures: Vec<String>) -> Self {
        Self {
            passed,
            failures,
            in_progress: false,
            interrupted_before: 0,
            ..self
        }
    }

    /// With the X11 outcome of a [`RequalifyTrigger::PreconditionCleared`]
    /// attempt.
    #[must_use]
    pub fn with_x11_outcome(self, outcome: X11RequalifyOutcome) -> Self {
        Self {
            x11_outcome: Some(outcome),
            ..self
        }
    }
}

/// What start-up does about a refused record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRequalify {
    /// Re-qualify now; `reasons` says why the record was refused.
    Run {
        /// The refusal, for the log.
        reasons: String,
        /// Attempts on this stack that ended without an outcome just
        /// before this one (for [`RequalifyAttempt::started`]).
        interrupted_before: u32,
    },
    /// A recent automatic attempt on this same stack failed: refuse without
    /// trying again.
    RecentFailure {
        /// When it ran.
        attempted_at_unix_s: u64,
        /// What failed.
        failures: Vec<String>,
    },
    /// [`RequalifyTrigger::PreconditionCleared`] only: an attempt on this
    /// stack already finished during this boot. Serve as the record says.
    AttemptedThisBoot {
        /// When it ran.
        attempted_at_unix_s: u64,
        /// What it found.
        outcome: X11RequalifyOutcome,
    },
    /// Not a case for automatic re-qualification: refuse as before.
    NotApplicable,
}

/// A stable digest of the stack a record is valid for.
#[must_use]
pub fn identity_digest(identity: &HostIdentity) -> String {
    let driver = identity.stack.gpu_driver.as_ref().map_or_else(
        || "none".to_owned(),
        |driver| {
            format!(
                "{}|{}|{}|{:?}",
                driver.vendor, driver.version, driver.drm, driver.modeset
            )
        },
    );
    let canonical = [
        identity.pier.version.as_str(),
        identity.pier.revision.as_str(),
        &identity.pier.executable_sha256.to_ascii_lowercase(),
        identity.stack.compositor.token(),
        identity.stack.compositor_version.as_str(),
        identity.stack.pipewire_version.as_str(),
        &driver,
    ]
    .join("\n");
    Sha256::digest(canonical.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Decides whether start-up re-qualifies the host after `refusal`.
///
/// `identity` is the running stack (`None` when it cannot be identified,
/// which no qualification could record either), `last` the previous
/// automatic attempt.
#[must_use]
pub fn auto_requalify_decision(
    refusal: &ProviderRefusal,
    enabled: bool,
    identity: Option<&HostIdentity>,
    last: Option<&RequalifyAttempt>,
    now_unix_s: u64,
) -> AutoRequalify {
    let ProviderRefusal::WaylandUnavailable {
        blockers,
        qualification: Some(error),
        ..
    } = refusal
    else {
        return AutoRequalify::NotApplicable;
    };
    let version_only = matches!(
        error,
        QualificationError::Stale(_) | QualificationError::UnsupportedSchema { .. }
    );
    let record_supplied_only = blockers.iter().all(|blocker| {
        matches!(
            blocker,
            WaylandBlocker::CaptureRoute(_) | WaylandBlocker::InputInjection(_)
        )
    });
    let (true, true, true, Some(identity)) =
        (enabled, version_only, record_supplied_only, identity)
    else {
        return AutoRequalify::NotApplicable;
    };
    let reasons = error.to_string();
    let last = last.filter(|last| last.trigger == RequalifyTrigger::RecordVersion);
    match recent_attempt(last, identity, now_unix_s) {
        Some(last) if last.in_progress => interrupted_retry(last, reasons),
        Some(last) if !last.passed => AutoRequalify::RecentFailure {
            attempted_at_unix_s: last.attempted_at_unix_s,
            failures: last.failures.clone(),
        },
        _ => AutoRequalify::Run {
            reasons,
            interrupted_before: 0,
        },
    }
}

/// `last` when it is a marker of this build's schema for `identity`'s stack
/// that ran less than [`REQUALIFY_RETRY_AFTER_S`] ago.
fn recent_attempt<'a>(
    last: Option<&'a RequalifyAttempt>,
    identity: &HostIdentity,
    now_unix_s: u64,
) -> Option<&'a RequalifyAttempt> {
    last.filter(|last| {
        last.schema_version == REQUALIFY_MARKER_SCHEMA_VERSION
            && last.identity_sha256 == identity_digest(identity)
            // A marker from the future (a clock set back) has expired.
            && last.attempted_at_unix_s <= now_unix_s
            && now_unix_s - last.attempted_at_unix_s < REQUALIFY_RETRY_AFTER_S
    })
}

/// After `last` never reached an outcome (not the stack's failure): try
/// again, up to [`AUTO_REQUALIFY_MAX_INTERRUPTED`] in a row.
fn interrupted_retry(last: &RequalifyAttempt, reasons: String) -> AutoRequalify {
    let interrupted = last.interrupted_before.saturating_add(1);
    if interrupted >= AUTO_REQUALIFY_MAX_INTERRUPTED {
        return AutoRequalify::RecentFailure {
            attempted_at_unix_s: last.attempted_at_unix_s,
            failures: vec![format!(
                "{interrupted} attempts in a row ended without an outcome (the Pier was killed \
                 or crashed while re-qualifying)"
            )],
        };
    }
    AutoRequalify::Run {
        reasons,
        interrupted_before: interrupted,
    }
}

/// Decides whether start-up re-qualifies a host whose valid record skipped
/// X11 applications for a host precondition that holds again.
///
/// `reason` is why start-up would serve without X11 applications (from
/// [`crate::wayland_x11::resolve_x11_apps`], including an `on` refusal's),
/// `proof` the record's X11 proof, `identity` the record's (running) stack,
/// `boot_id` the kernel's current boot id and `last` the previous attempt
/// for this trigger. Applies only to
/// [`X11AppsDisabledReason::PreconditionCleared`]. One attempt per boot and
/// stack: a finished one, passed or not, holds off another until the host
/// reboots (or the stack changes, or the operator's repair re-arms it); a
/// failed one also for [`REQUALIFY_RETRY_AFTER_S`]. Interrupted attempts
/// are retried as in [`auto_requalify_decision`].
#[must_use]
pub fn precondition_requalify_decision(
    reason: Option<X11AppsDisabledReason>,
    proof: Option<&XwaylandProof>,
    enabled: bool,
    identity: Option<&HostIdentity>,
    boot_id: Option<&str>,
    last: Option<&RequalifyAttempt>,
    now_unix_s: u64,
) -> AutoRequalify {
    let (
        true,
        Some(X11AppsDisabledReason::PreconditionCleared),
        Some(precondition),
        Some(identity),
    ) = (
        enabled,
        reason,
        proof.and_then(|proof| proof.host_precondition.as_deref()),
        identity,
    )
    else {
        return AutoRequalify::NotApplicable;
    };
    let reasons = format!(
        "{}: the qualification skipped X11 applications for {precondition}, which holds again",
        X11AppsDisabledReason::PreconditionCleared.token()
    );
    let last = last.filter(|last| {
        last.trigger == RequalifyTrigger::PreconditionCleared
            && last.schema_version == REQUALIFY_MARKER_SCHEMA_VERSION
            && last.identity_sha256 == identity_digest(identity)
            && last.attempted_at_unix_s <= now_unix_s
    });
    let same_boot = |last: &RequalifyAttempt| matches!((boot_id, last.boot_id.as_deref()), (Some(now), Some(then)) if now == then);
    match last {
        Some(last) if last.in_progress => {
            if recent_attempt(Some(last), identity, now_unix_s).is_some() {
                interrupted_retry(last, reasons)
            } else {
                AutoRequalify::Run {
                    reasons,
                    interrupted_before: 0,
                }
            }
        }
        Some(last) if same_boot(last) => AutoRequalify::AttemptedThisBoot {
            attempted_at_unix_s: last.attempted_at_unix_s,
            outcome: last.x11_outcome.unwrap_or(if last.passed {
                X11RequalifyOutcome::X11AppsFailed
            } else {
                X11RequalifyOutcome::NotRecorded
            }),
        },
        Some(last)
            if !last.passed && recent_attempt(Some(last), identity, now_unix_s).is_some() =>
        {
            AutoRequalify::RecentFailure {
                attempted_at_unix_s: last.attempted_at_unix_s,
                failures: last.failures.clone(),
            }
        }
        _ => AutoRequalify::Run {
            reasons,
            interrupted_before: 0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_provider::CapabilityFact;
    use crate::wayland_qualification::{GpuDriver, PierBuild, StackVersions, StaleReason};
    use crate::wayland_session::HeadlessCompositor;

    fn identity() -> HostIdentity {
        HostIdentity {
            pier: PierBuild {
                version: "0.18.0".into(),
                revision: "abc".into(),
                executable_sha256: "ab".repeat(32),
            },
            stack: StackVersions {
                compositor: HeadlessCompositor::GnomeShell,
                compositor_version: "49.5".into(),
                compositor_prefix: None,
                pipewire_version: "1.4.11".into(),
                gpu_driver: Some(GpuDriver {
                    vendor: "nvidia".into(),
                    version: "580.105.08".into(),
                    drm: true,
                    modeset: Some(true),
                }),
            },
        }
    }

    fn refusal(error: QualificationError, blockers: Vec<WaylandBlocker>) -> ProviderRefusal {
        ProviderRefusal::WaylandUnavailable {
            requested: false,
            blockers,
            qualification: Some(error),
        }
    }

    fn stale() -> QualificationError {
        QualificationError::Stale(vec![StaleReason::CompositorVersion {
            recorded: "49.4".into(),
            current: "49.5".into(),
        }])
    }

    const RECORD_BLOCKERS: [WaylandBlocker; 2] = [
        WaylandBlocker::CaptureRoute(CapabilityFact::Unknown),
        WaylandBlocker::InputInjection(CapabilityFact::Unknown),
    ];

    #[test]
    fn a_version_change_re_qualifies_once_per_stack() {
        let id = identity();
        let stale = refusal(stale(), RECORD_BLOCKERS.to_vec());
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), None, 1_000),
            AutoRequalify::Run { reasons, .. } if reasons.contains("compositor version 49.4 -> 49.5")
        ));
        let schema = refusal(
            QualificationError::UnsupportedSchema { found: 2 },
            RECORD_BLOCKERS.to_vec(),
        );
        assert!(matches!(
            auto_requalify_decision(&schema, true, Some(&id), None, 1_000),
            AutoRequalify::Run { .. }
        ));
        // A failure on this stack holds off another try for the window.
        let failed = RequalifyAttempt {
            schema_version: REQUALIFY_MARKER_SCHEMA_VERSION,
            identity_sha256: identity_digest(&id),
            attempted_at_unix_s: 1_000,
            passed: false,
            failures: vec!["FAIL session: did not start".into()],
            in_progress: false,
            interrupted_before: 0,
            trigger: RequalifyTrigger::RecordVersion,
            boot_id: None,
            precondition: None,
            x11_outcome: None,
        };
        assert_eq!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&failed), 2_000),
            AutoRequalify::RecentFailure {
                attempted_at_unix_s: 1_000,
                failures: failed.failures.clone(),
            }
        );
        assert!(matches!(
            auto_requalify_decision(
                &stale,
                true,
                Some(&id),
                Some(&failed),
                1_000 + REQUALIFY_RETRY_AFTER_S
            ),
            AutoRequalify::Run { .. }
        ));
        // A marker dated after now (the clock went back) holds nothing off.
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&failed), 999),
            AutoRequalify::Run { .. }
        ));
        // Another stack (a further update) tries again at once.
        let mut updated = id.clone();
        updated.stack.compositor_version = "49.6".into();
        assert_ne!(identity_digest(&updated), identity_digest(&id));
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&updated), Some(&failed), 2_000),
            AutoRequalify::Run { .. }
        ));
        // A passed attempt never holds anything off.
        let passed = RequalifyAttempt {
            passed: true,
            ..failed
        };
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&passed), 2_000),
            AutoRequalify::Run { .. }
        ));
    }

    #[test]
    fn an_interrupted_attempt_is_retried_at_once_but_not_for_ever() {
        let id = identity();
        let stale = refusal(stale(), RECORD_BLOCKERS.to_vec());
        let killed = RequalifyAttempt::started(identity_digest(&id), 1_000, 0);
        assert!(killed.in_progress && !killed.passed);
        // A start that finds its predecessor's marker still running retries.
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&killed), 1_100),
            AutoRequalify::Run {
                interrupted_before: 1,
                ..
            }
        ));
        let again = RequalifyAttempt::started(
            identity_digest(&id),
            1_100,
            AUTO_REQUALIFY_MAX_INTERRUPTED - 2,
        );
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&again), 1_200),
            AutoRequalify::Run { interrupted_before, .. }
                if interrupted_before == AUTO_REQUALIFY_MAX_INTERRUPTED - 1
        ));
        // The bound: a run that kills the Pier every time stops being tried.
        let last = RequalifyAttempt::started(
            identity_digest(&id),
            1_200,
            AUTO_REQUALIFY_MAX_INTERRUPTED - 1,
        );
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&last), 1_300),
            AutoRequalify::RecentFailure { attempted_at_unix_s: 1_200, failures }
                if failures[0].contains("without an outcome")
        ));
        // ...for the window only, and never across a stack change.
        assert!(matches!(
            auto_requalify_decision(
                &stale,
                true,
                Some(&id),
                Some(&last),
                1_200 + REQUALIFY_RETRY_AFTER_S
            ),
            AutoRequalify::Run {
                interrupted_before: 0,
                ..
            }
        ));
        let mut updated = id.clone();
        updated.stack.pipewire_version = "1.4.12".into();
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&updated), Some(&last), 1_300),
            AutoRequalify::Run {
                interrupted_before: 0,
                ..
            }
        ));
        // A finished attempt carries its outcome and forgets interruptions.
        let failed = last.finished(false, vec!["FAIL x".into()]);
        assert!(!failed.in_progress && failed.interrupted_before == 0);
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&failed), 1_300),
            AutoRequalify::RecentFailure { .. }
        ));
    }

    #[test]
    fn the_marker_reads_markers_written_before_interruptions_were_kept() {
        let old = r#"{"schema_version":1,"identity_sha256":"ab","attempted_at_unix_s":5,
            "passed":false,"failures":["qualify: x"]}"#;
        let marker: RequalifyAttempt = serde_json::from_str(old).unwrap();
        assert!(!marker.in_progress);
        assert_eq!(marker.interrupted_before, 0);
        // A finished marker is written as before, so an older Pier reads it.
        let text = serde_json::to_string(&marker).unwrap();
        assert!(!text.contains("in_progress") && !text.contains("interrupted_before"));
    }

    fn skipped_proof() -> XwaylandProof {
        XwaylandProof {
            display_proven: false,
            window_manager: false,
            pointer: false,
            glamor: crate::wayland_x11::GlamorRoute::Unknown,
            xwayland_version: Some("24.1.9".into()),
            failure: Some("/tmp/.X11-unix is owned by uid 65534, not root".into()),
            host_precondition: Some("x11_unix_dir_foreign_owner".into()),
        }
    }

    const CLEARED: Option<X11AppsDisabledReason> = Some(X11AppsDisabledReason::PreconditionCleared);

    fn x11_marker(id: &HostIdentity, at: u64, boot: &str) -> RequalifyAttempt {
        RequalifyAttempt::started(identity_digest(id), at, 0)
            .for_precondition("x11_unix_dir_foreign_owner".into(), Some(boot.into()))
    }

    #[test]
    fn a_cleared_x11_precondition_is_re_proved_once_per_boot() {
        let id = identity();
        let proof = skipped_proof();
        let decide = |last: Option<&RequalifyAttempt>, boot: &str, now: u64| {
            precondition_requalify_decision(
                CLEARED,
                Some(&proof),
                true,
                Some(&id),
                Some(boot),
                last,
                now,
            )
        };
        assert!(matches!(
            decide(None, "boot-a", 1_000),
            AutoRequalify::Run { reasons, interrupted_before: 0 }
                if reasons.starts_with("x11_precondition_cleared:")
                    && reasons.contains("x11_unix_dir_foreign_owner")
        ));
        // Finished in this boot, whatever it found: not again until a reboot.
        for (passed, outcome) in [
            (true, X11RequalifyOutcome::PreconditionReturned),
            (true, X11RequalifyOutcome::X11AppsFailed),
            (false, X11RequalifyOutcome::NotRecorded),
        ] {
            let done = x11_marker(&id, 1_000, "boot-a")
                .finished(passed, vec![])
                .with_x11_outcome(outcome);
            assert_eq!(
                decide(Some(&done), "boot-a", 1_000 + 2 * REQUALIFY_RETRY_AFTER_S),
                AutoRequalify::AttemptedThisBoot {
                    attempted_at_unix_s: 1_000,
                    outcome
                }
            );
        }
        // After a reboot: a pass holds nothing off, a failure only for the
        // window.
        let passed = x11_marker(&id, 1_000, "boot-a")
            .finished(true, vec![])
            .with_x11_outcome(X11RequalifyOutcome::PreconditionReturned);
        assert!(matches!(
            decide(Some(&passed), "boot-b", 1_100),
            AutoRequalify::Run { .. }
        ));
        let failed = x11_marker(&id, 1_000, "boot-a")
            .finished(false, vec!["FAIL session: did not start".into()])
            .with_x11_outcome(X11RequalifyOutcome::NotRecorded);
        assert!(matches!(
            decide(Some(&failed), "boot-b", 1_100),
            AutoRequalify::RecentFailure { attempted_at_unix_s: 1_000, failures }
                if failures == failed.failures
        ));
        assert!(matches!(
            decide(Some(&failed), "boot-b", 1_000 + REQUALIFY_RETRY_AFTER_S),
            AutoRequalify::Run { .. }
        ));
        // Another stack (an update) tries again at once.
        let mut updated = id.clone();
        updated.stack.compositor_version = "49.6".into();
        assert!(matches!(
            precondition_requalify_decision(
                CLEARED,
                Some(&proof),
                true,
                Some(&updated),
                Some("boot-a"),
                Some(&failed),
                1_100
            ),
            AutoRequalify::Run { .. }
        ));
        // A version marker never holds the X11 trigger off, nor the reverse.
        let version_failed = RequalifyAttempt::started(identity_digest(&id), 1_000, 0)
            .finished(false, vec!["FAIL x".into()]);
        assert!(matches!(
            decide(Some(&version_failed), "boot-a", 1_100),
            AutoRequalify::Run { .. }
        ));
        let stale = refusal(stale(), RECORD_BLOCKERS.to_vec());
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&failed), 1_100),
            AutoRequalify::Run { .. }
        ));
    }

    #[test]
    fn an_interrupted_x11_re_proof_is_retried_but_bounded() {
        let id = identity();
        let proof = skipped_proof();
        let decide = |last: &RequalifyAttempt, now: u64| {
            precondition_requalify_decision(
                CLEARED,
                Some(&proof),
                true,
                Some(&id),
                Some("boot-a"),
                Some(last),
                now,
            )
        };
        let killed = x11_marker(&id, 1_000, "boot-a");
        assert!(matches!(
            decide(&killed, 1_100),
            AutoRequalify::Run {
                interrupted_before: 1,
                ..
            }
        ));
        let mut last = killed;
        last.interrupted_before = AUTO_REQUALIFY_MAX_INTERRUPTED - 1;
        assert!(matches!(
            decide(&last, 1_100),
            AutoRequalify::RecentFailure { failures, .. } if failures[0].contains("without an outcome")
        ));
        assert!(matches!(
            decide(&last, 1_000 + REQUALIFY_RETRY_AFTER_S),
            AutoRequalify::Run {
                interrupted_before: 0,
                ..
            }
        ));
    }

    #[test]
    fn only_a_cleared_precondition_re_proves_x11() {
        let id = identity();
        let proof = skipped_proof();
        let fault = crate::wayland_x11::SocketDirFault {
            dir: crate::wayland_x11::SharedSocketDir::X11Unix,
            problem: crate::wayland_x11::SocketDirProblem::ForeignOwner { owner: 65_534 },
        };
        for reason in [
            None,
            Some(X11AppsDisabledReason::OperatorOff),
            Some(X11AppsDisabledReason::QualificationFailed),
            Some(X11AppsDisabledReason::NotQualified),
            Some(X11AppsDisabledReason::SocketDirectory(fault)),
        ] {
            assert_eq!(
                precondition_requalify_decision(
                    reason,
                    Some(&proof),
                    true,
                    Some(&id),
                    None,
                    None,
                    0
                ),
                AutoRequalify::NotApplicable
            );
        }
        let mut failed = proof.clone();
        failed.host_precondition = None;
        for (proof, enabled, identity) in [
            (Some(&failed), true, Some(&id)),
            (None, true, Some(&id)),
            (Some(&proof), false, Some(&id)),
            (Some(&proof), true, None),
        ] {
            assert_eq!(
                precondition_requalify_decision(CLEARED, proof, enabled, identity, None, None, 0),
                AutoRequalify::NotApplicable
            );
        }
    }

    #[test]
    fn the_x11_outcome_follows_the_new_proof() {
        let skipped = skipped_proof();
        let mut proven = skipped.clone();
        proven.host_precondition = None;
        proven.failure = None;
        proven.display_proven = true;
        proven.window_manager = true;
        proven.pointer = true;
        let mut failed = proven.clone();
        failed.pointer = false;
        assert_eq!(
            x11_requalify_outcome(true, Some(&proven)),
            X11RequalifyOutcome::X11AppsProven
        );
        assert_eq!(
            x11_requalify_outcome(true, Some(&failed)),
            X11RequalifyOutcome::X11AppsFailed
        );
        assert_eq!(
            x11_requalify_outcome(true, Some(&skipped)),
            X11RequalifyOutcome::PreconditionReturned
        );
        assert_eq!(
            x11_requalify_outcome(true, None),
            X11RequalifyOutcome::X11AppsNotTried
        );
        assert_eq!(
            x11_requalify_outcome(false, Some(&proven)),
            X11RequalifyOutcome::NotRecorded
        );
        // The X11 marker and record section carry typed tokens; a version
        // marker is written exactly as before.
        let marker = RequalifyAttempt::started("ab".into(), 5, 0)
            .for_precondition("x11_unix_dir_foreign_owner".into(), Some("b1".into()))
            .finished(true, vec![])
            .with_x11_outcome(X11RequalifyOutcome::X11AppsProven);
        let text = serde_json::to_string(&marker).unwrap();
        assert!(text.contains(r#""trigger":"precondition_cleared""#));
        assert!(text.contains(r#""x11_outcome":"x11_apps_proven""#));
        assert_eq!(
            serde_json::from_str::<RequalifyAttempt>(&text).unwrap(),
            marker
        );
        let version = RequalifyAttempt::started("ab".into(), 5, 0).finished(false, vec![]);
        let text = serde_json::to_string(&version).unwrap();
        assert!(!text.contains("trigger") && !text.contains("boot_id"));
        let section = AutomaticRequalification {
            trigger: RequalifyTrigger::PreconditionCleared,
            cleared_precondition: Some("x11_unix_dir_foreign_owner".into()),
            x11_outcome: Some(X11RequalifyOutcome::PreconditionReturned),
            at_unix_s: None,
        };
        assert_eq!(
            serde_json::to_string(&section).unwrap(),
            r#"{"trigger":"precondition_cleared","cleared_precondition":"x11_unix_dir_foreign_owner","x11_outcome":"precondition_returned"}"#
        );
    }

    #[test]
    fn values_a_newer_build_writes_stay_readable() {
        let section: AutomaticRequalification = serde_json::from_str(
            r#"{"trigger":"operator_request","x11_outcome":"x11_apps_partly","at_unix_s":7,"new_field":1}"#,
        )
        .expect("a newer section stays readable");
        assert_eq!(section.trigger, RequalifyTrigger::Unknown);
        assert_eq!(section.x11_outcome, Some(X11RequalifyOutcome::Unknown));
        let marker: RequalifyAttempt = serde_json::from_str(
            r#"{"schema_version":1,"identity_sha256":"ab","attempted_at_unix_s":5,"passed":true,
                "failures":[],"trigger":"operator_request","x11_outcome":"x11_apps_partly"}"#,
        )
        .expect("a newer marker stays readable");
        assert_eq!(marker.trigger, RequalifyTrigger::Unknown);
        // ...and holds neither trigger off.
        let id = identity();
        let marker = RequalifyAttempt {
            identity_sha256: identity_digest(&id),
            passed: false,
            ..marker
        };
        let stale = refusal(stale(), RECORD_BLOCKERS.to_vec());
        assert!(matches!(
            auto_requalify_decision(&stale, true, Some(&id), Some(&marker), 6),
            AutoRequalify::Run { .. }
        ));
        assert!(matches!(
            precondition_requalify_decision(
                CLEARED,
                Some(&skipped_proof()),
                true,
                Some(&id),
                None,
                Some(&marker),
                6
            ),
            AutoRequalify::Run { .. }
        ));
    }

    fn full_record() -> WaylandQualificationRecord {
        use crate::desktop_provider::CapabilityFact;
        use crate::wayland_qualification::{
            CaptureRoute, MultiOutputFacts, OsRelease, QualifiedFacts, RateEvidence,
            SCHEMA_VERSION, SourceRateMeasurement,
        };
        let rate = |width, height| SourceRateMeasurement {
            width,
            height,
            display_refresh_hz: 60,
            refresh_hz: 60,
            codec: "h265".into(),
            route: CaptureRoute::GpuConvert,
            sustained_source_fps: Some(60),
            seconds: 8,
            evidence: RateEvidence::Measured,
        };
        WaylandQualificationRecord {
            schema_version: SCHEMA_VERSION,
            qualified_at_unix_s: 1_000,
            host: identity(),
            os: OsRelease {
                id: "rocky".into(),
                version_id: "10.2".into(),
                kernel: "6.12.0".into(),
            },
            facts: QualifiedFacts {
                capture_route: None,
                input: None,
                // An operator's run: ten displays at 8 s each.
                source_rates: (1..=10)
                    .map(|n| rate(1280 + 64 * n, 720 + 36 * n))
                    .collect(),
                audio_monitor: CapabilityFact::Available,
                grading: CapabilityFact::Available,
                hdr: CapabilityFact::Missing,
                x11_apps: Some(skipped_proof()),
                multi_output: Some(MultiOutputFacts { runs: Vec::new() }),
                encoder: None,
                fidelity: None,
                cursor_metadata: CapabilityFact::Available,
                native_seat: None,
                automatic_requalification: None,
            },
        }
    }

    #[test]
    fn an_x11_re_proof_replaces_only_the_x11_proof() {
        let previous = full_record();
        let mut proven = skipped_proof();
        proven.host_precondition = None;
        proven.failure = None;
        proven.display_proven = true;
        proven.window_manager = true;
        proven.pointer = true;
        let (outcome, written) = x11_reproof_record(&previous, Some(&proven), 2_000);
        assert_eq!(outcome, X11RequalifyOutcome::X11AppsProven);
        let X11Reproof::Write(record) = written else {
            panic!("a proven re-proof is written");
        };
        let mut expected = previous.clone();
        expected.facts.x11_apps = Some(proven.clone());
        expected.facts.automatic_requalification = Some(AutomaticRequalification {
            trigger: RequalifyTrigger::PreconditionCleared,
            cleared_precondition: Some("x11_unix_dir_foreign_owner".into()),
            x11_outcome: Some(X11RequalifyOutcome::X11AppsProven),
            at_unix_s: Some(2_000),
        });
        // Every other fact, the identity and the qualification time are kept.
        assert_eq!(*record, expected);
        assert_eq!(record.facts.source_rates.len(), 10);

        // A failed proof replaces the skipped one too (no further trigger).
        let mut failed = proven.clone();
        failed.pointer = false;
        failed.failure = Some("pointer did not move".into());
        assert!(matches!(
            x11_reproof_record(&previous, Some(&failed), 2_000),
            (X11RequalifyOutcome::X11AppsFailed, X11Reproof::Write(record))
                if record.facts.x11_apps == Some(failed.clone())
                    && record.facts.source_rates == previous.facts.source_rates
        ));
    }

    #[test]
    fn a_returned_precondition_leaves_the_record_unchanged() {
        let previous = full_record();
        assert_eq!(
            x11_reproof_record(&previous, Some(&skipped_proof()), 2_000),
            (
                X11RequalifyOutcome::PreconditionReturned,
                X11Reproof::Unchanged
            )
        );
        assert_eq!(
            x11_reproof_record(&previous, None, 2_000),
            (X11RequalifyOutcome::X11AppsNotTried, X11Reproof::Unchanged)
        );
        // A record requalified meanwhile (no precondition left) is not
        // touched either.
        let mut requalified = previous;
        requalified
            .facts
            .x11_apps
            .as_mut()
            .expect("proof")
            .host_precondition = None;
        let mut proven = skipped_proof();
        proven.host_precondition = None;
        proven.display_proven = true;
        proven.window_manager = true;
        proven.pointer = true;
        assert_eq!(
            x11_reproof_record(&requalified, Some(&proven), 2_000).1,
            X11Reproof::Unchanged
        );
    }

    #[test]
    fn untrusted_missing_or_live_failures_are_never_re_qualified() {
        let id = identity();
        for error in [
            QualificationError::Missing {
                path: "/var/lib/arcen/wayland-qualification.json".into(),
            },
            QualificationError::Unreadable {
                path: "/var/lib/arcen/wayland-qualification.json".into(),
                reason: "it must be owned by root".into(),
            },
            QualificationError::Malformed {
                reason: "missing field".into(),
            },
        ] {
            assert_eq!(
                auto_requalify_decision(
                    &refusal(error, RECORD_BLOCKERS.to_vec()),
                    true,
                    Some(&id),
                    None,
                    0
                ),
                AutoRequalify::NotApplicable
            );
        }
        let no_render_node = refusal(
            stale(),
            vec![WaylandBlocker::RenderNodeAccess(CapabilityFact::Missing)],
        );
        assert_eq!(
            auto_requalify_decision(&no_render_node, true, Some(&id), None, 0),
            AutoRequalify::NotApplicable
        );
        let stale = refusal(stale(), RECORD_BLOCKERS.to_vec());
        assert_eq!(
            auto_requalify_decision(&stale, false, Some(&id), None, 0),
            AutoRequalify::NotApplicable
        );
        assert_eq!(
            auto_requalify_decision(&stale, true, None, None, 0),
            AutoRequalify::NotApplicable
        );
        assert_eq!(
            auto_requalify_decision(
                &ProviderRefusal::XorgMissing { requested: true },
                true,
                Some(&id),
                None,
                0
            ),
            AutoRequalify::NotApplicable
        );
    }
}
