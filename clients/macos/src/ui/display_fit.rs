//! Tracks viewer surface size changes and issues `display_update` messages to
//! the host when the surface has been stable long enough and the delta exceeds
//! the minimum threshold.

use std::time::{Duration, Instant};

use crate::protocol::messages::{
    DisplayUpdateMsg, DisplayUpdateResultMsg, DISPLAY_UPDATE, MAX_STREAM_HEIGHT, MAX_STREAM_WIDTH,
    MIN_STREAM_HEIGHT, MIN_STREAM_WIDTH,
};

const STABILITY_DELAY: Duration = Duration::from_millis(450);
const RATE_LIMIT: Duration = Duration::from_secs(1);
const MIN_DELTA_PX: u32 = 8;

/// Debounced, rate-limited tracker that emits a `DisplayUpdateMsg` when the
/// viewer surface diverges from the streaming resolution by enough to matter.
pub struct DisplayFitTracker {
    sequence: u64,
    stable_candidate: Option<([u32; 2], Instant)>,
    last_sent_at: Option<Instant>,
    last_sent_size: Option<[u32; 2]>,
    rejected: Option<[u32; 2]>,
}

impl Default for DisplayFitTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplayFitTracker {
    pub fn new() -> Self {
        Self {
            sequence: 0,
            stable_candidate: None,
            last_sent_at: None,
            last_sent_size: None,
            rejected: None,
        }
    }

    /// Reset ephemeral state on session start / resume. Sequence is intentionally
    /// kept so the host never sees a monotonically regressing sequence number.
    pub fn reset(&mut self) {
        self.stable_candidate = None;
        self.last_sent_at = None;
        self.last_sent_size = None;
        self.rejected = None;
    }

    /// Convert surface points + HiDPI multiplier into an encoder-safe size.
    pub fn prepare(width_pts: f32, height_pts: f32, scale: f32) -> [u32; 2] {
        let w =
            ((width_pts * scale).round() as u32 / 4 * 4).clamp(MIN_STREAM_WIDTH, MAX_STREAM_WIDTH);
        let h = ((height_pts * scale).round() as u32 / 2 * 2)
            .clamp(MIN_STREAM_HEIGHT, MAX_STREAM_HEIGHT);
        [w, h]
    }

    /// Feed the current observed surface size each frame. Returns a message to
    /// send when all conditions are satisfied:
    /// - `gate` is true (host supports + mode allows + in session + frame fresh)
    /// - Δ ≥ 8 px versus the current streaming size
    /// - The candidate has been stable for 450 ms
    /// - At least 1 s has elapsed since the last send
    /// - The size was not rejected by the host
    pub fn poll(
        &mut self,
        observed: [u32; 2],
        actual: Option<[u32; 2]>,
        gate: bool,
        scale: f32,
        reason: &str,
    ) -> Option<DisplayUpdateMsg> {
        if !gate {
            self.stable_candidate = None;
            return None;
        }

        let [ow, oh] = observed;
        let [rw, rh] = actual.unwrap_or(observed);

        // Within delta threshold — nothing to do.
        if ow.abs_diff(rw) < MIN_DELTA_PX && oh.abs_diff(rh) < MIN_DELTA_PX {
            self.stable_candidate = None;
            return None;
        }

        // Don't retry a size the host already rejected.
        if self.rejected == Some(observed) {
            self.stable_candidate = None;
            return None;
        }

        let now = Instant::now();

        // Stability gate: candidate must be unchanged for STABILITY_DELAY.
        match self.stable_candidate {
            Some(([cw, ch], at)) if cw == ow && ch == oh => {
                if now.duration_since(at) < STABILITY_DELAY {
                    return None;
                }
            }
            _ => {
                self.stable_candidate = Some((observed, now));
                return None;
            }
        }

        // Rate limit.
        if let Some(last) = self.last_sent_at {
            if now.duration_since(last) < RATE_LIMIT {
                return None;
            }
        }

        self.sequence += 1;
        self.last_sent_at = Some(now);
        self.last_sent_size = Some(observed);
        self.stable_candidate = None;

        Some(DisplayUpdateMsg {
            msg_type: DISPLAY_UPDATE.to_string(),
            sequence: self.sequence,
            width: ow,
            height: oh,
            scale,
            reason: reason.to_string(),
        })
    }

    /// Apply a `DisplayUpdateResultMsg` from the host. On rejection, latch the
    /// rejected size so we don't immediately retry.
    pub fn handle_result(&mut self, result: &DisplayUpdateResultMsg) {
        if result.accepted {
            self.rejected = None;
        } else if result.sequence == self.sequence {
            if let Some(sent) = self.last_sent_size {
                self.rejected = Some(sent);
                tracing::warn!(
                    target: "arcen::display",
                    width = result.width,
                    height = result.height,
                    message = result.message.as_str(),
                    "display_update rejected by host",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> bool {
        true
    }

    #[test]
    fn no_send_below_delta_threshold() {
        let mut t = DisplayFitTracker::new();
        // actual == observed → no delta
        let msg = t.poll([1920, 1080], Some([1920, 1080]), gate(), 1.0, "test");
        assert!(msg.is_none());
    }

    #[test]
    fn no_send_before_stability_delay() {
        let mut t = DisplayFitTracker::new();
        // First call starts candidate timer
        let msg = t.poll([1512, 945], Some([1920, 1080]), gate(), 1.0, "test");
        assert!(msg.is_none(), "should not send before stability window");
    }

    #[test]
    fn no_send_when_gate_is_false() {
        let mut t = DisplayFitTracker::new();
        let msg = t.poll([1512, 945], Some([1920, 1080]), false, 1.0, "test");
        assert!(msg.is_none());
    }

    #[test]
    fn rejected_size_suppressed() {
        let mut t = DisplayFitTracker::new();
        // Simulate a rejection being latched
        t.rejected = Some([1512, 945]);
        let msg = t.poll([1512, 945], Some([1920, 1080]), gate(), 1.0, "test");
        assert!(msg.is_none(), "rejected size should be suppressed");
    }

    #[test]
    fn prepare_aligns_for_encoder_and_clamps() {
        let [w, h] = DisplayFitTracker::prepare(1512.0, 945.3, 1.0);
        assert_eq!(w % 4, 0);
        assert_eq!(h % 2, 0);
        assert_eq!(DisplayFitTracker::prepare(1398.0, 760.0, 1.0), [1396, 760]);
        // Below min
        let [w2, h2] = DisplayFitTracker::prepare(100.0, 100.0, 1.0);
        assert_eq!(w2, MIN_STREAM_WIDTH);
        assert_eq!(h2, MIN_STREAM_HEIGHT);
    }

    #[test]
    fn handle_result_clears_rejected_on_accept() {
        let mut t = DisplayFitTracker::new();
        t.rejected = Some([1512, 945]);
        use crate::protocol::messages::{DisplayUpdateResultMsg, DISPLAY_UPDATE_RESULT};
        t.handle_result(&DisplayUpdateResultMsg {
            msg_type: DISPLAY_UPDATE_RESULT.to_string(),
            sequence: 0,
            accepted: true,
            width: 1512,
            height: 945,
            message: String::new(),
        });
        assert!(t.rejected.is_none());
    }

    #[test]
    fn reset_clears_ephemeral_state() {
        let mut t = DisplayFitTracker::new();
        t.sequence = 5;
        t.rejected = Some([100, 100]);
        t.last_sent_at = Some(Instant::now());
        t.reset();
        assert!(t.rejected.is_none());
        assert!(t.last_sent_at.is_none());
        assert_eq!(t.sequence, 5, "sequence must survive reset");
    }
}
