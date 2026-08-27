use std::fmt;
use std::time::{Duration, Instant};

use zeroize::Zeroize;

const FIRST_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const MAX_RECONNECT_WINDOW: Duration = Duration::from_secs(7_200);

pub trait MonotonicClock {
    fn now(&self) -> Duration;
}

#[derive(Debug, Clone)]
pub struct SystemClock {
    origin: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl SystemClock {
    pub fn observed_at(&self, observed_at: Instant) -> Duration {
        observed_at
            .checked_duration_since(self.origin)
            .unwrap_or(Duration::ZERO)
    }

    pub fn instant_at(&self, timestamp: Duration) -> Option<Instant> {
        self.origin.checked_add(timestamp)
    }
}

impl MonotonicClock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

pub trait JitterSource {
    fn inclusive(&mut self, minimum: Duration, maximum: Duration) -> Option<Duration>;
}

#[derive(Debug, Default)]
pub struct SystemJitter;

impl JitterSource for SystemJitter {
    fn inclusive(&mut self, minimum: Duration, maximum: Duration) -> Option<Duration> {
        let minimum_ms = u64::try_from(minimum.as_millis()).ok()?;
        let maximum_ms = u64::try_from(maximum.as_millis()).ok()?;
        let width = maximum_ms.checked_sub(minimum_ms)?.checked_add(1)?;
        let mut random = [0_u8; 8];
        getrandom::getrandom(&mut random).ok()?;
        Some(Duration::from_millis(
            minimum_ms + u64::from_le_bytes(random) % width,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionIdentity {
    pub endpoint: String,
    pub security: String,
    pub topology: String,
}

pub struct ResumeCredential {
    grant: String,
    holder_nonce: String,
    pub window: Duration,
    pub identity: ConnectionIdentity,
    pub previous_sid: String,
}

impl fmt::Debug for ResumeCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeCredential")
            .field("grant", &"<redacted>")
            .field("holder_nonce", &"<redacted>")
            .field("window", &self.window)
            .field("identity", &self.identity)
            .field("previous_sid", &self.previous_sid)
            .finish()
    }
}

impl Drop for ResumeCredential {
    fn drop(&mut self) {
        self.grant.zeroize();
        self.holder_nonce.zeroize();
        self.previous_sid.zeroize();
    }
}

impl ResumeCredential {
    fn new(
        mut grant: String,
        holder_nonce: String,
        window: Duration,
        identity: ConnectionIdentity,
        previous_sid: String,
    ) -> Option<Self> {
        if grant.is_empty() || window.is_zero() {
            grant.zeroize();
            return None;
        }
        Some(Self {
            grant,
            holder_nonce,
            window,
            identity,
            previous_sid,
        })
    }

    fn rotate(
        &mut self,
        mut successor_grant: String,
        window: Duration,
        previous_sid: String,
    ) -> bool {
        if successor_grant.is_empty() || window.is_zero() {
            successor_grant.zeroize();
            return false;
        }
        self.grant.zeroize();
        self.grant = successor_grant;
        self.previous_sid.zeroize();
        self.previous_sid = previous_sid;
        self.window = window;
        true
    }

    #[cfg(test)]
    fn grant_for_test(&self) -> &str {
        &self.grant
    }
}

pub struct ResumeAttempt {
    pub holder_nonce: String,
    pub grant: String,
    pub previous_sid: String,
    pub generation: u64,
    pub identity: ConnectionIdentity,
    pub attempt: u32,
    pub gap: Duration,
}

impl fmt::Debug for ResumeAttempt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeAttempt")
            .field("holder_nonce", &"<redacted>")
            .field("grant", &"<redacted>")
            .field("previous_sid", &self.previous_sid)
            .field("generation", &self.generation)
            .field("identity", &self.identity)
            .field("attempt", &self.attempt)
            .field("gap", &self.gap)
            .finish()
    }
}

impl Drop for ResumeAttempt {
    fn drop(&mut self) {
        self.holder_nonce.zeroize();
        self.grant.zeroize();
        self.previous_sid.zeroize();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectPhase {
    Connected,
    Reconnecting {
        attempt: u32,
        next_retry: Duration,
        deadline: Duration,
        last_error: String,
    },
    Resuming {
        attempt: u32,
        deadline: Duration,
        last_error: String,
    },
    Cancelled,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialUpdate {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerError {
    UnexpectedAuthentication,
    MissingSuccessorGrant,
    InvalidWindow,
    Expired,
    RandomnessUnavailable,
    GenerationMismatch,
}

pub struct ReconnectController {
    generation: u64,
    identity: ConnectionIdentity,
    holder_nonce: String,
    credential: Option<ResumeCredential>,
    detached_at: Option<Duration>,
    detach_deadline: Option<Duration>,
    phase: ReconnectPhase,
    next_attempt: u32,
}

impl fmt::Debug for ReconnectController {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReconnectController")
            .field("generation", &self.generation)
            .field("identity", &self.identity)
            .field("holder_nonce", &"<redacted>")
            .field("credential", &self.credential)
            .field("detach_deadline", &self.detach_deadline)
            .field("phase", &self.phase)
            .field("next_attempt", &self.next_attempt)
            .finish()
    }
}

impl Drop for ReconnectController {
    fn drop(&mut self) {
        self.holder_nonce.zeroize();
    }
}

impl ReconnectController {
    pub fn new(generation: u64, identity: ConnectionIdentity, holder_nonce: String) -> Self {
        Self {
            generation,
            identity,
            holder_nonce,
            credential: None,
            detached_at: None,
            detach_deadline: None,
            phase: ReconnectPhase::Connected,
            next_attempt: 1,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn identity(&self) -> &ConnectionIdentity {
        &self.identity
    }

    pub fn holder_nonce(&self) -> &str {
        &self.holder_nonce
    }

    pub fn phase(&self) -> &ReconnectPhase {
        &self.phase
    }

    pub fn has_credential(&self) -> bool {
        self.credential.is_some()
    }

    pub fn accept_initial(
        &mut self,
        mut grant: Option<String>,
        window: Option<Duration>,
        resumed: bool,
        sid: String,
    ) -> Result<CredentialUpdate, ControllerError> {
        if resumed {
            if let Some(grant) = grant.as_mut() {
                grant.zeroize();
            }
            return Err(ControllerError::UnexpectedAuthentication);
        }
        let update = match (grant.take(), window) {
            (Some(grant), Some(window))
                if !grant.is_empty() && !window.is_zero() && window <= MAX_RECONNECT_WINDOW =>
            {
                self.credential = ResumeCredential::new(
                    grant,
                    self.holder_nonce.clone(),
                    window,
                    self.identity.clone(),
                    sid,
                );
                CredentialUpdate::Enabled
            }
            (Some(mut grant), Some(window)) if window.is_zero() => {
                grant.zeroize();
                self.credential = None;
                self.detach_deadline = None;
                CredentialUpdate::Disabled
            }
            (None, None | Some(Duration::ZERO)) => {
                self.credential = None;
                self.detach_deadline = None;
                CredentialUpdate::Disabled
            }
            (Some(mut grant), None) => {
                grant.zeroize();
                return Err(ControllerError::UnexpectedAuthentication);
            }
            (None, Some(_)) => return Err(ControllerError::UnexpectedAuthentication),
            (Some(mut grant), Some(_)) => {
                grant.zeroize();
                return Err(ControllerError::InvalidWindow);
            }
        };
        self.detach_deadline = None;
        self.detached_at = None;
        self.phase = ReconnectPhase::Connected;
        self.next_attempt = 1;
        Ok(update)
    }

    pub fn accept_resume(
        &mut self,
        mut grant: Option<String>,
        window: Option<Duration>,
        resumed: bool,
        sid: String,
        now: Duration,
    ) -> Result<(), ControllerError> {
        if self.expire(now) {
            if let Some(grant) = grant.as_mut() {
                grant.zeroize();
            }
            return Err(ControllerError::Expired);
        }
        if !resumed {
            if let Some(grant) = grant.as_mut() {
                grant.zeroize();
            }
            return Err(ControllerError::UnexpectedAuthentication);
        }
        let (grant, window) = match (grant.take(), window) {
            (Some(grant), Some(window))
                if !grant.is_empty() && !window.is_zero() && window <= MAX_RECONNECT_WINDOW =>
            {
                (grant, window)
            }
            (Some(mut grant), _) => {
                grant.zeroize();
                return Err(ControllerError::MissingSuccessorGrant);
            }
            (None, _) => return Err(ControllerError::MissingSuccessorGrant),
        };
        let Some(credential) = self.credential.as_mut() else {
            let mut grant = grant;
            grant.zeroize();
            return Err(ControllerError::UnexpectedAuthentication);
        };
        if !credential.rotate(grant, window, sid) {
            return Err(ControllerError::MissingSuccessorGrant);
        }
        self.detach_deadline = None;
        self.detached_at = None;
        self.phase = ReconnectPhase::Connected;
        self.next_attempt = 1;
        Ok(())
    }

    pub fn accept_refresh(
        &mut self,
        mut grant: Option<String>,
        window: Option<Duration>,
        resumed: bool,
        sid: String,
    ) -> Result<(), ControllerError> {
        if resumed || self.phase != ReconnectPhase::Connected {
            if let Some(grant) = grant.as_mut() {
                grant.zeroize();
            }
            return Err(ControllerError::UnexpectedAuthentication);
        }
        let (grant, window) = match (grant.take(), window) {
            (Some(grant), Some(window))
                if !grant.is_empty() && !window.is_zero() && window <= MAX_RECONNECT_WINDOW =>
            {
                (grant, window)
            }
            (Some(mut grant), Some(window)) if window > MAX_RECONNECT_WINDOW => {
                grant.zeroize();
                return Err(ControllerError::InvalidWindow);
            }
            (Some(mut grant), _) => {
                grant.zeroize();
                return Err(ControllerError::MissingSuccessorGrant);
            }
            (None, _) => return Err(ControllerError::MissingSuccessorGrant),
        };
        let Some(credential) = self.credential.as_mut() else {
            let mut grant = grant;
            grant.zeroize();
            return Err(ControllerError::UnexpectedAuthentication);
        };
        if !credential.rotate(grant, window, sid) {
            return Err(ControllerError::MissingSuccessorGrant);
        }
        self.detach_deadline = None;
        self.next_attempt = 1;
        Ok(())
    }

    pub fn schedule<J: JitterSource>(
        &mut self,
        last_error: String,
        now: Duration,
        jitter: &mut J,
    ) -> Result<(), ControllerError> {
        let Some(window) = self.credential.as_ref().map(|credential| credential.window) else {
            self.phase = ReconnectPhase::Terminal;
            return Err(ControllerError::Expired);
        };
        if self.detached_at.is_none() {
            self.detached_at = Some(now);
        }
        let deadline = match self.detach_deadline {
            Some(deadline) => deadline,
            None => {
                let Some(deadline) = now.checked_add(window) else {
                    self.terminal();
                    return Err(ControllerError::Expired);
                };
                self.detach_deadline = Some(deadline);
                deadline
            }
        };
        if now >= deadline {
            self.terminal();
            return Err(ControllerError::Expired);
        }
        let attempt = self.next_attempt;
        let base = backoff_base(attempt);
        let delay = jitter
            .inclusive(base / 2, base)
            .ok_or(ControllerError::RandomnessUnavailable)?;
        let next_retry = now.saturating_add(delay);
        if next_retry >= deadline {
            self.terminal();
            return Err(ControllerError::Expired);
        }
        self.phase = ReconnectPhase::Reconnecting {
            attempt,
            next_retry,
            deadline,
            last_error,
        };
        self.next_attempt = self.next_attempt.saturating_add(1);
        Ok(())
    }

    pub fn retry_now(&mut self, now: Duration) -> Result<(), ControllerError> {
        match &mut self.phase {
            ReconnectPhase::Reconnecting {
                next_retry,
                deadline,
                ..
            } if now < *deadline => {
                *next_retry = now;
                Ok(())
            }
            _ => Err(ControllerError::Expired),
        }
    }

    pub fn take_due(
        &mut self,
        now: Duration,
        generation: u64,
        identity: &ConnectionIdentity,
    ) -> Result<Option<ResumeAttempt>, ControllerError> {
        // Live blocker fix: only the `Reconnecting` phase ever considers
        // issuing a resume attempt, so only it may ever fail closed on a
        // generation/identity mismatch. `drive_reconnect` calls this every
        // frame with the *live* identity (display topology, UI scale,
        // notch setting, ...) recomputed fresh each time, for as long as
        // `self.reconnect` is `Some` -- which starts immediately once a
        // fresh, still `Connected` (never yet disconnected) auto-reconnect-
        // eligible session begins, not merely once a real reconnect attempt
        // is underway. Checking the mismatch before confirming the phase
        // let ordinary live drift (a display plugged in/out, a settings
        // change) while still `Connected`/`Resuming`/`Cancelled`/`Terminal`
        // wrongly `terminal()`-ize and drop a perfectly healthy session that
        // never actually disconnected. Active topology changes belong to
        // the multi-window session path (teardown/reconnect handled there),
        // never this controller. Once genuinely `Reconnecting` -- a real
        // disconnect already happened and this controller is trying to
        // resume -- the exact same fail-closed check still applies,
        // immediately below and still strictly before ever issuing a
        // resume.
        let (attempt, deadline, last_error, due) = match &self.phase {
            ReconnectPhase::Reconnecting {
                attempt,
                next_retry,
                deadline,
                last_error,
            } => (*attempt, *deadline, last_error.clone(), now >= *next_retry),
            _ => return Ok(None),
        };
        if generation != self.generation || identity != &self.identity {
            self.terminal();
            return Err(ControllerError::GenerationMismatch);
        }
        if !due {
            return Ok(None);
        }
        if now >= deadline {
            self.terminal();
            return Err(ControllerError::Expired);
        }
        let Some(detached_at) = self.detached_at else {
            self.terminal();
            return Err(ControllerError::Expired);
        };
        let credential = self.credential.as_ref().ok_or(ControllerError::Expired)?;
        if credential.identity != self.identity {
            self.terminal();
            return Err(ControllerError::GenerationMismatch);
        }
        let resume = ResumeAttempt {
            holder_nonce: credential.holder_nonce.clone(),
            grant: credential.grant.clone(),
            previous_sid: credential.previous_sid.clone(),
            generation: self.generation,
            identity: self.identity.clone(),
            attempt,
            gap: now.saturating_sub(detached_at),
        };
        self.phase = ReconnectPhase::Resuming {
            attempt,
            deadline,
            last_error,
        };
        Ok(Some(resume))
    }

    pub fn resume_budget(&self, now: Duration) -> Result<Duration, ControllerError> {
        let ReconnectPhase::Resuming { deadline, .. } = self.phase else {
            return Err(ControllerError::Expired);
        };
        let remaining = deadline.saturating_sub(now);
        if remaining.is_zero() {
            Err(ControllerError::Expired)
        } else {
            Ok(remaining)
        }
    }

    pub fn resume_deadline(&self) -> Result<Duration, ControllerError> {
        match self.phase {
            ReconnectPhase::Resuming { deadline, .. } => Ok(deadline),
            _ => Err(ControllerError::Expired),
        }
    }

    pub fn expire(&mut self, now: Duration) -> bool {
        let deadline = match self.phase {
            ReconnectPhase::Reconnecting { deadline, .. }
            | ReconnectPhase::Resuming { deadline, .. } => deadline,
            ReconnectPhase::Connected | ReconnectPhase::Cancelled | ReconnectPhase::Terminal => {
                return false;
            }
        };
        if now < deadline {
            return false;
        }
        self.terminal();
        true
    }

    pub fn manual_cancel(&mut self) {
        self.credential = None;
        self.detached_at = None;
        self.detach_deadline = None;
        self.holder_nonce.zeroize();
        self.phase = ReconnectPhase::Cancelled;
    }

    pub fn terminal(&mut self) {
        self.credential = None;
        self.detached_at = None;
        self.detach_deadline = None;
        self.holder_nonce.zeroize();
        self.phase = ReconnectPhase::Terminal;
    }
}

pub fn fresh_holder_nonce() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(encoded, "{byte:02x}");
    }
    bytes.zeroize();
    Ok(encoded)
}

fn backoff_base(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(63);
    FIRST_BACKOFF
        .checked_mul(1_u32.checked_shl(exponent).unwrap_or(u32::MAX))
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Default)]
    struct TestClock(Cell<Duration>);

    impl TestClock {
        fn set(&self, now: Duration) {
            self.0.set(now);
        }
    }

    impl MonotonicClock for TestClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    struct EdgeJitter {
        maximum: bool,
    }

    impl JitterSource for EdgeJitter {
        fn inclusive(&mut self, minimum: Duration, maximum: Duration) -> Option<Duration> {
            Some(if self.maximum { maximum } else { minimum })
        }
    }

    fn identity(name: &str) -> ConnectionIdentity {
        ConnectionIdentity {
            endpoint: format!("quic://{name}:18444"),
            security: "medium:pin".to_string(),
            topology: "direct:match_layout".to_string(),
        }
    }

    fn controller(window: Duration) -> (ReconnectController, TestClock) {
        let clock = TestClock::default();
        let mut controller = ReconnectController::new(7, identity("pier"), "nonce".to_string());
        controller
            .accept_initial(
                Some("grant-1".to_string()),
                Some(window),
                false,
                "sid-1".to_string(),
            )
            .unwrap();
        (controller, clock)
    }

    #[test]
    fn exact_backoff_and_equal_jitter_edges() {
        let (mut controller, clock) = controller(Duration::from_secs(60));
        let bases_ms = [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000];
        for (index, base_ms) in bases_ms.into_iter().enumerate() {
            let maximum = index % 2 == 1;
            let mut jitter = EdgeJitter { maximum };
            controller
                .schedule(format!("failure {index}"), clock.now(), &mut jitter)
                .unwrap();
            let expected = if maximum { base_ms } else { base_ms / 2 };
            let ReconnectPhase::Reconnecting { next_retry, .. } = controller.phase() else {
                panic!("expected retry");
            };
            assert_eq!(*next_retry, clock.now() + Duration::from_millis(expected));
            clock.set(*next_retry);
            assert!(controller
                .take_due(clock.now(), 7, &identity("pier"))
                .unwrap()
                .is_some());
        }
    }

    #[test]
    fn resume_attempts_preserve_counter_and_disconnect_gap() {
        let (mut controller, clock) = controller(Duration::from_secs(60));
        let mut jitter = EdgeJitter { maximum: false };
        clock.set(Duration::from_secs(10));
        controller
            .schedule("connection reset".to_string(), clock.now(), &mut jitter)
            .unwrap();

        clock.set(Duration::from_millis(10_125));
        let first = controller
            .take_due(clock.now(), 7, &identity("pier"))
            .unwrap()
            .unwrap();
        assert_eq!(first.attempt, 1);
        assert_eq!(first.gap, Duration::from_millis(125));

        clock.set(Duration::from_millis(10_200));
        controller
            .schedule(
                "resume transport failed".to_string(),
                clock.now(),
                &mut jitter,
            )
            .unwrap();
        clock.set(Duration::from_millis(10_450));
        let second = controller
            .take_due(clock.now(), 7, &identity("pier"))
            .unwrap()
            .unwrap();
        assert_eq!(second.attempt, 2);
        assert_eq!(second.gap, Duration::from_millis(450));
    }

    #[test]
    fn successful_resume_resets_attempt_and_starts_a_fresh_gap() {
        let (mut controller, clock) = controller(Duration::from_secs(60));
        let mut jitter = EdgeJitter { maximum: false };
        clock.set(Duration::from_secs(10));
        controller
            .schedule("connection reset".to_string(), clock.now(), &mut jitter)
            .unwrap();
        clock.set(Duration::from_millis(10_125));
        let first = controller
            .take_due(clock.now(), 7, &identity("pier"))
            .unwrap()
            .unwrap();
        assert_eq!(first.attempt, 1);
        controller
            .accept_resume(
                Some("grant-2".to_string()),
                Some(Duration::from_secs(60)),
                true,
                "sid-2".to_string(),
                clock.now(),
            )
            .unwrap();
        assert_eq!(controller.detached_at, None);

        clock.set(Duration::from_secs(20));
        controller
            .schedule("new connection reset".to_string(), clock.now(), &mut jitter)
            .unwrap();
        clock.set(Duration::from_millis(20_125));
        let after_success = controller
            .take_due(clock.now(), 7, &identity("pier"))
            .unwrap()
            .unwrap();
        assert_eq!(after_success.attempt, 1);
        assert_eq!(after_success.gap, Duration::from_millis(125));
    }

    #[test]
    fn deadline_prevents_late_retry() {
        let (mut controller, clock) = controller(Duration::from_millis(200));
        assert_eq!(controller.detach_deadline, None);
        let mut jitter = EdgeJitter { maximum: false };
        assert_eq!(
            controller.schedule("reset".to_string(), clock.now(), &mut jitter),
            Ok(())
        );
        clock.set(Duration::from_millis(200));
        assert!(matches!(
            controller.take_due(clock.now(), 7, &identity("pier")),
            Err(ControllerError::Expired)
        ));
        assert!(!controller.has_credential());
    }

    #[test]
    fn manual_cancel_clears_credential_and_timer() {
        let (mut controller, _) = controller(Duration::from_secs(10));
        controller.manual_cancel();
        assert_eq!(controller.phase(), &ReconnectPhase::Cancelled);
        assert!(!controller.has_credential());
        assert!(controller.holder_nonce().is_empty());
    }

    /// Live blocker regression: `drive_reconnect` (`app.rs`) calls
    /// `take_due` on *every frame* for as long as `self.reconnect` is
    /// `Some` -- which starts immediately once a fresh, auto-reconnect-
    /// eligible session connects, long before any real disconnect -- and
    /// always passes a freshly recomputed *live* identity (display
    /// topology, UI scale, notch setting, ...). A controller that is still
    /// `Connected` (nothing has actually disconnected) must never be
    /// terminalized by ordinary live drift in that identity: a display
    /// hotplug, a settings change, or any other topology change while
    /// attached is exclusively the multi-window session path's job (see
    /// `multi_window_session.rs`'s own teardown tests), never this
    /// controller's. This also covers "initial Deck start with live-vs-
    /// baseline display drift does not drop the session" and "active
    /// topology changes are handled by the multi-window session path, not
    /// the reconnect controller" -- both are direct consequences of the
    /// same guarantee proven here.
    #[test]
    fn take_due_ignores_a_live_identity_mismatch_while_connected_and_never_terminalizes() {
        let (mut controller, clock) = controller(Duration::from_secs(60));
        assert_eq!(controller.phase(), &ReconnectPhase::Connected);
        assert!(controller.has_credential());

        assert!(controller
            .take_due(clock.now(), 99, &identity("some-other-host"))
            .unwrap()
            .is_none());

        assert_eq!(controller.phase(), &ReconnectPhase::Connected);
        assert!(
            controller.has_credential(),
            "a live mismatch while merely Connected must not drop the resume credential"
        );
    }

    /// Companion to the test above: once a real disconnect actually put the
    /// controller into `Reconnecting`, the exact same generation/identity
    /// fail-closed check must still apply, strictly before ever issuing a
    /// resume.
    #[test]
    fn config_replacement_generation_and_identity_are_terminal() {
        let (mut controller, clock) = controller(Duration::from_secs(10));
        let mut jitter = EdgeJitter { maximum: false };
        controller
            .schedule("timeout".to_string(), clock.now(), &mut jitter)
            .unwrap();
        assert!(matches!(
            controller.phase(),
            ReconnectPhase::Reconnecting { .. }
        ));
        assert!(matches!(
            controller.take_due(Duration::from_millis(125), 8, &identity("new-pier")),
            Err(ControllerError::GenerationMismatch)
        ));
        assert_eq!(controller.phase(), &ReconnectPhase::Terminal);
        assert!(!controller.has_credential());
    }

    #[test]
    fn old_host_without_grant_disables_reconnect() {
        let mut controller = ReconnectController::new(1, identity("old"), "nonce".to_string());
        assert_eq!(
            controller.accept_initial(None, None, false, "sid".to_string()),
            Ok(CredentialUpdate::Disabled)
        );
        assert!(!controller.has_credential());
    }

    #[test]
    fn host_window_above_two_hours_is_rejected() {
        let mut controller = ReconnectController::new(1, identity("pier"), "nonce".to_string());
        assert_eq!(
            controller.accept_initial(
                Some("grant".to_string()),
                Some(MAX_RECONNECT_WINDOW + Duration::from_secs(1)),
                false,
                "sid".to_string(),
            ),
            Err(ControllerError::InvalidWindow)
        );
        assert!(!controller.has_credential());
    }

    #[test]
    fn rotated_grant_replaces_old_and_debug_redacts_both() {
        let (mut controller, _) = controller(Duration::from_secs(10));
        let mut jitter = EdgeJitter { maximum: false };
        controller
            .schedule("connection reset".to_string(), Duration::ZERO, &mut jitter)
            .unwrap();
        let generation = controller.generation();
        assert!(controller
            .take_due(Duration::from_millis(125), generation, &identity("pier"),)
            .unwrap()
            .is_some());
        assert_eq!(controller.detach_deadline, Some(Duration::from_secs(10)));
        controller
            .accept_resume(
                Some("grant-2".to_string()),
                Some(Duration::from_secs(20)),
                true,
                "sid-2".to_string(),
                Duration::from_millis(125),
            )
            .unwrap();
        assert_eq!(controller.phase(), &ReconnectPhase::Connected);
        assert_eq!(controller.detach_deadline, None);
        let credential = controller.credential.as_ref().unwrap();
        assert_eq!(credential.grant_for_test(), "grant-2");
        assert_eq!(credential.previous_sid, "sid-2");
        let debug = format!("{controller:?}");
        assert!(!debug.contains("grant-1"));
        assert!(!debug.contains("grant-2"));
    }

    #[test]
    fn long_attached_refresh_has_no_deadline_and_loss_gets_full_window() {
        let (mut controller, clock) = controller(Duration::from_secs(60));
        let holder_nonce = controller.holder_nonce().to_string();
        let connection_identity = controller.identity().clone();
        clock.set(Duration::from_secs(86_400));
        controller
            .accept_refresh(
                Some("grant-latest".to_string()),
                Some(Duration::from_secs(60)),
                false,
                "sid-current".to_string(),
            )
            .unwrap();
        assert_eq!(controller.phase(), &ReconnectPhase::Connected);
        assert_eq!(controller.detach_deadline, None);
        assert_eq!(controller.holder_nonce(), holder_nonce);
        assert_eq!(controller.identity(), &connection_identity);
        assert_eq!(
            controller.credential.as_ref().unwrap().previous_sid,
            "sid-current"
        );
        let debug = format!("{controller:?}");
        assert!(!debug.contains("grant-latest"));
        assert!(debug.contains("<redacted>"));

        let mut jitter = EdgeJitter { maximum: false };
        controller
            .schedule("connection reset".to_string(), clock.now(), &mut jitter)
            .unwrap();
        let expected_deadline = Duration::from_secs(86_460);
        assert!(matches!(
            controller.phase(),
            ReconnectPhase::Reconnecting { deadline, .. } if *deadline == expected_deadline
        ));
        clock.set(Duration::from_secs(86_400) + Duration::from_millis(125));
        assert!(controller
            .take_due(clock.now(), 7, &identity("pier"))
            .unwrap()
            .is_some());
        clock.set(clock.now() + Duration::from_millis(1));
        controller
            .schedule(
                "resume transport failed".to_string(),
                clock.now(),
                &mut jitter,
            )
            .unwrap();
        assert!(matches!(
            controller.phase(),
            ReconnectPhase::Reconnecting { deadline, .. } if *deadline == expected_deadline
        ));
    }

    #[test]
    fn suspended_ui_and_resuming_share_the_loss_anchored_deadline() {
        let (mut controller, clock) = controller(Duration::from_secs(10));
        clock.set(Duration::from_secs(40));
        let mut jitter = EdgeJitter { maximum: false };
        controller
            .schedule("connection reset".to_string(), clock.now(), &mut jitter)
            .unwrap();
        assert!(matches!(
            controller.phase(),
            ReconnectPhase::Reconnecting { deadline, .. }
                if *deadline == Duration::from_secs(50)
        ));

        clock.set(Duration::from_millis(40_125));
        assert!(controller
            .take_due(clock.now(), 7, &identity("pier"))
            .unwrap()
            .is_some());
        assert_eq!(
            controller.resume_budget(Duration::from_secs(45)),
            Ok(Duration::from_secs(5))
        );
        assert!(!controller.expire(Duration::from_nanos(49_999_999_999)));
        assert!(controller.expire(Duration::from_secs(50)));
        assert_eq!(controller.phase(), &ReconnectPhase::Terminal);
        assert!(!controller.has_credential());
    }

    #[test]
    fn system_clock_converts_transport_observation_without_poll_delay() {
        let origin = Instant::now();
        let clock = SystemClock { origin };
        assert_eq!(
            clock.observed_at(origin + Duration::from_secs(7)),
            Duration::from_secs(7)
        );
        assert_eq!(
            clock.observed_at(origin - Duration::from_secs(1)),
            Duration::ZERO
        );
        assert_eq!(
            clock.instant_at(Duration::from_secs(7)),
            Some(origin + Duration::from_secs(7))
        );
    }

    #[test]
    fn malformed_and_overbound_refreshes_fail_closed_without_replacing_grant() {
        let (mut controller, _) = controller(Duration::from_secs(60));
        assert_eq!(
            controller.accept_refresh(
                Some("overbound".to_string()),
                Some(MAX_RECONNECT_WINDOW + Duration::from_secs(1)),
                false,
                "sid".to_string(),
            ),
            Err(ControllerError::InvalidWindow)
        );
        assert_eq!(
            controller.credential.as_ref().unwrap().grant_for_test(),
            "grant-1"
        );
        assert_eq!(
            controller.accept_refresh(
                None,
                Some(Duration::from_secs(60)),
                false,
                "sid".to_string(),
            ),
            Err(ControllerError::MissingSuccessorGrant)
        );
        assert_eq!(
            controller.credential.as_ref().unwrap().grant_for_test(),
            "grant-1"
        );
    }

    #[test]
    fn reconnect_phase_exposes_overlay_data_and_retry_now() {
        let (mut controller, clock) = controller(Duration::from_secs(10));
        let mut jitter = EdgeJitter { maximum: true };
        controller
            .schedule("connection reset".to_string(), clock.now(), &mut jitter)
            .unwrap();
        assert_eq!(
            controller.phase(),
            &ReconnectPhase::Reconnecting {
                attempt: 1,
                next_retry: Duration::from_millis(250),
                deadline: Duration::from_secs(10),
                last_error: "connection reset".to_string(),
            }
        );
        controller.retry_now(Duration::from_millis(10)).unwrap();
        assert!(controller
            .take_due(Duration::from_millis(10), 7, &identity("pier"))
            .unwrap()
            .is_some());
        assert!(matches!(
            controller.phase(),
            ReconnectPhase::Resuming { attempt: 1, .. }
        ));
    }
}
