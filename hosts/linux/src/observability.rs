//! Host-delivery/client-experience health assessment for one active session.
//!
//! Wraps the shared, pure `arcen_telemetry::{assess_health, HealthTracker}`
//! hysteresis with the host-local bookkeeping needed to turn periodic
//! counters and (optional) client telemetry into a `HealthStatsMsg` for the
//! client and `StructuredFields` for native `HEALTH_*`/`HEALTH_SNAPSHOT`
//! lifecycle events. A legacy (pre-observability) client that never sends
//! `client_telemetry` always reads as "unavailable" (`None`), never a
//! healthy zero — see [`SessionHealth::observe`].

use arcen_protocol::messages::{
    ClientTelemetrySnapshotMsg, HealthStateMsg, HealthStatsMsg, SampleWindowSecs,
};
use arcen_telemetry::{
    FieldValue, HealthAssessment, HealthCause, HealthState, HealthTracker, LifecycleEventKind,
    QosSample, QosTargets, StructuredFields,
};

/// Cadence of one host counter sample, matching `net::server::HEALTH_BEAT`.
const HOST_SAMPLE_WINDOW_SECS: u32 = 2;
/// 60-second cadence required for `HEALTH_SNAPSHOT` (event 1806).
const SNAPSHOT_INTERVAL_MS: u64 = 60_000;
/// Client telemetry older than this is treated as absent, never stale-zero.
const CLIENT_TELEMETRY_STALE_MS: u64 = 15_000;

/// Host-side counters sampled once per `observe` tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HostCounters {
    pub(crate) frames_sent: u64,
    pub(crate) frames_dropped: u64,
    pub(crate) bytes_sent: u64,
    pub(crate) input_events: u64,
    pub(crate) last_input_sequence: u64,
    pub(crate) last_input_type: &'static str,
}

/// Result of one `observe` tick.
#[derive(Debug)]
pub(crate) struct HealthObservation {
    pub(crate) sample: QosSample,
    pub(crate) assessment: HealthAssessment,
    pub(crate) transition: Option<(LifecycleEventKind, StructuredFields)>,
    pub(crate) snapshot_due: bool,
    pub(crate) bandwidth_mbps: f64,
    /// `Some` when the hysteresis tracker rejected this tick's monotonic
    /// `timestamp_ms` because it moved backwards (a caller clock defect,
    /// never silently discarded — the caller logs this once per
    /// occurrence rather than treating a skipped transition as a normal,
    /// unremarkable tick).
    pub(crate) clock_error: Option<arcen_telemetry::HealthTrackerError>,
}

/// Per-session health state: hysteresis tracker, last-seen client telemetry,
/// and the counters/clock needed to turn cumulative totals into per-window
/// deltas.
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

    /// Replaces the QoS/hysteresis thresholds this session assesses
    /// against, letting a validated SIGHUP reload take effect on the next
    /// `observe` tick without dropping or restarting the active session
    /// (finding: SIGHUP-reloaded `qos_targets` must reach active sessions,
    /// not only future ones).
    pub(crate) fn set_targets(&mut self, targets: QosTargets) {
        self.targets = targets;
    }

    #[cfg(test)]
    pub(crate) fn targets(&self) -> QosTargets {
        self.targets
    }

    /// Records a `HealthPingMsg.client_telemetry` snapshot at the caller's
    /// clock. A stale sample (older than `CLIENT_TELEMETRY_STALE_MS` per its
    /// own `sample_age_ms`) is discarded rather than recorded, so a slow or
    /// wedged client degrades to "unavailable" instead of stale-healthy.
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

    /// Returns the latest still-fresh client telemetry snapshot (already
    /// discarded by [`Self::observe`] once stale), used by the health tick
    /// to carry the client's own normalized network facts into
    /// `HEALTH_SNAPSHOT`/`SESSION_END` (finding: retain the latest client
    /// network snapshot for the host session summary).
    pub(crate) fn client(&self) -> Option<&ClientTelemetrySnapshotMsg> {
        self.client.as_ref()
    }

    /// Test-only accessor for the monotonic timestamp `record_client_at`
    /// stored alongside the current client telemetry, used to prove
    /// receipt is recorded on the same clock basis `observe`'s staleness
    /// check uses (re-review finding #1), rather than a wall-clock reading.
    #[cfg(test)]
    pub(crate) fn client_received_ms(&self) -> Option<u64> {
        self.client_received_ms
    }

    /// Assesses one window of host counters (and any still-fresh client
    /// telemetry), advancing the two-window hysteresis tracker.
    ///
    /// `timestamp_ms` must be a monotonic, never-decreasing clock (for
    /// example `Instant::elapsed` since a fixed session-start anchor), not
    /// wall-clock time: it drives window-elapsed math, hysteresis, and the
    /// 60-second snapshot cadence, all of which would misbehave across a
    /// wall-clock step (leap-second/NTP adjustment). Wall-clock time is
    /// used only for the lifecycle event's own record `timestamp` field,
    /// attached later by [`crate::eventlog::LifecycleEmitter`], never here.
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
        let mut clock_error = None;
        let transition = match self.tracker.update(timestamp_ms, &assessment) {
            Ok(transition) => transition.map(|transition| {
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
            }),
            Err(error) => {
                // Never silently swallowed: the caller (the per-session
                // health tick) logs this once per occurrence. The tick
                // itself still completes — a rejected hysteresis update is
                // not a fatal condition, just an untrusted transition for
                // this one tick.
                clock_error = Some(error);
                None
            }
        };

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
            clock_error,
        }
    }

    /// Builds the outbound `HealthStatsMsg` for one observation. Frame and
    /// input counts are sourced from `observation.sample` (already
    /// window-deltas computed by `observe`, matching the declared
    /// `sample_window_secs`), never from the raw cumulative `counters` —
    /// only `last_input_sequence`/`last_input_type` (last-value state, not
    /// counts) still come from `counters`. Carries the host-assessed
    /// `overall` state (never a stand-in for `Ok`).
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
            frames_sent: observation.sample.frames_sent.unwrap_or(0),
            frames_dropped: observation.sample.frames_dropped.unwrap_or(0),
            input_events: observation.sample.input_events.unwrap_or(0),
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

/// Builds `HEALTH_SNAPSHOT` fields (event 1806) for one observation.
///
/// `client_network` is the latest retained client-reported network snapshot
/// (finding: normalized fields must reach the host session summary as the
/// schema permits). Only normalized, non-identity facts are carried —
/// `interface_kind`/`scope`/`link_mbps`/`mtu` — never the client's SSID or
/// signal strength, matching this host's own probe's default-omit policy.
pub(crate) fn snapshot_fields(
    sample: &QosSample,
    assessment: &HealthAssessment,
    client_network: Option<&arcen_protocol::messages::ClientNetworkSnapshotMsg>,
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
        if let Some(basis_points) = dropped.saturating_mul(10_000).checked_div(total) {
            let _ = fields.insert(
                "drop_basis_points",
                FieldValue::Integer(i64::try_from(basis_points).unwrap_or(i64::MAX)),
            );
        }
    }
    insert_client_network_fields(&mut fields, client_network);
    fields
}

/// Builds the fields for the Level2 (Info) 10-second QoS window summary:
/// five 2-second ticks' already-windowed (not cumulative) frame/input
/// deltas plus the worst overall health state observed in that window.
/// Distinct from — and never a substitute for — `snapshot_fields`'s
/// 60-second Level0 proof-of-life `HEALTH_SNAPSHOT`.
pub(crate) fn window_summary_fields(
    window_ticks: u32,
    frames_sent: u64,
    frames_dropped: u64,
    input_events: u64,
    worst_overall: Option<HealthState>,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("window_ticks", FieldValue::Integer(i64::from(window_ticks)));
    let _ = fields.insert(
        "frames_sent",
        FieldValue::Integer(i64::try_from(frames_sent).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "frames_dropped",
        FieldValue::Integer(i64::try_from(frames_dropped).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "input_events",
        FieldValue::Integer(i64::try_from(input_events).unwrap_or(i64::MAX)),
    );
    let _ = fields.insert(
        "worst_overall_state",
        FieldValue::String(
            worst_overall
                .map_or("unavailable", health_state_name)
                .to_owned(),
        ),
    );
    fields
}

/// Inserts normalized, bounded client network-path facts (never SSID/RSSI)
/// into any lifecycle event's fields, shared by `snapshot_fields` and
/// `net::server`'s `SESSION_END`/`SESSION_INTERRUPTED` builder.
pub(crate) fn insert_client_network_fields(
    fields: &mut StructuredFields,
    client_network: Option<&arcen_protocol::messages::ClientNetworkSnapshotMsg>,
) {
    let Some(network) = client_network else {
        return;
    };
    let _ = fields.insert(
        "client_network_kind",
        FieldValue::String(client_interface_kind_name(network.interface_kind()).to_owned()),
    );
    let _ = fields.insert(
        "client_network_scope",
        FieldValue::String(client_scope_name(network.scope()).to_owned()),
    );
    if let Some(link_mbps) = network.link_mbps() {
        let _ = fields.insert(
            "client_link_mbps",
            FieldValue::Integer(i64::from(link_mbps)),
        );
    }
    if let Some(mtu) = network.mtu() {
        let _ = fields.insert("client_mtu", FieldValue::Integer(i64::from(mtu)));
    }
}

fn client_interface_kind_name(
    kind: arcen_protocol::messages::NetworkInterfaceKind,
) -> &'static str {
    use arcen_protocol::messages::NetworkInterfaceKind;
    match kind {
        NetworkInterfaceKind::Ethernet => "ethernet",
        NetworkInterfaceKind::Wifi => "wifi",
        NetworkInterfaceKind::Cellular => "cellular",
        NetworkInterfaceKind::Vpn => "vpn",
        NetworkInterfaceKind::Loopback => "loopback",
        NetworkInterfaceKind::Other => "other",
    }
}

fn client_scope_name(scope: arcen_protocol::messages::NetworkScopeMsg) -> &'static str {
    use arcen_protocol::messages::NetworkScopeMsg;
    match scope {
        NetworkScopeMsg::Lan => "lan",
        NetworkScopeMsg::Wan => "wan",
    }
}

#[allow(clippy::too_many_arguments)]
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
    let side = if assessment.client_experience.state == Some(current) {
        assessment.client_experience
    } else {
        assessment.host_delivery
    };
    let cause = side.dominant_cause.unwrap_or(HealthCause::Heartbeat);
    let (value, threshold) = cause_value_threshold(cause, current, sample, targets);
    let _ = fields.insert(
        "dominant_cause",
        FieldValue::String(health_cause_name(cause).to_owned()),
    );
    let _ = fields.insert("value", FieldValue::Integer(i64::from(value)));
    if let Some(threshold) = threshold {
        let _ = fields.insert("threshold", FieldValue::Integer(i64::from(threshold)));
    }
    fields
}

fn cause_value_threshold(
    cause: HealthCause,
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
            let (sent, dropped) = (
                sample.frames_sent.unwrap_or(0),
                sample.frames_dropped.unwrap_or(0),
            );
            let total = sent.saturating_add(dropped);
            let value = dropped
                .saturating_mul(10_000)
                .checked_div(total)
                .map_or(0, |basis_points| {
                    u32::try_from(basis_points).unwrap_or(u32::MAX)
                });
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

fn health_cause_name(cause: HealthCause) -> &'static str {
    match cause {
        HealthCause::Fps => "fps",
        HealthCause::Rtt => "rtt",
        HealthCause::Loss => "loss",
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
    use arcen_protocol::messages::ClientQosSampleMsg;

    /// Finding #4: the latest retained client network snapshot must reach
    /// `snapshot_fields()` as normalized fields, and must never carry SSID
    /// or RSSI regardless of what the client itself reported.
    #[test]
    fn snapshot_fields_include_normalized_client_network_facts_never_ssid_or_rssi() {
        let mut health = SessionHealth::new(QosTargets::default());
        let network = arcen_protocol::messages::ClientNetworkSnapshotMsg::new(
            arcen_protocol::messages::NetworkInterfaceKind::Wifi,
            arcen_protocol::messages::NetworkScopeMsg::Wan,
            Some(433),
            Some(-52),
            Some(1_500),
            Some(
                arcen_protocol::messages::NetworkIdentityMsg::try_from("home-network".to_string())
                    .expect("valid ssid"),
            ),
        )
        .expect("valid client network snapshot");
        health.record_client_at(
            1_000,
            Some(ClientTelemetrySnapshotMsg {
                qos: None,
                network: Some(network),
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
        let client_network = health
            .client()
            .and_then(|telemetry| telemetry.network.as_ref());
        let fields = snapshot_fields(&observation.sample, &observation.assessment, client_network);
        let map = fields.as_map();
        assert_eq!(
            map.get("client_network_kind"),
            Some(&FieldValue::String("wifi".to_owned()))
        );
        assert_eq!(
            map.get("client_network_scope"),
            Some(&FieldValue::String("wan".to_owned()))
        );
        assert_eq!(map.get("client_link_mbps"), Some(&FieldValue::Integer(433)));
        assert_eq!(map.get("client_mtu"), Some(&FieldValue::Integer(1_500)));
        assert!(
            !map.contains_key("client_ssid") && !map.contains_key("client_rssi_dbm"),
            "SSID/RSSI must never reach a persisted lifecycle field"
        );
    }

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
        assert!(health.client().is_none());
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

    #[test]
    fn snapshot_fields_never_show_a_healthy_zero_for_an_absent_client() {
        let mut health = SessionHealth::new(QosTargets::default());
        let observation = health.observe(
            2_000,
            30,
            HostCounters {
                frames_sent: 60,
                ..HostCounters::default()
            },
        );
        let fields = snapshot_fields(&observation.sample, &observation.assessment, None);
        let map = fields.as_map();
        assert_eq!(
            map.get("host_state"),
            Some(&FieldValue::String("ok".to_owned()))
        );
        assert!(
            !map.contains_key("client_state"),
            "an absent client must omit client_state, never report it as healthy"
        );
    }

    /// Finding #7: `health_stats` must report the window delta since the
    /// previous tick, never the raw cumulative counters, for a
    /// `sample_window_secs == 2` message.
    #[test]
    fn health_stats_reports_window_deltas_not_cumulative_totals() {
        let mut health = SessionHealth::new(QosTargets::default());
        let _ = health.observe(
            2_000,
            30,
            HostCounters {
                frames_sent: 1_000,
                frames_dropped: 40,
                input_events: 500,
                ..HostCounters::default()
            },
        );
        let counters = HostCounters {
            frames_sent: 1_060,
            frames_dropped: 42,
            input_events: 560,
            last_input_sequence: 7,
            last_input_type: "mouse_move",
            ..HostCounters::default()
        };
        let observation = health.observe(4_000, 30, counters);
        let stats = SessionHealth::health_stats(
            &observation,
            counters,
            30,
            "h264",
            "420",
            "1x1".to_string(),
        );
        assert_eq!(stats.frames_sent, 60, "must be the 1_060 - 1_000 delta");
        assert_eq!(stats.frames_dropped, 2, "must be the 42 - 40 delta");
        assert_eq!(stats.input_events, 60, "must be the 560 - 500 delta");
        // Last-value state (not a count) still reflects the raw counters.
        assert_eq!(stats.last_input_sequence, 7);
        assert_eq!(stats.last_input_type, "mouse_move");
        assert_eq!(stats.sample_window_secs, SampleWindowSecs::try_from(2).ok());
    }

    /// Finding #9: a decreasing monotonic clock must be reported through
    /// `clock_error`, never silently discarded, and must not panic or
    /// otherwise corrupt the tick.
    #[test]
    fn decreasing_monotonic_clock_is_reported_not_swallowed() {
        let mut health = SessionHealth::new(QosTargets::default());
        let first = health.observe(10_000, 30, HostCounters::default());
        assert!(first.clock_error.is_none());
        let second = health.observe(9_000, 30, HostCounters::default());
        assert_eq!(
            second.clock_error,
            Some(arcen_telemetry::HealthTrackerError::ClockMovedBackwards)
        );
        // The tick still completes and reports no (untrusted) transition,
        // rather than panicking or crashing the session.
        assert!(second.transition.is_none());
    }

    /// Finding #1: a validated SIGHUP reload must reach an active session's
    /// `SessionHealth` (not only sessions constructed afterward).
    #[test]
    fn set_targets_replaces_thresholds_for_an_active_session() {
        let mut health = SessionHealth::new(QosTargets::default());
        assert_eq!(health.targets(), QosTargets::default());
        let reloaded = QosTargets::new(80, 50, 100, 250, 50, 500, 100, 250, 3)
            .expect("valid reloaded targets");
        assert_ne!(reloaded, QosTargets::default());
        health.set_targets(reloaded);
        assert_eq!(health.targets(), reloaded);
    }
}
