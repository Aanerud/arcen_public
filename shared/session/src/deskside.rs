//! Pure deskside policy, physical-evidence validation, and restore ordering.

use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::restore_lease::{
    LeaseOwnerId, RestoreEvent, RestoreLease, RestorePhase, RestoreResource, StateFingerprint,
};

const INPUT_BIT: u8 = 1;
const DISPLAY_BIT: u8 = 2;

/// Operator-enforced deskside policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesksidePolicy {
    /// Do not mutate local physical resources.
    #[default]
    Disabled,
    /// Require both local input blocking and local display protection.
    Required,
}

impl DesksidePolicy {
    /// Evaluates a validated physical-host evidence result.
    #[must_use]
    pub const fn decide(
        self,
        evidence: Result<&PhysicalHostEvidence, DesksideRefusalReason>,
    ) -> DesksideDecision {
        match self {
            Self::Disabled => DesksideDecision::Disabled,
            Self::Required => match evidence {
                Ok(_) => DesksideDecision::Arm(DesksideControlSet::all()),
                Err(reason) => DesksideDecision::Refuse(reason),
            },
        }
    }

    /// Returns whether an approved reconnect keeps protection armed.
    #[must_use]
    pub const fn holds_through_reconnect(self) -> bool {
        matches!(self, Self::Required)
    }
}

/// Normalized disposition of one platform evidence fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceStatus {
    /// The fact positively proves the required physical/local property.
    Positive,
    /// Required operator or runtime evidence is absent.
    Missing,
    /// The adapter cannot classify the fact.
    Unknown,
    /// The fact identifies a virtual resource or host.
    Virtual,
    /// The fact identifies a remote session or resource.
    Remote,
    /// The fact identifies a paravirtual resource or host.
    Paravirtual,
    /// Runtime resources conflict with the pinned configuration.
    Conflicting,
}

/// Bounded, normalized evidence collected by a host adapter.
///
/// Fingerprints are hashes of normalized facts. Raw EDIDs, serials, paths, OS
/// handles, and hardware identifiers do not belong in this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalEvidenceSummary {
    /// Evidence was collected for this arm attempt rather than reused.
    pub runtime_fresh: bool,
    /// Host or firmware classification.
    pub host: EvidenceStatus,
    /// Interactive console/session classification.
    pub console_session: EvidenceStatus,
    /// Pinned physical keyboard and pointer classification.
    pub local_input: EvidenceStatus,
    /// Pinned physical display classification.
    pub local_displays: EvidenceStatus,
    /// Every relevant active physical-looking resource is pinned.
    pub active_resources_accounted: EvidenceStatus,
    /// Physical-console resources are distinct from capture resources.
    pub capture_separation: EvidenceStatus,
    /// Bounded hash of normalized pinned input facts.
    pub input_fingerprint: Option<StateFingerprint>,
    /// Bounded hash of normalized pinned display facts.
    pub display_fingerprint: Option<StateFingerprint>,
}

/// Positive physical-host evidence accepted for one arm attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalHostEvidence {
    input_fingerprint: StateFingerprint,
    display_fingerprint: StateFingerprint,
}

impl PhysicalHostEvidence {
    /// Validates a normalized evidence conjunction.
    ///
    /// # Errors
    ///
    /// Returns a stable refusal reason for stale, incomplete, unknown, negative,
    /// or conflicting evidence.
    pub fn validate(summary: PhysicalEvidenceSummary) -> Result<Self, DesksideRefusalReason> {
        if !summary.runtime_fresh {
            return Err(DesksideRefusalReason::StaleEvidence);
        }

        let statuses = [
            summary.host,
            summary.console_session,
            summary.local_input,
            summary.local_displays,
            summary.active_resources_accounted,
            summary.capture_separation,
        ];

        for status in statuses {
            let refusal = match status {
                EvidenceStatus::Virtual => Some(DesksideRefusalReason::VirtualEvidence),
                EvidenceStatus::Remote => Some(DesksideRefusalReason::RemoteEvidence),
                EvidenceStatus::Paravirtual => Some(DesksideRefusalReason::ParavirtualEvidence),
                EvidenceStatus::Conflicting => Some(DesksideRefusalReason::ConflictingEvidence),
                EvidenceStatus::Positive | EvidenceStatus::Missing | EvidenceStatus::Unknown => {
                    None
                }
            };
            if let Some(refusal) = refusal {
                return Err(refusal);
            }
        }

        if statuses.contains(&EvidenceStatus::Missing) {
            return Err(DesksideRefusalReason::MissingEvidence);
        }
        if statuses.contains(&EvidenceStatus::Unknown) {
            return Err(DesksideRefusalReason::UnknownEvidence);
        }

        let input_fingerprint = summary
            .input_fingerprint
            .ok_or(DesksideRefusalReason::MissingInputFingerprint)?;
        let display_fingerprint = summary
            .display_fingerprint
            .ok_or(DesksideRefusalReason::MissingDisplayFingerprint)?;

        Ok(Self {
            input_fingerprint,
            display_fingerprint,
        })
    }

    /// Returns the normalized pinned-input fingerprint.
    #[must_use]
    pub const fn input_fingerprint(self) -> StateFingerprint {
        self.input_fingerprint
    }

    /// Returns the normalized pinned-display fingerprint.
    #[must_use]
    pub const fn display_fingerprint(self) -> StateFingerprint {
        self.display_fingerprint
    }
}

/// Stable, non-sensitive pre-arm refusal reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesksideRefusalReason {
    /// Evidence was not collected for this arm attempt.
    StaleEvidence,
    /// Required positive facts or operator pins are absent.
    MissingEvidence,
    /// At least one required fact cannot be classified.
    UnknownEvidence,
    /// A virtual host or resource was observed.
    VirtualEvidence,
    /// A remote session or resource was observed.
    RemoteEvidence,
    /// A paravirtual host or resource was observed.
    ParavirtualEvidence,
    /// Runtime resources conflict with operator pins.
    ConflictingEvidence,
    /// The input pin set has no bounded normalized fingerprint.
    MissingInputFingerprint,
    /// The display pin set has no bounded normalized fingerprint.
    MissingDisplayFingerprint,
}

impl DesksideRefusalReason {
    /// Returns a stable code safe for operator-facing diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaleEvidence => "stale_evidence",
            Self::MissingEvidence => "missing_evidence",
            Self::UnknownEvidence => "unknown_evidence",
            Self::VirtualEvidence => "virtual_evidence",
            Self::RemoteEvidence => "remote_evidence",
            Self::ParavirtualEvidence => "paravirtual_evidence",
            Self::ConflictingEvidence => "conflicting_evidence",
            Self::MissingInputFingerprint => "missing_input_fingerprint",
            Self::MissingDisplayFingerprint => "missing_display_fingerprint",
        }
    }
}

impl Display for DesksideRefusalReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Error for DesksideRefusalReason {}

/// Result of evaluating the operator policy and fresh runtime evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesksideDecision {
    /// Deskside is disabled and no resource may be mutated.
    Disabled,
    /// Refuse before mutation.
    Refuse(DesksideRefusalReason),
    /// Arm the complete required control set.
    Arm(DesksideControlSet),
}

/// One protected local resource class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesksideControl {
    /// Physical keyboard and pointer input.
    LocalInput,
    /// Physical local displays.
    LocalDisplays,
}

impl DesksideControl {
    const fn bit(self) -> u8 {
        match self {
            Self::LocalInput => INPUT_BIT,
            Self::LocalDisplays => DISPLAY_BIT,
        }
    }
}

/// Fixed-size requested control set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesksideControlSet(u8);

impl DesksideControlSet {
    /// Returns the required all-or-nothing input and display set.
    #[must_use]
    pub const fn all() -> Self {
        Self(INPUT_BIT | DISPLAY_BIT)
    }

    /// Returns whether one control is required.
    #[must_use]
    pub const fn contains(self, control: DesksideControl) -> bool {
        self.0 & control.bit() != 0
    }
}

/// Original and protected fingerprints for one restore lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesksideLeaseSpec {
    /// State before deskside mutation.
    pub original: StateFingerprint,
    /// Fully applied and verified protected state.
    pub protected: StateFingerprint,
}

/// Public deskside lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesksideState {
    /// No deskside resource is owned.
    #[default]
    Inactive,
    /// Controls are being armed, applied, and verified.
    Arming,
    /// Every required control is applied and verified.
    Protected,
    /// The same protection is held through an approved reconnect window.
    ReconnectHeld,
    /// One or more resources are being restored.
    Restoring,
    /// Restoration was attempted for every resource but at least one failed.
    RestoreFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArmStage {
    Arm,
    Apply,
    Verify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreMode {
    Rollback,
    Terminal,
}

/// Host-observed result or lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesksideEvent {
    /// Durable ownership/journal arm completed.
    ArmSucceeded(DesksideControl),
    /// Durable ownership/journal arm failed before mutation.
    ArmFailed(DesksideControl),
    /// The requested mutation completed and requires verification.
    ApplySucceeded(DesksideControl),
    /// The mutation failed or may have partially applied.
    ApplyFailed(DesksideControl),
    /// Runtime verification proved the protected state.
    VerifySucceeded(DesksideControl),
    /// Runtime verification failed.
    VerifyFailed(DesksideControl),
    /// Transport was lost; `resumable` is authoritative reconnect output.
    TransportLost {
        /// Whether the landed reconnect model ordered leases held.
        resumable: bool,
    },
    /// The reconnect model accepted the replacement transport.
    Reconnected,
    /// Authoritative session drain began.
    BeginDraining,
    /// The authoritative reconnect deadline expired.
    ReconnectExpired,
    /// A host adapter or session owner failed.
    Failure,
    /// Session cancellation was requested.
    Cancel,
    /// Remote injection and media input processing have stopped.
    RemoteInjectionStopped,
    /// One resource restored successfully.
    RestoreSucceeded(DesksideControl),
    /// One resource restore failed.
    RestoreFailed(DesksideControl),
    /// Retry every resource whose previous restore failed.
    RetryRestore,
}

/// One bounded side-effect directive emitted by a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesksideEffect {
    /// No host work; the event was duplicate or inapplicable.
    #[default]
    None,
    /// Durably arm ownership for one resource before mutation.
    Arm(DesksideControl),
    /// Apply one resource mutation.
    Apply(DesksideControl),
    /// Verify one resource reached the protected state.
    Verify(DesksideControl),
    /// Stop remote injection before terminal restoration.
    StopRemoteInjection,
    /// Restore one resource. Displays are always emitted before input.
    Restore(DesksideControl),
    /// Every required control is verified; streaming may start.
    ProtectionEstablished,
    /// Arm failed and every possibly mutated resource was restored.
    ArmRolledBack,
    /// Terminal restoration succeeded; authoritative cleanup may complete.
    CleanupAuthorized,
    /// Keep the recovery journal armed and expose the restore failure.
    PreserveRecoveryJournal,
}

/// Error returned when starting a deskside composite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesksideStartError {
    /// Policy evaluation did not authorize arming.
    NotAuthorized,
    /// A composite is already active or failed restoration is unresolved.
    AlreadyActive,
}

impl Display for DesksideStartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAuthorized => formatter.write_str("deskside decision does not authorize arm"),
            Self::AlreadyActive => formatter.write_str("deskside protection is already active"),
        }
    }
}

impl Error for DesksideStartError {}

/// Deterministic composite over the landed generic input and display restore leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesksideProtection {
    state: DesksideState,
    leases: [Option<RestoreLease>; 2],
    arm_control: DesksideControl,
    arm_stage: ArmStage,
    mutated: u8,
    restore_pending: u8,
    restore_failures: u8,
    restoring_control: Option<DesksideControl>,
    restore_mode: RestoreMode,
}

impl Default for DesksideProtection {
    fn default() -> Self {
        Self {
            state: DesksideState::Inactive,
            leases: [None, None],
            arm_control: DesksideControl::LocalInput,
            arm_stage: ArmStage::Arm,
            mutated: 0,
            restore_pending: 0,
            restore_failures: 0,
            restoring_control: None,
            restore_mode: RestoreMode::Terminal,
        }
    }
}

impl DesksideProtection {
    /// Creates an inactive composite.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the public lifecycle state.
    #[must_use]
    pub const fn state(&self) -> DesksideState {
        self.state
    }

    /// Returns one underlying generic lease phase.
    #[must_use]
    pub fn lease_phase(&self, control: DesksideControl) -> Option<RestorePhase> {
        self.lease(control).map(RestoreLease::phase)
    }

    /// Starts the all-or-nothing arm sequence.
    ///
    /// # Errors
    ///
    /// Returns an error unless policy evaluation authorized the complete control
    /// set and this composite is inactive.
    pub fn begin_arm(
        &mut self,
        decision: DesksideDecision,
        owner: LeaseOwnerId,
        input: DesksideLeaseSpec,
        displays: DesksideLeaseSpec,
    ) -> Result<DesksideEffect, DesksideStartError> {
        if self.state != DesksideState::Inactive {
            return Err(DesksideStartError::AlreadyActive);
        }
        if decision != DesksideDecision::Arm(DesksideControlSet::all()) {
            return Err(DesksideStartError::NotAuthorized);
        }

        self.leases = [
            Some(RestoreLease::arm(
                RestoreResource::DesksideInput,
                owner.clone(),
                input.original,
                input.protected,
            )),
            Some(RestoreLease::arm(
                RestoreResource::DesksideDisplay,
                owner,
                displays.original,
                displays.protected,
            )),
        ];
        self.state = DesksideState::Arming;
        self.arm_control = DesksideControl::LocalInput;
        self.arm_stage = ArmStage::Arm;
        self.mutated = 0;
        self.restore_pending = 0;
        self.restore_failures = 0;
        self.restoring_control = None;
        self.restore_mode = RestoreMode::Terminal;

        Ok(DesksideEffect::Arm(DesksideControl::LocalInput))
    }

    /// Applies one host result or lifecycle event.
    #[must_use]
    pub fn apply(&mut self, event: DesksideEvent) -> DesksideEffect {
        match self.state {
            DesksideState::Inactive => DesksideEffect::None,
            DesksideState::Arming => self.apply_arming(event),
            DesksideState::Protected => self.apply_protected(event),
            DesksideState::ReconnectHeld => self.apply_reconnect_held(event),
            DesksideState::Restoring => self.apply_restoring(event),
            DesksideState::RestoreFailed => self.apply_restore_failed(event),
        }
    }

    fn apply_arming(&mut self, event: DesksideEvent) -> DesksideEffect {
        match (self.arm_stage, event) {
            (ArmStage::Arm, DesksideEvent::ArmSucceeded(control))
                if control == self.arm_control =>
            {
                if self
                    .lease_mut(control)
                    .and_then(|lease| lease.apply(RestoreEvent::BeginApply).ok())
                    .is_none()
                {
                    return self.fail_closed_restore();
                }
                self.arm_stage = ArmStage::Apply;
                DesksideEffect::Apply(control)
            }
            (ArmStage::Arm, DesksideEvent::ArmFailed(control)) if control == self.arm_control => {
                self.begin_rollback(false)
            }
            (ArmStage::Apply, DesksideEvent::ApplySucceeded(control))
                if control == self.arm_control =>
            {
                self.arm_stage = ArmStage::Verify;
                DesksideEffect::Verify(control)
            }
            (ArmStage::Apply, DesksideEvent::ApplyFailed(control))
                if control == self.arm_control =>
            {
                self.begin_rollback(true)
            }
            (ArmStage::Verify, DesksideEvent::VerifySucceeded(control))
                if control == self.arm_control =>
            {
                if self
                    .lease_mut(control)
                    .and_then(|lease| lease.apply(RestoreEvent::ApplySucceeded).ok())
                    .is_none()
                {
                    return self.fail_closed_restore();
                }
                self.mutated |= control.bit();
                if control == DesksideControl::LocalInput {
                    self.arm_control = DesksideControl::LocalDisplays;
                    self.arm_stage = ArmStage::Arm;
                    DesksideEffect::Arm(DesksideControl::LocalDisplays)
                } else {
                    self.state = DesksideState::Protected;
                    DesksideEffect::ProtectionEstablished
                }
            }
            (ArmStage::Verify, DesksideEvent::VerifyFailed(control))
                if control == self.arm_control =>
            {
                self.begin_rollback(true)
            }
            (
                ArmStage::Apply | ArmStage::Verify,
                DesksideEvent::BeginDraining
                | DesksideEvent::ReconnectExpired
                | DesksideEvent::Failure
                | DesksideEvent::Cancel,
            ) => self.begin_rollback(true),
            (
                ArmStage::Arm,
                DesksideEvent::BeginDraining
                | DesksideEvent::ReconnectExpired
                | DesksideEvent::Failure
                | DesksideEvent::Cancel,
            ) => self.begin_rollback(false),
            _ => DesksideEffect::None,
        }
    }

    fn apply_protected(&mut self, event: DesksideEvent) -> DesksideEffect {
        match event {
            DesksideEvent::TransportLost { resumable: true } => {
                self.state = DesksideState::ReconnectHeld;
                DesksideEffect::None
            }
            DesksideEvent::TransportLost { resumable: false }
            | DesksideEvent::BeginDraining
            | DesksideEvent::ReconnectExpired
            | DesksideEvent::Failure
            | DesksideEvent::Cancel => self.begin_terminal_restore(),
            _ => DesksideEffect::None,
        }
    }

    fn apply_reconnect_held(&mut self, event: DesksideEvent) -> DesksideEffect {
        match event {
            DesksideEvent::Reconnected => {
                self.state = DesksideState::Protected;
                DesksideEffect::None
            }
            DesksideEvent::BeginDraining
            | DesksideEvent::ReconnectExpired
            | DesksideEvent::Failure
            | DesksideEvent::Cancel
            | DesksideEvent::TransportLost { resumable: false } => self.begin_terminal_restore(),
            _ => DesksideEffect::None,
        }
    }

    fn apply_restoring(&mut self, event: DesksideEvent) -> DesksideEffect {
        if self.restore_mode == RestoreMode::Terminal
            && self.restoring_control.is_none()
            && event == DesksideEvent::RemoteInjectionStopped
        {
            return self.issue_next_restore();
        }

        let Some(control) = self.restoring_control else {
            return DesksideEffect::None;
        };
        match event {
            DesksideEvent::RestoreSucceeded(restored) if restored == control => {
                if self
                    .lease_mut(control)
                    .and_then(|lease| lease.apply(RestoreEvent::RestoreSucceeded).ok())
                    .is_none()
                {
                    self.restore_failures |= control.bit();
                } else {
                    self.mutated &= !control.bit();
                }
                self.restore_pending &= !control.bit();
                self.restoring_control = None;
                self.issue_next_restore()
            }
            DesksideEvent::RestoreFailed(failed) if failed == control => {
                self.restore_failures |= control.bit();
                self.restore_pending &= !control.bit();
                self.restoring_control = None;
                self.issue_next_restore()
            }
            _ => DesksideEffect::None,
        }
    }

    fn apply_restore_failed(&mut self, event: DesksideEvent) -> DesksideEffect {
        if event != DesksideEvent::RetryRestore || self.restore_failures == 0 {
            return DesksideEffect::None;
        }
        self.state = DesksideState::Restoring;
        self.restore_pending = self.restore_failures;
        self.restore_failures = 0;
        self.restoring_control = None;
        self.issue_next_restore()
    }

    fn begin_terminal_restore(&mut self) -> DesksideEffect {
        self.state = DesksideState::Restoring;
        self.restore_mode = RestoreMode::Terminal;
        self.restore_pending = self.mutated;
        self.restore_failures = 0;
        self.restoring_control = None;
        DesksideEffect::StopRemoteInjection
    }

    fn begin_rollback(&mut self, current_may_be_mutated: bool) -> DesksideEffect {
        if current_may_be_mutated {
            self.mutated |= self.arm_control.bit();
        }
        self.state = DesksideState::Restoring;
        self.restore_mode = RestoreMode::Rollback;
        self.restore_pending = self.mutated;
        self.restore_failures = 0;
        self.restoring_control = None;
        self.issue_next_restore()
    }

    fn issue_next_restore(&mut self) -> DesksideEffect {
        let next = if self.restore_pending & DISPLAY_BIT != 0 {
            Some(DesksideControl::LocalDisplays)
        } else if self.restore_pending & INPUT_BIT != 0 {
            Some(DesksideControl::LocalInput)
        } else {
            None
        };

        let Some(control) = next else {
            if self.restore_failures != 0 {
                self.state = DesksideState::RestoreFailed;
                return DesksideEffect::PreserveRecoveryJournal;
            }
            self.leases = [None, None];
            self.state = DesksideState::Inactive;
            return match self.restore_mode {
                RestoreMode::Rollback => DesksideEffect::ArmRolledBack,
                RestoreMode::Terminal => DesksideEffect::CleanupAuthorized,
            };
        };

        let began = self
            .lease_mut(control)
            .and_then(|lease| lease.apply(RestoreEvent::BeginRestore).ok())
            .is_some();
        if !began {
            self.restore_failures |= control.bit();
            self.restore_pending &= !control.bit();
            return self.issue_next_restore();
        }
        self.restoring_control = Some(control);
        DesksideEffect::Restore(control)
    }

    fn fail_closed_restore(&mut self) -> DesksideEffect {
        self.mutated |= self.arm_control.bit();
        self.begin_rollback(false)
    }

    fn lease(&self, control: DesksideControl) -> Option<&RestoreLease> {
        self.leases[Self::lease_index(control)].as_ref()
    }

    fn lease_mut(&mut self, control: DesksideControl) -> Option<&mut RestoreLease> {
        self.leases[Self::lease_index(control)].as_mut()
    }

    const fn lease_index(control: DesksideControl) -> usize {
        match control {
            DesksideControl::LocalInput => 0,
            DesksideControl::LocalDisplays => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(value: &[u8]) -> StateFingerprint {
        StateFingerprint::new(value).expect("bounded fixture")
    }

    fn positive_summary() -> PhysicalEvidenceSummary {
        PhysicalEvidenceSummary {
            runtime_fresh: true,
            host: EvidenceStatus::Positive,
            console_session: EvidenceStatus::Positive,
            local_input: EvidenceStatus::Positive,
            local_displays: EvidenceStatus::Positive,
            active_resources_accounted: EvidenceStatus::Positive,
            capture_separation: EvidenceStatus::Positive,
            input_fingerprint: Some(fingerprint(b"input-pins")),
            display_fingerprint: Some(fingerprint(b"display-pins")),
        }
    }

    fn started() -> DesksideProtection {
        let evidence =
            PhysicalHostEvidence::validate(positive_summary()).expect("positive evidence");
        let decision = DesksidePolicy::Required.decide(Ok(&evidence));
        let owner = LeaseOwnerId::new("deskside-test").expect("bounded owner");
        let mut protection = DesksideProtection::new();
        let first = protection
            .begin_arm(
                decision,
                owner,
                DesksideLeaseSpec {
                    original: fingerprint(b"input-original"),
                    protected: fingerprint(b"input-protected"),
                },
                DesksideLeaseSpec {
                    original: fingerprint(b"display-original"),
                    protected: fingerprint(b"display-protected"),
                },
            )
            .expect("start");
        assert_eq!(first, DesksideEffect::Arm(DesksideControl::LocalInput));
        protection
    }

    fn protected() -> DesksideProtection {
        let mut protection = started();
        assert_eq!(
            protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput)),
            DesksideEffect::Apply(DesksideControl::LocalInput)
        );
        assert_eq!(
            protection.apply(DesksideEvent::ApplySucceeded(DesksideControl::LocalInput)),
            DesksideEffect::Verify(DesksideControl::LocalInput)
        );
        assert_eq!(
            protection.apply(DesksideEvent::VerifySucceeded(DesksideControl::LocalInput)),
            DesksideEffect::Arm(DesksideControl::LocalDisplays)
        );
        assert_eq!(
            protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalDisplays)),
            DesksideEffect::Apply(DesksideControl::LocalDisplays)
        );
        assert_eq!(
            protection.apply(DesksideEvent::ApplySucceeded(
                DesksideControl::LocalDisplays
            )),
            DesksideEffect::Verify(DesksideControl::LocalDisplays)
        );
        assert_eq!(
            protection.apply(DesksideEvent::VerifySucceeded(
                DesksideControl::LocalDisplays
            )),
            DesksideEffect::ProtectionEstablished
        );
        protection
    }

    #[test]
    fn disabled_policy_never_arms() {
        assert_eq!(
            DesksidePolicy::Disabled.decide(Err(DesksideRefusalReason::MissingEvidence)),
            DesksideDecision::Disabled
        );
        assert!(!DesksidePolicy::Disabled.holds_through_reconnect());
        assert!(DesksidePolicy::Required.holds_through_reconnect());
    }

    #[test]
    fn evidence_requires_fresh_positive_conjunction_and_fingerprints() {
        let evidence = PhysicalHostEvidence::validate(positive_summary()).expect("positive");
        assert_eq!(evidence.input_fingerprint(), fingerprint(b"input-pins"));
        assert_eq!(evidence.display_fingerprint(), fingerprint(b"display-pins"));

        let cases = [
            (
                EvidenceStatus::Missing,
                DesksideRefusalReason::MissingEvidence,
            ),
            (
                EvidenceStatus::Unknown,
                DesksideRefusalReason::UnknownEvidence,
            ),
            (
                EvidenceStatus::Virtual,
                DesksideRefusalReason::VirtualEvidence,
            ),
            (
                EvidenceStatus::Remote,
                DesksideRefusalReason::RemoteEvidence,
            ),
            (
                EvidenceStatus::Paravirtual,
                DesksideRefusalReason::ParavirtualEvidence,
            ),
            (
                EvidenceStatus::Conflicting,
                DesksideRefusalReason::ConflictingEvidence,
            ),
        ];
        for (status, expected) in cases {
            let mut summary = positive_summary();
            summary.local_input = status;
            assert_eq!(PhysicalHostEvidence::validate(summary), Err(expected));
        }

        let mut stale = positive_summary();
        stale.runtime_fresh = false;
        assert_eq!(
            PhysicalHostEvidence::validate(stale),
            Err(DesksideRefusalReason::StaleEvidence)
        );
    }

    #[test]
    fn negative_evidence_wins_over_unknown_or_missing() {
        let mut summary = positive_summary();
        summary.host = EvidenceStatus::Unknown;
        summary.local_input = EvidenceStatus::Missing;
        summary.local_displays = EvidenceStatus::Virtual;
        assert_eq!(
            PhysicalHostEvidence::validate(summary),
            Err(DesksideRefusalReason::VirtualEvidence)
        );
    }

    #[test]
    fn required_policy_refuses_invalid_evidence_before_arm() {
        let decision = DesksidePolicy::Required.decide(Err(DesksideRefusalReason::UnknownEvidence));
        assert_eq!(
            decision,
            DesksideDecision::Refuse(DesksideRefusalReason::UnknownEvidence)
        );
        let mut protection = DesksideProtection::new();
        assert_eq!(
            protection.begin_arm(
                decision,
                LeaseOwnerId::new("owner").expect("owner"),
                DesksideLeaseSpec {
                    original: fingerprint(b"a"),
                    protected: fingerprint(b"b"),
                },
                DesksideLeaseSpec {
                    original: fingerprint(b"c"),
                    protected: fingerprint(b"d"),
                },
            ),
            Err(DesksideStartError::NotAuthorized)
        );
        assert_eq!(protection.state(), DesksideState::Inactive);
    }

    #[test]
    fn protection_requires_both_verified_resources() {
        let protection = protected();
        assert_eq!(protection.state(), DesksideState::Protected);
        assert_eq!(
            protection.lease_phase(DesksideControl::LocalInput),
            Some(RestorePhase::Applied)
        );
        assert_eq!(
            protection.lease_phase(DesksideControl::LocalDisplays),
            Some(RestorePhase::Applied)
        );
    }

    #[test]
    fn input_apply_failure_rolls_back_input_only() {
        let mut protection = started();
        let _ = protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput));
        assert_eq!(
            protection.apply(DesksideEvent::ApplyFailed(DesksideControl::LocalInput)),
            DesksideEffect::Restore(DesksideControl::LocalInput)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput)),
            DesksideEffect::ArmRolledBack
        );
        assert_eq!(protection.state(), DesksideState::Inactive);
    }

    #[test]
    fn display_verify_failure_rolls_back_in_reverse_order() {
        let mut protection = started();
        let _ = protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput));
        let _ = protection.apply(DesksideEvent::ApplySucceeded(DesksideControl::LocalInput));
        let _ = protection.apply(DesksideEvent::VerifySucceeded(DesksideControl::LocalInput));
        let _ = protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalDisplays));
        let _ = protection.apply(DesksideEvent::ApplySucceeded(
            DesksideControl::LocalDisplays,
        ));
        assert_eq!(
            protection.apply(DesksideEvent::VerifyFailed(DesksideControl::LocalDisplays)),
            DesksideEffect::Restore(DesksideControl::LocalDisplays)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(
                DesksideControl::LocalDisplays
            )),
            DesksideEffect::Restore(DesksideControl::LocalInput)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput)),
            DesksideEffect::ArmRolledBack
        );
    }

    #[test]
    fn reconnect_holds_without_rearm_and_resume_returns_protected() {
        let mut protection = protected();
        assert_eq!(
            protection.apply(DesksideEvent::TransportLost { resumable: true }),
            DesksideEffect::None
        );
        assert_eq!(protection.state(), DesksideState::ReconnectHeld);
        assert_eq!(
            protection.apply(DesksideEvent::TransportLost { resumable: true }),
            DesksideEffect::None
        );
        assert_eq!(
            protection.apply(DesksideEvent::Reconnected),
            DesksideEffect::None
        );
        assert_eq!(protection.state(), DesksideState::Protected);
    }

    #[test]
    fn expiry_stops_injection_then_restores_display_before_input() {
        let mut protection = protected();
        let _ = protection.apply(DesksideEvent::TransportLost { resumable: true });
        assert_eq!(
            protection.apply(DesksideEvent::ReconnectExpired),
            DesksideEffect::StopRemoteInjection
        );
        assert_eq!(
            protection.apply(DesksideEvent::RemoteInjectionStopped),
            DesksideEffect::Restore(DesksideControl::LocalDisplays)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(
                DesksideControl::LocalDisplays
            )),
            DesksideEffect::Restore(DesksideControl::LocalInput)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput)),
            DesksideEffect::CleanupAuthorized
        );
        assert_eq!(protection.state(), DesksideState::Inactive);
    }

    #[test]
    fn display_restore_failure_still_releases_input_and_preserves_journal() {
        let mut protection = protected();
        let _ = protection.apply(DesksideEvent::BeginDraining);
        let _ = protection.apply(DesksideEvent::RemoteInjectionStopped);
        assert_eq!(
            protection.apply(DesksideEvent::RestoreFailed(DesksideControl::LocalDisplays)),
            DesksideEffect::Restore(DesksideControl::LocalInput)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput)),
            DesksideEffect::PreserveRecoveryJournal
        );
        assert_eq!(protection.state(), DesksideState::RestoreFailed);
    }

    #[test]
    fn failed_restore_retries_only_failed_resources() {
        let mut protection = protected();
        let _ = protection.apply(DesksideEvent::BeginDraining);
        let _ = protection.apply(DesksideEvent::RemoteInjectionStopped);
        let _ = protection.apply(DesksideEvent::RestoreFailed(DesksideControl::LocalDisplays));
        let _ = protection.apply(DesksideEvent::RestoreSucceeded(DesksideControl::LocalInput));
        assert_eq!(
            protection.apply(DesksideEvent::RetryRestore),
            DesksideEffect::Restore(DesksideControl::LocalDisplays)
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(
                DesksideControl::LocalDisplays
            )),
            DesksideEffect::CleanupAuthorized
        );
        assert_eq!(protection.state(), DesksideState::Inactive);
    }

    #[test]
    fn duplicate_and_out_of_order_events_are_noops() {
        let mut protection = started();
        assert_eq!(
            protection.apply(DesksideEvent::ApplySucceeded(DesksideControl::LocalInput)),
            DesksideEffect::None
        );
        let _ = protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput));
        assert_eq!(
            protection.apply(DesksideEvent::ArmSucceeded(DesksideControl::LocalInput)),
            DesksideEffect::None
        );
        assert_eq!(
            protection.apply(DesksideEvent::RestoreSucceeded(
                DesksideControl::LocalDisplays
            )),
            DesksideEffect::None
        );
    }
}
