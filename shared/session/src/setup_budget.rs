// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! One budget for a host session's setup, shared by a Pier's broker and the
//! per-session agent it launches.
//!
//! The broker waits [`AGENT_READY_TIMEOUT`] after sending the agent its start
//! packet, then terminates the agent. Any wait inside the agent's setup must
//! therefore end — and leave time to roll back what it changed and send a
//! refusal — before that. [`AGENT_SETUP_BUDGET`] is that limit, kept
//! [`REFUSAL_MARGIN`] short of the broker's, and [`SetupDeadline`] turns it
//! into an absolute deadline from the moment the start packet arrived, so the
//! time spent before a wait (display provisioning, topology) counts too. A
//! wait that reaches it ends with a typed refusal instead of a terminated
//! agent the Deck only sees as "connection timed out".

use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

const AGENT_READY_SECS: u64 = 30;
const REFUSAL_MARGIN_SECS: u64 = 5;

/// How long a broker waits for its agent's readiness (or refusal) after it
/// sends the start packet.
pub const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(AGENT_READY_SECS);
/// Kept back from the broker's wait for the agent to roll back and refuse.
pub const REFUSAL_MARGIN: Duration = Duration::from_secs(REFUSAL_MARGIN_SECS);
/// The longest any agent setup step may run, counted from the start packet.
pub const AGENT_SETUP_BUDGET: Duration =
    Duration::from_secs(AGENT_READY_SECS - REFUSAL_MARGIN_SECS);

/// The agent's setup deadline while it is setting up, and nothing after.
#[derive(Debug, Default)]
pub struct SetupDeadline(Mutex<Option<Instant>>);

impl SetupDeadline {
    #[must_use]
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// The agent received its start packet at `started`.
    pub fn begin(&self, started: Instant) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(started + AGENT_SETUP_BUDGET);
    }

    /// The agent answered the broker; later waits (re-attachments) keep only
    /// their own limit.
    pub fn end(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// When a wait starting `now` that would take at most `longest` must
    /// give up.
    #[must_use]
    pub fn wait_deadline(&self, now: Instant, longest: Duration) -> Instant {
        let setup = *self.0.lock().unwrap_or_else(PoisonError::into_inner);
        bounded_deadline(now, longest, setup)
    }
}

/// The earlier of a wait's own limit and the setup deadline, if any.
#[must_use]
pub fn bounded_deadline(now: Instant, longest: Duration, setup: Option<Instant>) -> Instant {
    let own = now + longest;
    setup.map_or(own, |setup| setup.min(own))
}

/// Why a host session refused HDR while it was being set up. The code leads
/// the message the Deck shows and the agent failure the Pier logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrSetupRefusal {
    /// The operating system did not activate HDR on the session display
    /// within the setup budget; the display and its HDR state were restored.
    NotActivated,
    /// The requested display size cannot carry ten-bit HDR10 at or above the
    /// HDR stream rate over the virtual display's link.
    LinkExceeded,
}

impl HdrSetupRefusal {
    pub const ALL: [Self; 2] = [Self::NotActivated, Self::LinkExceeded];

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotActivated => "hdr_not_activated",
            Self::LinkExceeded => "hdr_link_exceeded",
        }
    }

    #[must_use]
    pub fn message(self, detail: &str) -> String {
        format!("{}: {detail}", self.code())
    }
}

/// Puts a refusal code back in front of a message that layers of context
/// wrapped (`apply authenticated client display: output verify failed:
/// hdr_not_activated: …`), so the Deck and the log lead with it. A message
/// without a code, or already led by one, is returned unchanged.
#[must_use]
pub fn lead_with_refusal_code(message: &str) -> String {
    let found = HdrSetupRefusal::ALL
        .iter()
        .filter_map(|refusal| {
            let marker = format!("{}: ", refusal.code());
            message.find(&marker).map(|at| (at, marker))
        })
        .min_by_key(|(at, _)| *at);
    let Some((at, marker)) = found else {
        return message.to_string();
    };
    if at == 0 {
        return message.to_string();
    }
    let context = message[..at].trim_end().trim_end_matches(':');
    let detail = &message[at + marker.len()..];
    format!("{marker}{context}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setup_wait_ends_before_the_broker_gives_up_with_margin_to_refuse() {
        assert!(AGENT_SETUP_BUDGET + REFUSAL_MARGIN <= AGENT_READY_TIMEOUT);
        assert!(REFUSAL_MARGIN >= Duration::from_secs(5));
    }

    #[test]
    fn a_wait_started_late_in_setup_ends_at_the_setup_deadline() {
        let started = Instant::now();
        let deadline = SetupDeadline::new();
        deadline.begin(started);
        // Six seconds of provisioning before a 45 s wait: the setup deadline wins.
        let now = started + Duration::from_secs(6);
        assert_eq!(
            deadline.wait_deadline(now, Duration::from_secs(45)),
            started + AGENT_SETUP_BUDGET
        );
        // A short wait keeps its own limit.
        assert_eq!(
            deadline.wait_deadline(now, Duration::from_secs(3)),
            now + Duration::from_secs(3)
        );
        // After the agent answered (re-attachment) only the wait's own limit applies.
        deadline.end();
        assert_eq!(
            deadline.wait_deadline(now, Duration::from_secs(45)),
            now + Duration::from_secs(45)
        );
    }

    #[test]
    fn a_wrapped_refusal_is_led_by_its_code_again() {
        let inner = HdrSetupRefusal::NotActivated.message(
            "Windows did not activate HDR on all 1 session display target(s) within 19750ms",
        );
        let wrapped = format!(
            "apply authenticated client display: output verify failed: {inner}; \
             previous display mode was restored"
        );
        assert_eq!(
            lead_with_refusal_code(&wrapped),
            "hdr_not_activated: apply authenticated client display: output verify failed: \
             Windows did not activate HDR on all 1 session display target(s) within 19750ms; \
             previous display mode was restored"
        );
        let led = lead_with_refusal_code(&wrapped);
        assert_eq!(lead_with_refusal_code(&led), led);
        assert_eq!(lead_with_refusal_code("no code here"), "no code here");
    }

    #[test]
    fn a_refusal_names_its_code_first() {
        let message = HdrSetupRefusal::NotActivated.message("the display stayed SDR");
        assert!(message.starts_with("hdr_not_activated: "), "{message}");
        assert_eq!(HdrSetupRefusal::LinkExceeded.code(), "hdr_link_exceeded");
    }
}
