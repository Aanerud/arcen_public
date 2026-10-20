//! Liveness of a live QUIC session connection and the Pier's incumbent
//! attachment (see [`crate::liveness`] for the rules).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use quinn::VarInt;

use crate::liveness::{LIVENESS_SAMPLE_INTERVAL, PeerLivenessVerdict, SilenceObserver};

/// Application close code for an incumbent connection closed because its
/// peer was proven silent (`ARL\x02`, beside the lane version-mismatch code
/// `ARL\x01`).
pub const PEER_SILENT_CLOSE_CODE: u32 = 0x4152_4C02;

/// Close reason sent with [`PEER_SILENT_CLOSE_CODE`].
pub const PEER_SILENT_CLOSE_REASON: &str =
    "peer silent; replaced by a new attachment of the same user";

fn received_datagrams(connection: &quinn::Connection) -> u64 {
    connection.stats().udp_rx.datagrams
}

/// Waits up to `proof` for `connection`'s peer to be heard.
///
/// Returns [`PeerLivenessVerdict::Alive`] as soon as any datagram arrives,
/// [`PeerLivenessVerdict::Silent`] when none arrived for `proof`, and
/// [`PeerLivenessVerdict::Closed`] when the connection is (or becomes)
/// closed. The proof relies on the lane carrier's keep-alive
/// ([`crate::liveness::LANE_KEEPALIVE_INTERVAL`]): a live peer acknowledges
/// it, so it is heard even on an idle session.
pub async fn prove_peer_liveness(
    connection: &quinn::Connection,
    proof: Duration,
) -> PeerLivenessVerdict {
    if connection.close_reason().is_some() {
        return PeerLivenessVerdict::Closed;
    }
    let started = tokio::time::Instant::now();
    let mut observer = SilenceObserver::new(Duration::ZERO, received_datagrams(connection));
    let mut ticks = tokio::time::interval(LIVENESS_SAMPLE_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = connection.closed() => return PeerLivenessVerdict::Closed,
        }
        if let Some(verdict) =
            observer.observe(started.elapsed(), received_datagrams(connection), proof)
        {
            return verdict;
        }
    }
}

/// What [`IncumbentAttachment::displace_if_silent`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncumbentOutcome {
    /// No attachment is registered (none, or one between connections).
    NoIncumbent,
    /// The incumbent belongs to another user; it is never probed.
    OtherOwner,
    /// The incumbent's peer was heard; it keeps the session.
    Alive,
    /// The incumbent's connection was already closed; its attachment is
    /// tearing down.
    AlreadyClosed,
    /// The incumbent's peer was silent; its connection was closed as lost.
    Displaced {
        /// How long nothing arrived from it.
        silent_for: Duration,
    },
}

impl IncumbentOutcome {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NoIncumbent => "no_incumbent",
            Self::OtherOwner => "other_owner",
            Self::Alive => "alive",
            Self::AlreadyClosed => "already_closed",
            Self::Displaced { .. } => "displaced",
        }
    }

    /// Whether waiting for the session slot can succeed soon.
    #[must_use]
    pub const fn slot_may_free(self) -> bool {
        matches!(
            self,
            Self::NoIncumbent | Self::AlreadyClosed | Self::Displaced { .. }
        )
    }
}

#[derive(Debug)]
struct Incumbent {
    token: u64,
    owner: String,
    connection: quinn::Connection,
    displaced: Arc<tokio::sync::watch::Sender<bool>>,
}

/// The Pier's record of the connection that currently holds its
/// capacity-one session, so a same-user newcomer can prove it silent.
#[derive(Debug, Default)]
pub struct IncumbentAttachment {
    current: Mutex<Option<Incumbent>>,
    next_token: AtomicU64,
}

impl IncumbentAttachment {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Incumbent>> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `connection` of authenticated `owner` as the incumbent. The
    /// returned guard clears the record when dropped (only if it is still
    /// this registration) and can move it to a resumed connection.
    #[must_use]
    pub fn register(
        self: &Arc<Self>,
        owner: &str,
        connection: quinn::Connection,
    ) -> IncumbentRegistration {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let (displaced, watched) = tokio::sync::watch::channel(false);
        *self.lock() = Some(Incumbent {
            token,
            owner: owner.to_owned(),
            connection,
            displaced: Arc::new(displaced),
        });
        IncumbentRegistration {
            record: Arc::clone(self),
            token,
            displaced: watched,
        }
    }

    /// For a newcomer authenticated as `claimant` that found the session
    /// slot held: proves the incumbent's peer alive or silent within
    /// `proof`. A silent incumbent is marked displaced and its connection
    /// closed with [`PEER_SILENT_CLOSE_CODE`]; one whose connection already
    /// closed (it timed out on its own, during the proof or before) is
    /// marked displaced too, so its owner hands the desktop over instead of
    /// holding it for a resume. Either mark is set only when the record
    /// still names the same registration and the same connection after the
    /// proof: a registration that moved to a resumed connection meanwhile
    /// is live and never marked. Never touches another user's attachment.
    pub async fn displace_if_silent(&self, claimant: &str, proof: Duration) -> IncumbentOutcome {
        let (token, connection) = {
            let current = self.lock();
            match current.as_ref() {
                None => return IncumbentOutcome::NoIncumbent,
                Some(incumbent) if incumbent.owner != claimant => {
                    return IncumbentOutcome::OtherOwner;
                }
                Some(incumbent) => (incumbent.token, incumbent.connection.clone()),
            }
        };
        let verdict = prove_peer_liveness(&connection, proof).await;
        if verdict == PeerLivenessVerdict::Alive {
            return IncumbentOutcome::Alive;
        }
        {
            let current = self.lock();
            match current.as_ref() {
                None => return IncumbentOutcome::NoIncumbent,
                Some(incumbent)
                    if incumbent.token != token
                        || incumbent.connection.stable_id() != connection.stable_id() =>
                {
                    // Replaced or resumed while it was being proven.
                    return IncumbentOutcome::Alive;
                }
                // Set before the close, so the owner sees why its connection
                // ended: not a loss to wait out, but a hand-over.
                Some(incumbent) => {
                    incumbent.displaced.send_replace(true);
                }
            }
        }
        match verdict {
            PeerLivenessVerdict::Silent { silent_for } => {
                connection.close(
                    VarInt::from_u32(PEER_SILENT_CLOSE_CODE),
                    PEER_SILENT_CLOSE_REASON.as_bytes(),
                );
                IncumbentOutcome::Displaced { silent_for }
            }
            PeerLivenessVerdict::Alive | PeerLivenessVerdict::Closed => {
                IncumbentOutcome::AlreadyClosed
            }
        }
    }
}

/// One registration in an [`IncumbentAttachment`]; clears it on drop.
#[derive(Debug)]
pub struct IncumbentRegistration {
    record: Arc<IncumbentAttachment>,
    token: u64,
    displaced: tokio::sync::watch::Receiver<bool>,
}

impl IncumbentRegistration {
    /// Whether a same-user newcomer proved this attachment's peer silent
    /// and closed its connection. The owner must then hand the desktop
    /// over (release the session slot, keep the desktop) rather than hold
    /// it for a resume that will not come.
    #[must_use]
    pub fn displaced(&self) -> bool {
        *self.displaced.borrow()
    }

    /// Resolves once this attachment is marked displaced: while its owner
    /// waits for a resume, a same-user newcomer may still prove it gone.
    pub async fn wait_displaced(&self) {
        let mut displaced = self.displaced.clone();
        if displaced.wait_for(|displaced| *displaced).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Moves this registration to a resumed attachment's connection. A
    /// displacement mark set against the connection it replaces (a check
    /// that proved the old, closed connection while the resume was being
    /// validated) is cleared under the same lock: the resumed attachment is
    /// live, and a later ordinary loss of it is not a hand-over.
    pub fn replace_connection(&self, connection: quinn::Connection) {
        if let Some(incumbent) = self.record.lock().as_mut() {
            if incumbent.token == self.token {
                incumbent.connection = connection;
                incumbent.displaced.send_replace(false);
            }
        }
    }
}

impl Drop for IncumbentRegistration {
    fn drop(&mut self) {
        let mut current = self.record.lock();
        if current
            .as_ref()
            .is_some_and(|incumbent| incumbent.token == self.token)
        {
            *current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_freed_or_displaced_slot_is_worth_waiting_for() {
        assert!(IncumbentOutcome::NoIncumbent.slot_may_free());
        assert!(IncumbentOutcome::AlreadyClosed.slot_may_free());
        assert!(
            IncumbentOutcome::Displaced {
                silent_for: Duration::from_secs(3)
            }
            .slot_may_free()
        );
        assert!(!IncumbentOutcome::Alive.slot_may_free());
        assert!(!IncumbentOutcome::OtherOwner.slot_may_free());
    }

    #[test]
    fn close_code_is_beside_the_lane_mismatch_code() {
        assert_eq!(
            PEER_SILENT_CLOSE_CODE,
            arcen_protocol::lanes::LANE_VERSION_MISMATCH_CLOSE_CODE + 1
        );
        assert!(PEER_SILENT_CLOSE_REASON.len() <= 123);
    }
}
