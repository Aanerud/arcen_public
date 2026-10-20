use core::time::Duration;

/// Paces the submissions of a capture source that delivers frames on its own
/// clock (a display head's refresh) so that none is lost to the encoder's
/// rate limit.
///
/// Sampling such a source with a fixed tick at the contract rate loses frames
/// whenever the two clocks run at the same rate: a frame that jitters past a
/// tick lands in the same interval as the next one and overwrites it, while
/// the following tick finds nothing new. Measured on an Xorg head refreshing
/// at 60 Hz with a 60 fps Speed contract: 60 new frames a second captured,
/// 40 encoded.
///
/// The pacer submits fresh content as it arrives instead, holding a frame
/// back only while it is more than half an interval ahead of its slot. The
/// next slot is one interval after the later of the current slot and the
/// submission, so slots follow late arrivals and arrival jitter up to a whole
/// interval loses nothing, while a fresh submission always advances the slot
/// by at least one interval and the long-run rate never exceeds the contract.
/// Submissions without fresh content (keepalives, encoder pipeline flushes)
/// wait until half an interval past the slot, so a frame arriving a little
/// late is not preceded by a duplicate, and do not move it.
///
/// Times are offsets from any fixed origin the caller chooses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArrivalPacer {
    interval: Duration,
    next: Option<Duration>,
}

impl ArrivalPacer {
    /// A pacer for one frame per `interval`.
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: None,
        }
    }

    /// The contract interval.
    #[must_use]
    pub const fn interval(self) -> Duration {
        self.interval
    }

    /// Changes the contract interval from the next slot on.
    pub const fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Forgets the slot; the next fresh frame is submitted at once.
    pub const fn reset(&mut self) {
        self.next = None;
    }

    /// Half an interval, rounded up.
    fn half_interval(self) -> Duration {
        self.interval.saturating_sub(self.interval / 2)
    }

    /// Whether a submission carrying fresh content may go at `now`.
    #[must_use]
    pub fn fresh_due(self, now: Duration) -> bool {
        self.next
            .is_none_or(|next| now.saturating_add(self.half_interval()) >= next)
    }

    /// Whether a submission without fresh content (a keepalive or encoder
    /// pipeline flush) may go at `now`: only once a fresh frame due at the
    /// slot is half an interval late.
    #[must_use]
    pub fn slot_due(self, now: Duration) -> bool {
        self.next
            .is_none_or(|next| now >= next.saturating_add(self.half_interval()))
    }

    /// Records a submission of fresh content at `now`.
    pub fn on_fresh_submitted(&mut self, now: Duration) {
        // An early frame keeps the grid; a late one (or the first after
        // idle) moves it, so the source's own clock sets the phase.
        let after_now = now.saturating_add(self.interval);
        let following = self.next.map_or(after_now, |next| {
            next.saturating_add(self.interval).max(after_now)
        });
        self.next = Some(following);
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::unchecked_time_subtraction
)]
mod tests {
    use super::ArrivalPacer;
    use core::time::Duration;

    const SIXTY: Duration = Duration::from_nanos(16_666_667);
    const POLL: Duration = Duration::from_micros(500);

    /// Deterministic arrival jitter in `[-span/2, span/2]`.
    fn jitter(index: u64, span: Duration) -> i64 {
        let mut x = index.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03;
        x ^= x >> 31;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 29;
        let span = i64::try_from(span.as_nanos()).expect("span fits");
        i64::try_from(x % u64::try_from(span + 1).expect("positive")).expect("fits") - span / 2
    }

    fn arrivals(period: Duration, count: u64, span: Duration, phase: Duration) -> Vec<Duration> {
        let period = i64::try_from(period.as_nanos()).expect("fits");
        let phase = i64::try_from(phase.as_nanos()).expect("fits");
        (0..count)
            .map(|index| {
                let base = i64::try_from(index).expect("fits") * period + phase + 20_000_000;
                Duration::from_nanos(u64::try_from(base + jitter(index, span)).expect("positive"))
            })
            .collect()
    }

    struct Outcome {
        submitted: Vec<Duration>,
        lost: usize,
    }

    /// A capture loop polling every [`POLL`]: the newest arrived frame
    /// overwrites one not yet submitted.
    fn run(arrivals: &[Duration], mut submit_due: impl FnMut(Duration, bool) -> bool) -> Outcome {
        let end = *arrivals.last().expect("frames") + Duration::from_millis(100);
        let mut next_arrival = 0;
        let mut pending = false;
        let mut lost = 0;
        let mut submitted = Vec::new();
        let mut now = Duration::ZERO;
        while now < end {
            while next_arrival < arrivals.len() && arrivals[next_arrival] <= now {
                if pending {
                    lost += 1;
                }
                pending = true;
                next_arrival += 1;
            }
            if submit_due(now, pending) && pending {
                pending = false;
                submitted.push(now);
            }
            now += POLL;
        }
        Outcome { submitted, lost }
    }

    fn paced(arrivals: &[Duration], interval: Duration) -> Outcome {
        let mut pacer = ArrivalPacer::new(interval);
        run(arrivals, |now, fresh| {
            if fresh && pacer.fresh_due(now) {
                pacer.on_fresh_submitted(now);
                true
            } else {
                false
            }
        })
    }

    /// The loop this replaces: one decision per contract tick.
    fn ticked(arrivals: &[Duration], interval: Duration) -> Outcome {
        let mut next = Duration::ZERO;
        run(arrivals, |now, _| {
            if now >= next {
                next += interval;
                true
            } else {
                false
            }
        })
    }

    #[test]
    fn a_jittery_source_at_the_contract_rate_loses_frames_to_a_fixed_tick() {
        // The measured failure: both clocks at 60, arrivals near the tick.
        let frames = arrivals(
            SIXTY,
            600,
            Duration::from_millis(6),
            Duration::from_nanos(13_333_333),
        );
        let outcome = ticked(&frames, SIXTY);
        assert!(outcome.lost > 100, "lost {}", outcome.lost);
    }

    #[test]
    fn the_pacer_loses_nothing_from_a_jittery_source_at_the_contract_rate() {
        for phase_ms in [0, 4, 8, 12, 16] {
            let frames = arrivals(
                SIXTY,
                600,
                Duration::from_millis(12),
                Duration::from_millis(phase_ms),
            );
            let outcome = paced(&frames, SIXTY);
            assert_eq!(outcome.lost, 0, "phase {phase_ms} ms");
            assert_eq!(outcome.submitted.len(), 600);
        }
    }

    #[test]
    fn fresh_frames_go_out_as_they_arrive() {
        let frames = arrivals(SIXTY, 300, Duration::from_millis(6), Duration::ZERO);
        let outcome = paced(&frames, SIXTY);
        for (arrived, sent) in frames.iter().zip(&outcome.submitted) {
            assert!(*sent - *arrived <= POLL, "held {:?}", *sent - *arrived);
        }
    }

    #[test]
    fn a_faster_source_is_held_to_the_contract_rate() {
        let frames = arrivals(SIXTY / 2, 1200, Duration::from_millis(2), Duration::ZERO);
        let outcome = paced(&frames, SIXTY);
        let span = *outcome.submitted.last().expect("sent") - outcome.submitted[0];
        let rate = (outcome.submitted.len() - 1) as f64 / span.as_secs_f64();
        assert!(rate <= 60.5, "rate {rate}");
        assert!(rate >= 59.0, "rate {rate}");
        for pair in outcome.submitted[1..].windows(2) {
            assert!(
                pair[1] - pair[0] >= SIXTY / 2,
                "burst {:?}",
                pair[1] - pair[0]
            );
        }
    }

    /// Flushes a loop would send between fresh frames: one is owed after each
    /// fresh submission and goes once the slot allows it.
    fn flushes(arrivals: &[Duration], interval: Duration) -> usize {
        let mut pacer = ArrivalPacer::new(interval);
        let mut flush_owed = false;
        let mut flushes = 0;
        run(arrivals, |now, fresh| {
            if fresh && pacer.fresh_due(now) {
                pacer.on_fresh_submitted(now);
                flush_owed = true;
                true
            } else {
                if !fresh && flush_owed && pacer.slot_due(now) {
                    flush_owed = false;
                    flushes += 1;
                }
                false
            }
        })
        .submitted
        .len();
        flushes
    }

    #[test]
    fn a_steady_source_is_not_padded_with_duplicates() {
        // Measured before: each 60 Hz frame landing just after its slot was
        // preceded by a flush, 120 encodes a second for 60 frames.
        for phase_ms in [0, 4, 8, 12, 16] {
            let frames = arrivals(
                SIXTY,
                600,
                Duration::from_millis(4),
                Duration::from_millis(phase_ms),
            );
            // Only the flush after the last frame.
            assert_eq!(flushes(&frames, SIXTY), 1, "phase {phase_ms} ms");
        }
    }

    #[test]
    fn a_frame_ahead_of_its_slot_waits_for_it_and_is_then_sent() {
        let mut pacer = ArrivalPacer::new(SIXTY);
        let start = Duration::from_secs(1);
        assert!(pacer.fresh_due(start));
        pacer.on_fresh_submitted(start);
        let early = start + Duration::from_millis(5);
        assert!(!pacer.fresh_due(early));
        assert!(pacer.fresh_due(start + SIXTY / 2));
    }

    #[test]
    fn non_fresh_submissions_wait_for_the_slot() {
        let mut pacer = ArrivalPacer::new(SIXTY);
        let start = Duration::from_secs(1);
        assert!(pacer.slot_due(start));
        pacer.on_fresh_submitted(start);
        assert!(!pacer.slot_due(start + SIXTY));
        assert!(pacer.slot_due(start + SIXTY + SIXTY / 2 + Duration::from_nanos(1)));
    }

    #[test]
    fn after_idle_the_next_frame_goes_at_once() {
        let mut pacer = ArrivalPacer::new(SIXTY);
        let start = Duration::from_secs(1);
        pacer.on_fresh_submitted(start);
        let later = start + Duration::from_secs(2);
        assert!(pacer.fresh_due(later));
        pacer.on_fresh_submitted(later);
        assert!(!pacer.fresh_due(later + Duration::from_millis(1)));
        assert!(pacer.fresh_due(later + SIXTY));
    }

    #[test]
    fn reset_and_interval_changes_take_effect() {
        let mut pacer = ArrivalPacer::new(SIXTY);
        let start = Duration::from_secs(1);
        pacer.on_fresh_submitted(start);
        pacer.reset();
        assert!(pacer.fresh_due(start));
        pacer.set_interval(SIXTY * 2);
        assert_eq!(pacer.interval(), SIXTY * 2);
        pacer.on_fresh_submitted(start);
        assert!(!pacer.fresh_due(start + SIXTY / 2));
        assert!(pacer.fresh_due(start + SIXTY));
    }
}
