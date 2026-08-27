//! QUIC-to-WSS fallback and transport circuit breaker.
//!
//! Provides a lightweight state machine that tracks transport health and
//! recommends a [`FallbackMode`] (QUIC-preferred, QUIC-streams-only, or
//! WSS-only) based on accumulated failure counts and elapsed time.
//!
//! No I/O is performed here; the caller probes health outcomes into the
//! circuit breaker and reads back the recommended mode. This keeps the
//! policy testable without any async runtime.
//!
//! # Mode ladder
//!
//! ```text
//! A: QuicWithDatagrams   (preferred; full QUIC + encrypted datagrams)
//!   |
//!   v  datagram failures or max_datagram_size → None
//! B: QuicStreamsOnly     (QUIC reliable streams only; no datagrams)
//!   |
//!   v  handshake failures or sustained loss exceeds threshold
//! C: WebSocketSecure    (TLS-over-TCP; safe baseline)
//!   |
//!   (periodic probe to upgrade back toward A)
//! ```
//!
//! The caller is responsible for persisting `FallbackMode` per endpoint/
//! network fingerprint to avoid reconnect thrash across restarts.

use std::time::Duration;

/// Default failure threshold for downgrade decisions.
pub const CB_DEFAULT_FAILURE_THRESHOLD: u32 = 3;
/// Default failure counting window.
pub const CB_DEFAULT_WINDOW_SECS: u64 = 60;
/// Default probe interval before attempting an upgrade.
pub const CB_DEFAULT_PROBE_INTERVAL_SECS: u64 = 300;
/// Required consecutive successful probes before upgrade.
pub const CB_UPGRADE_PROBES_REQUIRED: u32 = 2;

/// The three-tier transport fallback mode ladder.
///
/// Ordered from most capable (A) to most compatible (C).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FallbackMode {
    /// Mode A: QUIC with encrypted datagrams for low-latency media.
    QuicWithDatagrams,
    /// Mode B: QUIC reliable streams only (datagram disabled or unreliable).
    QuicStreamsOnly,
    /// Mode C: TLS-over-TCP WebSocket (QUIC handshake failing or UDP blocked).
    WebSocketSecure,
}

impl FallbackMode {
    /// Returns the next lower (more compatible) mode, or `None` if already
    /// at the minimum.
    #[must_use]
    pub fn downgrade(self) -> Option<Self> {
        match self {
            Self::QuicWithDatagrams => Some(Self::QuicStreamsOnly),
            Self::QuicStreamsOnly => Some(Self::WebSocketSecure),
            Self::WebSocketSecure => None,
        }
    }

    /// Returns `true` if QUIC is being used (modes A or B).
    #[must_use]
    pub fn is_quic(self) -> bool {
        self != Self::WebSocketSecure
    }

    /// Returns `true` if datagrams are active (mode A only).
    #[must_use]
    pub fn datagrams_active(self) -> bool {
        self == Self::QuicWithDatagrams
    }
}

/// Reason a transport failure should be recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// QUIC handshake failed (UDP may be blocked or the remote is WSS-only).
    ///
    /// This is an **early-path failure** — the connection never established.
    /// Treat consecutive `QuicHandshake` failures as a strong signal to
    /// downgrade to `WebSocketSecure` rather than staying in a QUIC mode.
    QuicHandshake,
    /// Repeated packet-timeout / path-timeout (PTO spike).
    PacketTimeout,
    /// Sustained loss exceeded application threshold.
    SustainedLoss,
    /// Encrypted datagram not acknowledged / dropped persistently.
    DatagramDrop,
    /// QUIC connection closed unexpectedly.
    ConnectionClosed,
    /// Network path changed (Wi-Fi ↔ wired / mobile handoff).
    ///
    /// Per RFC 9308 §9, path migration costs at least one RTT of path
    /// validation plus a congestion-window reset. Record this as a failure
    /// when the post-migration path proves unhealthy rather than immediately
    /// downgrading. Distinguish from `QuicHandshake` (no established session)
    /// and `PacketTimeout` (stable path, transient loss).
    PathChange,
}

/// Thresholds controlling when the circuit breaker downgrades.
#[derive(Debug, Clone, Copy)]
pub struct CircuitBreakerPolicy {
    /// Number of failures within `window` before triggering a downgrade.
    pub failure_threshold: u32,
    /// Time window over which failures are counted.
    pub window: Duration,
    /// How long to stay in a downgraded mode before attempting a probe upgrade.
    pub probe_interval: Duration,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: CB_DEFAULT_FAILURE_THRESHOLD,
            window: Duration::from_secs(CB_DEFAULT_WINDOW_SECS),
            probe_interval: Duration::from_secs(CB_DEFAULT_PROBE_INTERVAL_SECS),
        }
    }
}

impl CircuitBreakerPolicy {
    const fn validated(self) -> Self {
        Self {
            failure_threshold: if self.failure_threshold == 0 {
                1
            } else {
                self.failure_threshold
            },
            window: if self.window.as_secs() == 0 {
                Duration::from_secs(1)
            } else {
                self.window
            },
            probe_interval: if self.probe_interval.as_secs() == 0 {
                Duration::from_secs(1)
            } else {
                self.probe_interval
            },
        }
    }
}

/// Lightweight circuit breaker for transport fallback decisions.
///
/// The caller feeds failure and success observations; the circuit breaker
/// returns the current [`FallbackMode`] recommendation. All time is provided
/// by the caller (monotonic `u64` seconds) to keep this `no_std`-friendly.
#[derive(Debug)]
pub struct TransportCircuitBreaker {
    mode: FallbackMode,
    policy: CircuitBreakerPolicy,
    failure_count: u32,
    window_start_secs: u64,
    downgraded_at_secs: Option<u64>,
    consecutive_probe_successes: u32,
}

impl TransportCircuitBreaker {
    /// Creates a circuit breaker starting in mode A.
    #[must_use]
    pub fn new(policy: CircuitBreakerPolicy) -> Self {
        let policy = policy.validated();
        Self {
            mode: FallbackMode::QuicWithDatagrams,
            policy,
            failure_count: 0,
            window_start_secs: 0,
            downgraded_at_secs: None,
            consecutive_probe_successes: 0,
        }
    }

    /// Creates a circuit breaker starting in the given mode (for persisted state).
    #[must_use]
    pub fn with_mode(policy: CircuitBreakerPolicy, mode: FallbackMode) -> Self {
        let policy = policy.validated();
        Self {
            mode,
            policy,
            failure_count: 0,
            window_start_secs: 0,
            downgraded_at_secs: if mode == FallbackMode::QuicWithDatagrams {
                None
            } else {
                Some(0)
            },
            consecutive_probe_successes: 0,
        }
    }

    /// Returns the current recommended fallback mode.
    #[must_use]
    pub fn mode(&self) -> FallbackMode {
        self.mode
    }

    /// Records a transport failure observation at `now_secs` (monotonic seconds).
    ///
    /// Returns the current (possibly downgraded) [`FallbackMode`].
    pub fn record_failure(&mut self, kind: FailureKind, now_secs: u64) -> FallbackMode {
        if kind == FailureKind::PathChange {
            return self.mode;
        }

        if kind == FailureKind::DatagramDrop && self.mode != FallbackMode::QuicWithDatagrams {
            return self.mode;
        }

        // Reset the window if it has expired
        if now_secs.saturating_sub(self.window_start_secs) > self.policy.window.as_secs() {
            self.failure_count = 0;
            self.window_start_secs = now_secs;
        }

        self.failure_count += 1;

        let threshold_reached = self.failure_count >= self.policy.failure_threshold;
        if threshold_reached {
            self.consecutive_probe_successes = 0;
            self.failure_count = 0;
            self.window_start_secs = now_secs;

            match kind {
                FailureKind::QuicHandshake => {
                    self.mode = FallbackMode::WebSocketSecure;
                    self.downgraded_at_secs = Some(now_secs);
                }
                FailureKind::DatagramDrop => {
                    self.mode = FallbackMode::QuicStreamsOnly;
                    self.downgraded_at_secs = Some(now_secs);
                }
                FailureKind::PacketTimeout
                | FailureKind::SustainedLoss
                | FailureKind::ConnectionClosed => {
                    if let Some(lower) = self.mode.downgrade() {
                        self.mode = lower;
                        self.downgraded_at_secs = Some(now_secs);
                    }
                }
                FailureKind::PathChange => {}
            }
        }

        self.mode
    }

    /// Records a successful connection/session at `now_secs`.
    ///
    /// Resets the failure counter. Does not automatically upgrade the mode
    /// (use [`probe_upgrade`] for that).
    pub fn record_success(&mut self, now_secs: u64) {
        self.failure_count = 0;
        self.window_start_secs = now_secs;
        self.consecutive_probe_successes = 0;
    }

    /// Returns `true` when enough time has passed since the last downgrade to
    /// attempt an upgrade probe.
    ///
    /// The caller should attempt a higher-fidelity connection and, if it
    /// succeeds, call [`apply_upgrade`] to advance the mode.
    #[must_use]
    pub fn should_probe_upgrade(&self, now_secs: u64) -> bool {
        if self.mode == FallbackMode::QuicWithDatagrams {
            return false; // already at maximum
        }
        match self.downgraded_at_secs {
            None => false,
            Some(at) => now_secs.saturating_sub(at) >= self.policy.probe_interval.as_secs(),
        }
    }

    /// Applies one step of upgrade when a probe succeeds.
    ///
    /// Returns the new mode.
    pub fn apply_upgrade(&mut self, now_secs: u64) -> FallbackMode {
        self.mode = match self.mode {
            FallbackMode::WebSocketSecure => FallbackMode::QuicStreamsOnly,
            FallbackMode::QuicStreamsOnly | FallbackMode::QuicWithDatagrams => {
                FallbackMode::QuicWithDatagrams
            }
        };
        self.downgraded_at_secs = if self.mode == FallbackMode::QuicWithDatagrams {
            None
        } else {
            Some(now_secs)
        };
        self.failure_count = 0;
        self.consecutive_probe_successes = 0;
        self.mode
    }

    /// Records one probe attempt result and performs at most one-step upgrade.
    ///
    /// A single successful probe does not upgrade. Two consecutive successful
    /// probes upgrade one level. Any failed probe resets the success counter.
    pub fn record_probe_result(&mut self, success: bool, now_secs: u64) -> FallbackMode {
        if self.mode == FallbackMode::QuicWithDatagrams {
            self.consecutive_probe_successes = 0;
            return self.mode;
        }
        if !success {
            self.consecutive_probe_successes = 0;
            return self.mode;
        }
        if !self.should_probe_upgrade(now_secs) {
            return self.mode;
        }
        self.consecutive_probe_successes += 1;
        if self.consecutive_probe_successes >= CB_UPGRADE_PROBES_REQUIRED {
            return self.apply_upgrade(now_secs);
        }
        self.mode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> CircuitBreakerPolicy {
        CircuitBreakerPolicy {
            failure_threshold: 3,
            window: Duration::from_secs(60),
            probe_interval: Duration::from_secs(300),
        }
    }

    #[test]
    fn starts_in_quic_with_datagrams() {
        let cb = TransportCircuitBreaker::new(policy());
        assert_eq!(cb.mode(), FallbackMode::QuicWithDatagrams);
    }

    #[test]
    fn downgrades_after_threshold_failures() {
        // DatagramDrop trips A→B (QuicStreamsOnly) in one step.
        let mut cb = TransportCircuitBreaker::new(policy());
        cb.record_failure(FailureKind::DatagramDrop, 0);
        cb.record_failure(FailureKind::DatagramDrop, 1);
        let mode = cb.record_failure(FailureKind::DatagramDrop, 2);
        assert_eq!(mode, FallbackMode::QuicStreamsOnly);
    }

    #[test]
    fn quic_handshake_trips_directly_to_wss() {
        // QuicHandshake skips B and goes straight to C (UDP may be blocked).
        let mut cb = TransportCircuitBreaker::new(policy());
        cb.record_failure(FailureKind::QuicHandshake, 0);
        cb.record_failure(FailureKind::QuicHandshake, 1);
        let mode = cb.record_failure(FailureKind::QuicHandshake, 2);
        assert_eq!(mode, FallbackMode::WebSocketSecure);
    }

    #[test]
    fn second_downgrade_reaches_wss() {
        // DatagramDrop A→B, then PacketTimeout B→C.
        let mut cb = TransportCircuitBreaker::new(policy());
        for i in 0..3 {
            cb.record_failure(FailureKind::DatagramDrop, i);
        }
        assert_eq!(cb.mode(), FallbackMode::QuicStreamsOnly);
        for i in 0..3 {
            cb.record_failure(FailureKind::PacketTimeout, 10 + i);
        }
        assert_eq!(cb.mode(), FallbackMode::WebSocketSecure);
    }

    #[test]
    fn failure_window_reset_prevents_premature_downgrade() {
        let mut cb = TransportCircuitBreaker::new(policy());
        cb.record_failure(FailureKind::PacketTimeout, 0);
        cb.record_failure(FailureKind::PacketTimeout, 1);
        // Jump past window
        cb.record_failure(FailureKind::PacketTimeout, 120);
        assert_eq!(
            cb.mode(),
            FallbackMode::QuicWithDatagrams,
            "window reset prevents downgrade"
        );
    }

    #[test]
    fn probe_upgrade_not_ready_immediately() {
        let mut cb = TransportCircuitBreaker::new(policy());
        for i in 0..3 {
            cb.record_failure(FailureKind::QuicHandshake, i);
        }
        assert!(!cb.should_probe_upgrade(5), "too soon to probe");
    }

    #[test]
    fn probe_upgrade_ready_after_interval() {
        let mut cb = TransportCircuitBreaker::new(policy());
        for i in 0..3 {
            cb.record_failure(FailureKind::QuicHandshake, i);
        }
        assert!(cb.should_probe_upgrade(305), "probe ready after interval");
    }

    #[test]
    fn path_change_does_not_downgrade_by_itself() {
        let mut cb = TransportCircuitBreaker::new(policy());
        for i in 0..32 {
            cb.record_failure(FailureKind::PathChange, i);
        }
        assert_eq!(cb.mode(), FallbackMode::QuicWithDatagrams);
    }

    #[test]
    fn apply_upgrade_steps_back_to_quic_with_datagrams() {
        let mut cb = TransportCircuitBreaker::with_mode(policy(), FallbackMode::WebSocketSecure);
        let m1 = cb.apply_upgrade(0);
        assert_eq!(m1, FallbackMode::QuicStreamsOnly);
        let m2 = cb.apply_upgrade(0);
        assert_eq!(m2, FallbackMode::QuicWithDatagrams);
        assert!(!cb.should_probe_upgrade(9999), "no probe once at max mode");
    }

    #[test]
    fn success_resets_failure_count() {
        let mut cb = TransportCircuitBreaker::new(policy());
        cb.record_failure(FailureKind::SustainedLoss, 0);
        cb.record_failure(FailureKind::SustainedLoss, 1);
        cb.record_success(2);
        // Two failures within threshold then success: mode unchanged
        let mode = cb.record_failure(FailureKind::SustainedLoss, 3);
        assert_eq!(mode, FallbackMode::QuicWithDatagrams);
    }

    #[test]
    fn one_probe_success_does_not_upgrade_but_two_do() {
        let mut cb = TransportCircuitBreaker::with_mode(policy(), FallbackMode::WebSocketSecure);
        assert_eq!(
            cb.record_probe_result(true, 301),
            FallbackMode::WebSocketSecure
        );
        assert_eq!(
            cb.record_probe_result(true, 302),
            FallbackMode::QuicStreamsOnly
        );
    }

    #[test]
    fn failed_probe_resets_consecutive_counter() {
        let mut cb = TransportCircuitBreaker::with_mode(policy(), FallbackMode::WebSocketSecure);
        assert_eq!(
            cb.record_probe_result(true, 301),
            FallbackMode::WebSocketSecure
        );
        assert_eq!(
            cb.record_probe_result(false, 302),
            FallbackMode::WebSocketSecure
        );
        assert_eq!(
            cb.record_probe_result(true, 303),
            FallbackMode::WebSocketSecure
        );
        assert_eq!(
            cb.record_probe_result(true, 304),
            FallbackMode::QuicStreamsOnly
        );
    }

    #[test]
    fn with_mode_restores_probe_eligibility() {
        let cb = TransportCircuitBreaker::with_mode(policy(), FallbackMode::QuicStreamsOnly);
        assert!(cb.should_probe_upgrade(301));
    }

    #[test]
    fn zero_policy_values_are_safely_validated() {
        let cb = TransportCircuitBreaker::new(CircuitBreakerPolicy {
            failure_threshold: 0,
            window: Duration::from_secs(0),
            probe_interval: Duration::from_secs(0),
        });
        assert_eq!(cb.policy.failure_threshold, 1);
        assert_eq!(cb.policy.window, Duration::from_secs(1));
        assert_eq!(cb.policy.probe_interval, Duration::from_secs(1));
    }

    #[test]
    fn fallback_mode_helpers() {
        assert!(FallbackMode::QuicWithDatagrams.is_quic());
        assert!(FallbackMode::QuicStreamsOnly.is_quic());
        assert!(!FallbackMode::WebSocketSecure.is_quic());
        assert!(FallbackMode::QuicWithDatagrams.datagrams_active());
        assert!(!FallbackMode::QuicStreamsOnly.datagrams_active());
    }
}
