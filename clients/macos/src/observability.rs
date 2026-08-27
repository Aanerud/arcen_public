use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_protocol::messages::{
    ClientQosSampleMsg, ClientTelemetrySnapshotMsg, HealthStateMsg, SampleWindowSecs,
};
use arcen_telemetry::{assess_health, HealthAssessment, HealthState, QosSample, QosTargets};

#[derive(Debug)]
pub struct ClientTelemetry {
    session_started: Instant,
    frames_received: AtomicU64,
    frames_decoded: AtomicU64,
    frames_presented: AtomicU64,
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
}

impl Default for ClientTelemetry {
    fn default() -> Self {
        Self {
            session_started: Instant::now(),
            frames_received: AtomicU64::new(0),
            frames_decoded: AtomicU64::new(0),
            frames_presented: AtomicU64::new(0),
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
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct TelemetryWindow {
    timestamp_ms: u64,
    frames_received: u64,
    frames_decoded: u64,
    frames_presented: u64,
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
    pub fn record_media(&self, received: u64, decoded: u64, dropped: u64, decode: Duration) {
        self.frames_received.store(received, Ordering::Relaxed);
        self.frames_decoded.store(decoded, Ordering::Relaxed);
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

    pub fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn window(&self, timestamp_ms: u64) -> TelemetryWindow {
        let (received, decoded, presented, dropped) = self.counters();
        TelemetryWindow {
            timestamp_ms,
            frames_received: received,
            frames_decoded: decoded,
            frames_presented: presented,
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
        let (received, decoded, presented, dropped) = self.counters();
        let received_delta = received.saturating_sub(window.frames_received);
        let decoded_delta = decoded.saturating_sub(window.frames_decoded);
        let presented_delta = presented.saturating_sub(window.frames_presented);
        let dropped_delta = dropped.saturating_sub(window.frames_dropped);
        let elapsed_ms = timestamp_ms.saturating_sub(window.timestamp_ms).max(1);

        window.timestamp_ms = timestamp_ms;
        window.frames_received = received;
        window.frames_decoded = decoded;
        window.frames_presented = presented;
        window.frames_dropped = dropped;
        window.ever_streamed |= decoded_delta != 0 || presented_delta != 0;

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

        let sample = QosSample {
            timestamp_ms,
            fps_target: Some(target_fps),
            fps_actual,
            frames_decoded: window.ever_streamed.then_some(decoded_delta),
            frames_presented: window.ever_streamed.then_some(presented_delta),
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
        self.record_health(health.overall);
        let media_observed = self.media_observed.load(Ordering::Acquire);
        let qos = ClientQosSampleMsg {
            frames_received: media_observed.then_some(received_delta),
            frames_decoded: sample.frames_decoded,
            frames_presented: sample.frames_presented,
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
        let avg_fps = if duration_ms == 0 {
            0
        } else {
            presented.saturating_mul(1_000) / duration_ms
        };
        let rtt_samples = self.rtt_samples.load(Ordering::Relaxed);
        let avg_rtt_ms = if rtt_samples == 0 {
            0
        } else {
            self.rtt_sum_ms.load(Ordering::Relaxed) / rtt_samples
        };
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

    fn record_health(&self, state: Option<HealthState>) {
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

    #[test]
    fn hot_path_updates_are_atomic_and_snapshot_serializes() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        telemetry.record_media(10, 9, 1, Duration::from_millis(7));
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
        let telemetry = ClientTelemetry::default();
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
    fn windowed_fps_stall_transitions_ok_degraded_critical_with_hysteresis() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        let mut tracker = arcen_telemetry::HealthTracker::default();

        telemetry.record_media(300, 300, 0, Duration::from_millis(2));
        for _ in 0..300 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        let (_, first, sample) = telemetry.snapshot(&mut window, None, 5_000, 60, 0);
        assert_eq!(sample.frames_decoded, Some(300));
        assert_eq!(sample.frames_presented, Some(300));
        assert_eq!(sample.fps_actual, Some(60));
        assert!(tracker.update(5_000, &first).unwrap().is_none());

        telemetry.record_media(600, 600, 0, Duration::from_millis(2));
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
    fn heartbeat_sample_and_session_summary_use_real_aggregates() {
        let telemetry = ClientTelemetry::default();
        let mut window = telemetry.window(0);
        telemetry.record_media(120, 120, 4, Duration::from_millis(2));
        for _ in 0..120 {
            telemetry.record_presented(Duration::from_millis(1));
        }
        telemetry.record_rtt(Duration::from_millis(20));
        telemetry.record_rtt(Duration::from_millis(40));
        telemetry.record_reconnect();
        let (_, health, sample) = telemetry.snapshot(&mut window, None, 2_000, 60, 3);
        assert_eq!(sample.heartbeat_misses, Some(3));
        assert_eq!(health.overall, Some(HealthState::Critical));

        let summary = telemetry.summary(Duration::from_secs(2));
        assert_eq!(summary.frames_decoded, 120);
        assert_eq!(summary.frames_dropped, 4);
        assert_eq!(summary.avg_fps, 60);
        assert_eq!(summary.avg_rtt_ms, 30);
        assert_eq!(summary.worst_health, Some(HealthState::Critical));
        assert_eq!(summary.reconnects, 1);
    }
}
