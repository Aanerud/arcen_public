use std::collections::VecDeque;
use std::time::Duration;

const DEFAULT_QUEUE_CAP: usize = 3;
const DEFAULT_STALE_REFRESHES: u64 = 3;
const DEFAULT_STALE_AGE: Duration = Duration::from_millis(50);

/// Why a frame left the display-refresh pacer without ever being presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePacerDropReason {
    /// More than two frames arrived before the display could consume them; the
    /// oldest frame was discarded so the newest two remain available.
    Overflow,
    /// A queued frame survived past the coarse age cap after a stall or hidden
    /// interval and was discarded rather than extending visible latency.
    Stale,
    /// The caller explicitly cleared the queue because the presentation surface
    /// was hidden, rebound or torn down.
    Hidden,
}

/// Static policy knobs for [`FramePacer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePacerConfig {
    pub queue_cap: usize,
    pub stale_refreshes: u64,
    pub stale_age: Duration,
    pub source_period: Option<Duration>,
}

impl Default for FramePacerConfig {
    fn default() -> Self {
        Self {
            queue_cap: DEFAULT_QUEUE_CAP,
            stale_refreshes: DEFAULT_STALE_REFRESHES,
            stale_age: DEFAULT_STALE_AGE,
            source_period: None,
        }
    }
}

impl FramePacerConfig {
    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.queue_cap = self.queue_cap.clamp(1, DEFAULT_QUEUE_CAP);
        self.stale_refreshes = self.stale_refreshes.max(1);
        if self.stale_age.is_zero() {
            self.stale_age = DEFAULT_STALE_AGE;
        }
        self.source_period = self.source_period.filter(|period| !period.is_zero());
        self
    }
}

/// One frame selected for presentation by [`FramePacer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacedFrame<T> {
    pub frame: T,
    pub arrival: Duration,
    /// Estimated latency from decode arrival to the display callback that chose
    /// this frame. This is not a Metal `presentedTime` confirmation.
    pub queued_for: Duration,
    pub refresh_seq: u64,
    pub callback_host_time: Duration,
}

/// The display-refresh decision returned by [`FramePacer::on_refresh`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramePacerRefresh<T> {
    Present(PacedFrame<T>),
    Idle,
}

/// Cumulative counters for a [`FramePacer`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FramePacerCounters {
    pub frames_received: u64,
    pub frames_presented: u64,
    pub dropped_overflow: u64,
    pub dropped_stale: u64,
    pub dropped_hidden: u64,
    pub underruns: u64,
    pub refreshes: u64,
    pub queue_depth_samples: u64,
    pub queue_depth_total: u64,
    pub queue_depth_max: usize,
    pub interval_1x: u64,
    pub interval_2x: u64,
    pub interval_3x_plus: u64,
}

impl FramePacerCounters {
    #[must_use]
    pub const fn never_presented(self) -> u64 {
        self.dropped_overflow
            .saturating_add(self.dropped_stale)
            .saturating_add(self.dropped_hidden)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedFrame<T> {
    frame: T,
    arrival: Duration,
    admitted: Duration,
    arrival_seq: Option<u64>,
}

/// Portable display-refresh pacing for decoded frames.
///
/// This pacer is deliberately sequence-based, not timestamp-slot-based. Each
/// display refresh callback carries a monotonically increasing sequence number;
/// the pacer consumes at most one queued frame on that callback, FIFO. Enqueue
/// keeps a hard cap of three frames by dropping the oldest on overflow. That
/// absorbs 60 Hz input with arrival jitter up to ±8 ms and callback jitter up
/// to ±1 ms while still bounding latency with stale drops. Stale frames are
/// dropped only by coarse age caps so a stall cannot create seconds of visible
/// backlog. Underruns are counted only when a configured source cadence says a
/// frame is due.
#[derive(Debug, Clone)]
pub struct FramePacer<T> {
    queue: VecDeque<QueuedFrame<T>>,
    counters: FramePacerCounters,
    config: FramePacerConfig,
    last_refresh_seq: Option<u64>,
    last_arrival: Option<Duration>,
    last_present_seq: Option<u64>,
    last_present_callback: Option<Duration>,
}

impl<T> Default for FramePacer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> FramePacer<T> {
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(FramePacerConfig::default())
    }

    #[must_use]
    pub fn with_config(config: FramePacerConfig) -> Self {
        let config = config.bounded();
        Self {
            queue: VecDeque::with_capacity(config.queue_cap),
            counters: FramePacerCounters::default(),
            config,
            last_refresh_seq: None,
            last_arrival: None,
            last_present_seq: None,
            last_present_callback: None,
        }
    }

    pub fn enqueue(&mut self, frame: T, arrival: Duration) {
        self.enqueue_with_admission(frame, arrival, arrival);
    }

    pub fn enqueue_replay(&mut self, frame: T, original_arrival: Duration, admission: Duration) {
        self.enqueue_with_admission(frame, original_arrival, admission);
    }

    pub fn set_source_fps(&mut self, fps: Option<u32>) {
        self.config.source_period = fps
            .filter(|fps| *fps > 0)
            .map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps)));
    }

    pub fn reset_epoch(&mut self) {
        self.last_refresh_seq = None;
        self.last_present_seq = None;
        self.last_present_callback = None;
    }

    fn enqueue_with_admission(&mut self, frame: T, arrival: Duration, admitted: Duration) {
        self.counters.frames_received = self.counters.frames_received.saturating_add(1);
        while self.queue.len() >= self.config.queue_cap {
            let _ = self.drop_oldest(FramePacerDropReason::Overflow);
        }
        self.queue.push_back(QueuedFrame {
            frame,
            arrival,
            admitted,
            arrival_seq: self.last_refresh_seq,
        });
        self.last_arrival = Some(arrival);
        self.update_queue_depth_max();
    }

    pub fn on_refresh(
        &mut self,
        refresh_seq: u64,
        callback_host_time: Duration,
        _refresh_period: Duration,
    ) -> FramePacerRefresh<T> {
        if self
            .last_refresh_seq
            .is_some_and(|last_seq| refresh_seq <= last_seq)
        {
            return FramePacerRefresh::Idle;
        }
        let refresh_delta = self
            .last_refresh_seq
            .map_or(1, |last_seq| refresh_seq.saturating_sub(last_seq).max(1));
        self.last_refresh_seq = Some(refresh_seq);
        self.counters.refreshes = self.counters.refreshes.saturating_add(refresh_delta);
        self.sample_queue_depth();
        self.drop_stale(refresh_seq, callback_host_time);

        let Some(queued) = self.queue.pop_front() else {
            if self.frames_are_flowing(callback_host_time, refresh_seq) {
                self.counters.underruns = self.counters.underruns.saturating_add(1);
            }
            return FramePacerRefresh::Idle;
        };

        self.counters.frames_presented = self.counters.frames_presented.saturating_add(1);
        if let Some(previous) = self.last_present_seq {
            match refresh_seq.saturating_sub(previous) {
                0 => {}
                1 => self.counters.interval_1x = self.counters.interval_1x.saturating_add(1),
                2 => self.counters.interval_2x = self.counters.interval_2x.saturating_add(1),
                _ => {
                    self.counters.interval_3x_plus =
                        self.counters.interval_3x_plus.saturating_add(1);
                }
            }
        }
        self.last_present_seq = Some(refresh_seq);
        self.last_present_callback = Some(callback_host_time);
        FramePacerRefresh::Present(PacedFrame {
            queued_for: callback_host_time
                .checked_sub(queued.arrival)
                .unwrap_or(Duration::ZERO),
            frame: queued.frame,
            arrival: queued.arrival,
            refresh_seq,
            callback_host_time,
        })
    }

    /// Records a display refresh on which nothing could be shown because the
    /// presentation surface had no free drawable.
    ///
    /// The refresh sequence still advances, so frames queued meanwhile carry a
    /// current arrival stamp and age by real refreshes; otherwise a wait of
    /// several refreshes would leave every queued frame stamped before it and
    /// the next presentable refresh would discard them all as stale, newest
    /// included. Nothing is consumed, dropped, or counted as an underrun.
    pub fn on_refresh_blocked(&mut self, refresh_seq: u64) {
        if self
            .last_refresh_seq
            .is_some_and(|last_seq| refresh_seq <= last_seq)
        {
            return;
        }
        let refresh_delta = self
            .last_refresh_seq
            .map_or(1, |last_seq| refresh_seq.saturating_sub(last_seq).max(1));
        self.last_refresh_seq = Some(refresh_seq);
        self.counters.refreshes = self.counters.refreshes.saturating_add(refresh_delta);
    }

    pub fn clear(&mut self, reason: FramePacerDropReason) {
        while self.drop_oldest(reason) {}
        self.reset_epoch();
    }

    #[must_use]
    pub const fn counters(&self) -> FramePacerCounters {
        self.counters
    }

    #[must_use]
    pub fn queue_depth(&self) -> usize {
        self.queue.len()
    }

    fn drop_stale(&mut self, refresh_seq: u64, callback_host_time: Duration) {
        loop {
            let Some(oldest) = self.queue.front() else {
                return;
            };
            let stale_by_sequence = oldest.arrival_seq.is_some_and(|arrival_seq| {
                refresh_seq.saturating_sub(arrival_seq) > self.config.stale_refreshes
            });
            let stale_by_age = callback_host_time
                .checked_sub(oldest.admitted)
                .is_some_and(|age| age > self.config.stale_age);
            if !(stale_by_sequence || stale_by_age) {
                return;
            }
            let _ = self.drop_oldest(FramePacerDropReason::Stale);
        }
    }

    fn frames_are_flowing(&self, callback_host_time: Duration, _refresh_seq: u64) -> bool {
        let Some(source_period) = self.config.source_period else {
            return false;
        };
        let due = self
            .last_present_callback
            .and_then(|presented| callback_host_time.checked_sub(presented))
            .is_some_and(|elapsed| {
                elapsed.saturating_add(Duration::from_micros(500)) >= source_period
            });
        if !due {
            return false;
        }
        self.last_arrival.is_some_and(|arrival| {
            callback_host_time
                .checked_sub(arrival)
                .is_some_and(|age| age <= source_period.saturating_mul(2))
        })
    }

    fn drop_oldest(&mut self, reason: FramePacerDropReason) -> bool {
        if self.queue.pop_front().is_none() {
            return false;
        }
        match reason {
            FramePacerDropReason::Overflow => {
                self.counters.dropped_overflow = self.counters.dropped_overflow.saturating_add(1);
            }
            FramePacerDropReason::Stale => {
                self.counters.dropped_stale = self.counters.dropped_stale.saturating_add(1);
            }
            FramePacerDropReason::Hidden => {
                self.counters.dropped_hidden = self.counters.dropped_hidden.saturating_add(1);
            }
        }
        true
    }

    fn sample_queue_depth(&mut self) {
        self.counters.queue_depth_samples = self.counters.queue_depth_samples.saturating_add(1);
        self.counters.queue_depth_total = self
            .counters
            .queue_depth_total
            .saturating_add(u64::try_from(self.queue.len()).unwrap_or(u64::MAX));
        self.update_queue_depth_max();
    }

    fn update_queue_depth_max(&mut self) {
        self.counters.queue_depth_max = self.counters.queue_depth_max.max(self.queue.len());
    }
}

/// One monitor's presentation/accounting input over a telemetry window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationWindow {
    /// Frames admitted to this monitor during the window.
    pub frames_received: u64,
    /// Frames the client presentation loop uploaded/painted for this monitor.
    pub frames_presented_by_client: u64,
    /// Frames rejected before they could become presentable media.
    pub frames_rejected: u64,
    /// Frames deliberately skipped because this display was not due for
    /// another physical refresh.
    pub frames_superseded_by_refresh: u64,
    /// Nominal display refresh rate. `0` means unknown and disables refresh
    /// attribution.
    pub display_refresh_hz: u32,
    /// Window duration.
    pub elapsed: Duration,
}

/// One monitor's classified presentation result over a telemetry window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PresentationWindowReport {
    /// Observed receive rate during the window.
    pub fps_received: f64,
    /// Estimated physically presentable/displayed rate, capped by refresh.
    pub fps_presented: f64,
    /// Total received frames that did not reach physical presentation.
    pub frames_dropped: u64,
    /// Subset of [`Self::frames_dropped`] explained by the display refreshing
    /// more slowly than the stream.
    pub frames_superseded_by_refresh: u64,
    /// Subset of [`Self::frames_dropped`] lost before display refresh was the
    /// limiting factor: decoder/admission rejection, stale latest-frame
    /// replacement, or a client presentation loop that did not keep up.
    pub frames_dropped_before_presentation: u64,
}

/// The presentable cadence for a stream on a particular display.
#[must_use]
pub fn expected_presentable_fps(stream_fps: f64, display_refresh_hz: u32) -> f64 {
    if !stream_fps.is_finite() || stream_fps <= 0.0 {
        return 0.0;
    }
    if display_refresh_hz == 0 {
        stream_fps
    } else {
        stream_fps.min(f64::from(display_refresh_hz))
    }
}

/// Whether a display should be asked to present another frame now.
///
/// `None` means no frame has been presented/requested yet and is always due.
/// A zero/unknown refresh rate disables pacing.
#[must_use]
pub fn refresh_present_due(
    elapsed_since_last_present: Option<Duration>,
    display_refresh_hz: u32,
) -> bool {
    let Some(elapsed) = elapsed_since_last_present else {
        return true;
    };
    if display_refresh_hz == 0 {
        return true;
    }
    let interval_nanos = 1_000_000_000_u128 / u128::from(display_refresh_hz);
    elapsed.as_nanos() >= interval_nanos
}

/// Classifies a telemetry window into display-refresh supersession versus
/// losses before a frame could be physically shown.
///
/// The Deck may run its UI renderer in a non-blocking/Mailbox-like mode so a
/// 30 Hz monitor cannot stall a 120 Hz sibling. In that mode the client can
/// upload every 60 Hz decoded frame to the 30 Hz window even though the panel
/// can scan out only ~30 of them. This helper keeps telemetry honest by
/// reporting physical presentation as `min(client presentation, refresh)` and
/// attributing the excess to `frames_superseded_by_refresh` instead of mixing
/// it with real receive/decode/presentation-loop losses.
#[must_use]
pub fn classify_presentation_window(window: PresentationWindow) -> PresentationWindowReport {
    let elapsed = window.elapsed.as_secs_f64();
    if elapsed <= 0.0 || !elapsed.is_finite() {
        return PresentationWindowReport {
            fps_received: 0.0,
            fps_presented: 0.0,
            frames_dropped: window.frames_received,
            frames_superseded_by_refresh: 0,
            frames_dropped_before_presentation: window.frames_received,
        };
    }

    let frames_after_rejection = window
        .frames_received
        .saturating_sub(window.frames_rejected);
    let display_capacity = if window.display_refresh_hz == 0 {
        frames_after_rejection
    } else {
        rounded_refresh_capacity(window.display_refresh_hz, window.elapsed)
            .min(frames_after_rejection)
    };
    let physically_presented = window
        .frames_presented_by_client
        .min(display_capacity)
        .min(frames_after_rejection);
    let frames_dropped = window.frames_received.saturating_sub(physically_presented);
    let explicit_superseded = window
        .frames_superseded_by_refresh
        .min(frames_dropped)
        .min(frames_after_rejection.saturating_sub(physically_presented));
    let inferred_superseded =
        if window.display_refresh_hz > 0 && window.frames_presented_by_client >= display_capacity {
            frames_after_rejection.saturating_sub(display_capacity)
        } else {
            0
        };
    let frames_superseded_by_refresh = explicit_superseded.max(inferred_superseded);
    let frames_dropped_before_presentation =
        frames_dropped.saturating_sub(frames_superseded_by_refresh);

    PresentationWindowReport {
        fps_received: arcen_telemetry::rate_per_second(window.frames_received, window.elapsed),
        fps_presented: arcen_telemetry::rate_per_second(physically_presented, window.elapsed),
        frames_dropped,
        frames_superseded_by_refresh,
        frames_dropped_before_presentation,
    }
}

fn rounded_refresh_capacity(refresh_hz: u32, elapsed: Duration) -> u64 {
    let frames = (u128::from(refresh_hz) * elapsed.as_nanos() + 500_000_000) / 1_000_000_000;
    u64::try_from(frames).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS_PER_S: u64 = 1_000_000_000;

    #[derive(Clone)]
    struct SimResult {
        pacer: FramePacer<u64>,
        presented: Vec<(u64, Duration, u64)>,
        depths_after_refresh: Vec<usize>,
    }

    #[derive(Clone)]
    struct Rng(u64);

    impl Rng {
        const fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn unit(&mut self) -> f64 {
            self.next_u64() as f64 / u64::MAX as f64
        }

        fn uniform_i64(&mut self, bound: i64) -> i64 {
            let span = (bound * 2 + 1) as f64;
            (self.unit() * span).floor() as i64 - bound
        }

        fn gaussian_i64(&mut self, sigma_ns: i64) -> i64 {
            let mut sum = 0.0;
            for _ in 0..12 {
                sum += self.unit();
            }
            ((sum - 6.0) * sigma_ns as f64).round() as i64
        }
    }

    fn ns(value: u64) -> Duration {
        Duration::from_nanos(value)
    }

    fn run_timeline(
        source_hz: u64,
        refresh_hz: u64,
        seconds: u64,
        arrivals: Vec<Duration>,
    ) -> SimResult {
        let refresh = NS_PER_S / refresh_hz;
        let source_frames = source_hz * seconds;
        let mut indexed: Vec<(Duration, u64)> = arrivals
            .into_iter()
            .take(source_frames as usize)
            .enumerate()
            .map(|(index, arrival)| (arrival, index as u64))
            .collect();
        indexed.sort_by_key(|(arrival, frame)| (*arrival, *frame));
        let mut pacer = FramePacer::new();
        pacer.set_source_fps(Some(u32::try_from(source_hz).unwrap()));
        let mut presented = Vec::new();
        let mut depths_after_refresh = Vec::new();
        let mut next_frame = 0usize;
        let max_refreshes = refresh_hz * seconds + refresh_hz;
        for seq in 0..max_refreshes {
            let now = ns(seq * refresh);
            while indexed
                .get(next_frame)
                .is_some_and(|(arrival, _)| *arrival <= now)
            {
                let (arrival, frame) = indexed[next_frame];
                pacer.enqueue(frame, arrival);
                next_frame += 1;
            }
            if let FramePacerRefresh::Present(frame) = pacer.on_refresh(seq, now, ns(refresh)) {
                presented.push((frame.frame, frame.queued_for, frame.refresh_seq));
            }
            depths_after_refresh.push(pacer.queue_depth());
            if next_frame >= indexed.len() && pacer.queue_depth() == 0 && seq > refresh_hz * seconds
            {
                break;
            }
        }
        SimResult {
            pacer,
            presented,
            depths_after_refresh,
        }
    }

    fn steady_arrivals(
        source_hz: u64,
        seconds: u64,
        mut jitter: impl FnMut(u64) -> i64,
    ) -> Vec<Duration> {
        let period = NS_PER_S / source_hz;
        let mut arrivals = Vec::new();
        let mut previous = 0u64;
        for frame in 0..source_hz * seconds {
            let nominal = i128::from(frame * period);
            let raw = (nominal + i128::from(jitter(frame))).max(0) as u64;
            let arrival = if frame == 0 {
                raw
            } else {
                raw.max(previous + 1_000)
            };
            previous = arrival;
            arrivals.push(ns(arrival));
        }
        arrivals
    }

    fn burst_and_stall_arrivals(source_hz: u64, seconds: u64, seed: u64) -> Vec<Duration> {
        let period = NS_PER_S / source_hz;
        let mut rng = Rng::new(seed);
        let mut arrivals = Vec::new();
        let mut previous = 0u64;
        for frame in 0..source_hz * seconds {
            let mut jitter = rng.uniform_i64(8_000_000) + rng.gaussian_i64(2_000_000);
            if frame % 113 == 40 {
                jitter += 100_000_000;
            }
            if frame % 113 == 41 || frame % 113 == 42 {
                jitter -= i64::try_from(period).unwrap() * 2;
            }
            if frame % 157 == 12 {
                jitter -= i64::try_from(period).unwrap();
            }
            let nominal = i128::from(frame * period);
            let raw = (nominal + i128::from(jitter)).max(0) as u64;
            let arrival = if frame == 0 {
                raw
            } else {
                raw.max(previous + 1_000)
            };
            previous = arrival;
            arrivals.push(ns(arrival));
        }
        arrivals
    }

    fn assert_presented_once_in_order(result: &SimResult, expected: u64) {
        assert_eq!(result.presented.len(), expected as usize);
        for (index, (frame, _, _)) in result.presented.iter().enumerate() {
            assert_eq!(*frame, index as u64);
        }
        assert_eq!(result.pacer.counters().never_presented(), 0);
    }

    #[test]
    fn sequence_pacer_presents_every_60fps_frame_once_on_60hz_with_half_interval_jitter() {
        let arrivals = steady_arrivals(60, 10, |frame| {
            if frame % 2 == 0 {
                -8_000_000
            } else {
                8_000_000
            }
        });
        let result = run_timeline(60, 60, 10, arrivals);
        assert_presented_once_in_order(&result, 600);
        assert!(result.pacer.counters().queue_depth_max <= 3);
        assert!(
            result
                .presented
                .iter()
                .all(|(_, latency, _)| *latency <= Duration::from_millis(34))
        );
    }

    #[test]
    fn sequence_pacer_presents_60fps_on_120hz_with_idle_refreshes_between_frames() {
        let arrivals = steady_arrivals(60, 5, |frame| {
            if frame % 2 == 0 {
                -2_000_000
            } else {
                2_000_000
            }
        });
        let result = run_timeline(60, 120, 5, arrivals);
        assert_presented_once_in_order(&result, 300);
        assert_eq!(result.pacer.counters().dropped_overflow, 0);
        assert!(
            result.pacer.counters().interval_2x > 0 || result.pacer.counters().interval_3x_plus > 0
        );
        assert!(result.pacer.counters().queue_depth_max <= 3);
    }

    #[test]
    fn sequence_pacer_presents_30fps_on_60hz_without_false_drops() {
        let arrivals = steady_arrivals(30, 5, |frame| {
            if frame % 2 == 0 {
                -4_000_000
            } else {
                4_000_000
            }
        });
        let result = run_timeline(30, 60, 5, arrivals);
        assert_presented_once_in_order(&result, 150);
        assert_eq!(result.pacer.counters().dropped_stale, 0);
        assert!(
            result.pacer.counters().interval_2x > 0 || result.pacer.counters().interval_3x_plus > 0
        );
    }

    #[test]
    fn sequence_pacer_keeps_two_frame_bursts_for_consecutive_refreshes() {
        let refresh = NS_PER_S / 60;
        let mut pacer = FramePacer::new();
        pacer.enqueue(1, ns(0));
        pacer.enqueue(2, ns(1_000_000));
        assert_eq!(pacer.queue_depth(), 2);
        assert!(matches!(
            pacer.on_refresh(1, ns(refresh), ns(refresh)),
            FramePacerRefresh::Present(PacedFrame { frame: 1, .. })
        ));
        assert!(matches!(
            pacer.on_refresh(2, ns(2 * refresh), ns(refresh)),
            FramePacerRefresh::Present(PacedFrame { frame: 2, .. })
        ));
        assert_eq!(pacer.counters().never_presented(), 0);
    }

    #[test]
    fn blocked_refreshes_keep_the_newest_frame_presentable() {
        let refresh = Duration::from_nanos(NS_PER_S / 60);
        let mut pacer = FramePacer::new();
        pacer.enqueue(0_u64, Duration::ZERO);
        assert!(matches!(
            pacer.on_refresh(1, refresh, refresh),
            FramePacerRefresh::Present(_)
        ));
        for seq in 2..=5_u64 {
            pacer.enqueue(seq, refresh * u32::try_from(seq).expect("small"));
            pacer.on_refresh_blocked(seq);
        }

        let FramePacerRefresh::Present(shown) = pacer.on_refresh(6, refresh * 6, refresh) else {
            panic!("a frame queued during the wait must be presentable");
        };
        assert_eq!(shown.frame, 4);
        let counters = pacer.counters();
        assert_eq!(counters.dropped_stale, 1);
        assert_eq!(counters.dropped_overflow, 1);
        assert_eq!(counters.underruns, 0);
        assert_eq!(counters.refreshes, 6);
        assert_eq!(pacer.queue_depth(), 1);
    }

    #[test]
    fn sequence_pacer_stale_drops_after_a_stall_and_recovers_depth() {
        let arrivals = burst_and_stall_arrivals(60, 4, 0xace5_2026);
        let result = run_timeline(60, 60, 4, arrivals);
        assert!(
            result.pacer.counters().dropped_stale > 0
                || result.pacer.counters().dropped_overflow > 0
        );
        assert!(result.pacer.counters().queue_depth_max <= 3);
        let last_high = result
            .depths_after_refresh
            .iter()
            .rposition(|depth| *depth > 1)
            .unwrap_or(0);
        assert!(
            result
                .depths_after_refresh
                .iter()
                .skip(last_high + 4)
                .all(|depth| *depth <= 1),
            "depth should return to <=1 within a few refreshes after bursts"
        );
    }

    #[test]
    fn sequence_pacer_drops_about_ten_per_second_for_60fps_on_50hz() {
        let arrivals = steady_arrivals(60, 10, |_| 0);
        let result = run_timeline(60, 50, 10, arrivals);
        let counters = result.pacer.counters();
        assert!(result.presented.len() < 600);
        assert!(
            (80..=120).contains(&counters.dropped_overflow),
            "overflow drops={}",
            counters.dropped_overflow
        );
        assert_eq!(
            counters.frames_presented + counters.never_presented(),
            counters.frames_received
        );
        assert!(
            result
                .presented
                .iter()
                .all(|(_, latency, _)| *latency <= Duration::from_millis(50))
        );
        assert!(counters.queue_depth_max <= 3);
    }

    #[test]
    fn sequence_pacer_overflow_drops_oldest_and_keeps_newest_two() {
        let mut pacer = FramePacer::new();
        pacer.enqueue(1, ns(0));
        pacer.enqueue(2, ns(1));
        pacer.enqueue(3, ns(2));
        pacer.enqueue(4, ns(3));
        assert_eq!(pacer.counters().dropped_overflow, 1);
        assert_eq!(pacer.queue_depth(), 3);
        assert!(matches!(
            pacer.on_refresh(1, ns(10), ns(10)),
            FramePacerRefresh::Present(PacedFrame { frame: 2, .. })
        ));
        assert!(matches!(
            pacer.on_refresh(2, ns(20), ns(10)),
            FramePacerRefresh::Present(PacedFrame { frame: 3, .. })
        ));
        assert!(matches!(
            pacer.on_refresh(3, ns(30), ns(10)),
            FramePacerRefresh::Present(PacedFrame { frame: 4, .. })
        ));
    }

    #[test]
    fn codex_jitter_regression_keeps_three_frames_without_drop() {
        let refresh = Duration::from_nanos(16_666_667);
        let mut pacer = FramePacer::new();
        pacer.set_source_fps(Some(60));
        assert!(matches!(
            pacer.on_refresh(1, Duration::from_micros(7_833), refresh),
            FramePacerRefresh::Idle
        ));
        pacer.enqueue(1, Duration::from_micros(8_000));
        pacer.enqueue(2, Duration::from_nanos(16_667_000));
        pacer.enqueue(3, Duration::from_micros(25_333));
        assert_eq!(pacer.queue_depth(), 3);
        assert_eq!(pacer.counters().dropped_overflow, 0);
        assert!(matches!(
            pacer.on_refresh(2, Duration::from_micros(25_500), refresh),
            FramePacerRefresh::Present(PacedFrame { frame: 1, .. })
        ));
        assert_eq!(pacer.counters().dropped_overflow, 0);
    }

    #[test]
    fn isolated_60hz_miss_counts_as_underrun_but_30_on_120_does_not() {
        let mut pacer = FramePacer::new();
        pacer.set_source_fps(Some(60));
        pacer.enqueue(1, Duration::ZERO);
        assert!(matches!(
            pacer.on_refresh(1, Duration::ZERO, Duration::from_nanos(NS_PER_S / 60)),
            FramePacerRefresh::Present(_)
        ));
        assert!(matches!(
            pacer.on_refresh(
                2,
                Duration::from_nanos(NS_PER_S / 60),
                Duration::from_nanos(NS_PER_S / 60)
            ),
            FramePacerRefresh::Idle
        ));
        assert_eq!(pacer.counters().underruns, 1);

        let mut slow = FramePacer::new();
        slow.set_source_fps(Some(30));
        slow.enqueue(1, Duration::ZERO);
        assert!(matches!(
            slow.on_refresh(1, Duration::ZERO, Duration::from_nanos(NS_PER_S / 120)),
            FramePacerRefresh::Present(_)
        ));
        for seq in 2..=4 {
            assert!(matches!(
                slow.on_refresh(
                    seq,
                    Duration::from_nanos((seq - 1) * (NS_PER_S / 120)),
                    Duration::from_nanos(NS_PER_S / 120)
                ),
                FramePacerRefresh::Idle
            ));
        }
        assert_eq!(slow.counters().underruns, 0);
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SimEventKind {
        Arrival(u64),
        Callback(u64),
        RenderComplete(u64),
        Vblank(u64),
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct SimEvent {
        time: Duration,
        kind: SimEventKind,
    }

    impl SimEvent {
        const fn priority(self) -> u8 {
            match self.kind {
                SimEventKind::Arrival(_) => 0,
                SimEventKind::RenderComplete(_) => 1,
                SimEventKind::Vblank(_) => 2,
                SimEventKind::Callback(_) => 3,
            }
        }
    }

    #[derive(Debug, Clone)]
    struct PendingDrawable {
        frame: u64,
        eligible: Duration,
    }

    #[derive(Debug, Clone)]
    struct DiscretePresentationStats {
        generated: u64,
        physical_presented: Vec<(u64, u64)>,
        pacer: FramePacer<u64>,
        metal_dropped: u64,
        max_in_flight: u8,
        pending_drawables: usize,
        in_flight: u8,
    }

    impl DiscretePresentationStats {
        fn accounted(&self) -> u64 {
            self.physical_presented.len().try_into().unwrap_or(u64::MAX)
                + self.pacer.counters().never_presented()
                + self.metal_dropped
                + u64::try_from(self.pacer.queue_depth()).unwrap_or(u64::MAX)
                + u64::from(self.in_flight)
                + u64::try_from(self.pending_drawables).unwrap_or(u64::MAX)
        }
    }

    fn pop_next_event(events: &mut Vec<SimEvent>) -> Option<SimEvent> {
        events.sort_by_key(|event| (event.time, event.priority()));
        if events.is_empty() {
            None
        } else {
            Some(events.remove(0))
        }
    }

    fn jittered_monotonic_times(
        count: u64,
        period: Duration,
        jitter_ns: i64,
        seed: u64,
    ) -> Vec<Duration> {
        let mut rng = Rng::new(seed);
        let mut out = Vec::new();
        let mut previous = Duration::ZERO;
        for index in 0..count {
            let nominal = Duration::from_nanos(
                u64::try_from(u128::from(index) * period.as_nanos()).unwrap_or(u64::MAX),
            );
            let jitter = rng.uniform_i64(jitter_ns);
            let raw = if jitter >= 0 {
                nominal.saturating_add(Duration::from_nanos(jitter as u64))
            } else {
                nominal.saturating_sub(Duration::from_nanos(jitter.unsigned_abs()))
            };
            let time = if index == 0 {
                raw
            } else {
                raw.max(previous.saturating_add(Duration::from_nanos(1_000)))
            };
            previous = time;
            out.push(time);
        }
        out
    }

    fn run_discrete_presentation_sim(
        source_hz: u64,
        refresh_hz: u64,
        frame_count: u64,
        arrival_jitter_ns: i64,
        callback_jitter_ns: i64,
        render_ns: impl Fn(u64) -> u64,
        seed: u64,
    ) -> DiscretePresentationStats {
        let source_period = Duration::from_nanos(NS_PER_S / source_hz);
        let refresh = Duration::from_nanos(NS_PER_S / refresh_hz);
        let arrivals =
            jittered_monotonic_times(frame_count, source_period, arrival_jitter_ns, seed);
        let callback_count = frame_count
            .saturating_mul(refresh_hz)
            .div_ceil(source_hz)
            .saturating_add(refresh_hz);
        let callbacks = jittered_monotonic_times(
            callback_count,
            refresh,
            callback_jitter_ns,
            seed ^ 0xfeed_beef,
        );
        let mut events = Vec::new();
        for (frame, arrival) in arrivals.iter().copied().enumerate() {
            events.push(SimEvent {
                time: arrival,
                kind: SimEventKind::Arrival(frame as u64),
            });
        }
        for (index, callback) in callbacks.iter().copied().enumerate() {
            events.push(SimEvent {
                time: callback,
                kind: SimEventKind::Callback(index as u64 + 1),
            });
        }
        let last_time = arrivals
            .last()
            .copied()
            .unwrap_or(Duration::ZERO)
            .saturating_add(refresh.saturating_mul(8));
        for index in 0..=callback_count.saturating_add(16) {
            let time = Duration::from_nanos(
                u64::try_from(u128::from(index) * refresh.as_nanos()).unwrap_or(u64::MAX),
            );
            if time > last_time.saturating_add(refresh.saturating_mul(8)) {
                break;
            }
            events.push(SimEvent {
                time,
                kind: SimEventKind::Vblank(index),
            });
        }

        let mut pacer = FramePacer::new();
        pacer.set_source_fps(Some(u32::try_from(source_hz).unwrap()));
        let mut pending = Vec::<PendingDrawable>::new();
        let mut previous_actual = None;
        let mut physical_presented = Vec::new();
        let mut in_flight = 0u8;
        let mut max_in_flight = 0u8;
        let mut metal_dropped = 0u64;
        while let Some(event) = pop_next_event(&mut events) {
            match event.kind {
                SimEventKind::Arrival(frame) => pacer.enqueue(frame, event.time),
                SimEventKind::Callback(seq) => {
                    if let FramePacerRefresh::Present(frame) =
                        pacer.on_refresh(seq, event.time, refresh)
                    {
                        if in_flight >= 2 {
                            metal_dropped = metal_dropped.saturating_add(1);
                        } else {
                            in_flight = in_flight.saturating_add(1);
                            max_in_flight = max_in_flight.max(in_flight);
                            events.push(SimEvent {
                                time: event
                                    .time
                                    .saturating_add(Duration::from_nanos(render_ns(frame.frame))),
                                kind: SimEventKind::RenderComplete(frame.frame),
                            });
                        }
                    }
                }
                SimEventKind::RenderComplete(frame) => {
                    let eligible = previous_actual.map_or(event.time, |previous: Duration| {
                        event.time.max(previous.saturating_add(refresh))
                    });
                    pending.push(PendingDrawable { frame, eligible });
                }
                SimEventKind::Vblank(vblank) => {
                    if let Some(index) = pending
                        .iter()
                        .position(|drawable| drawable.eligible <= event.time)
                    {
                        let drawable = pending.remove(index);
                        physical_presented.push((drawable.frame, vblank));
                        previous_actual = Some(event.time);
                        in_flight = in_flight.saturating_sub(1);
                    }
                }
            }
        }

        DiscretePresentationStats {
            generated: frame_count,
            physical_presented,
            pacer,
            metal_dropped,
            max_in_flight,
            pending_drawables: pending.len(),
            in_flight,
        }
    }

    fn assert_discrete_conservation(stats: &DiscretePresentationStats) {
        assert_eq!(
            stats.generated,
            stats.accounted(),
            "generated={} presented={} pacer_dropped={} metal_dropped={} queued={} in_flight={} pending={}",
            stats.generated,
            stats.physical_presented.len(),
            stats.pacer.counters().never_presented(),
            stats.metal_dropped,
            stats.pacer.queue_depth(),
            stats.in_flight,
            stats.pending_drawables,
        );
    }

    fn assert_no_duplicate_physical_refresh(stats: &DiscretePresentationStats) {
        let mut used = std::collections::BTreeSet::new();
        for (_, vblank) in &stats.physical_presented {
            assert!(used.insert(*vblank), "duplicate physical refresh {vblank}");
        }
    }

    fn assert_every_frame_presented_once(stats: &DiscretePresentationStats) {
        let mut frames: Vec<u64> = stats
            .physical_presented
            .iter()
            .map(|(frame, _)| *frame)
            .collect();
        frames.sort_unstable();
        assert_eq!(frames, (0..stats.generated).collect::<Vec<_>>());
        assert_eq!(stats.pacer.counters().never_presented(), 0);
        assert_eq!(stats.metal_dropped, 0);
    }

    #[test]
    fn discrete_sim_presents_60_on_60_once_without_collision_or_loss() {
        let stats = run_discrete_presentation_sim(
            60,
            60,
            600,
            8_000_000,
            1_000_000,
            |_| 15_000_000,
            0x6060,
        );
        assert_discrete_conservation(&stats);
        assert_every_frame_presented_once(&stats);
        assert_no_duplicate_physical_refresh(&stats);
        assert!(stats.max_in_flight >= 2, "in-flight gate was not exercised");
    }

    #[test]
    fn discrete_sim_presents_60_on_120_once_without_collision_or_loss() {
        let stats = run_discrete_presentation_sim(
            60,
            120,
            600,
            8_000_000,
            1_000_000,
            |_| 12_000_000,
            0x6120,
        );
        assert_discrete_conservation(&stats);
        assert_every_frame_presented_once(&stats);
        assert_no_duplicate_physical_refresh(&stats);
    }

    #[test]
    fn discrete_sim_codex_min_duration_case_lands_on_refreshes_two_and_three() {
        let refresh = Duration::from_nanos(NS_PER_S / 60);
        let stats = run_discrete_presentation_sim(
            60,
            60,
            2,
            0,
            0,
            |frame| {
                if frame == 0 {
                    u64::try_from(
                        refresh
                            .saturating_add(Duration::from_micros(100))
                            .as_nanos(),
                    )
                    .unwrap()
                } else {
                    u64::try_from(
                        refresh
                            .saturating_mul(2)
                            .saturating_sub(Duration::from_micros(100))
                            .as_nanos(),
                    )
                    .unwrap()
                }
            },
            0xc0de,
        );
        assert_discrete_conservation(&stats);
        assert_eq!(stats.physical_presented, vec![(0, 2), (1, 3)]);
    }

    #[test]
    fn expected_presentable_fps_is_stream_capped_by_refresh() {
        assert!((expected_presentable_fps(60.0, 120) - 60.0).abs() < f64::EPSILON);
        assert!((expected_presentable_fps(60.0, 30) - 30.0).abs() < f64::EPSILON);
        assert!((expected_presentable_fps(42.0, 0) - 42.0).abs() < f64::EPSILON);
        assert!(expected_presentable_fps(f64::NAN, 60).abs() < f64::EPSILON);
    }

    #[test]
    fn refresh_present_due_respects_display_cadence() {
        assert!(refresh_present_due(None, 30));
        assert!(refresh_present_due(Some(Duration::from_millis(1)), 0));
        assert!(!refresh_present_due(Some(Duration::from_millis(16)), 30));
        assert!(refresh_present_due(Some(Duration::from_millis(34)), 30));
        assert!(refresh_present_due(Some(Duration::from_millis(9)), 120));
    }

    #[test]
    fn slow_display_supersedes_otherwise_presented_frames_by_refresh() {
        let report = classify_presentation_window(PresentationWindow {
            frames_received: 300,
            frames_presented_by_client: 300,
            frames_rejected: 0,
            frames_superseded_by_refresh: 0,
            display_refresh_hz: 30,
            elapsed: Duration::from_secs(5),
        });

        assert!((report.fps_received - 60.0).abs() < f64::EPSILON);
        assert!((report.fps_presented - 30.0).abs() < f64::EPSILON);
        assert_eq!(report.frames_dropped, 150);
        assert_eq!(report.frames_superseded_by_refresh, 150);
        assert_eq!(report.frames_dropped_before_presentation, 0);
    }

    #[test]
    fn fast_display_exposes_client_presentation_coupling_as_real_loss() {
        let report = classify_presentation_window(PresentationWindow {
            frames_received: 300,
            frames_presented_by_client: 200,
            frames_rejected: 0,
            frames_superseded_by_refresh: 0,
            display_refresh_hz: 120,
            elapsed: Duration::from_secs(5),
        });

        assert!((report.fps_received - 60.0).abs() < f64::EPSILON);
        assert!((report.fps_presented - 40.0).abs() < f64::EPSILON);
        assert_eq!(report.frames_dropped, 100);
        assert_eq!(report.frames_superseded_by_refresh, 0);
        assert_eq!(report.frames_dropped_before_presentation, 100);
    }

    #[test]
    fn rejected_frames_stay_distinct_from_refresh_supersession() {
        let report = classify_presentation_window(PresentationWindow {
            frames_received: 300,
            frames_presented_by_client: 150,
            frames_rejected: 10,
            frames_superseded_by_refresh: 0,
            display_refresh_hz: 30,
            elapsed: Duration::from_secs(5),
        });

        assert!((report.fps_presented - 30.0).abs() < f64::EPSILON);
        assert_eq!(report.frames_dropped, 150);
        assert_eq!(report.frames_superseded_by_refresh, 140);
        assert_eq!(report.frames_dropped_before_presentation, 10);
    }

    #[test]
    fn explicit_refresh_supersession_is_not_reported_as_real_loss() {
        let report = classify_presentation_window(PresentationWindow {
            frames_received: 300,
            frames_presented_by_client: 100,
            frames_rejected: 0,
            frames_superseded_by_refresh: 150,
            display_refresh_hz: 30,
            elapsed: Duration::from_secs(5),
        });

        assert!((report.fps_presented - 20.0).abs() < f64::EPSILON);
        assert_eq!(report.frames_dropped, 200);
        assert_eq!(report.frames_superseded_by_refresh, 150);
        assert_eq!(report.frames_dropped_before_presentation, 50);
    }
}
