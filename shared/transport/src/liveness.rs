//! Peer liveness on a session carrier.
//!
//! A Deck that vanishes without closing its connection (killed, crashed,
//! suspended, unplugged) leaves the Pier's capacity-one session slot held
//! until the carrier notices. Two shared rules bound that:
//!
//! - **Keep-alive and idle timeout.** The lane carrier sends a keep-alive
//!   every [`LANE_KEEPALIVE_INTERVAL`] and closes a connection that heard
//!   nothing for [`LANE_IDLE_TIMEOUT`]. QUIC uses the smaller of the two
//!   peers' idle timeouts, so either side's value bounds the session.
//! - **Proven silence.** A new attachment of the *same authenticated user*
//!   that finds the slot held may ask whether the incumbent's peer is still
//!   there: a live peer acknowledges the keep-alive (or the media it
//!   receives) within [`PEER_SILENCE_PROOF`]; a peer that sent nothing for
//!   that long is silent, and the incumbent connection is closed as lost so
//!   the newcomer can take the slot as soon as the old attachment has torn
//!   down ([`ATTACHMENT_HANDOVER_WAIT`]). A live incumbent is never
//!   displaced, and another user's never is.
//!
//! [`SilenceObserver`] is the pure decision over a received-datagram
//! counter; `quic::PeerLiveness` applies it to a live connection.

use std::time::Duration;

/// Keep-alive interval of the lane carrier.
///
/// Short enough that a live peer is heard several times within
/// [`PEER_SILENCE_PROOF`]; a QUIC PING is one tiny ack-eliciting packet, and
/// none is sent while other traffic flows.
pub const LANE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);

/// Idle timeout of the lane carrier: a connection that received nothing for
/// this long is closed. Ten keep-alive intervals, so a few lost packets on a
/// lossy path never end a session, while a vanished peer frees the session
/// within seconds rather than half a minute. A longer outage ends the
/// connection; resume (`reconnect_window_secs`) is the path back.
pub const LANE_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a peer must have sent nothing before it is proven silent: three
/// keep-alive intervals, so one lost keep-alive and its acknowledgement do
/// not condemn a live peer.
pub const PEER_SILENCE_PROOF: Duration = Duration::from_secs(3);

/// How long a new attachment waits for a held session slot before it is
/// refused as busy: the idle timeout plus a teardown margin, so a vanished
/// incumbent that could not be proven silent (a resumed attachment, a
/// carrier without counters) is still outwaited.
pub const ATTACHMENT_HANDOVER_WAIT: Duration = Duration::from_secs(12);

/// How often a liveness proof samples the received-datagram counter.
pub const LIVENESS_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

/// What a liveness proof concluded about a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLivenessVerdict {
    /// The peer sent at least one datagram during the proof.
    Alive,
    /// The peer sent nothing for at least the proof window.
    Silent {
        /// How long nothing arrived.
        silent_for: Duration,
    },
    /// The connection is already closed.
    Closed,
}

impl PeerLivenessVerdict {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Alive => "alive",
            Self::Silent { .. } => "silent",
            Self::Closed => "closed",
        }
    }
}

/// Pure silence decision over a monotonically increasing received-datagram
/// counter sampled at monotonic times (any common origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilenceObserver {
    started_at: Duration,
    received: u64,
    heard_since_start: bool,
}

impl SilenceObserver {
    /// Starts a proof at `at` with the counter at `received`.
    #[must_use]
    pub const fn new(at: Duration, received: u64) -> Self {
        Self {
            started_at: at,
            received,
            heard_since_start: false,
        }
    }

    /// Records one sample and returns the verdict once it is decided:
    /// [`PeerLivenessVerdict::Alive`] as soon as the counter moved,
    /// [`PeerLivenessVerdict::Silent`] once `proof` elapsed without a move,
    /// `None` while undecided. A counter that went backwards (a replaced
    /// connection) is treated as movement, never as silence.
    #[must_use]
    pub fn observe(
        &mut self,
        at: Duration,
        received: u64,
        proof: Duration,
    ) -> Option<PeerLivenessVerdict> {
        if received != self.received {
            self.received = received;
            self.heard_since_start = true;
        }
        if self.heard_since_start {
            return Some(PeerLivenessVerdict::Alive);
        }
        let silent_for = at.saturating_sub(self.started_at);
        (silent_for >= proof).then_some(PeerLivenessVerdict::Silent { silent_for })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn keepalive_fits_the_proof_and_the_idle_timeout() {
        assert!(LANE_KEEPALIVE_INTERVAL * 3 <= PEER_SILENCE_PROOF);
        assert!(PEER_SILENCE_PROOF < LANE_IDLE_TIMEOUT);
        assert!(LANE_IDLE_TIMEOUT < ATTACHMENT_HANDOVER_WAIT);
        assert!(LIVENESS_SAMPLE_INTERVAL * 10 <= LANE_KEEPALIVE_INTERVAL);
    }

    #[test]
    fn a_moving_counter_is_alive_at_once() {
        let mut observer = SilenceObserver::new(MS(1_000), 40);
        assert_eq!(observer.observe(MS(1_100), 40, PEER_SILENCE_PROOF), None);
        assert_eq!(
            observer.observe(MS(1_200), 41, PEER_SILENCE_PROOF),
            Some(PeerLivenessVerdict::Alive)
        );
        // Once heard, later quiet samples do not turn it silent.
        assert_eq!(
            observer.observe(MS(9_000), 41, PEER_SILENCE_PROOF),
            Some(PeerLivenessVerdict::Alive)
        );
    }

    #[test]
    fn a_still_counter_is_silent_only_after_the_whole_proof() {
        let mut observer = SilenceObserver::new(MS(0), 7);
        assert_eq!(observer.observe(MS(2_999), 7, PEER_SILENCE_PROOF), None);
        assert_eq!(
            observer.observe(MS(3_000), 7, PEER_SILENCE_PROOF),
            Some(PeerLivenessVerdict::Silent {
                silent_for: MS(3_000)
            })
        );
    }

    #[test]
    fn a_counter_that_went_backwards_is_movement() {
        let mut observer = SilenceObserver::new(MS(0), 500);
        assert_eq!(
            observer.observe(MS(100), 3, PEER_SILENCE_PROOF),
            Some(PeerLivenessVerdict::Alive)
        );
    }

    #[test]
    fn a_clock_that_did_not_advance_never_condemns() {
        let mut observer = SilenceObserver::new(MS(5_000), 1);
        assert_eq!(observer.observe(MS(4_000), 1, PEER_SILENCE_PROOF), None);
    }

    #[test]
    fn verdict_tokens_are_stable() {
        assert_eq!(PeerLivenessVerdict::Alive.token(), "alive");
        assert_eq!(
            PeerLivenessVerdict::Silent {
                silent_for: Duration::ZERO
            }
            .token(),
            "silent"
        );
        assert_eq!(PeerLivenessVerdict::Closed.token(), "closed");
    }
}
