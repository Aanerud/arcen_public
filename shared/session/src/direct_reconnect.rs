//! Pure direct-transport reconnect lifecycle and single-slot replay contract.

use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use subtle::ConstantTimeEq;

/// Maximum supported reconnect window.
pub const MAX_RECONNECT_WINDOW_SECONDS: u32 = 7_200;
/// Default reconnect window.
///
/// The window exists so a client that loses its network for a moment can pick
/// the same session back up without signing in again. It is not a reservation:
/// a host holds its display authority for the whole window, so until it expires
/// nobody else can start a session, and they are told to "retry after it
/// disconnects" with no indication of how long that is.
///
/// It used to be 20 minutes, which is far longer than any recovery it enables
/// and long enough that a machine several people share reads as broken after
/// one of them closes a laptop lid. Three minutes still covers a Wi-Fi
/// handover, a VPN reconnect or a suspended laptop, and bounds how long a host
/// can be unavailable to everyone else. Administrators who genuinely want the
/// old behaviour can set `auth.reconnect_window_secs` back up to 7200.
pub const DEFAULT_RECONNECT_WINDOW_SECONDS: u32 = 180;

/// Exact monotonic millisecond timestamp supplied by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicMillis(u128);

impl MonotonicMillis {
    /// Wraps a host monotonic timestamp measured in milliseconds.
    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Returns the exact millisecond value.
    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }

    const fn add_seconds(self, seconds: u32) -> Self {
        Self(self.0.saturating_add((seconds as u128) * 1_000))
    }
}

/// Validated reconnect policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    window_secs: u32,
}

impl ReconnectPolicy {
    /// Creates a policy in the inclusive range `0..=7200`.
    ///
    /// # Errors
    ///
    /// Returns an error when the reconnect window exceeds two hours.
    pub const fn new(window_secs: u32) -> Result<Self, ReconnectPolicyError> {
        if window_secs > MAX_RECONNECT_WINDOW_SECONDS {
            return Err(ReconnectPolicyError);
        }
        Ok(Self { window_secs })
    }

    /// Returns the reconnect window in seconds.
    #[must_use]
    pub const fn window_secs(self) -> u32 {
        self.window_secs
    }

    /// Returns whether reconnect is disabled.
    #[must_use]
    pub const fn is_disabled(self) -> bool {
        self.window_secs == 0
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            window_secs: DEFAULT_RECONNECT_WINDOW_SECONDS,
        }
    }
}

/// Reconnect policy validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicyError;

impl Display for ReconnectPolicyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("reconnect window exceeds 7200 seconds")
    }
}

impl Error for ReconnectPolicyError {}

/// Direct-session reconnect phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectState {
    /// Reconnect is disabled while the current transport remains attached.
    Disabled,
    /// Session has an authenticated attached transport.
    Attached,
    /// Transport is absent while the deadline timer remains authoritative.
    Detached {
        /// Exact monotonic deadline in milliseconds.
        deadline: MonotonicMillis,
        /// Generation identifying the authoritative timer.
        timer_generation: u64,
    },
    /// A replacement transport is authenticating before the same deadline.
    Resuming {
        /// Exact monotonic deadline in milliseconds.
        deadline: MonotonicMillis,
        /// Generation identifying the authoritative timer.
        timer_generation: u64,
    },
    /// Terminal cleanup is in progress.
    Draining,
    /// Cleanup completed.
    Closed,
}

/// Event applied to the pure reconnect model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectEvent {
    /// Attached transport was lost unexpectedly.
    UnexpectedLoss,
    /// User or peer explicitly disconnected.
    ExplicitDisconnect,
    /// A replacement transport began resume authentication.
    BeginResume,
    /// Resume authentication and slot rotation succeeded.
    ResumeAccepted,
    /// Resume authentication failed.
    ResumeFailed,
    /// A previously armed timer fired.
    DeadlineReached {
        /// Timer generation carried by the callback.
        timer_generation: u64,
    },
    /// Native desktop/session ended.
    NativeSessionEnded,
    /// Owning host process or worker crashed.
    OwnerCrashed,
    /// All drain actions completed.
    DrainComplete,
}

/// Bounded timer directive emitted by one transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReconnectTimerAction {
    /// No timer work.
    #[default]
    None,
    /// Arm one exact monotonic deadline.
    Arm {
        /// Deadline in monotonic milliseconds.
        deadline: MonotonicMillis,
        /// New timer generation.
        timer_generation: u64,
    },
    /// Cancel the current timer.
    Cancel {
        /// Timer generation to cancel.
        timer_generation: u64,
    },
}

/// Bounded pure side-effect directives for a reconnect transition.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconnectActions {
    /// Keep existing restore leases active through detachment.
    pub hold_restore_leases: bool,
    /// Execute existing restore leases during terminal drain.
    pub restore_leases: bool,
    /// Stop media production/transmission.
    pub stop_media: bool,
    /// Start media after accepted resume.
    pub start_media: bool,
    /// Clear all input ownership and pressed state.
    pub reset_input: bool,
    /// Rotate and issue the next direct-resume grant.
    pub rotate_grant: bool,
    /// Revoke the current direct-resume slot.
    pub revoke_grant: bool,
    /// Close the current or attempted transport.
    pub close_transport: bool,
    /// Arm or cancel the one authoritative deadline timer.
    pub timer: ReconnectTimerAction,
}

/// Pure direct reconnect state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectReconnect {
    policy: ReconnectPolicy,
    state: ReconnectState,
    next_timer_generation: u64,
}

impl DirectReconnect {
    /// Creates an attached model, or a disabled model for a zero window.
    #[must_use]
    pub const fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            state: if policy.is_disabled() {
                ReconnectState::Disabled
            } else {
                ReconnectState::Attached
            },
            next_timer_generation: 1,
        }
    }

    /// Returns current phase.
    #[must_use]
    pub const fn state(self) -> ReconnectState {
        self.state
    }

    /// Applies one event at an injected exact monotonic millisecond.
    ///
    /// Repeated, stale, early, or inapplicable events are explicit no-ops.
    #[must_use]
    pub fn apply(&mut self, event: ReconnectEvent, now: MonotonicMillis) -> ReconnectActions {
        match (self.state, event) {
            (ReconnectState::Attached, ReconnectEvent::UnexpectedLoss) => self.detach(now),
            (ReconnectState::Disabled, ReconnectEvent::UnexpectedLoss) => self.begin_drain(None),
            (
                ReconnectState::Attached
                | ReconnectState::Disabled
                | ReconnectState::Detached { .. }
                | ReconnectState::Resuming { .. },
                ReconnectEvent::ExplicitDisconnect
                | ReconnectEvent::NativeSessionEnded
                | ReconnectEvent::OwnerCrashed,
            ) => self.begin_drain(self.active_timer_generation()),
            (
                ReconnectState::Detached {
                    deadline,
                    timer_generation,
                },
                ReconnectEvent::BeginResume,
            ) if now < deadline => {
                self.state = ReconnectState::Resuming {
                    deadline,
                    timer_generation,
                };
                ReconnectActions::default()
            }
            (
                ReconnectState::Detached {
                    deadline,
                    timer_generation,
                }
                | ReconnectState::Resuming {
                    deadline,
                    timer_generation,
                },
                ReconnectEvent::BeginResume
                | ReconnectEvent::ResumeAccepted
                | ReconnectEvent::ResumeFailed,
            ) if now >= deadline => self.begin_drain(Some(timer_generation)),
            (
                ReconnectState::Resuming {
                    timer_generation, ..
                },
                ReconnectEvent::ResumeAccepted,
            ) => {
                self.state = ReconnectState::Attached;
                ReconnectActions {
                    start_media: true,
                    reset_input: true,
                    rotate_grant: true,
                    timer: ReconnectTimerAction::Cancel { timer_generation },
                    ..ReconnectActions::default()
                }
            }
            (
                ReconnectState::Resuming {
                    deadline,
                    timer_generation,
                },
                ReconnectEvent::ResumeFailed,
            ) => {
                self.state = ReconnectState::Detached {
                    deadline,
                    timer_generation,
                };
                ReconnectActions {
                    reset_input: true,
                    close_transport: true,
                    ..ReconnectActions::default()
                }
            }
            (
                ReconnectState::Detached {
                    deadline,
                    timer_generation: active,
                }
                | ReconnectState::Resuming {
                    deadline,
                    timer_generation: active,
                },
                ReconnectEvent::DeadlineReached { timer_generation },
            ) if timer_generation == active && now >= deadline => self.begin_drain(Some(active)),
            (ReconnectState::Draining, ReconnectEvent::DrainComplete) => {
                self.state = ReconnectState::Closed;
                ReconnectActions::default()
            }
            _ => ReconnectActions::default(),
        }
    }

    fn detach(&mut self, now: MonotonicMillis) -> ReconnectActions {
        let timer_generation = self.next_timer_generation;
        self.next_timer_generation = self.next_timer_generation.saturating_add(1);
        let deadline = now.add_seconds(self.policy.window_secs());
        self.state = ReconnectState::Detached {
            deadline,
            timer_generation,
        };
        ReconnectActions {
            hold_restore_leases: true,
            stop_media: true,
            reset_input: true,
            close_transport: true,
            timer: ReconnectTimerAction::Arm {
                deadline,
                timer_generation,
            },
            ..ReconnectActions::default()
        }
    }

    fn begin_drain(&mut self, timer_generation: Option<u64>) -> ReconnectActions {
        self.state = ReconnectState::Draining;
        ReconnectActions {
            restore_leases: true,
            stop_media: true,
            reset_input: true,
            revoke_grant: true,
            close_transport: true,
            timer: timer_generation.map_or(ReconnectTimerAction::None, |timer_generation| {
                ReconnectTimerAction::Cancel { timer_generation }
            }),
            ..ReconnectActions::default()
        }
    }

    const fn active_timer_generation(self) -> Option<u64> {
        match self.state {
            ReconnectState::Detached {
                timer_generation, ..
            }
            | ReconnectState::Resuming {
                timer_generation, ..
            } => Some(timer_generation),
            ReconnectState::Disabled
            | ReconnectState::Attached
            | ReconnectState::Draining
            | ReconnectState::Closed => None,
        }
    }
}

/// Current direct-resume grant slot protected by a host synchronization primitive.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DirectResumeSlot {
    generation: u64,
    nonce: [u8; 32],
    active: bool,
}

impl Debug for DirectResumeSlot {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DirectResumeSlot")
            .field("generation", &self.generation)
            .field("nonce", &"<redacted>")
            .field("active", &self.active)
            .finish()
    }
}

impl DirectResumeSlot {
    /// Creates an active current slot.
    #[must_use]
    pub const fn new(generation: u64, nonce: [u8; 32]) -> Self {
        Self {
            generation,
            nonce,
            active: true,
        }
    }

    /// Returns the current generation and nonce when active.
    #[must_use]
    pub const fn current(self) -> Option<(u64, [u8; 32])> {
        if self.active {
            Some((self.generation, self.nonce))
        } else {
            None
        }
    }

    /// Atomically compares and rotates when called under host synchronization.
    ///
    /// Only the exact current generation and nonce can win. Every stale,
    /// duplicated, or revoked attempt reports [`DirectResumeSlotResult::Replayed`].
    pub fn compare_and_rotate(
        &mut self,
        expected_generation: u64,
        expected_nonce: &[u8; 32],
        next_nonce: [u8; 32],
    ) -> DirectResumeSlotResult {
        if !self.active
            || self.generation != expected_generation
            || !bool::from(self.nonce.ct_eq(expected_nonce))
        {
            return DirectResumeSlotResult::Replayed;
        }
        let Some(next_generation) = self.generation.checked_add(1) else {
            self.active = false;
            return DirectResumeSlotResult::GenerationExhausted;
        };
        self.generation = next_generation;
        self.nonce = next_nonce;
        DirectResumeSlotResult::Rotated {
            generation: next_generation,
            nonce: next_nonce,
        }
    }

    /// Revokes the current slot. Repeated revocation is a no-op.
    pub fn revoke(&mut self) {
        self.active = false;
        self.nonce.fill(0);
    }
}

/// Compare-and-rotate result.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DirectResumeSlotResult {
    /// The exact current slot won and was replaced.
    Rotated {
        /// New generation.
        generation: u64,
        /// New random nonce.
        nonce: [u8; 32],
    },
    /// Attempt was stale, duplicated, mismatched, or revoked.
    Replayed,
    /// The generation counter could not safely advance and the slot was revoked.
    GenerationExhausted,
}

impl Debug for DirectResumeSlotResult {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rotated { generation, .. } => formatter
                .debug_struct("Rotated")
                .field("generation", generation)
                .field("nonce", &"<redacted>")
                .finish(),
            Self::Replayed => formatter.write_str("Replayed"),
            Self::GenerationExhausted => formatter.write_str("GenerationExhausted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: MonotonicMillis = MonotonicMillis::new(10_000);

    #[test]
    fn policy_is_bounded_and_defaults_to_twenty_minutes() {
        assert_eq!(ReconnectPolicy::default().window_secs(), 180);
        assert_eq!(ReconnectPolicy::new(0).expect("zero").window_secs(), 0);
        assert_eq!(
            ReconnectPolicy::new(MAX_RECONNECT_WINDOW_SECONDS)
                .expect("boundary")
                .window_secs(),
            MAX_RECONNECT_WINDOW_SECONDS
        );
        assert_eq!(
            ReconnectPolicy::new(MAX_RECONNECT_WINDOW_SECONDS + 1),
            Err(ReconnectPolicyError)
        );
    }

    #[test]
    fn unexpected_loss_holds_leases_stops_media_and_arms_exact_deadline() {
        let mut model = DirectReconnect::new(ReconnectPolicy::new(20).expect("policy"));
        let actions = model.apply(ReconnectEvent::UnexpectedLoss, T0);
        let deadline = MonotonicMillis::new(30_000);
        assert_eq!(
            model.state(),
            ReconnectState::Detached {
                deadline,
                timer_generation: 1
            }
        );
        assert!(actions.hold_restore_leases);
        assert!(actions.stop_media);
        assert!(actions.reset_input);
        assert!(!actions.rotate_grant);
        assert_eq!(
            actions.timer,
            ReconnectTimerAction::Arm {
                deadline,
                timer_generation: 1
            }
        );
    }

    #[test]
    fn exact_deadline_drains_but_one_millisecond_before_can_resume() {
        let mut before = DirectReconnect::new(ReconnectPolicy::new(1).expect("policy"));
        let _ = before.apply(ReconnectEvent::UnexpectedLoss, T0);
        assert_eq!(
            before.apply(ReconnectEvent::BeginResume, MonotonicMillis::new(10_999)),
            ReconnectActions::default()
        );
        assert!(matches!(before.state(), ReconnectState::Resuming { .. }));

        let mut boundary = DirectReconnect::new(ReconnectPolicy::new(1).expect("policy"));
        let _ = boundary.apply(ReconnectEvent::UnexpectedLoss, T0);
        let actions = boundary.apply(ReconnectEvent::BeginResume, MonotonicMillis::new(11_000));
        assert_eq!(boundary.state(), ReconnectState::Draining);
        assert!(actions.restore_leases);
        assert!(actions.revoke_grant);
        assert!(!actions.rotate_grant);
    }

    #[test]
    fn stale_and_early_timers_are_noops() {
        let mut model = DirectReconnect::new(ReconnectPolicy::new(1).expect("policy"));
        let _ = model.apply(ReconnectEvent::UnexpectedLoss, T0);
        assert_eq!(
            model.apply(
                ReconnectEvent::DeadlineReached {
                    timer_generation: 9
                },
                MonotonicMillis::new(20_000)
            ),
            ReconnectActions::default()
        );
        assert_eq!(
            model.apply(
                ReconnectEvent::DeadlineReached {
                    timer_generation: 1
                },
                MonotonicMillis::new(10_999)
            ),
            ReconnectActions::default()
        );
        assert!(matches!(model.state(), ReconnectState::Detached { .. }));
    }

    #[test]
    fn failed_resume_returns_detached_and_keeps_authoritative_timer() {
        let mut model = DirectReconnect::new(ReconnectPolicy::new(5).expect("policy"));
        let _ = model.apply(ReconnectEvent::UnexpectedLoss, T0);
        let _ = model.apply(ReconnectEvent::BeginResume, MonotonicMillis::new(11_000));
        let actions = model.apply(ReconnectEvent::ResumeFailed, MonotonicMillis::new(12_000));
        assert!(actions.close_transport);
        assert_eq!(actions.timer, ReconnectTimerAction::None);
        assert_eq!(
            model.state(),
            ReconnectState::Detached {
                deadline: MonotonicMillis::new(15_000),
                timer_generation: 1
            }
        );
    }

    #[test]
    fn accepted_resume_restarts_media_resets_input_and_rotates() {
        let mut model = DirectReconnect::new(ReconnectPolicy::new(5).expect("policy"));
        let _ = model.apply(ReconnectEvent::UnexpectedLoss, T0);
        let _ = model.apply(ReconnectEvent::BeginResume, MonotonicMillis::new(11_000));
        let actions = model.apply(ReconnectEvent::ResumeAccepted, MonotonicMillis::new(12_000));
        assert_eq!(model.state(), ReconnectState::Attached);
        assert!(actions.start_media);
        assert!(actions.reset_input);
        assert!(actions.rotate_grant);
        assert_eq!(
            actions.timer,
            ReconnectTimerAction::Cancel {
                timer_generation: 1
            }
        );
    }

    #[test]
    fn zero_window_immediately_drains_and_never_rotates() {
        let mut model = DirectReconnect::new(ReconnectPolicy::new(0).expect("policy"));
        assert_eq!(model.state(), ReconnectState::Disabled);
        let actions = model.apply(ReconnectEvent::UnexpectedLoss, T0);
        assert_eq!(model.state(), ReconnectState::Draining);
        assert!(actions.restore_leases);
        assert!(actions.revoke_grant);
        assert!(!actions.rotate_grant);
        assert_eq!(actions.timer, ReconnectTimerAction::None);
    }

    #[test]
    fn repeated_terminal_events_and_drain_completion_are_idempotent() {
        let mut model = DirectReconnect::new(ReconnectPolicy::default());
        let first = model.apply(ReconnectEvent::ExplicitDisconnect, T0);
        assert!(first.restore_leases);
        assert_eq!(
            model.apply(ReconnectEvent::ExplicitDisconnect, T0),
            ReconnectActions::default()
        );
        assert_eq!(
            model.apply(ReconnectEvent::DrainComplete, T0),
            ReconnectActions::default()
        );
        assert_eq!(model.state(), ReconnectState::Closed);
        assert_eq!(
            model.apply(ReconnectEvent::DrainComplete, T0),
            ReconnectActions::default()
        );
    }

    #[test]
    fn every_terminal_cause_enters_the_same_idempotent_drain() {
        for event in [
            ReconnectEvent::ExplicitDisconnect,
            ReconnectEvent::NativeSessionEnded,
            ReconnectEvent::OwnerCrashed,
        ] {
            let mut model = DirectReconnect::new(ReconnectPolicy::default());
            let actions = model.apply(event, T0);
            assert_eq!(model.state(), ReconnectState::Draining);
            assert!(actions.restore_leases);
            assert!(actions.stop_media);
            assert!(actions.reset_input);
            assert!(actions.revoke_grant);
            assert!(actions.close_transport);
        }
    }

    #[test]
    fn policy_cutoff_drains_detached_or_resuming_state_and_cancels_exact_timer() {
        for begin_resume in [false, true] {
            let mut model = DirectReconnect::new(ReconnectPolicy::new(60).unwrap());
            let detached = model.apply(ReconnectEvent::UnexpectedLoss, T0);
            let ReconnectTimerAction::Arm {
                timer_generation, ..
            } = detached.timer
            else {
                panic!("detach must arm one timer");
            };
            if begin_resume {
                let _ = model.apply(
                    ReconnectEvent::BeginResume,
                    MonotonicMillis::new(T0.get() + 1),
                );
                assert!(matches!(model.state(), ReconnectState::Resuming { .. }));
            } else {
                assert!(matches!(model.state(), ReconnectState::Detached { .. }));
            }

            let cutoff = model.apply(
                ReconnectEvent::ExplicitDisconnect,
                MonotonicMillis::new(T0.get() + 2),
            );
            assert_eq!(model.state(), ReconnectState::Draining);
            assert!(cutoff.restore_leases);
            assert!(cutoff.revoke_grant);
            assert_eq!(
                cutoff.timer,
                ReconnectTimerAction::Cancel { timer_generation }
            );
            assert_eq!(
                model.apply(
                    ReconnectEvent::DeadlineReached { timer_generation },
                    MonotonicMillis::new(T0.get() + 60_000),
                ),
                ReconnectActions::default()
            );
        }
    }

    #[test]
    fn current_slot_allows_only_one_exact_winner_and_reports_replay() {
        let mut slot = DirectResumeSlot::new(7, [1; 32]);
        assert_eq!(
            slot.compare_and_rotate(7, &[1; 32], [2; 32]),
            DirectResumeSlotResult::Rotated {
                generation: 8,
                nonce: [2; 32]
            }
        );
        assert_eq!(
            slot.compare_and_rotate(7, &[1; 32], [3; 32]),
            DirectResumeSlotResult::Replayed
        );
        assert_eq!(
            slot.compare_and_rotate(8, &[9; 32], [3; 32]),
            DirectResumeSlotResult::Replayed
        );
        slot.revoke();
        slot.revoke();
        assert_eq!(slot.current(), None);
        assert_eq!(
            slot.compare_and_rotate(8, &[2; 32], [3; 32]),
            DirectResumeSlotResult::Replayed
        );
    }
}
