// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! BBR with a loss ceiling: Arcen's congestion controller for interactive
//! video.
//!
//! Quinn's BBR models the bottleneck from delivery rate and RTT, so the
//! random loss of Wi-Fi, cellular and VPN paths does not collapse its window
//! the way it collapses Cubic's. It also ignores loss entirely: measured
//! against a shallow-buffered 8 Mbit/s bottleneck it kept sending at about
//! twice the link rate and lost more than half of its packets, and the
//! reliable video stream then waited on retransmissions for over a second.
//!
//! [`LossGuardedBbrConfig`] keeps BBR and adds one rule, the idea of the
//! later BBR revisions' loss-bounded in-flight ceiling:
//!
//! - A round lasts at least one smoothed RTT and until
//!   [`MIN_ROUND_BYTES`] have been resolved (acknowledged or lost), so a
//!   small window or a quiet desktop still produces judgeable rounds; a round
//!   that cannot gather that much within [`MAX_ROUND_RTTS`] RTTs is closed
//!   unjudged.
//! - A round is lossy when at least [`MIN_ROUND_LOST_BYTES`] and more than
//!   [`LOSS_CEILING_THRESHOLD_PERMILLE`] of its resolved bytes were lost.
//! - After [`LOSSY_ROUNDS_TO_ENGAGE`] lossy rounds in a row, the window is
//!   capped at three quarters of the lower of the current cap and what that
//!   round delivered (a round that overflowed a queue delivered the link rate
//!   times the queue-inflated RTT, so headroom above it would keep the queue
//!   full), never below [`CEILING_FLOOR_BYTES`]. Whether the sender was
//!   application-limited does not matter: Quinn marks a connection
//!   application-limited whenever its send buffer was empty at the last poll,
//!   which a video sender's is between frames, so an overshooting video
//!   sender is "application-limited" in nearly every round.
//! - A clean round relaxes the cap by an eighth while the round's largest
//!   bytes in flight came within [`CEILING_RELEASE_PERCENT`] of it; a clean
//!   round, or a loss-free stretch too quiet to judge, whose largest bytes in
//!   flight stayed below that share removes the cap, because the cap was not
//!   what limited the sender. The loss rule re-engages within two rounds if
//!   the path is still congested.
//!
//! A random loss floor below the threshold never engages it, so BBR's
//! behaviour on lossy-but-uncongested paths is unchanged.

use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::congestion::{BbrConfig, Controller, ControllerFactory, ControllerMetrics};

/// Lost share of one round's resolved bytes, in permille, above which the
/// round counts as congestion rather than random loss. Five percent sits
/// well above the few tenths of a percent to low percent that Wi-Fi and VPN
/// paths drop at random, and well below the 20-50 % an overshooting BBR
/// causes at a shallow bottleneck.
pub const LOSS_CEILING_THRESHOLD_PERMILLE: u64 = 50;

/// Consecutive lossy rounds before the ceiling acts. One round of a few dozen
/// packets crosses 5 % by chance on a path with 1 % random loss about once
/// in fifty rounds; two in a row almost never do, while an overshooting BBR
/// loses in every round.
pub const LOSSY_ROUNDS_TO_ENGAGE: u32 = 2;

/// Bytes a round must resolve before it is judged.
pub const MIN_ROUND_BYTES: u64 = 10 * 1200;

/// Lost bytes a round needs, besides the share, to count as lossy: three
/// packets, so one or two unlucky packets in a small round never do.
pub const MIN_ROUND_LOST_BYTES: u64 = 3 * 1200;

/// Smoothed RTTs a round may span while gathering [`MIN_ROUND_BYTES`].
pub const MAX_ROUND_RTTS: u32 = 8;

/// The lowest cap: twice [`MIN_ROUND_BYTES`], so a capped window still
/// resolves a judgeable round every RTT and can always relax.
pub const CEILING_FLOOR_BYTES: u64 = 2 * MIN_ROUND_BYTES;

/// A cap is removed only when a round's largest bytes in flight stayed
/// below this share of it: well below, so a sender pressing against the cap
/// keeps it and only relaxes it step by step.
pub const CEILING_RELEASE_PERCENT: u64 = 50;

/// The ceiling set after a lossy round, relative to the bytes that round
/// delivered: enough below it to drain the queue the overshoot built.
const CEILING_BACKOFF_NUMERATOR: u64 = 3;
const CEILING_BACKOFF_DENOMINATOR: u64 = 4;

/// The pure round accounting behind [`LossGuardedBbr`].
#[derive(Debug, Clone, Default)]
pub struct LossCeiling {
    round_started: Option<Instant>,
    round_acked: u64,
    round_lost: u64,
    round_max_in_flight: u64,
    lossy_rounds: u32,
    ceiling: Option<u64>,
}

impl LossCeiling {
    /// Bytes newly acknowledged.
    pub fn on_ack(&mut self, now: Instant, bytes: u64) {
        self.round_started.get_or_insert(now);
        self.round_acked = self.round_acked.saturating_add(bytes);
    }

    /// Bytes in flight now; the round keeps the largest value it saw.
    pub fn on_in_flight(&mut self, bytes: u64) {
        self.round_max_in_flight = self.round_max_in_flight.max(bytes);
    }

    /// Bytes declared lost.
    pub fn on_loss(&mut self, now: Instant, bytes: u64) {
        self.round_started.get_or_insert(now);
        self.round_lost = self.round_lost.saturating_add(bytes);
    }

    /// Closes the round once it has lasted one smoothed RTT and resolved
    /// [`MIN_ROUND_BYTES`] (or reached [`MAX_ROUND_RTTS`]), and judges it.
    /// `controller_window` is the wrapped controller's own window, so a
    /// relaxed ceiling that no longer binds is dropped. `min_window` is the
    /// controller's floor.
    pub fn end_acks(
        &mut self,
        now: Instant,
        smoothed_rtt: Duration,
        controller_window: u64,
        min_window: u64,
    ) {
        let Some(started) = self.round_started else {
            return;
        };
        let elapsed = now.saturating_duration_since(started);
        if elapsed < smoothed_rtt {
            return;
        }
        let resolved = self.round_acked.saturating_add(self.round_lost);
        if resolved < MIN_ROUND_BYTES {
            if elapsed < smoothed_rtt.saturating_mul(MAX_ROUND_RTTS) {
                return;
            }
            // Too little traffic to judge: a loss-free stretch that stayed
            // well below the cap means the cap was not what limited the
            // sender, so it goes; a lossy one is ignored rather than allowed
            // to tighten anything.
            if self.round_lost == 0 && self.cap_was_unused() {
                self.ceiling = None;
                self.lossy_rounds = 0;
            }
            self.reset_round();
            return;
        }
        let lossy = self.round_lost >= MIN_ROUND_LOST_BYTES
            && self.round_lost.saturating_mul(1000)
                > resolved.saturating_mul(LOSS_CEILING_THRESHOLD_PERMILLE);
        if lossy {
            self.lossy_rounds = self.lossy_rounds.saturating_add(1);
            if self.lossy_rounds >= LOSSY_ROUNDS_TO_ENGAGE {
                let ceiling = (self
                    .ceiling
                    .unwrap_or(controller_window)
                    .min(self.round_acked)
                    .saturating_mul(CEILING_BACKOFF_NUMERATOR)
                    / CEILING_BACKOFF_DENOMINATOR)
                    .max(min_window)
                    .max(CEILING_FLOOR_BYTES);
                self.ceiling = (ceiling < controller_window).then_some(ceiling);
            }
        } else {
            self.lossy_rounds = 0;
            if self.cap_was_unused() {
                self.ceiling = None;
            } else if let Some(ceiling) = self.ceiling {
                let relaxed = ceiling.saturating_add((ceiling / 8).max(min_window / 4));
                self.ceiling = (relaxed < controller_window).then_some(relaxed);
            }
        }
        self.reset_round();
    }

    /// Whether this round's largest bytes in flight stayed well below the
    /// cap (true when there is no cap).
    fn cap_was_unused(&self) -> bool {
        self.ceiling.is_none_or(|ceiling| {
            self.round_max_in_flight.saturating_mul(100)
                < ceiling.saturating_mul(CEILING_RELEASE_PERCENT)
        })
    }

    fn reset_round(&mut self) {
        self.round_started = None;
        self.round_acked = 0;
        self.round_lost = 0;
        self.round_max_in_flight = 0;
    }

    /// The current ceiling, if a lossy round set one.
    #[must_use]
    pub const fn ceiling(&self) -> Option<u64> {
        self.ceiling
    }

    /// `controller_window` limited by the ceiling, never below `min_window`.
    #[must_use]
    pub fn window(&self, controller_window: u64, min_window: u64) -> u64 {
        self.ceiling
            .map_or(controller_window, |ceiling| controller_window.min(ceiling))
            .max(min_window)
    }
}

/// Builds [`LossGuardedBbr`] controllers.
#[derive(Debug, Clone, Default)]
pub struct LossGuardedBbrConfig {
    bbr: Arc<BbrConfig>,
}

impl ControllerFactory for LossGuardedBbrConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(LossGuardedBbr {
            inner: Arc::clone(&self.bbr).build(now, current_mtu),
            guard: LossCeiling::default(),
            smoothed_rtt: Duration::ZERO,
            in_flight: 0,
            min_window: min_window(current_mtu),
        })
    }
}

fn min_window(mtu: u16) -> u64 {
    4 * u64::from(mtu)
}

/// Quinn's BBR under a loss-bounded window ceiling; see the module docs.
pub struct LossGuardedBbr {
    inner: Box<dyn Controller>,
    guard: LossCeiling,
    smoothed_rtt: Duration,
    /// Bytes in flight: Quinn's exact count at each end of acknowledgements,
    /// plus what was sent since.
    in_flight: u64,
    min_window: u64,
}

impl std::fmt::Debug for LossGuardedBbr {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LossGuardedBbr")
            .field("window", &self.window())
            .field("guard", &self.guard)
            .field("smoothed_rtt", &self.smoothed_rtt)
            .finish_non_exhaustive()
    }
}

impl Controller for LossGuardedBbr {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        self.in_flight = self.in_flight.saturating_add(bytes);
        self.guard.on_in_flight(self.in_flight);
        self.inner.on_sent(now, bytes, last_packet_number);
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &quinn_proto::RttEstimator,
    ) {
        self.smoothed_rtt = rtt.get();
        self.guard.on_ack(now, bytes);
        self.inner.on_ack(now, sent, bytes, app_limited, rtt);
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        self.inner
            .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
        self.in_flight = in_flight;
        self.guard
            .end_acks(now, self.smoothed_rtt, self.inner.window(), self.min_window);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.guard.on_loss(now, lost_bytes);
        self.inner
            .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.min_window = min_window(new_mtu);
        self.inner.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        self.guard.window(self.inner.window(), self.min_window)
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = self.inner.metrics();
        metrics.congestion_window = self.window();
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            inner: self.inner.clone_box(),
            guard: self.guard.clone(),
            smoothed_rtt: self.smoothed_rtt,
            in_flight: self.in_flight,
            min_window: self.min_window,
        })
    }

    fn initial_window(&self) -> u64 {
        self.inner.initial_window()
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{CEILING_FLOOR_BYTES, LossCeiling, MAX_ROUND_RTTS};
    use std::time::{Duration, Instant};

    const RTT: Duration = Duration::from_millis(40);
    const MIN: u64 = 4 * 1200;
    const BBR_WINDOW: u64 = 1_500_000;

    /// One judged round in which the sender filled `acked + lost` bytes.
    fn round(guard: &mut LossCeiling, start: Instant, acked: u64, lost: u64, window: u64) {
        guard.on_in_flight(acked + lost);
        guard.on_ack(start, acked);
        guard.on_loss(start, lost);
        guard.end_acks(start + RTT, RTT, window, MIN);
    }

    #[test]
    fn random_loss_below_the_threshold_never_caps_bbr() {
        let mut guard = LossCeiling::default();
        let start = Instant::now();
        // 3 % loss: a lossy Wi-Fi/VPN floor, not congestion.
        round(&mut guard, start, 970_000, 30_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), None);
        assert_eq!(guard.window(BBR_WINDOW, MIN), BBR_WINDOW);
        // A chance 6 % round between clean ones never engages it either.
        round(&mut guard, start + RTT * 2, 940_000, 60_000, BBR_WINDOW);
        round(&mut guard, start + RTT * 4, 990_000, 10_000, BBR_WINDOW);
        round(&mut guard, start + RTT * 6, 940_000, 60_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), None);
    }

    #[test]
    fn a_lossy_round_caps_the_window_near_what_was_delivered_then_relaxes() {
        let mut guard = LossCeiling::default();
        let start = Instant::now();
        // An overshooting BBR: half of every round lost. One lossy round may
        // be chance; the second engages the ceiling.
        round(&mut guard, start, 80_000, 80_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), None);
        round(&mut guard, start + RTT * 2, 80_000, 80_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(60_000));
        assert_eq!(guard.window(BBR_WINDOW, MIN), 60_000);
        // Another lossy round backs off from the lower of ceiling and
        // delivery, never below the floor.
        round(&mut guard, start + RTT * 4, 50_000, 20_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(37_500));
        round(&mut guard, start + RTT * 6, 20_000, 20_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(CEILING_FLOOR_BYTES));
        // Clean rounds that fill the cap relax it by an eighth until it no
        // longer binds.
        round(&mut guard, start + RTT * 8, 24_000, 0, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(27_000));
        let mut at = start + RTT * 10;
        for _ in 0..40 {
            let cap = guard.window(200_000, MIN);
            round(&mut guard, at, cap, 0, 200_000);
            at += RTT * 2;
        }
        assert_eq!(guard.ceiling(), None);
        assert_eq!(guard.window(200_000, MIN), 200_000);
    }

    #[test]
    fn a_sender_pressing_against_the_cap_keeps_it_and_one_well_below_frees_it() {
        let mut guard = LossCeiling::default();
        let start = Instant::now();
        round(&mut guard, start, 80_000, 80_000, BBR_WINDOW);
        round(&mut guard, start + RTT * 2, 80_000, 80_000, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(60_000));
        // Clean, and in flight reached the cap: relax, do not remove.
        round(&mut guard, start + RTT * 4, 60_000, 0, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(67_500));
        // Clean, but in flight stayed above half the cap: still relax only.
        round(&mut guard, start + RTT * 6, 40_000, 0, BBR_WINDOW);
        assert_eq!(guard.ceiling(), Some(75_937));
        // Clean, and in flight stayed well below it: the cap goes.
        round(&mut guard, start + RTT * 8, 30_000, 0, BBR_WINDOW);
        assert_eq!(guard.ceiling(), None);
    }

    #[test]
    fn a_small_cap_clears_on_quiet_clean_rounds() {
        // The reviewer's lock: a cap near 7 KB with a quiet desktop.
        let mut guard = LossCeiling {
            ceiling: Some(7_425),
            ..LossCeiling::default()
        };
        let start = Instant::now();
        // 1.4 KB per RTT: never MIN_ROUND_BYTES within one RTT.
        let mut at = start;
        for _ in 0..u64::from(MAX_ROUND_RTTS) {
            guard.on_in_flight(1_400);
            guard.on_ack(at, 1_400);
            at += RTT;
            guard.end_acks(at, RTT, BBR_WINDOW, MIN);
        }
        assert_eq!(guard.ceiling(), None, "a quiet loss-free stretch clears it");
        // And a cap whose rounds do gather enough bytes clears once clean
        // and well below it.
        let mut guard = LossCeiling {
            ceiling: Some(30_000),
            ..LossCeiling::default()
        };
        guard.on_in_flight(6_000);
        guard.on_ack(start, 6_000);
        guard.end_acks(start + RTT, RTT, BBR_WINDOW, MIN);
        assert_eq!(guard.ceiling(), Some(30_000), "round still gathering");
        guard.on_in_flight(6_000);
        guard.on_ack(start + RTT, 6_000);
        guard.end_acks(start + RTT * 2, RTT, BBR_WINDOW, MIN);
        assert_eq!(guard.ceiling(), None);
    }

    /// Deterministic path model: a bottleneck with a queue, random loss and
    /// an application that offers `demand` bytes per RTT.
    struct Path {
        rng: u64,
        capacity: u64,
        buffer: u64,
        loss_permille: u64,
    }

    impl Path {
        fn next(&mut self) -> u64 {
            self.rng ^= self.rng << 13;
            self.rng ^= self.rng >> 7;
            self.rng ^= self.rng << 17;
            self.rng
        }

        /// One RTT: returns `(sent, acked, lost)` for `window` and `demand`
        /// bytes.
        fn round(&mut self, window: u64, demand: u64) -> (u64, u64, u64) {
            let sent = window.min(demand);
            let overflow = sent.saturating_sub(self.capacity + self.buffer);
            let mut lost = overflow;
            for _ in 0..(sent - overflow) / 1200 {
                if self.next() % 1000 < self.loss_permille {
                    lost += 1200;
                }
            }
            let lost = lost.min(sent);
            (sent, sent - lost, lost)
        }
    }

    /// Runs `rounds` RTTs; returns `(sent, lost)` over them.
    fn run(
        guard: &mut LossCeiling,
        path: &mut Path,
        at: &mut Instant,
        rounds: u32,
        demand: u64,
    ) -> (u64, u64) {
        let (mut total_sent, mut total_lost) = (0, 0);
        for _ in 0..rounds {
            let window = guard.window(BBR_WINDOW, MIN);
            let (sent, acked, lost) = path.round(window, demand);
            guard.on_in_flight(sent);
            guard.on_ack(*at, acked);
            guard.on_loss(*at, lost);
            *at += RTT;
            guard.end_acks(*at, RTT, BBR_WINDOW, MIN);
            total_sent += sent;
            total_lost += lost;
        }
        (total_sent, total_lost)
    }

    #[test]
    fn simulated_path_phases() {
        for seed in 1..=8_u64 {
            let mut guard = LossCeiling::default();
            let mut at = Instant::now();
            // 10 s of a quiet desktop (1.2 Mbit/s) on a path with 1 % random
            // loss: random loss never caps.
            let mut path = Path {
                rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
                capacity: 250_000,
                buffer: 100_000,
                loss_permille: 10,
            };
            run(&mut guard, &mut path, &mut at, 250, 6_000);
            assert_eq!(guard.ceiling(), None, "seed {seed}: random loss set a cap");
            // 10 s of a saturating sender squeezed through 400 kbit/s at 8 %
            // loss.
            path.capacity = 2_000;
            path.buffer = 2_000;
            path.loss_permille = 80;
            run(&mut guard, &mut path, &mut at, 250, 2 * BBR_WINDOW);
            assert_eq!(
                guard.ceiling(),
                Some(CEILING_FLOOR_BYTES),
                "seed {seed}: a saturated lossy path pins the cap at its floor"
            );
            // A clean 50 Mbit/s path and a 20 Mbit/s demand: within 5 s the
            // cap must stop limiting it.
            path.capacity = 250_000;
            path.buffer = 100_000;
            path.loss_permille = 0;
            run(&mut guard, &mut path, &mut at, 125, 100_000);
            assert!(
                guard.window(BBR_WINDOW, MIN) >= 100_000,
                "seed {seed}: window stuck at {}",
                guard.window(BBR_WINDOW, MIN)
            );
            // Video demand (24 Mbit/s) between a shallow 8 Mbit/s bottleneck
            // and BBR's window, with 1 % random loss: the sender never fills
            // BBR's window, yet the cap must engage and hold loss far below
            // the half that the uncapped sender loses.
            path.capacity = 40_000;
            path.buffer = 20_000;
            path.loss_permille = 10;
            run(&mut guard, &mut path, &mut at, 50, 120_000);
            let (sent, lost) = run(&mut guard, &mut path, &mut at, 250, 120_000);
            assert!(
                guard.ceiling().is_some_and(|cap| cap < 120_000),
                "seed {seed}: no cap below the demand ({:?})",
                guard.ceiling()
            );
            assert!(
                lost * 100 < sent * 10,
                "seed {seed}: lost {lost} of {sent} under the cap"
            );
            // The desktop goes quiet on the same path: in flight stays well
            // below the cap, so the cap goes.
            run(&mut guard, &mut path, &mut at, 50, 6_000);
            assert_eq!(guard.ceiling(), None, "seed {seed}: an unused cap was kept");
        }
    }
}
