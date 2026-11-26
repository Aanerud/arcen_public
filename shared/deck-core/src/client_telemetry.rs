// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_protocol::messages::{
    ClientQosSampleMsg, ClientTelemetrySnapshotMsg, HealthStateMsg, SampleWindowSecs,
};
use arcen_telemetry::{HealthAssessment, HealthState, QosSample, QosTargets, assess_health};

const FPS_WARMUP: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub struct ClientTelemetry {
    session_started: Instant,
    media_started: Mutex<Option<Instant>>,
    frames_received: AtomicU64,
    frames_decoded: AtomicU64,
    frames_presented: AtomicU64,
    /// Frames the presenters handed on without showing (see
    /// [`arcen_telemetry::QosSample::frames_superseded`]).
    presenter_superseded: AtomicU64,
    /// The part of `presenter_superseded` that reached the compositor and
    /// came back without a timestamp (see
    /// [`arcen_telemetry::QosSample::frames_submitted_untimed`]).
    presenter_untimed: AtomicU64,
    /// Frames the media worker replaced before a presenter took them,
    /// cumulative as the worker counts them.
    worker_superseded: AtomicU64,
    /// The frame counts as of the last time the Deck observed its
    /// presenters ([`PresentationPass`]).
    presentation_pass: Mutex<PresentationPass>,
    frames_dropped: AtomicU64,
    decode_time_ms: AtomicU32,
    display_time_ms: AtomicU32,
    input_send_time_ms: AtomicU32,
    rtt_ms: AtomicU32,
    media_observed: AtomicBool,
    presentation_observed: AtomicBool,
    input_observed: AtomicBool,
    rtt_observed: AtomicBool,
    rtt_sum_ms: AtomicU64,
    rtt_samples: AtomicU64,
    worst_health: AtomicU32,
    reconnects: AtomicU64,
    /// The Pier's wire clock against this Deck's (ADR 0026), from the
    /// health ping/pong.
    pier_clock: Mutex<arcen_media::timeline::PierClockSync>,
}

impl Default for ClientTelemetry {
    fn default() -> Self {
        Self {
            session_started: Instant::now(),
            media_started: Mutex::new(None),
            frames_received: AtomicU64::new(0),
            frames_decoded: AtomicU64::new(0),
            frames_presented: AtomicU64::new(0),
            presenter_superseded: AtomicU64::new(0),
            presenter_untimed: AtomicU64::new(0),
            worker_superseded: AtomicU64::new(0),
            presentation_pass: Mutex::new(PresentationPass::default()),
            frames_dropped: AtomicU64::new(0),
            decode_time_ms: AtomicU32::new(0),
            display_time_ms: AtomicU32::new(0),
            input_send_time_ms: AtomicU32::new(0),
            rtt_ms: AtomicU32::new(0),
            media_observed: AtomicBool::new(false),
            presentation_observed: AtomicBool::new(false),
            input_observed: AtomicBool::new(false),
            rtt_observed: AtomicBool::new(false),
            rtt_sum_ms: AtomicU64::new(0),
            rtt_samples: AtomicU64::new(0),
            worst_health: AtomicU32::new(0),
            reconnects: AtomicU64::new(0),
            pier_clock: Mutex::new(arcen_media::timeline::PierClockSync::default()),
        }
    }
}

/// The frame counts when the Deck last observed what its root presenters
/// did (a UI pass that took their outcomes, or a frame it presented
/// itself): what a health window judges.
///
/// The presenters' outcomes reach the telemetry only when the Deck looks:
/// the Windows Deck takes them a few times a second, in batches of a dozen
/// frames at 60 fps. Frames decoded after that look have had no chance to
/// be counted as presented. A health window that compared every frame
/// decoded up to its end with the frames presented up to the last look read
/// a steady stream as 295 of 300 presented, then 309 of 300: "unpresented"
/// one window in two. Counting decoded, presented, superseded and untimed
/// frames up to the same look counts each frame in the window that saw its
/// outcome; a frame decoded after it counts in the next.
///
/// Only a whole look counts as one: a multi-window session's further
/// windows report their frames at every media wake, between the root's
/// looks. A look marked there counted the root's frames decoded since its
/// last look, frames nobody had yet looked for; and the further windows'
/// frames reported after the root's last look are counted with the next.
#[derive(Debug, Default, Clone, Copy)]
struct PresentationPass {
    /// Looks so far, so a window can tell whether it saw one.
    passes: u64,
    frames_decoded: u64,
    frames_presented: u64,
    /// Presenter- and worker-superseded frames.
    frames_superseded: u64,
    frames_untimed: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct TelemetryWindow {
    timestamp_ms: u64,
    frames_received: u64,
    /// Decoded, presented, superseded and untimed frames counted so far: up
    /// to the last presentation pass, or all of them when the window saw no
    /// pass ([`PresentationPass`]).
    frames_decoded: u64,
    presentation_passes: u64,
    frames_presented: u64,
    frames_superseded: u64,
    frames_untimed: u64,
    frames_dropped: u64,
    ever_streamed: bool,
    stalled_windows: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTelemetrySummary {
    pub frames_decoded: u64,
    pub frames_dropped: u64,
    pub avg_fps: u64,
    pub avg_rtt_ms: u64,
    pub worst_health: Option<HealthState>,
    pub reconnects: u64,
}

impl ClientTelemetry {
    /// The media worker's cumulative counts: frames received and decoded,
    /// packets dropped, and `superseded`, decoded frames a newer one
    /// replaced before a presenter took them.
    pub fn record_media(
        &self,
        received: u64,
        decoded: u64,
        dropped: u64,
        superseded: u64,
        decode: Duration,
    ) {
        if received != 0 || decoded != 0 {
            let mut media_started = self
                .media_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            media_started.get_or_insert_with(Instant::now);
        }
        self.frames_received.store(received, Ordering::Relaxed);
        self.frames_decoded.store(decoded, Ordering::Relaxed);
        self.worker_superseded.store(superseded, Ordering::Relaxed);
        self.frames_dropped.store(dropped, Ordering::Relaxed);
        self.decode_time_ms
            .store(duration_ms(decode), Ordering::Relaxed);
        self.media_observed.store(true, Ordering::Release);
    }

    pub fn record_presented(&self, display: Duration) {
        self.frames_presented.fetch_add(1, Ordering::Relaxed);
        self.display_time_ms
            .store(duration_ms(display), Ordering::Relaxed);
        self.presentation_observed.store(true, Ordering::Release);
        self.note_presentation_pass();
    }

    /// Frames a multi-window session's further windows put on screen. The
    /// decoded count covers every monitor's stream, so these count as
    /// presented too, or every multi-window session reads as unpresented.
    ///
    /// They are no look at the root presenters ([`PresentationPass`]).
    pub fn record_frames_presented(&self, frames: u64) {
        if frames == 0 {
            return;
        }
        self.frames_presented.fetch_add(frames, Ordering::Relaxed);
        self.presentation_observed.store(true, Ordering::Release);
    }

    /// What one pass of the dedicated presenters put on glass, counted the
    /// same way on both Decks
    /// ([`arcen_media::presentation::presented_frames_for_health`]):
    /// `confirmed` frames carry a presentation timestamp, `unconfirmed` ones
    /// do not and count only while `timestamp_ever_reported` is false: until
    /// a presenter has reported a timestamp, or on Windows while its frame
    /// statistics are not live
    /// ([`crate::present::confirmation::PresentStatisticsLive`]). Otherwise
    /// an unconfirmed frame is superseded, not lost: the compositor showed a
    /// later drawable, or kept the same picture (a still desktop's
    /// keepalives on macOS mostly come back this way).
    ///
    /// Call it as the outcomes are observed (every UI pass), not when they
    /// are logged: the health window compares them with frames decoded up
    /// to this look ([`PresentationPass`]). A pass that resolved nothing may
    /// call it with zeros: it still marks the look.
    pub fn record_presenter_frames(
        &self,
        confirmed: u64,
        unconfirmed: u64,
        timestamp_ever_reported: bool,
    ) {
        let presented = arcen_media::presentation::presented_frames_for_health(
            confirmed,
            unconfirmed,
            timestamp_ever_reported,
        );
        if timestamp_ever_reported && unconfirmed != 0 {
            self.record_presenter_superseded(unconfirmed);
            self.presenter_untimed
                .fetch_add(unconfirmed, Ordering::Relaxed);
        }
        if presented != 0 {
            self.frames_presented
                .fetch_add(presented, Ordering::Relaxed);
            self.display_time_ms.store(0, Ordering::Relaxed);
            self.presentation_observed.store(true, Ordering::Release);
        }
        self.note_presentation_pass();
    }

    /// Frames a presenter dropped for a newer one before submitting them
    /// (a newest-wins queue on bunched arrivals): handed on, and excused
    /// only as far as bunching explains
    /// ([`arcen_telemetry::NEWEST_WINS_EXCUSED_BASIS_POINTS`]); they are no
    /// proof the presenter reached the compositor. Report them as they are
    /// observed, like the presented frames (before
    /// [`Self::record_presenter_frames`], which marks the look).
    pub fn record_presenter_superseded(&self, frames: u64) {
        if frames != 0 {
            self.presenter_superseded
                .fetch_add(frames, Ordering::Relaxed);
        }
    }

    /// The Deck has observed its presenters now: frames decoded so far
    /// have had their chance to be counted ([`PresentationPass`]).
    fn note_presentation_pass(&self) {
        let mut pass = self
            .presentation_pass
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *pass = PresentationPass {
            passes: pass.passes.saturating_add(1),
            ..self.frame_counts()
        };
    }

    /// The frame counts now, with no pass.
    fn frame_counts(&self) -> PresentationPass {
        PresentationPass {
            passes: 0,
            frames_decoded: self.frames_decoded.load(Ordering::Relaxed),
            frames_presented: self.frames_presented.load(Ordering::Relaxed),
            frames_superseded: self
                .presenter_superseded
                .load(Ordering::Relaxed)
                .saturating_add(self.worker_superseded.load(Ordering::Relaxed)),
            frames_untimed: self.presenter_untimed.load(Ordering::Relaxed),
        }
    }

    /// The frame counts a window ending now judges: as of the last
    /// presentation pass if one happened since `passes_seen`, else
    /// everything so far (a Deck that never observes a presenter, or one
    /// that stopped looking, is judged on all it decoded).
    fn judged_counts(&self, passes_seen: u64) -> PresentationPass {
        let pass = *self
            .presentation_pass
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pass.passes > passes_seen {
            pass
        } else {
            PresentationPass {
                passes: pass.passes,
                ..self.frame_counts()
            }
        }
    }

    pub fn record_input_send(&self, elapsed: Duration) {
        self.input_send_time_ms
            .store(duration_ms(elapsed), Ordering::Relaxed);
        self.input_observed.store(true, Ordering::Release);
    }

    pub fn record_rtt(&self, rtt: Duration) {
        let milliseconds = duration_ms(rtt);
        self.rtt_ms.store(milliseconds, Ordering::Relaxed);
        self.rtt_sum_ms
            .fetch_add(u64::from(milliseconds), Ordering::Relaxed);
        self.rtt_samples.fetch_add(1, Ordering::Relaxed);
        self.rtt_observed.store(true, Ordering::Release);
    }

    /// Records one health ping/pong as a clock sample: the ping left at
    /// this Deck's `deck_sent_ms` (epoch milliseconds), the Pier stamped
    /// its pong `pier_ms`, and the pong arrived `round_trip` after the ping.
    pub fn record_clock_sample(&self, deck_sent_ms: u64, pier_ms: u64, round_trip: Duration) {
        use arcen_media::timeline::wire_ms;
        if deck_sent_ms == 0 || pier_ms == 0 {
            return;
        }
        let estimate = self
            .pier_clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(wire_ms(deck_sent_ms), wire_ms(pier_ms), round_trip);
        if let Some(estimate) = estimate {
            tracing::debug!(
                target: crate::log_target::HEALTH,
                offset_ms = estimate.offset_ms,
                round_trip_ms = estimate.round_trip_ms,
                samples = estimate.samples,
                "pier clock estimate"
            );
        }
    }

    /// The current estimate of the Pier's wire clock, once a ping/pong
    /// sample exists.
    pub fn pier_clock(&self) -> Option<arcen_media::timeline::PierClockEstimate> {
        self.pier_clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .estimate()
    }

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
        self.reset_media_warmup();
    }

    pub fn reset_media_warmup(&self) {
        *self
            .media_started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub fn window(&self, timestamp_ms: u64) -> TelemetryWindow {
        let (received, decoded, presented, dropped) = self.counters();
        let judged = self.judged_counts(0);
        TelemetryWindow {
            timestamp_ms,
            frames_received: received,
            frames_decoded: judged.frames_decoded,
            presentation_passes: judged.passes,
            frames_presented: judged.frames_presented,
            frames_superseded: judged.frames_superseded,
            frames_untimed: judged.frames_untimed,
            frames_dropped: dropped,
            ever_streamed: decoded != 0 || presented != 0,
            stalled_windows: 0,
        }
    }

    fn qos_sample(
        &self,
        window: &mut TelemetryWindow,
        timestamp_ms: u64,
        target_fps: u32,
        misses: u32,
    ) -> (QosSample, u64, u64) {
        let (received, raw_decoded, raw_presented, dropped) = self.counters();
        // Decoded, presented, superseded and untimed over the same frames:
        // up to the last time the Deck observed its presenters
        // ([`PresentationPass`]).
        let judged = self.judged_counts(window.presentation_passes);
        let received_delta = received.saturating_sub(window.frames_received);
        let decoded_delta = judged.frames_decoded.saturating_sub(window.frames_decoded);
        let presented_delta = judged
            .frames_presented
            .saturating_sub(window.frames_presented);
        let superseded_delta = judged
            .frames_superseded
            .saturating_sub(window.frames_superseded);
        let untimed_delta = judged.frames_untimed.saturating_sub(window.frames_untimed);
        let dropped_delta = dropped.saturating_sub(window.frames_dropped);
        let elapsed_ms = timestamp_ms.saturating_sub(window.timestamp_ms).max(1);

        window.timestamp_ms = timestamp_ms;
        window.frames_received = received;
        window.frames_decoded = judged.frames_decoded;
        window.presentation_passes = judged.passes;
        window.frames_presented = judged.frames_presented;
        window.frames_superseded = judged.frames_superseded;
        window.frames_untimed = judged.frames_untimed;
        window.frames_dropped = dropped;
        window.ever_streamed |= raw_decoded != 0 || raw_presented != 0;

        let experience_delta = if self.presentation_observed.load(Ordering::Acquire) {
            presented_delta
        } else {
            decoded_delta
        };
        let fps_actual = window
            .ever_streamed
            .then(|| window_rate(experience_delta, elapsed_ms));
        if fps_actual == Some(0) {
            window.stalled_windows = window.stalled_windows.saturating_add(1);
        } else {
            window.stalled_windows = 0;
        }

        // The host sends only what changed, so a still desktop arrives as a
        // keepalive a second. The client's own share of the experience is
        // whether it presented what arrived, not whether the host had
        // anything to send: a host that fell behind reports that itself, and
        // a window with nothing presented is still judged as a stall.
        let fps_content = window
            .ever_streamed
            .then(|| window_rate(received_delta, elapsed_ms));
        let sample = QosSample {
            timestamp_ms,
            fps_target: Some(target_fps),
            fps_actual,
            fps_content,
            fps_warmup: self
                .media_started
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some_and(|started| started.elapsed() < FPS_WARMUP),
            frames_decoded: window.ever_streamed.then_some(decoded_delta),
            frames_presented: window.ever_streamed.then_some(presented_delta),
            frames_superseded: window.ever_streamed.then_some(superseded_delta),
            frames_submitted_untimed: window.ever_streamed.then_some(untimed_delta),
            decode_time_ms: observed_nonzero(&self.media_observed, &self.decode_time_ms),
            display_time_ms: observed_nonzero(&self.presentation_observed, &self.display_time_ms),
            rtt_ms: observed_nonzero(&self.rtt_observed, &self.rtt_ms),
            input_latency_ms: observed_nonzero(&self.input_observed, &self.input_send_time_ms),
            heartbeat_misses: (misses != 0).then_some(misses),
            ..QosSample::default()
        };
        (sample, received_delta, dropped_delta)
    }

    pub fn snapshot(
        &self,
        window: &mut TelemetryWindow,
        network: Option<arcen_protocol::messages::ClientNetworkSnapshotMsg>,
        timestamp_ms: u64,
        target_fps: u32,
        misses: u32,
    ) -> (ClientTelemetrySnapshotMsg, HealthAssessment, QosSample) {
        let (sample, received_delta, dropped_delta) =
            self.qos_sample(window, timestamp_ms, target_fps, misses);
        let targets = QosTargets::default();
        let mut health = assess_health(&sample, &targets);
        if window.stalled_windows <= 2
            && sample.fps_actual == Some(0)
            && misses < targets.heartbeat_critical_misses()
            && health.client_experience.state == Some(HealthState::Critical)
            && health.client_experience.dominant_cause == Some(arcen_telemetry::HealthCause::Fps)
        {
            health.client_experience.state = Some(HealthState::Degraded);
            health.overall = Some(
                health
                    .host_delivery
                    .state
                    .unwrap_or(HealthState::Ok)
                    .max(HealthState::Degraded),
            );
        }
        let media_observed = self.media_observed.load(Ordering::Acquire);
        let qos = ClientQosSampleMsg {
            frames_received: media_observed.then_some(received_delta),
            frames_decoded: sample.frames_decoded,
            frames_presented: sample.frames_presented,
            frames_superseded: sample.frames_superseded,
            frames_submitted_untimed: sample.frames_submitted_untimed,
            frames_dropped: media_observed.then_some(dropped_delta),
            decode_time_ms: sample.decode_time_ms,
            display_time_ms: sample.display_time_ms,
            input_send_time_ms: sample.input_latency_ms,
            client_health: health.overall.map(health_state_msg),
            sample_window_secs: SampleWindowSecs::try_from(5).ok(),
            sample_age_ms: Some(0),
        };
        (
            ClientTelemetrySnapshotMsg {
                qos: Some(qos),
                network,
            },
            health,
            sample,
        )
    }

    pub fn summary(&self, duration: Duration) -> SessionTelemetrySummary {
        let (_, decoded, presented, dropped) = self.counters();
        let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        let avg_fps = presented
            .saturating_mul(1_000)
            .checked_div(duration_ms)
            .unwrap_or(0);
        let rtt_samples = self.rtt_samples.load(Ordering::Relaxed);
        let avg_rtt_ms = self
            .rtt_sum_ms
            .load(Ordering::Relaxed)
            .checked_div(rtt_samples)
            .unwrap_or(0);
        SessionTelemetrySummary {
            frames_decoded: decoded,
            frames_dropped: dropped,
            avg_fps,
            avg_rtt_ms,
            worst_health: health_from_code(self.worst_health.load(Ordering::Relaxed)),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }

    pub fn session_duration(&self) -> Duration {
        self.session_started.elapsed()
    }

    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (
            self.frames_received.load(Ordering::Relaxed),
            self.frames_decoded.load(Ordering::Relaxed),
            self.frames_presented.load(Ordering::Relaxed),
            self.frames_dropped.load(Ordering::Relaxed),
        )
    }

    /// The session's committed health (after the two-window hysteresis),
    /// for the session summary's `worst_health`: one window outside its
    /// targets is not what the session was reported as, and the summary
    /// says what the `HEALTH_*` events said.
    pub fn record_committed_health(&self, state: Option<HealthState>) {
        if let Some(state) = state {
            self.worst_health
                .fetch_max(health_code(state), Ordering::Relaxed);
        }
    }
}

fn window_rate(frames: u64, elapsed_ms: u64) -> u32 {
    let rate = frames.saturating_mul(1_000).saturating_add(elapsed_ms / 2) / elapsed_ms;
    u32::try_from(rate).unwrap_or(u32::MAX)
}

const fn health_code(state: HealthState) -> u32 {
    match state {
        HealthState::Ok => 1,
        HealthState::Degraded => 2,
        HealthState::Critical => 3,
    }
}

const fn health_from_code(code: u32) -> Option<HealthState> {
    match code {
        1 => Some(HealthState::Ok),
        2 => Some(HealthState::Degraded),
        3 => Some(HealthState::Critical),
        _ => None,
    }
}

fn duration_ms(value: Duration) -> u32 {
    u32::try_from(value.as_millis()).unwrap_or(u32::MAX)
}

fn nonzero(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

fn observed_nonzero(observed: &AtomicBool, value: &AtomicU32) -> Option<u32> {
    observed
        .load(Ordering::Acquire)
        .then(|| value.load(Ordering::Relaxed))
        .and_then(nonzero)
}

#[must_use]
pub fn health_state_msg(state: HealthState) -> HealthStateMsg {
    match state {
        HealthState::Ok => HealthStateMsg::Ok,
        HealthState::Degraded => HealthStateMsg::Degraded,
        HealthState::Critical => HealthStateMsg::Critical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warmed_telemetry() -> ClientTelemetry {
        ClientTelemetry {
            session_started: Instant::now() - FPS_WARMUP - Duration::from_secs(1),
            media_started: Mutex::new(Some(Instant::now() - FPS_WARMUP - Duration::from_secs(1))),
            ..ClientTelemetry::default()
        }
    }

    #[test]
    fn hot_path_updates_are_atomic_and_snapshot_serializes() {
        let telemetry = warmed_telemetry();
        let mut window = telemetry.window(0);
        telemetry.record_media(10, 9, 1, 0, Duration::from_millis(7));
        telemetry.record_presented(Duration::from_millis(2));
        telemetry.record_input_send(Duration::from_millis(3));
        let (snapshot, _, sample) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["qos"]["frames_received"], 10);
        assert_eq!(json["qos"]["frames_decoded"], 9);
        assert_eq!(json["qos"]["frames_presented"], 1);
        assert_eq!(json["qos"]["frames_dropped"], 1);
        assert_eq!(json["qos"]["decode_time_ms"], 7);
        assert_eq!(json["qos"]["display_time_ms"], 2);
        assert_eq!(json["qos"]["input_send_time_ms"], 3);
        assert_eq!(json["qos"]["sample_window_secs"], 5);
        assert!(json.get("network").is_none());
        assert_eq!(sample.fps_actual, Some(0));
        assert_eq!(sample.fps_target, Some(60));
    }

    #[test]
    fn shared_health_thresholds_drive_client_state() {
        let telemetry = warmed_telemetry();
        let mut window = telemetry.window(0);
        telemetry.record_rtt(Duration::from_millis(151));
        let (snapshot, health, _) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        assert_eq!(health.overall, Some(HealthState::Critical));
        assert_eq!(
            snapshot.qos.unwrap().client_health,
            Some(HealthStateMsg::Critical)
        );
    }

    #[test]
    fn unavailable_smoke_metrics_remain_absent() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        let (snapshot, health, _) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        let qos = snapshot.qos.unwrap();
        assert_eq!(qos.frames_received, None);
        assert_eq!(qos.frames_decoded, None);
        assert_eq!(qos.frames_presented, None);
        assert_eq!(qos.frames_dropped, None);
        assert_eq!(qos.decode_time_ms, None);
        assert_eq!(qos.display_time_ms, None);
        assert_eq!(qos.input_send_time_ms, None);
        assert_eq!(qos.client_health, None);
        assert_eq!(health.overall, None);
    }

    #[test]
    fn further_windows_count_as_presented_in_a_multi_window_session() {
        // Seen live: two monitors decoded, only the root window counted,
        // every session critical with 40-89 % "unpresented".
        let telemetry = warmed_telemetry();
        let mut window = telemetry.window(0);
        telemetry.record_media(40, 40, 0, 0, Duration::from_millis(2));
        // The further window's frames are counted with the root's next
        // look ([`PresentationPass`]).
        for _ in 0..20 {
            telemetry.record_frames_presented(1);
            telemetry.record_presented(Duration::from_millis(1));
        }
        telemetry.record_frames_presented(0);
        let (_, _, sample) = telemetry.snapshot(&mut window, None, 5_000, 30, 0);
        assert_eq!(sample.frames_presented, Some(40));
        assert_eq!(arcen_telemetry::unpresented_basis_points(&sample), Some(0));
    }

    /// Thirty frames a second for twenty seconds, reported as each is
    /// presented, and the same stream reported in 1.175 s batches (as the
    /// macOS Deck once did with its log): every five-second window
    /// balances either way, because decoded frames are counted up to the
    /// same look that counted the presents.
    #[test]
    fn presents_reported_as_observed_balance_every_window() {
        let telemetry = warmed_telemetry();
        telemetry.record_presenter_frames(1, 0, true);
        let mut window = telemetry.window(0);
        let batched = warmed_telemetry();
        batched.record_presenter_frames(1, 0, true);
        let mut batched_window = batched.window(0);
        let (mut decoded, mut pending_batch) = (0_u64, 0_u64);
        for frame in 1..=600_u64 {
            let at_ms = frame * 1_000 / 30;
            decoded += 1;
            telemetry.record_media(decoded, decoded, 0, 0, Duration::from_millis(2));
            telemetry.record_presenter_frames(1, 0, true);
            batched.record_media(decoded, decoded, 0, 0, Duration::from_millis(2));
            pending_batch += 1;
            if at_ms % 1_175 < 1_000 / 30 {
                batched.record_presenter_frames(pending_batch, 0, true);
                pending_batch = 0;
            }
            if at_ms % 5_000 == 0 {
                let (_, health, sample) = telemetry.snapshot(&mut window, None, at_ms, 30, 0);
                assert_eq!(sample.frames_decoded, sample.frames_presented);
                assert_eq!(health.client_experience.state, Some(HealthState::Ok));
                let (_, health, sample) = batched.snapshot(&mut batched_window, None, at_ms, 30, 0);
                assert_eq!(sample.frames_decoded, sample.frames_presented);
                assert_eq!(health.client_experience.state, Some(HealthState::Ok));
            }
        }
    }

    /// One simulated Deck: the source sends `fps` frames a second, the
    /// media worker decodes each at once, and every `pass_ms` the UI takes
    /// the presenters' outcomes, which confirm frames `lag` frames behind
    /// the newest decoded (`present` says how many of them reached the
    /// glass). A health snapshot every five seconds, off the passes' phase,
    /// against a 60 fps target.
    struct Simulated {
        telemetry: ClientTelemetry,
        window: TelemetryWindow,
        decoded: u64,
        confirmed_through: u64,
    }

    impl Simulated {
        fn new() -> Self {
            let telemetry = warmed_telemetry();
            telemetry.record_presenter_frames(1, 0, true);
            let window = telemetry.window(0);
            Self {
                telemetry,
                window,
                decoded: 0,
                confirmed_through: 0,
            }
        }

        /// Runs `windows` five-second windows; returns each window's
        /// sample, health and the frames decoded in it by the old rule
        /// (everything up to the snapshot).
        fn run(
            &mut self,
            windows: u64,
            fps: u64,
            pass_ms: u64,
            lag: u64,
            present: impl Fn(u64) -> u64,
        ) -> Vec<(QosSample, HealthAssessment, u64)> {
            let mut out = Vec::new();
            let mut raw_at_snapshot = self.decoded;
            let start_ms = self.window.timestamp_ms;
            let mut next_pass = start_ms + pass_ms;
            let mut next_snapshot = start_ms + 5_000;
            let end_ms = start_ms + windows * 5_000;
            let mut next_frame = 0_u64;
            for at_ms in start_ms + 1..=end_ms {
                while (start_ms + next_frame * 1_000 / fps) < at_ms {
                    next_frame += 1;
                    self.decoded += 1;
                    self.telemetry.record_media(
                        self.decoded,
                        self.decoded,
                        0,
                        0,
                        Duration::from_millis(2),
                    );
                }
                if at_ms == next_pass {
                    next_pass += pass_ms;
                    let through = self.decoded.saturating_sub(lag);
                    let resolved = through.saturating_sub(self.confirmed_through);
                    self.confirmed_through = through;
                    let shown = present(resolved).min(resolved);
                    self.telemetry.record_presenter_superseded(0);
                    self.telemetry.record_presenter_frames(shown, 0, true);
                }
                if at_ms == next_snapshot {
                    next_snapshot += 5_000;
                    let (_, health, sample) =
                        self.telemetry
                            .snapshot(&mut self.window, None, at_ms + 7, 60, 0);
                    out.push((sample, health, self.decoded - raw_at_snapshot));
                    raw_at_snapshot = self.decoded;
                }
            }
            out
        }
    }

    /// The live Windows Deck at a steady 60 fps: its UI takes the
    /// presenters' outcomes about four times a second, so they arrive a
    /// dozen frames at a time. Counted against everything decoded up to
    /// the snapshot, two windows in three read 295 of 300 presented
    /// ("Degraded, unpresented 100") and the next 309. Counted up to the
    /// same look, every window balances.
    #[test]
    fn smooth_sixty_fps_seen_in_batches_is_healthy_every_window() {
        let mut deck = Simulated::new();
        let windows = deck.run(12, 60, 233, 2, |frames| frames);
        let mut old_rule_alarms = 0;
        for (sample, health, raw_decoded) in &windows[1..] {
            assert_eq!(arcen_telemetry::unpresented_basis_points(sample), Some(0));
            assert_eq!(health.client_experience.state, Some(HealthState::Ok));
            assert!(sample.fps_actual.is_some_and(|fps| fps >= 57));
            let old = QosSample {
                frames_decoded: Some(*raw_decoded),
                ..*sample
            };
            old_rule_alarms +=
                usize::from(arcen_telemetry::unpresented_basis_points(&old) >= Some(50));
        }
        assert!(old_rule_alarms > 0, "the batched looks misattribute frames");
        let decoded: u64 = windows
            .iter()
            .filter_map(|(s, _, _)| s.frames_decoded)
            .sum();
        let presented: u64 = windows
            .iter()
            .filter_map(|(s, _, _)| s.frames_presented)
            .sum();
        assert!(
            decoded.abs_diff(presented) <= 2 + 1,
            "{decoded} vs {presented}"
        );
    }

    /// Frames decoded but never shown still read as lost, however the
    /// looks are batched: a presenter that shows nine in ten, and one that
    /// shows nothing while the UI keeps looking.
    #[test]
    fn a_genuine_presentation_stall_is_still_degraded_or_critical() {
        let mut deck = Simulated::new();
        let lossy = deck.run(4, 60, 233, 2, |frames| frames * 9 / 10);
        for (sample, health, _) in &lossy[1..] {
            assert!(arcen_telemetry::unpresented_basis_points(sample) >= Some(500));
            assert_eq!(health.client_experience.state, Some(HealthState::Critical));
        }
        let stalled = deck.run(4, 60, 233, 2, |_| 0);
        for (sample, health, _) in &stalled[1..] {
            assert_eq!(sample.frames_presented, Some(0));
            // All but the two frames a window allows in flight.
            assert!(arcen_telemetry::unpresented_basis_points(sample) >= Some(9_900));
            // The first stalled windows are a short stall's grace
            // (`snapshot`); the state never reads Ok.
            assert!(health.client_experience.state >= Some(HealthState::Degraded));
        }
        assert_eq!(
            stalled
                .last()
                .and_then(|(_, health, _)| health.client_experience.state),
            Some(HealthState::Critical)
        );
    }

    /// An idle desktop's keepalive a second, seen in batches: healthy.
    #[test]
    fn an_idle_desktop_seen_in_batches_is_healthy() {
        let mut deck = Simulated::new();
        for (sample, health, _) in deck.run(6, 1, 233, 0, |frames| frames) {
            assert_eq!(arcen_telemetry::unpresented_basis_points(&sample), Some(0));
            assert_eq!(health.client_experience.state, Some(HealthState::Ok));
        }
    }

    /// A Deck that stops looking at its presenters is judged on every
    /// frame it decoded: a frozen UI pass is a stall, not a pass.
    #[test]
    fn a_deck_that_stops_looking_still_reads_as_a_stall() {
        let mut deck = Simulated::new();
        let _ = deck.run(2, 60, 233, 2, |frames| frames);
        let stopped = deck.run(4, 60, u64::MAX / 4, 2, |frames| frames);
        for (sample, health, raw_decoded) in &stopped[1..] {
            assert_eq!(sample.frames_decoded, Some(*raw_decoded));
            assert_eq!(sample.frames_presented, Some(0));
            assert!(health.client_experience.state >= Some(HealthState::Degraded));
        }
        assert_eq!(
            stopped
                .last()
                .and_then(|(_, health, _)| health.client_experience.state),
            Some(HealthState::Critical)
        );
    }

    /// Match My Layout on two displays at 60 fps: the further window
    /// reports each frame as it presents it (every media wake), the root's
    /// outcomes arrive a dozen frames at a time. Only the root's looks mark
    /// the decoded count; a look marked at a further window's report counted
    /// root frames nobody had looked for yet, and a smooth session read up
    /// to 14 of 600 frames unpresented ("Degraded") in some windows.
    #[test]
    fn further_window_reports_between_root_looks_keep_every_window_balanced() {
        let telemetry = warmed_telemetry();
        telemetry.record_presenter_frames(1, 0, true);
        let mut window = telemetry.window(0);
        let (mut decoded, mut root_decoded, mut root_confirmed) = (0_u64, 0_u64, 0_u64);
        let mut secondary_pending = 0_u64;
        let mut next_pass = 233_u64;
        let mut next_snapshot = 5_007_u64;
        let mut samples = Vec::new();
        let mut next_frame = 0_u64;
        for at_ms in 1..=60_000_u64 {
            // One frame per display every 1/60 s.
            while next_frame * 1_000 / 60 < at_ms {
                next_frame += 1;
                decoded += 2;
                root_decoded += 1;
                telemetry.record_media(decoded, decoded, 0, 0, Duration::from_millis(2));
                // The further window shows the frame decoded before.
                telemetry.record_frames_presented(secondary_pending);
                secondary_pending = 1;
            }
            if at_ms == next_pass {
                next_pass += 233;
                let through = root_decoded.saturating_sub(2);
                telemetry.record_presenter_superseded(0);
                telemetry.record_presenter_frames(through - root_confirmed, 0, true);
                root_confirmed = through;
            }
            if at_ms == next_snapshot {
                next_snapshot += 5_000;
                let (_, health, sample) = telemetry.snapshot(&mut window, None, at_ms, 60, 0);
                samples.push((sample, health));
            }
        }
        for (sample, health) in &samples[1..] {
            assert!(
                sample
                    .frames_decoded
                    .is_some_and(|frames| (560..=640).contains(&frames)),
                "{sample:?}"
            );
            assert_eq!(arcen_telemetry::unpresented_basis_points(sample), Some(0));
            assert_eq!(health.client_experience.state, Some(HealthState::Ok));
        }
    }

    /// The lab's Windows Deck: frame statistics confirm frames, then stop
    /// (its window flipped independently) while it goes on presenting 60
    /// frames a second; each present resolves unconfirmed at the shared
    /// deadline. Shown with their time unknown, they count as presented
    /// while the statistics are not live. Taken as superseded for good (the
    /// latch once set), the stream read 0 fps: "Critical".
    #[test]
    fn presents_without_statistics_count_while_the_statistics_are_not_live() {
        use crate::present::confirmation::PresentStatisticsLive;
        let live_telemetry = warmed_telemetry();
        let latched_telemetry = warmed_telemetry();
        let mut live = PresentStatisticsLive::default();
        let mut latched = false;
        let mut live_window = live_telemetry.window(0);
        let mut latched_window = latched_telemetry.window(0);
        let (mut decoded, mut resolved) = (0_u64, 0_u64);
        let mut states = (Vec::new(), Vec::new());
        for pass in 1..=130_u64 {
            let at_ms = pass * 233;
            decoded = at_ms * 60 / 1_000;
            // Confirmed for six seconds; then unconfirmed, a second late.
            let through = if at_ms <= 6_000 {
                decoded.saturating_sub(2)
            } else {
                decoded.saturating_sub(60)
            };
            let batch = through.saturating_sub(resolved);
            resolved = through;
            let (confirmed, unconfirmed) = if at_ms <= 6_000 {
                (batch, 0)
            } else {
                (0, batch)
            };
            for (telemetry, ever) in [
                (&live_telemetry, live.observe(confirmed, unconfirmed)),
                (&latched_telemetry, {
                    latched |= confirmed > 0;
                    latched
                }),
            ] {
                telemetry.record_media(decoded, decoded, 0, 0, Duration::from_millis(2));
                telemetry.record_presenter_superseded(0);
                telemetry.record_presenter_frames(confirmed, unconfirmed, ever);
            }
            if pass % 21 == 0 {
                let (_, health, sample) =
                    live_telemetry.snapshot(&mut live_window, None, at_ms + 7, 60, 0);
                states.0.push((sample, health));
                let (_, health, _) =
                    latched_telemetry.snapshot(&mut latched_window, None, at_ms + 7, 60, 0);
                states.1.push(health.client_experience.state);
            }
        }
        assert!(decoded > 1_800);
        // From the first whole window after the statistics stopped.
        for (sample, health) in &states.0[2..] {
            assert!(sample.fps_actual.is_some_and(|fps| fps >= 57), "{sample:?}");
            assert_eq!(arcen_telemetry::unpresented_basis_points(sample), Some(0));
            assert_eq!(health.client_experience.state, Some(HealthState::Ok));
        }
        assert_eq!(states.1.last(), Some(&Some(HealthState::Critical)));
    }

    /// Frames the worker replaced before a presenter took them are counted
    /// with the frames decoded at the same look: handled, and the window
    /// balances.
    #[test]
    fn worker_superseded_frames_are_counted_at_the_same_look() {
        let telemetry = warmed_telemetry();
        telemetry.record_presenter_frames(1, 0, true);
        let mut window = telemetry.window(0);
        // 300 decoded, 4 of them replaced in the worker's slot; the UI
        // looked after 296 were shown; 12 more decoded (2 replaced) since.
        telemetry.record_media(300, 300, 0, 4, Duration::from_millis(2));
        telemetry.record_presenter_frames(296, 0, true);
        telemetry.record_media(312, 312, 0, 6, Duration::from_millis(2));
        let (_, health, sample) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        assert_eq!(sample.frames_decoded, Some(300));
        assert_eq!(sample.frames_superseded, Some(4));
        assert_eq!(arcen_telemetry::unpresented_basis_points(&sample), Some(0));
        assert_eq!(health.client_experience.state, Some(HealthState::Ok));
        // The next look counts the rest.
        telemetry.record_presenter_frames(10, 0, true);
        let (_, _, sample) = telemetry.snapshot(&mut window, None, 10_000, 60, 0);
        assert_eq!(sample.frames_decoded, Some(12));
        assert_eq!(sample.frames_superseded, Some(2));
        assert_eq!(sample.frames_presented, Some(10));
    }

    /// What is genuinely lost still raises the alarm, and a presenter that
    /// confirms nothing still reads as a stall.
    #[test]
    fn real_loss_and_a_silent_presenter_are_still_reported() {
        let telemetry = warmed_telemetry();
        telemetry.record_presenter_frames(1, 0, true);
        let mut window = telemetry.window(0);
        telemetry.record_media(150, 150, 0, 0, Duration::from_millis(2));
        telemetry.record_presenter_frames(130, 0, true);
        let (_, health, sample) = telemetry.snapshot(&mut window, None, 5_000, 30, 0);
        assert_eq!(sample.frames_decoded, Some(150));
        assert_eq!(health.client_experience.state, Some(HealthState::Critical));
        assert_eq!(
            health.client_experience.dominant_cause,
            Some(arcen_telemetry::HealthCause::Loss)
        );

        let mut decoded = 150;
        let mut last = None;
        for step in 2..=4_u64 {
            decoded += 150;
            telemetry.record_media(decoded, decoded, 0, 0, Duration::from_millis(2));
            telemetry.record_presenter_frames(0, 150, true);
            let (_, health, sample) = telemetry.snapshot(&mut window, None, step * 5_000, 30, 0);
            assert_eq!(sample.fps_actual, Some(0));
            last = health.client_experience.state;
        }
        assert_eq!(last, Some(HealthState::Critical));
    }

    #[test]
    fn startup_warmup_suppresses_client_fps_only() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        telemetry.record_media(1, 1, 0, 0, Duration::from_millis(2));
        let (_, health, sample) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        assert!(sample.fps_warmup);
        assert_ne!(
            health.client_experience.dominant_cause,
            Some(arcen_telemetry::HealthCause::Fps)
        );
    }

    #[test]
    fn windowed_fps_stall_transitions_ok_degraded_critical_with_hysteresis() {
        let telemetry = warmed_telemetry();
        let mut window = telemetry.window(0);
        let mut tracker = arcen_telemetry::HealthTracker::default();

        telemetry.record_media(300, 300, 0, 0, Duration::from_millis(2));
        for _ in 0..300 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        let (_, first, sample) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        assert_eq!(sample.frames_decoded, Some(300));
        assert_eq!(sample.frames_presented, Some(300));
        assert_eq!(sample.fps_actual, Some(60));
        assert!(tracker.update(5_000, &first).unwrap().is_none());

        telemetry.record_media(600, 600, 0, 0, Duration::from_millis(2));
        for _ in 0..300 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        let (_, healthy, _) = telemetry.snapshot(&mut window, None, 10_000, 60, 0);
        assert_eq!(
            tracker.update(10_000, &healthy).unwrap().unwrap().current,
            HealthState::Ok
        );

        let (_, stalled_one, sample) = telemetry.snapshot(&mut window, None, 15_000, 60, 0);
        assert_eq!(sample.fps_actual, Some(0));
        assert_eq!(stalled_one.overall, Some(HealthState::Degraded));
        assert!(tracker.update(15_000, &stalled_one).unwrap().is_none());
        let (_, stalled_two, _) = telemetry.snapshot(&mut window, None, 20_000, 60, 0);
        assert_eq!(
            tracker
                .update(20_000, &stalled_two)
                .unwrap()
                .unwrap()
                .current,
            HealthState::Degraded
        );
        let (_, stalled_three, _) = telemetry.snapshot(&mut window, None, 25_000, 60, 0);
        assert_eq!(stalled_three.overall, Some(HealthState::Critical));
        assert!(tracker.update(25_000, &stalled_three).unwrap().is_none());
        let (_, stalled_four, _) = telemetry.snapshot(&mut window, None, 30_000, 60, 0);
        assert_eq!(
            tracker
                .update(30_000, &stalled_four)
                .unwrap()
                .unwrap()
                .current,
            HealthState::Critical
        );
    }

    #[test]
    fn a_still_desktop_presenting_every_keepalive_is_healthy() {
        // The reported false alarm: a mostly idle desktop arrives at a few
        // frames a second against a target of thirty, all of them presented.
        let telemetry = warmed_telemetry();
        let mut window = telemetry.window(0);
        let mut tracker = arcen_telemetry::HealthTracker::default();
        let mut received = 0;
        for step in 1..=4_u64 {
            received += 15;
            telemetry.record_media(received, received, 0, 0, Duration::from_millis(2));
            for _ in 0..15 {
                telemetry.record_presented(Duration::from_millis(1));
            }
            let (_, health, sample) = telemetry.snapshot(&mut window, None, step * 5_000, 30, 0);
            assert_eq!(sample.fps_actual, Some(3));
            assert_eq!(sample.fps_content, Some(3));
            assert_eq!(health.client_experience.state, Some(HealthState::Ok));
            if let Some(transition) = tracker.update(step * 5_000, &health).unwrap() {
                assert_eq!(transition.current, HealthState::Ok);
            }
        }
        // Presenting far fewer than arrived is still the client's shortfall.
        telemetry.record_media(
            received + 150,
            received + 150,
            0,
            0,
            Duration::from_millis(2),
        );
        for _ in 0..50 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        let (_, health, _) = telemetry.snapshot(&mut window, None, 25_000, 30, 0);
        assert_eq!(
            health.client_experience.dominant_cause,
            Some(arcen_telemetry::HealthCause::Fps)
        );
        assert_eq!(health.client_experience.state, Some(HealthState::Critical));
    }

    #[test]
    fn heartbeat_sample_and_session_summary_use_real_aggregates() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        telemetry.record_media(120, 120, 4, 0, Duration::from_millis(2));
        for _ in 0..120 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        telemetry.record_rtt(Duration::from_millis(20));
        telemetry.record_rtt(Duration::from_millis(40));
        telemetry.record_reconnect();
        let (_, health, sample) = telemetry.snapshot(&mut window, None, 2_000, 60, 3);
        assert_eq!(sample.heartbeat_misses, Some(3));
        assert_eq!(health.overall, Some(HealthState::Critical));
        // One critical window is not the session's state until it commits.
        assert_eq!(telemetry.summary(Duration::from_secs(2)).worst_health, None);
        telemetry.record_committed_health(Some(HealthState::Ok));
        telemetry.record_committed_health(Some(HealthState::Critical));
        telemetry.record_committed_health(Some(HealthState::Ok));

        let summary = telemetry.summary(Duration::from_secs(2));
        assert_eq!(summary.frames_decoded, 120);
        assert_eq!(summary.frames_dropped, 4);
        assert_eq!(summary.avg_fps, 60);
        assert_eq!(summary.avg_rtt_ms, 30);
        assert_eq!(summary.worst_health, Some(HealthState::Critical));
        assert_eq!(summary.reconnects, 1);
    }

    #[test]
    fn presenter_frames_count_untimestamped_ones_only_until_a_timestamp_arrives() {
        let telemetry = ClientTelemetry::default();
        telemetry.record_presenter_frames(0, 0, false);
        assert_eq!(telemetry.counters().2, 0);
        assert!(!telemetry.presentation_observed.load(Ordering::Acquire));
        telemetry.record_presenter_frames(0, 30, false);
        assert_eq!(telemetry.counters().2, 30);
        telemetry.record_presenter_frames(20, 10, true);
        assert_eq!(telemetry.counters().2, 50);
        assert!(telemetry.presentation_observed.load(Ordering::Acquire));
        assert!(telemetry.summary(Duration::from_secs(10)).avg_fps > 0);
    }

    /// The live macOS Deck on a still desktop: keepalives handed to the
    /// compositor, most returned without a presentation time. They are
    /// superseded, not lost, and the window is not a stall.
    #[test]
    fn untimestamped_keepalives_on_a_still_desktop_are_superseded_not_lost() {
        let telemetry = warmed_telemetry();
        telemetry.record_presenter_frames(1, 0, true);
        let mut window = telemetry.window(0);
        let mut decoded = 0;
        for (step, confirmed) in (1..=4_u64).zip([0, 1, 0, 2]) {
            decoded += 5;
            telemetry.record_media(decoded, decoded, 0, 0, Duration::from_millis(2));
            telemetry.record_presenter_frames(confirmed, 5 - confirmed, true);
            let (snapshot, health, sample) =
                telemetry.snapshot(&mut window, None, step * 5_000, 60, 0);
            assert_eq!(sample.frames_superseded, Some(5 - confirmed));
            assert_eq!(sample.frames_submitted_untimed, Some(5 - confirmed));
            let qos = snapshot.qos.unwrap();
            assert_eq!(qos.frames_superseded, Some(5 - confirmed));
            assert_eq!(qos.frames_submitted_untimed, Some(5 - confirmed));
            assert_eq!(health.client_experience.state, Some(HealthState::Ok));
        }
        // A busy window on bunched arrivals: the newest-wins queue dropped
        // eight for newer frames and the worker replaced two.
        decoded += 300;
        telemetry.record_media(decoded, decoded, 0, 2, Duration::from_millis(2));
        telemetry.record_presenter_superseded(8);
        telemetry.record_presenter_frames(288, 2, true);
        let (_, health, sample) = telemetry.snapshot(&mut window, None, 25_000, 60, 0);
        assert_eq!(sample.frames_superseded, Some(12));
        assert_eq!(sample.frames_submitted_untimed, Some(2));
        assert_eq!(arcen_telemetry::unpresented_basis_points(&sample), Some(0));
        assert_eq!(health.client_experience.state, Some(HealthState::Ok));
    }

    /// A presenter that stopped on a still desktop: the media worker keeps
    /// replacing its slot with each keepalive, nothing reaches the
    /// compositor. Replaced frames are no proof of life: the window is a
    /// stall, critical once the stall outlasts the start-up grace.
    #[test]
    fn a_dead_presenter_on_a_still_desktop_is_a_stall() {
        let telemetry = warmed_telemetry();
        telemetry.record_presenter_frames(1, 0, true);
        let mut window = telemetry.window(0);
        let mut states = Vec::new();
        for step in 1..=3_u64 {
            telemetry.record_media(step * 5, step * 5, 0, step * 5, Duration::from_millis(2));
            let (_, health, sample) = telemetry.snapshot(&mut window, None, step * 5_000, 60, 0);
            assert_eq!(sample.frames_superseded, Some(5));
            assert_eq!(sample.frames_submitted_untimed, Some(0));
            states.push(health.client_experience.state);
        }
        assert_eq!(states.last(), Some(&Some(HealthState::Critical)));
    }
}
