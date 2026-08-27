use arcen_protocol::messages::{
    ClientTelemetrySnapshotMsg, HealthStateMsg, HealthStatsMsg, SampleWindowSecs,
};
use arcen_telemetry::{
    FieldValue, HealthAssessment, HealthCause, HealthState, HealthTracker, LifecycleEventKind,
    QosSample, QosTargets, StructuredFields,
};

const HOST_SAMPLE_WINDOW_SECS: u32 = 2;
const SNAPSHOT_INTERVAL_MS: u64 = 60_000;
const CLIENT_TELEMETRY_STALE_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostCounters {
    pub(crate) frames_sent: u64,
    pub(crate) frames_dropped: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) input_events: u64,
    pub(crate) last_input_sequence: u64,
    pub(crate) last_input_type: &'static str,
}

#[derive(Debug)]
pub(crate) struct HealthObservation {
    pub(crate) sample: QosSample,
    pub(crate) assessment: HealthAssessment,
    pub(crate) transition: Option<(LifecycleEventKind, StructuredFields)>,
    pub(crate) snapshot_due: bool,
    pub(crate) bandwidth_mbps: f64,
}

#[derive(Debug)]
pub(crate) struct SessionHealth {
    targets: QosTargets,
    tracker: HealthTracker,
    client: Option<ClientTelemetrySnapshotMsg>,
    client_received_ms: Option<u64>,
    previous: HostCounters,
    previous_timestamp_ms: Option<u64>,
    first_timestamp_ms: Option<u64>,
    last_snapshot_ms: Option<u64>,
    unhealthy_since_ms: Option<u64>,
}

impl SessionHealth {
    pub(crate) fn new(targets: QosTargets) -> Self {
        Self {
            targets,
            tracker: HealthTracker::default(),
            client: None,
            client_received_ms: None,
            previous: HostCounters::default(),
            previous_timestamp_ms: None,
            first_timestamp_ms: None,
            last_snapshot_ms: None,
            unhealthy_since_ms: None,
        }
    }

    pub(crate) fn set_targets(&mut self, targets: QosTargets) {
        self.targets = targets;
    }

    pub(crate) fn record_client_at(
        &mut self,
        timestamp_ms: u64,
        telemetry: Option<ClientTelemetrySnapshotMsg>,
    ) {
        let telemetry = telemetry.filter(|snapshot| {
            snapshot
                .qos
                .as_ref()
                .and_then(|qos| qos.sample_age_ms)
                .is_none_or(|age| age <= CLIENT_TELEMETRY_STALE_MS)
        });
        if telemetry.is_some() {
            self.client = telemetry;
            self.client_received_ms = Some(timestamp_ms);
        } else {
            self.client = None;
            self.client_received_ms = None;
        }
    }

    pub(crate) fn client(&self) -> Option<&ClientTelemetrySnapshotMsg> {
        self.client.as_ref()
    }

    pub(crate) fn observe(
        &mut self,
        timestamp_ms: u64,
        fps_target: u32,
        counters: HostCounters,
    ) -> HealthObservation {
        if self.client_received_ms.is_some_and(|received| {
            timestamp_ms.saturating_sub(received) > CLIENT_TELEMETRY_STALE_MS
        }) {
            self.client = None;
            self.client_received_ms = None;
        }
        let elapsed_ms = self
            .previous_timestamp_ms
            .map_or(u64::from(HOST_SAMPLE_WINDOW_SECS) * 1_000, |previous| {
                timestamp_ms.saturating_sub(previous).max(1)
            });
        let sent_delta = counters
            .frames_sent
            .saturating_sub(self.previous.frames_sent);
        let bytes_delta = counters.bytes_sent.saturating_sub(self.previous.bytes_sent);
        let dropped_delta = counters
            .frames_dropped
            .saturating_sub(self.previous.frames_dropped);
        let fps_actual = sent_delta
            .saturating_mul(1_000)
            .checked_div(elapsed_ms)
            .and_then(|value| u32::try_from(value).ok());
        let mut sample = QosSample {
            timestamp_ms,
            fps_actual,
            fps_target: Some(fps_target),
            frames_sent: Some(sent_delta),
            frames_dropped: Some(dropped_delta),
            input_events: Some(
                counters
                    .input_events
                    .saturating_sub(self.previous.input_events),
            ),
            ..QosSample::default()
        };
        let mut client_sample = QosSample {
            timestamp_ms,
            fps_target: Some(fps_target),
            ..QosSample::default()
        };
        apply_client_sample(&mut client_sample, self.client.as_ref());
        sample.frames_decoded = client_sample.frames_decoded;
        sample.frames_presented = client_sample.frames_presented;
        sample.decode_time_ms = client_sample.decode_time_ms;
        sample.display_time_ms = client_sample.display_time_ms;
        sample.input_latency_ms = client_sample.input_latency_ms;

        let host = arcen_telemetry::assess_health(&sample, &self.targets).host_delivery;
        let client =
            arcen_telemetry::assess_health(&client_sample, &self.targets).client_experience;
        let assessment = HealthAssessment {
            host_delivery: host,
            client_experience: client,
            overall: match (host.state, client.state) {
                (Some(host), Some(client)) => Some(host.max(client)),
                (state @ Some(_), None) | (None, state @ Some(_)) => state,
                (None, None) => None,
            },
        };
        let transition = self
            .tracker
            .update(timestamp_ms, &assessment)
            .ok()
            .flatten()
            .map(|transition| {
                let kind = match transition.current {
                    HealthState::Ok => LifecycleEventKind::HealthOk,
                    HealthState::Degraded => LifecycleEventKind::HealthDegraded,
                    HealthState::Critical => LifecycleEventKind::HealthCritical,
                };
                let transition_sample =
                    if assessment.client_experience.state == Some(transition.current) {
                        &client_sample
                    } else {
                        &sample
                    };
                let fields = transition_fields(
                    kind,
                    transition.previous,
                    transition.current,
                    timestamp_ms,
                    transition_sample,
                    &assessment,
                    &self.targets,
                    &mut self.unhealthy_since_ms,
                );
                (kind, fields)
            });

        let first = *self.first_timestamp_ms.get_or_insert(timestamp_ms);
        let snapshot_due = timestamp_ms.saturating_sub(first) >= SNAPSHOT_INTERVAL_MS
            && self
                .last_snapshot_ms
                .is_none_or(|last| timestamp_ms.saturating_sub(last) >= SNAPSHOT_INTERVAL_MS);
        if snapshot_due {
            self.last_snapshot_ms = Some(timestamp_ms);
        }
        self.previous = counters;
        self.previous_timestamp_ms = Some(timestamp_ms);

        let bounded_bytes = u32::try_from(bytes_delta).unwrap_or(u32::MAX);
        let bounded_elapsed_ms = u32::try_from(elapsed_ms).unwrap_or(u32::MAX);
        HealthObservation {
            sample,
            assessment,
            transition,
            snapshot_due,
            bandwidth_mbps: (f64::from(bounded_bytes) * 8.0)
                / (f64::from(bounded_elapsed_ms) * 1_000.0),
        }
    }

    pub(crate) fn health_stats(
        observation: &HealthObservation,
        counters: HostCounters,
        fps_target: u32,
        codec: &str,
        chroma: &str,
        resolution: String,
    ) -> HealthStatsMsg {
        HealthStatsMsg {
            fps_actual: f64::from(observation.sample.fps_actual.unwrap_or(0)),
            fps_target: f64::from(fps_target),
            bandwidth_mbps: observation.bandwidth_mbps,
            frames_sent: counters.frames_sent,
            frames_dropped: counters.frames_dropped,
            input_events: counters.input_events,
            last_input_sequence: counters.last_input_sequence,
            last_input_type: counters.last_input_type.to_owned(),
            codec: codec.to_owned(),
            chroma: chroma.to_owned(),
            resolution,
            clients_connected: 1,
            health_state: observation.assessment.overall.map(health_state_message),
            sample_window_secs: SampleWindowSecs::try_from(HOST_SAMPLE_WINDOW_SECS).ok(),
            ..HealthStatsMsg::default()
        }
    }
}

fn apply_client_sample(sample: &mut QosSample, telemetry: Option<&ClientTelemetrySnapshotMsg>) {
    let Some(qos) = telemetry.and_then(|telemetry| telemetry.qos.as_ref()) else {
        return;
    };
    sample.frames_decoded = qos.frames_decoded;
    sample.frames_presented = qos.frames_presented;
    sample.decode_time_ms = qos.decode_time_ms;
    sample.display_time_ms = qos.display_time_ms;
    sample.input_latency_ms = qos.input_send_time_ms;
    if let (Some(presented), Some(window)) = (qos.frames_presented, qos.sample_window_secs) {
        sample.fps_actual = u32::try_from(presented / u64::from(window.get())).ok();
    }
}

fn health_state_message(state: HealthState) -> HealthStateMsg {
    match state {
        HealthState::Ok => HealthStateMsg::Ok,
        HealthState::Degraded => HealthStateMsg::Degraded,
        HealthState::Critical => HealthStateMsg::Critical,
    }
}

pub(crate) fn snapshot_fields(
    sample: &QosSample,
    assessment: &HealthAssessment,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "overall_state",
        FieldValue::String(
            assessment
                .overall
                .map_or("unavailable", health_state_name)
                .to_owned(),
        ),
    );
    insert_state(&mut fields, "host_state", assessment.host_delivery.state);
    insert_state(
        &mut fields,
        "client_state",
        assessment.client_experience.state,
    );
    insert_u32(&mut fields, "fps_actual", sample.fps_actual);
    insert_u32(&mut fields, "fps_target", sample.fps_target);
    insert_u32(&mut fields, "rtt_ms", sample.rtt_ms);
    insert_u32(&mut fields, "heartbeat_misses", sample.heartbeat_misses);
    if let (Some(sent), Some(dropped)) = (sample.frames_sent, sample.frames_dropped) {
        let total = sent.saturating_add(dropped);
        if total != 0 {
            let basis_points = dropped.saturating_mul(10_000) / total;
            let _ = fields.insert(
                "drop_basis_points",
                FieldValue::Integer(i64::try_from(basis_points).unwrap_or(i64::MAX)),
            );
        }
    }
    // No `unpresented_basis_points` field here for the same reason
    // `transition_fields` omits `dominant_side`: `HEALTH_SNAPSHOT` has a closed
    // field schema and an undeclared key would make the whole snapshot be
    // dropped. Client supersession stays visible through the transition
    // event's `dominant_cause=unpresented` and through the Deck's own
    // telemetry.
    fields
}

fn transition_fields(
    kind: LifecycleEventKind,
    previous: Option<HealthState>,
    current: HealthState,
    timestamp_ms: u64,
    sample: &QosSample,
    assessment: &HealthAssessment,
    targets: &QosTargets,
    unhealthy_since_ms: &mut Option<u64>,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    if kind == LifecycleEventKind::HealthOk {
        let _ = fields.insert(
            "previous_state",
            FieldValue::String(previous.map_or("unavailable", health_state_name).to_owned()),
        );
        let duration = unhealthy_since_ms
            .take()
            .map_or(0, |started| timestamp_ms.saturating_sub(started));
        let _ = fields.insert(
            "degraded_duration_ms",
            FieldValue::Integer(i64::try_from(duration).unwrap_or(i64::MAX)),
        );
        return fields;
    }
    unhealthy_since_ms.get_or_insert(timestamp_ms);
    let (side, selected) = if assessment.client_experience.state == Some(current) {
        (
            assessment.client_experience,
            HealthSideKind::ClientExperience,
        )
    } else {
        (assessment.host_delivery, HealthSideKind::HostDelivery)
    };
    let cause = side.dominant_cause.unwrap_or(HealthCause::Heartbeat);
    let (value, threshold) = cause_value_threshold(cause, selected, current, sample, targets);
    // Deliberately no `dominant_side` field. It would be genuinely useful, but
    // `HEALTH_DEGRADED`/`HEALTH_CRITICAL` carry a *closed* field schema
    // (`arcen_telemetry::lifecycle`), mirrored in
    // `scripts/observability_event_definitions.py`, and any undeclared field
    // makes `ValidatedLifecycleEvent::new` reject the whole event — which
    // would silently delete every health transition rather than improve one.
    // The cause name already carries the distinction unambiguously: only the
    // client side can produce `unpresented`, and only host delivery can
    // produce `loss`.
    let _ = fields.insert(
        "dominant_cause",
        FieldValue::String(health_cause_name(cause, selected).to_owned()),
    );
    let _ = fields.insert("value", FieldValue::Integer(i64::from(value)));
    if let Some(threshold) = threshold {
        let _ = fields.insert("threshold", FieldValue::Integer(i64::from(threshold)));
    }
    fields
}

/// Which side of the session the reported cause was selected from.
///
/// [`HealthCause`] is deliberately shared by both sides, so the cause alone
/// does not say which counters produced it. Losing that distinction is what
/// let the host print a client presentation problem using host delivery
/// numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthSideKind {
    HostDelivery,
    ClientExperience,
}

/// Fraction of decoded frames that never reached the screen, in basis points.
///
/// **This is not transport loss**, despite arriving as [`HealthCause::Loss`]:
/// `assess_presentation_drop` reuses that cause for client supersession. Every
/// frame counted here arrived intact and decoded successfully before a newer
/// frame replaced it, which is the normal consequence of a client that always
/// shows the latest frame.
///
/// The Deck already reports exactly this, under the name `unpresented`. The
/// host did not: it selected the client side's cause and then computed the
/// value from host `frames_sent`/`frames_dropped`, which for a client-only
/// sample are absent. A grading session with 13.98% supersession and zero
/// packet drops therefore logged `dominant_cause=loss value=0`, which reads as
/// a network fault that does not exist and hides the real presentation
/// problem.
fn unpresented_basis_points(sample: &QosSample) -> u32 {
    let decoded = sample.frames_decoded.unwrap_or(0);
    if decoded == 0 {
        return 0;
    }
    let missed = decoded.saturating_sub(sample.frames_presented.unwrap_or(0));
    u32::try_from(missed.saturating_mul(10_000) / decoded).unwrap_or(u32::MAX)
}

fn cause_value_threshold(
    cause: HealthCause,
    side: HealthSideKind,
    state: HealthState,
    sample: &QosSample,
    targets: &QosTargets,
) -> (u32, Option<u32>) {
    match cause {
        HealthCause::Fps => (
            sample.fps_actual.unwrap_or(0),
            sample.fps_target.map(|target| {
                let percent = if state == HealthState::Critical {
                    targets.fps_critical_percent()
                } else {
                    targets.fps_degraded_percent()
                };
                target.saturating_mul(u32::from(percent)) / 100
            }),
        ),
        HealthCause::Rtt => (
            sample.rtt_ms.unwrap_or(0),
            Some(if state == HealthState::Critical {
                targets.rtt_critical_ms()
            } else {
                targets.rtt_degraded_ms()
            }),
        ),
        HealthCause::Loss => {
            let value = match side {
                HealthSideKind::ClientExperience => unpresented_basis_points(sample),
                HealthSideKind::HostDelivery => {
                    let (sent, dropped) = (
                        sample.frames_sent.unwrap_or(0),
                        sample.frames_dropped.unwrap_or(0),
                    );
                    let total = sent.saturating_add(dropped);
                    if total == 0 {
                        0
                    } else {
                        u32::try_from(dropped.saturating_mul(10_000) / total).unwrap_or(u32::MAX)
                    }
                }
            };
            (
                value,
                Some(u32::from(if state == HealthState::Critical {
                    targets.drop_critical_basis_points()
                } else {
                    targets.drop_degraded_basis_points()
                })),
            )
        }
        HealthCause::InputLatency => (
            sample.input_latency_ms.unwrap_or(0),
            Some(if state == HealthState::Critical {
                targets.input_critical_ms()
            } else {
                targets.input_degraded_ms()
            }),
        ),
        HealthCause::Heartbeat => (
            sample.heartbeat_misses.unwrap_or(0),
            Some(targets.heartbeat_critical_misses()),
        ),
    }
}

fn health_state_name(state: HealthState) -> &'static str {
    match state {
        HealthState::Ok => "ok",
        HealthState::Degraded => "degraded",
        HealthState::Critical => "critical",
    }
}

fn health_cause_name(cause: HealthCause, side: HealthSideKind) -> &'static str {
    match cause {
        HealthCause::Fps => "fps",
        HealthCause::Rtt => "rtt",
        // Matching the Deck exactly: on the client side this cause carries
        // presentation supersession, not transport loss. Real packet loss is
        // reported separately as loss epochs and keeps the name `loss`.
        HealthCause::Loss => match side {
            HealthSideKind::HostDelivery => "loss",
            HealthSideKind::ClientExperience => "unpresented",
        },
        HealthCause::InputLatency => "input_latency",
        HealthCause::Heartbeat => "heartbeat",
    }
}

fn insert_state(fields: &mut StructuredFields, key: &str, state: Option<HealthState>) {
    if let Some(state) = state {
        let _ = fields.insert(key, FieldValue::String(health_state_name(state).to_owned()));
    }
}

fn insert_u32(fields: &mut StructuredFields, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        let _ = fields.insert(key, FieldValue::Integer(i64::from(value)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::messages::{ClientQosSampleMsg, ClientTelemetrySnapshotMsg};

    #[test]
    fn legacy_client_absence_stays_unavailable() {
        let mut health = SessionHealth::new(QosTargets::default());
        let observation = health.observe(
            2_000,
            30,
            HostCounters {
                frames_sent: 60,
                ..HostCounters::default()
            },
        );
        assert_eq!(observation.assessment.client_experience.state, None);
        assert_eq!(
            observation.assessment.host_delivery.state,
            Some(HealthState::Ok)
        );
    }

    #[test]
    fn client_and_host_health_are_assessed_independently() {
        let mut health = SessionHealth::new(QosTargets::default());
        health.record_client_at(
            1_000,
            Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_decoded: Some(30),
                    frames_presented: Some(10),
                    sample_window_secs: SampleWindowSecs::try_from(2).ok(),
                    ..ClientQosSampleMsg::default()
                }),
                network: None,
            }),
        );
        let observation = health.observe(
            2_000,
            30,
            HostCounters {
                frames_sent: 60,
                ..HostCounters::default()
            },
        );
        assert_eq!(
            observation.assessment.host_delivery.state,
            Some(HealthState::Ok)
        );
        assert_eq!(
            observation.assessment.client_experience.state,
            Some(HealthState::Critical)
        );
        assert_eq!(observation.assessment.overall, Some(HealthState::Critical));
    }

    #[test]
    fn stale_client_telemetry_returns_to_unavailable() {
        let mut health = SessionHealth::new(QosTargets::default());
        health.record_client_at(
            1_000,
            Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_presented: Some(60),
                    sample_window_secs: SampleWindowSecs::try_from(2).ok(),
                    ..ClientQosSampleMsg::default()
                }),
                network: None,
            }),
        );
        let observation = health.observe(
            16_001,
            30,
            HostCounters {
                frames_sent: 60,
                ..HostCounters::default()
            },
        );
        assert_eq!(observation.assessment.client_experience.state, None);
    }

    #[test]
    fn snapshot_is_exactly_sixty_second_cadenced() {
        let mut health = SessionHealth::new(QosTargets::default());
        assert!(
            !health
                .observe(1_000, 30, HostCounters::default())
                .snapshot_due
        );
        assert!(
            !health
                .observe(60_999, 30, HostCounters::default())
                .snapshot_due
        );
        assert!(
            health
                .observe(61_000, 30, HostCounters::default())
                .snapshot_due
        );
        assert!(
            !health
                .observe(120_999, 30, HostCounters::default())
                .snapshot_due
        );
        assert!(
            health
                .observe(121_000, 30, HostCounters::default())
                .snapshot_due
        );
    }

    #[test]
    fn transitions_require_two_windows() {
        let mut health = SessionHealth::new(QosTargets::default());
        let first = health.observe(
            2_000,
            30,
            HostCounters {
                frames_sent: 10,
                ..HostCounters::default()
            },
        );
        assert!(first.transition.is_none());
        let second = health.observe(
            4_000,
            30,
            HostCounters {
                frames_sent: 20,
                ..HostCounters::default()
            },
        );
        assert_eq!(
            second.transition.as_ref().map(|(kind, _)| *kind),
            Some(LifecycleEventKind::HealthCritical)
        );
    }

    fn field_string(fields: &StructuredFields, key: &str) -> Option<String> {
        match fields.as_map().get(key) {
            Some(FieldValue::String(text)) => Some(text.clone()),
            _ => None,
        }
    }

    fn field_integer(fields: &StructuredFields, key: &str) -> Option<i64> {
        match fields.as_map().get(key) {
            Some(FieldValue::Integer(number)) => Some(*number),
            _ => None,
        }
    }

    /// Regression for the session that logged `dominant_cause=loss value=0`
    /// while the Deck simultaneously and correctly logged
    /// `dominant_cause=unpresented value=1398`.
    ///
    /// Both sides are critical, the host dropped nothing, and the client
    /// decoded far more than it presented. The host must therefore report the
    /// client's presentation number, not a host loss figure of zero that sends
    /// a tester looking for a network fault that does not exist.
    #[test]
    fn client_presentation_loss_is_reported_with_the_client_value_not_host_zero() {
        let mut health = SessionHealth::new(QosTargets::default());
        // Host delivery is critical on FPS (10 of a 30 target) with zero
        // drops; the client is critical on presentation supersession.
        let client = |decoded: u64, presented: u64| {
            Some(ClientTelemetrySnapshotMsg {
                qos: Some(ClientQosSampleMsg {
                    frames_decoded: Some(decoded),
                    frames_presented: Some(presented),
                    sample_window_secs: SampleWindowSecs::try_from(2).ok(),
                    ..ClientQosSampleMsg::default()
                }),
                network: None,
            })
        };
        health.record_client_at(1_000, client(1_000, 861));
        let first = health.observe(
            2_000,
            30,
            HostCounters {
                frames_sent: 20,
                frames_dropped: 0,
                ..HostCounters::default()
            },
        );
        assert!(first.transition.is_none());
        health.record_client_at(3_000, client(2_000, 1_722));
        let second = health.observe(
            4_000,
            30,
            HostCounters {
                frames_sent: 40,
                frames_dropped: 0,
                ..HostCounters::default()
            },
        );

        assert_eq!(
            second.assessment.host_delivery.state,
            Some(HealthState::Critical)
        );
        assert_eq!(
            second.assessment.client_experience.state,
            Some(HealthState::Critical)
        );
        assert_eq!(second.sample.frames_dropped, Some(0));
        assert!(second.sample.frames_decoded > second.sample.frames_presented);

        let (kind, fields) = second.transition.expect("critical transition");
        assert_eq!(kind, LifecycleEventKind::HealthCritical);
        assert_eq!(
            field_string(&fields, "dominant_cause").as_deref(),
            Some("unpresented")
        );
        // 1000 decoded, 861 presented -> 139 unpresented -> 1390 basis points.
        assert_eq!(field_integer(&fields, "value"), Some(1_390));
        assert_ne!(field_integer(&fields, "value"), Some(0));
        assert_eq!(
            field_integer(&fields, "threshold"),
            Some(i64::from(
                QosTargets::default().drop_critical_basis_points()
            ))
        );
    }

    /// `HEALTH_DEGRADED`, `HEALTH_CRITICAL`, `HEALTH_OK` and `HEALTH_SNAPSHOT`
    /// all carry **closed** field schemas: `ValidatedLifecycleEvent::new`
    /// rejects any undeclared key, and a rejected event is dropped entirely.
    ///
    /// Adding a plausible-looking diagnostic field to one of these is
    /// therefore not a small improvement that might be ignored — it deletes
    /// every event of that kind. Nothing else on the Windows host proves the
    /// fields it generates are actually accepted, so this does.
    #[test]
    fn every_generated_health_field_set_is_accepted_by_the_closed_schema() {
        let correlation = arcen_telemetry::CorrelationId::new("0123456789abcdef")
            .expect("valid correlation identifier");
        let mut health = SessionHealth::new(QosTargets::default());
        let targets = QosTargets::default();

        // Two windows per state, because a transition is only committed once
        // the same state has been observed twice.
        //
        // Delivered FPS against the target is what drives the state: critical
        // below `fps_critical_percent`, degraded below `fps_degraded_percent`,
        // otherwise ok.
        //
        // The target is 100 and every window is two seconds, so
        // `fps_actual = frames / 2` and `percent = fps_actual`: any whole
        // percentage is exactly representable and nothing is lost to integer
        // flooring. That matters because the bands are derived from
        // `QosTargets` rather than hardcoded, and a future default of, say,
        // critical=71/degraded=72 would otherwise quantise the degraded sample
        // straight back into the critical band and fail this test for no real
        // reason.
        const TARGET_FPS: u32 = 100;
        let frames_at_percent = |percent: u32| u64::from(percent) * 2;
        let critical_percent = u32::from(targets.fps_critical_percent());
        let degraded_percent = u32::from(targets.fps_degraded_percent());
        let steps: [u64; 6] = [
            frames_at_percent(critical_percent / 2),
            frames_at_percent(critical_percent / 2),
            frames_at_percent((critical_percent + degraded_percent) / 2),
            frames_at_percent((critical_percent + degraded_percent) / 2),
            frames_at_percent(degraded_percent + 30),
            frames_at_percent(degraded_percent + 30),
        ];

        let mut kinds = Vec::new();
        let mut snapshots = 0;
        let mut timestamp = 2_000u64;
        let mut sent = 0u64;
        for frames in steps {
            sent += frames;
            let observation = health.observe(
                timestamp,
                TARGET_FPS,
                HostCounters {
                    frames_sent: sent,
                    ..HostCounters::default()
                },
            );
            if let Some((kind, fields)) = observation.transition {
                arcen_telemetry::ValidatedLifecycleEvent::new(kind, correlation.clone(), fields)
                    .unwrap_or_else(|error| {
                        panic!("{kind:?} produced fields the closed schema rejects: {error:?}")
                    });
                kinds.push(kind);
            }
            let snapshot = snapshot_fields(&observation.sample, &observation.assessment);
            arcen_telemetry::ValidatedLifecycleEvent::new(
                LifecycleEventKind::HealthSnapshot,
                correlation.clone(),
                snapshot,
            )
            .unwrap_or_else(|error| {
                panic!("health snapshot produced fields the closed schema rejects: {error:?}")
            });
            snapshots += 1;
            timestamp += 2_000;
        }

        // The point of the test is that *every* health event kind the host can
        // emit was actually built and validated, not that some of them were.
        assert!(
            kinds.contains(&LifecycleEventKind::HealthCritical),
            "critical transition never fired: {kinds:?}"
        );
        assert!(
            kinds.contains(&LifecycleEventKind::HealthDegraded),
            "degraded transition never fired: {kinds:?}"
        );
        assert!(
            kinds.contains(&LifecycleEventKind::HealthOk),
            "recovery transition never fired, so HEALTH_OK's field set is unproven: {kinds:?}"
        );
        assert_eq!(snapshots, steps.len());
    }

    /// Real host packet/delivery loss keeps its name and its own counters, so
    /// this fix cannot make a genuine network fault look like a presentation
    /// problem.
    #[test]
    fn host_delivery_loss_keeps_its_name_and_host_counters() {
        let mut health = SessionHealth::new(QosTargets::default());
        let counters = |sent: u64, dropped: u64| HostCounters {
            frames_sent: sent,
            frames_dropped: dropped,
            ..HostCounters::default()
        };
        assert!(health
            .observe(2_000, 30, counters(60, 60))
            .transition
            .is_none());
        let second = health.observe(4_000, 30, counters(120, 120));

        assert_eq!(second.assessment.client_experience.state, None);
        let (_, fields) = second.transition.expect("critical transition");
        assert_eq!(
            field_string(&fields, "dominant_cause").as_deref(),
            Some("loss")
        );
        // 60 sent, 60 dropped in the window -> 5000 basis points, and the FPS
        // term stays Ok so loss is genuinely the dominant cause.
        assert_eq!(field_integer(&fields, "value"), Some(5_000));
    }

    #[test]
    fn snapshots_keep_host_loss_visible_without_undeclared_fields() {
        let sample = QosSample {
            timestamp_ms: 4_000,
            frames_sent: Some(60),
            frames_dropped: Some(0),
            frames_decoded: Some(1_000),
            frames_presented: Some(861),
            ..QosSample::default()
        };
        let fields = snapshot_fields(&sample, &HealthAssessment::default());
        assert_eq!(field_integer(&fields, "drop_basis_points"), Some(0));
        assert_eq!(field_integer(&fields, "unpresented_basis_points"), None);
    }
}
