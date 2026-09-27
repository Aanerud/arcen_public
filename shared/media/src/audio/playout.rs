//! How much audio a client holds before it plays, chosen from the path.
//!
//! A fixed playout target is wrong on any path but the one it was tuned for.
//! Measured on a 34 ms WAN with a still desktop, a 70 ms target (110 ms after a
//! rebuffer) still underran nine times a minute: every gap longer than the
//! reserve is heard as a click and a rebuffer. On a LAN the same reserve is
//! pure added latency.
//!
//! So the target follows what the path does, the way adaptive jitter buffers
//! in voice clients do: an underrun proves the reserve was too small and raises
//! it at once; a long calm lowers it slowly. Latency is only spent where the
//! path has shown it needs it, and given back when it stops.
//!
//! Pure and clock-free: the caller passes elapsed time, so it is tested exactly
//! and any client uses the same rule.

use std::time::Duration;

/// The rule, in milliseconds of queued audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutPolicy {
    /// The smallest target, used on a calm path.
    pub min_target_ms: u16,
    /// The largest target an unstable path may push it to.
    pub max_target_ms: u16,
    /// How much one underrun raises the target.
    pub raise_ms: u16,
    /// How much one calm period lowers it.
    pub lower_ms: u16,
    /// How long without an underrun counts as calm.
    pub calm_period: Duration,
    /// How far above the target the queue may grow before it is trimmed.
    pub trim_headroom_ms: u16,
}

impl Default for PlayoutPolicy {
    fn default() -> Self {
        Self {
            min_target_ms: 60,
            max_target_ms: 260,
            raise_ms: 40,
            lower_ms: 10,
            calm_period: Duration::from_secs(15),
            trim_headroom_ms: 90,
        }
    }
}

/// The current playout target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayoutTarget {
    policy: PlayoutPolicy,
    target_ms: u16,
    calm_for: Duration,
}

impl PlayoutTarget {
    /// Starts at the policy's minimum.
    #[must_use]
    pub const fn new(policy: PlayoutPolicy) -> Self {
        Self {
            policy,
            target_ms: policy.min_target_ms,
            calm_for: Duration::ZERO,
        }
    }

    /// How much audio to build before playing.
    #[must_use]
    pub const fn target_ms(&self) -> u16 {
        self.target_ms
    }

    /// The queue length above which audio is trimmed back towards the target.
    #[must_use]
    pub const fn trim_threshold_ms(&self) -> u16 {
        self.target_ms.saturating_add(self.policy.trim_headroom_ms)
    }

    /// Records an underrun: the reserve was too small for this path.
    pub fn record_underrun(&mut self) {
        self.target_ms = self
            .target_ms
            .saturating_add(self.policy.raise_ms)
            .min(self.policy.max_target_ms);
        self.calm_for = Duration::ZERO;
    }

    /// Advances time with no underrun; lowers the target after each calm
    /// period.
    pub fn record_calm(&mut self, elapsed: Duration) {
        self.calm_for = self.calm_for.saturating_add(elapsed);
        while self.calm_for >= self.policy.calm_period && self.target_ms > self.policy.min_target_ms
        {
            self.calm_for = self.calm_for.saturating_sub(self.policy.calm_period);
            self.target_ms = self
                .target_ms
                .saturating_sub(self.policy.lower_ms)
                .max(self.policy.min_target_ms);
        }
        if self.target_ms == self.policy.min_target_ms {
            self.calm_for = self.calm_for.min(self.policy.calm_period);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_calm_path_keeps_the_smallest_target() {
        let mut target = PlayoutTarget::new(PlayoutPolicy::default());
        target.record_calm(Duration::from_secs(600));
        assert_eq!(target.target_ms(), 60);
        assert_eq!(target.trim_threshold_ms(), 150);
    }

    #[test]
    fn each_underrun_raises_the_reserve_up_to_the_cap() {
        let mut target = PlayoutTarget::new(PlayoutPolicy::default());
        target.record_underrun();
        assert_eq!(target.target_ms(), 100);
        target.record_underrun();
        assert_eq!(target.target_ms(), 140);
        for _ in 0..20 {
            target.record_underrun();
        }
        assert_eq!(target.target_ms(), 260);
    }

    #[test]
    fn calm_gives_latency_back_slowly() {
        let mut target = PlayoutTarget::new(PlayoutPolicy::default());
        target.record_underrun();
        target.record_underrun();
        assert_eq!(target.target_ms(), 140);
        target.record_calm(Duration::from_secs(14));
        assert_eq!(target.target_ms(), 140, "not calm for long enough yet");
        target.record_calm(Duration::from_secs(1));
        assert_eq!(target.target_ms(), 130);
        target.record_calm(Duration::from_secs(120));
        assert_eq!(target.target_ms(), 60);
    }

    #[test]
    fn an_underrun_restarts_the_calm_clock() {
        let mut target = PlayoutTarget::new(PlayoutPolicy::default());
        target.record_underrun();
        target.record_calm(Duration::from_secs(14));
        target.record_underrun();
        target.record_calm(Duration::from_secs(14));
        assert_eq!(target.target_ms(), 140);
    }
}
