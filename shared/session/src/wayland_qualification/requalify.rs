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

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{HostIdentity, QualificationError};
use crate::desktop_provider::{ProviderRefusal, WaylandBlocker};

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
    let Some(last) = last.filter(|last| {
        last.schema_version == REQUALIFY_MARKER_SCHEMA_VERSION
            && !last.passed
            && last.identity_sha256 == identity_digest(identity)
            // A marker from the future (a clock set back) has expired.
            && last.attempted_at_unix_s <= now_unix_s
            && now_unix_s - last.attempted_at_unix_s < REQUALIFY_RETRY_AFTER_S
    }) else {
        return AutoRequalify::Run {
            reasons,
            interrupted_before: 0,
        };
    };
    if !last.in_progress {
        return AutoRequalify::RecentFailure {
            attempted_at_unix_s: last.attempted_at_unix_s,
            failures: last.failures.clone(),
        };
    }
    // The last attempt never reached an outcome: not the stack's failure.
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
