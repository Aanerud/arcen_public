// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host-delivery and client-experience health for one active session, the
//! same on every Pier.
//!
//! Wraps the pure `arcen_telemetry` pieces — [`HostWindowTracker`] (cumulative
//! counters to per-window samples), [`assess_health`] and the two-window
//! [`HealthTracker`] hysteresis — with the bookkeeping a Pier needs to turn a
//! periodic tick of its counters and the Deck's (optional) client telemetry
//! into:
//!
//! - the `health_stats` message for the Deck ([`SessionHealth::health_stats`]);
//! - the fields of the `HEALTH_OK`/`HEALTH_DEGRADED`/`HEALTH_CRITICAL`
//!   transition events ([`HealthObservation::transition`]);
//! - the 60-second `HEALTH_SNAPSHOT` ([`snapshot_fields`]) and the 10-second
//!   window summary ([`window_summary_fields`]).
//!
//! A client that never sends `client_telemetry` reads as "unavailable"
//! (`None`), never as a healthy zero.
//!
//! Everything here is plain arithmetic on caller-owned values: no locks, no
//! I/O and no allocation beyond the returned fields, so a health tick cannot
//! block a video thread. The caller supplies a monotonic clock.

use arcen_protocol::messages::{
    ClientNetworkSnapshotMsg, ClientTelemetrySnapshotMsg, HealthStateMsg, HealthStatsMsg,
    NetworkInterfaceKind, NetworkScopeMsg, SampleWindowSecs,
};
use arcen_telemetry::{
    FieldValue, HealthAssessment, HealthCause, HealthSide, HealthState, HealthTracker,
    HealthTrackerError, HostCounterTotals, HostWindowTracker, LifecycleEventKind, QosSample,
    QosTargets, StructuredFields, assess_health,
};

/// Logs a change of the rate a Pier holds a session at (the shared
/// `arcen_media::video::HostRateGovernor`): `reason` is its token
/// (`host_shortfall` with the encoded and delivered counts that showed it,
/// or `probe`). A rate below the session's is never silent.
pub fn log_governed_frame_rate(
    fps: u32,
    previous_fps: u32,
    session_fps: u32,
    reason: &str,
    counts: Option<(u32, u32)>,
) {
    let (encoded_fps, source_fps) = counts.unzip();
    if counts.is_some() {
        tracing::warn!(
            target: arcen_telemetry::names::target::MEDIA,
            fps,
            previous_fps,
            session_fps,
            encoded_fps,
            source_fps,
            fps_basis = reason,
            "the host encoded below the session's frame rate while its capture delivered it; \
             lowering the rate until it holds"
        );
    } else {
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            fps,
            previous_fps,
            session_fps,
            reason,
            "probing a higher frame rate after the host held the lower one"
        );
    }
}

/// Cadence of one host counter sample: every Pier sends `health_stats`
/// every two seconds.
pub const HOST_SAMPLE_WINDOW_SECS: u32 = 2;
/// Cadence of `HEALTH_SNAPSHOT` (event 1806).
pub const SNAPSHOT_INTERVAL_MS: u64 = 60_000;
/// Client telemetry older than this is treated as absent, never stale-zero.
pub const CLIENT_TELEMETRY_STALE_MS: u64 = 15_000;

/// What the capture source proved about the rate its content changed at,
/// for [`QosSample::fps_content`]: a still desktop is judged against what
/// changed rather than against the target alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEvidence {
    /// A rate the capture pipeline proved for this window (Linux capenc's
    /// `no_damage` seconds, the Windows pipeline's idle polls), or `None`
    /// when it proved nothing.
    Rate(Option<u32>),
    /// Cumulative changed-content frames a damage-driven source offered
    /// (macOS `ScreenCaptureKit`). The window's rate counts as the content
    /// rate only when `proven` (the window also showed the screen still);
    /// the total is passed every tick so the next window's delta stays right.
    Offered {
        /// Changed-content frames offered since the session started.
        total: u64,
        /// Whether this window proved the source, not the pipeline, set the
        /// pace.
        proven: bool,
    },
}

impl Default for ContentEvidence {
    fn default() -> Self {
        Self::Rate(None)
    }
}

/// Host-side counters sampled once per [`SessionHealth::observe`] tick.
/// Counts are cumulative since the session started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostCounters {
    /// Frames sent.
    pub frames_sent: u64,
    /// Frames dropped before sending.
    pub frames_dropped: u64,
    /// Media bytes sent.
    pub bytes_sent: u64,
    /// Input events applied.
    pub input_events: u64,
    /// The last input sequence number (last-value state, not a count).
    pub last_input_sequence: u64,
    /// The last input type (last-value state, not a count).
    pub last_input_type: &'static str,
    /// The content rate the capture source proved, if any. A host that
    /// captures several outputs reports their combined bound
    /// ([`arcen_telemetry::outputs_content`]).
    pub content: ContentEvidence,
    /// Outputs (monitors) whose frames the counts above include; `0` reads
    /// as one.
    pub outputs: u32,
    /// The window's whole frame-rate target when the outputs run at their
    /// own rates: the sum of each output's
    /// ([`arcen_telemetry::OutputsContent::fps_target`]). `0` reads as the
    /// `fps_target` passed to [`SessionHealth::observe`] times
    /// [`Self::outputs`], every output at that one rate. The counts are the
    /// whole session's, so the window is judged against the whole target.
    pub outputs_fps_target: u32,
}

impl HostCounters {
    fn output_count(&self) -> u32 {
        self.outputs.max(1)
    }

    fn window_fps_target(&self, per_output: u32) -> u32 {
        match self.outputs_fps_target {
            0 => per_output.saturating_mul(self.output_count()),
            whole => whole,
        }
    }
}

/// Result of one [`SessionHealth::observe`] tick.
#[derive(Debug)]
pub struct HealthObservation {
    /// The host-delivery sample of this window (client facts merged in).
    pub sample: QosSample,
    /// Host, client and overall states.
    pub assessment: HealthAssessment,
    /// A committed state change and its event fields.
    pub transition: Option<(LifecycleEventKind, StructuredFields)>,
    /// Whether a `HEALTH_SNAPSHOT` is due on this tick.
    pub snapshot_due: bool,
    /// Media bytes sent over the window, in megabits per second.
    pub bandwidth_mbps: f64,
    /// `Some` when the hysteresis tracker rejected this tick because the
    /// monotonic `timestamp_ms` moved backwards: a caller clock defect the
    /// caller logs, never a silently skipped tick.
    pub clock_error: Option<HealthTrackerError>,
}

/// Per-session health state: the window tracker, the hysteresis trackers and
/// the last fresh client telemetry.
///
/// Each side keeps its own two-window hysteresis over its own windows: the
/// host's every tick, the client's once per client sample. A Deck reports
/// every five seconds and a Pier ticks every two, so one client window was
/// judged on two or three ticks and a single window outside its targets
/// passed the two-window rule by itself (seen live: one window of a motion
/// burst committed `HEALTH_CRITICAL` on the Pier while the Deck itself
/// stayed Ok). The session's committed state is the worse of the two.
#[derive(Debug)]
pub struct SessionHealth {
    targets: QosTargets,
    host_tracker: HealthTracker,
    client_tracker: HealthTracker,
    committed: Option<HealthState>,
    /// Each side's window when its tracker last committed: what a
    /// transition the side holds reports (cause, value, threshold).
    host_evidence: Option<(HealthSide, QosSample)>,
    client_evidence: Option<(HealthSide, QosSample)>,
    client_generation: u64,
    client_assessed_generation: u64,
    /// Consecutive ticks without client telemetry; the client side's
    /// committed state is dropped after two, as any change must hold.
    client_absent_ticks: u8,
    window: HostWindowTracker,
    client: Option<ClientTelemetrySnapshotMsg>,
    client_received_ms: Option<u64>,
    first_timestamp_ms: Option<u64>,
    last_snapshot_ms: Option<u64>,
    unhealthy_since_ms: Option<u64>,
    fps_warmup_enabled: bool,
}

impl SessionHealth {
    /// A session judged against `targets` from its first window.
    #[must_use]
    pub fn new(targets: QosTargets) -> Self {
        Self {
            targets,
            host_tracker: HealthTracker::default(),
            client_tracker: HealthTracker::default(),
            committed: None,
            host_evidence: None,
            client_evidence: None,
            client_generation: 0,
            client_assessed_generation: 0,
            client_absent_ticks: 0,
            window: HostWindowTracker::default(),
            client: None,
            client_received_ms: None,
            first_timestamp_ms: None,
            last_snapshot_ms: None,
            unhealthy_since_ms: None,
            fps_warmup_enabled: false,
        }
    }

    /// A session whose frame rate is not judged for its first
    /// [`arcen_telemetry::HOST_FPS_WARMUP_MS`]: a stream ramps up after start, resume and
    /// recovery. What every Pier runs.
    #[must_use]
    pub fn new_with_fps_warmup(targets: QosTargets) -> Self {
        Self {
            fps_warmup_enabled: true,
            ..Self::new(targets)
        }
    }

    /// Replaces the thresholds this session is judged against, so a
    /// validated configuration reload reaches active sessions on their next
    /// tick, not only future ones.
    pub fn set_targets(&mut self, targets: QosTargets) {
        self.targets = targets;
    }

    /// The thresholds this session is judged against.
    #[must_use]
    pub const fn targets(&self) -> QosTargets {
        self.targets
    }

    /// Records a `health_ping`'s `client_telemetry` at the caller's
    /// monotonic clock. A sample older than [`CLIENT_TELEMETRY_STALE_MS`]
    /// by its own `sample_age_ms` is discarded, so a slow or wedged client
    /// degrades to "unavailable" instead of stale-healthy.
    pub fn record_client_at(
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
            self.client_generation = self.client_generation.wrapping_add(1);
        } else {
            self.client = None;
            self.client_received_ms = None;
        }
    }

    /// The latest still-fresh client telemetry (discarded by
    /// [`Self::observe`] once stale), whose normalized network facts reach
    /// `HEALTH_SNAPSHOT` and the session summary.
    #[must_use]
    pub const fn client(&self) -> Option<&ClientTelemetrySnapshotMsg> {
        self.client.as_ref()
    }

    /// The monotonic time the current client telemetry was recorded at.
    #[must_use]
    pub const fn client_received_ms(&self) -> Option<u64> {
        self.client_received_ms
    }

    /// Assesses one window of host counters (and any still-fresh client
    /// telemetry), advancing the two-window hysteresis tracker.
    ///
    /// `timestamp_ms` must be a monotonic, never-decreasing clock (for
    /// example `Instant::elapsed` since a fixed session-start anchor), not
    /// wall-clock time: it drives the window maths, the hysteresis and the
    /// 60-second snapshot cadence. Wall-clock time belongs only to the
    /// lifecycle record's own `timestamp`, set by the emitter.
    ///
    /// `fps_target` is the rate each output is served at; the window is
    /// judged against it times [`HostCounters::outputs`], or against
    /// [`HostCounters::outputs_fps_target`] when the outputs run at their
    /// own rates.
    pub fn observe(
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
        let frames_offered = match counters.content {
            ContentEvidence::Rate(_) => None,
            ContentEvidence::Offered { total, .. } => Some(total),
        };
        let fps_target = counters.window_fps_target(fps_target);
        let window = self.window.observe(
            timestamp_ms,
            fps_target,
            HostCounterTotals {
                frames_sent: counters.frames_sent,
                frames_dropped: counters.frames_dropped,
                bytes_sent: counters.bytes_sent,
                input_events: counters.input_events,
                frames_offered,
            },
            u64::from(HOST_SAMPLE_WINDOW_SECS) * 1_000,
        );
        let first = *self.first_timestamp_ms.get_or_insert(timestamp_ms);
        let fps_warmup = self.fps_warmup_enabled && window.sample.fps_warmup;
        let mut sample = window.sample;
        sample.fps_warmup = fps_warmup;
        sample.fps_content = match counters.content {
            ContentEvidence::Rate(rate) => rate,
            ContentEvidence::Offered { proven, .. } => sample.fps_content.filter(|_| proven),
        };
        let mut client_sample = QosSample {
            timestamp_ms,
            fps_target: Some(fps_target),
            fps_warmup,
            ..QosSample::default()
        };
        apply_client_sample(&mut client_sample, self.client.as_ref());
        merge_client_facts(&mut sample, &client_sample);

        let host = assess_health(&sample, &self.targets).host_delivery;
        let client = assess_health(&client_sample, &self.targets).client_experience;
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
        let transition = match self.commit(timestamp_ms, assessment, sample, client_sample) {
            Ok(transition) => transition.map(|committed| {
                let kind = match committed.current {
                    HealthState::Ok => LifecycleEventKind::HealthOk,
                    HealthState::Degraded => LifecycleEventKind::HealthDegraded,
                    HealthState::Critical => LifecycleEventKind::HealthCritical,
                };
                let fields = transition_fields(
                    kind,
                    committed.previous,
                    committed.current,
                    timestamp_ms,
                    &committed.sample,
                    (committed.kind, committed.side),
                    &self.targets,
                    &mut self.unhealthy_since_ms,
                );
                (kind, fields)
            }),
            Err(error) => {
                // The tick still completes: a rejected hysteresis update is
                // an untrusted transition for this one tick, which the
                // caller logs.
                clock_error = Some(error);
                None
            }
        };

        let snapshot_due = timestamp_ms.saturating_sub(first) >= SNAPSHOT_INTERVAL_MS
            && self
                .last_snapshot_ms
                .is_none_or(|last| timestamp_ms.saturating_sub(last) >= SNAPSHOT_INTERVAL_MS);
        if snapshot_due {
            self.last_snapshot_ms = Some(timestamp_ms);
        }
        HealthObservation {
            sample,
            assessment,
            transition,
            snapshot_due,
            bandwidth_mbps: window.bandwidth_mbps,
            clock_error,
        }
    }

    /// Advances each side's hysteresis (the client's only on a client sample
    /// it has not judged yet) and returns a change of the session's
    /// committed state, the worse of the two, with the window of the side
    /// that holds it.
    fn commit(
        &mut self,
        timestamp_ms: u64,
        assessment: HealthAssessment,
        sample: QosSample,
        client_sample: QosSample,
    ) -> Result<Option<CommittedChange>, HealthTrackerError> {
        let host = self.host_tracker.update(
            timestamp_ms,
            &HealthAssessment {
                host_delivery: assessment.host_delivery,
                overall: assessment.host_delivery.state,
                ..HealthAssessment::default()
            },
        );
        if host.is_ok_and(|transition| transition.is_some()) {
            self.host_evidence = Some((assessment.host_delivery, sample));
        }
        let client = if self.client.is_none() {
            self.client_absent_ticks = self.client_absent_ticks.saturating_add(1);
            if self.client_absent_ticks >= 2 {
                self.client_tracker = HealthTracker::default();
                self.client_evidence = None;
            }
            Ok(None)
        } else {
            self.client_absent_ticks = 0;
            if self.client_generation == self.client_assessed_generation {
                Ok(None)
            } else {
                self.client_assessed_generation = self.client_generation;
                self.client_tracker.update(
                    timestamp_ms,
                    &HealthAssessment {
                        client_experience: assessment.client_experience,
                        overall: assessment.client_experience.state,
                        ..HealthAssessment::default()
                    },
                )
            }
        };
        if client.is_ok_and(|transition| transition.is_some()) {
            self.client_evidence = Some((assessment.client_experience, client_sample));
        }
        host?;
        client?;
        let client_state = self.client_tracker.current();
        let committed = match (self.host_tracker.current(), client_state) {
            (Some(host), Some(client)) => Some(host.max(client)),
            (state @ Some(_), None) | (None, state @ Some(_)) => state,
            (None, None) => None,
        };
        let Some(current) = committed.filter(|state| Some(*state) != self.committed) else {
            return Ok(None);
        };
        let previous = self.committed.replace(current);
        let (kind, (side, sample)) = if client_state == Some(current) {
            (
                HealthSideKind::ClientExperience,
                self.client_evidence
                    .unwrap_or((assessment.client_experience, client_sample)),
            )
        } else {
            (
                HealthSideKind::HostDelivery,
                self.host_evidence
                    .unwrap_or((assessment.host_delivery, sample)),
            )
        };
        Ok(Some(CommittedChange {
            previous,
            current,
            kind,
            side,
            sample,
        }))
    }

    /// Builds the outbound `health_stats` for one observation.
    ///
    /// Frame and input counts are the window's (`observation.sample`), as
    /// the declared `sample_window_secs` says, never the cumulative
    /// `counters`; only the last-value `last_input_sequence` and
    /// `last_input_type` come from `counters`. Carries the assessed
    /// `overall` state, never a stand-in for `Ok`.
    ///
    /// `fps_target` and `fps_actual` are per stream, as a Deck paces each
    /// monitor's stream by them: the rate each output is served at, and
    /// the window's sent rate shared over the [`HostCounters::outputs`].
    /// The frame counts stay the whole session's.
    #[must_use]
    pub fn health_stats(
        observation: &HealthObservation,
        counters: HostCounters,
        fps_target: u32,
        codec: &str,
        chroma: &str,
        resolution: String,
    ) -> HealthStatsMsg {
        HealthStatsMsg {
            fps_actual: f64::from(observation.sample.fps_actual.unwrap_or(0))
                / f64::from(counters.output_count()),
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

/// The client's facts the host-delivery sample carries alongside its own.
const fn merge_client_facts(sample: &mut QosSample, client: &QosSample) {
    sample.frames_decoded = client.frames_decoded;
    sample.frames_presented = client.frames_presented;
    sample.frames_superseded = client.frames_superseded;
    sample.frames_submitted_untimed = client.frames_submitted_untimed;
    sample.decode_time_ms = client.decode_time_ms;
    sample.display_time_ms = client.display_time_ms;
    sample.input_latency_ms = client.input_latency_ms;
}

fn apply_client_sample(sample: &mut QosSample, telemetry: Option<&ClientTelemetrySnapshotMsg>) {
    let Some(qos) = telemetry.and_then(|telemetry| telemetry.qos.as_ref()) else {
        return;
    };
    sample.frames_decoded = qos.frames_decoded;
    sample.frames_presented = qos.frames_presented;
    sample.frames_superseded = qos.frames_superseded;
    sample.frames_submitted_untimed = qos.frames_submitted_untimed;
    sample.decode_time_ms = qos.decode_time_ms;
    sample.display_time_ms = qos.display_time_ms;
    sample.input_latency_ms = qos.input_send_time_ms;
    if let (Some(presented), Some(window)) = (qos.frames_presented, qos.sample_window_secs) {
        sample.fps_actual = u32::try_from(presented / u64::from(window.get())).ok();
    }
    // The client presents what arrived; a still desktop arrives as one
    // keepalive a second, and that is not the client falling short.
    if let (Some(received), Some(window)) = (qos.frames_received, qos.sample_window_secs) {
        sample.fps_content = u32::try_from(received / u64::from(window.get())).ok();
    }
}

/// An agent's `health_stats` with `path` filled from `measure`, for the
/// process that owns the Deck's connection and relays the messages of a
/// session agent that does not (the macOS network service, the Windows
/// broker). `None` for any other message and for stats that already carry
/// a path (an agent that owns its connection measured it), so the relay
/// forwards those unchanged.
pub fn health_stats_with_path(
    text: &str,
    measure: impl FnOnce() -> arcen_protocol::messages::PathQualityMsg,
) -> Option<String> {
    use arcen_protocol::messages::HEALTH_STATS;
    if !text.contains(HEALTH_STATS) {
        return None;
    }
    let mut stats = serde_json::from_str::<HealthStatsMsg>(text).ok()?;
    if stats.msg_type != HEALTH_STATS || stats.path.is_some() {
        return None;
    }
    stats.path = Some(measure());
    serde_json::to_string(&stats).ok()
}

/// The wire form of a health state (`health_stats.health_state`).
#[must_use]
pub const fn health_state_message(state: HealthState) -> HealthStateMsg {
    match state {
        HealthState::Ok => HealthStateMsg::Ok,
        HealthState::Degraded => HealthStateMsg::Degraded,
        HealthState::Critical => HealthStateMsg::Critical,
    }
}

/// Builds `HEALTH_SNAPSHOT` fields (event 1806) for one observation.
///
/// `client_network` is the latest retained client network snapshot. Only
/// normalized, non-identity facts are carried (`interface_kind`, `scope`,
/// `link_mbps`, `mtu`), never the client's SSID or signal strength.
#[must_use]
pub fn snapshot_fields(
    sample: &QosSample,
    assessment: &HealthAssessment,
    client_network: Option<&ClientNetworkSnapshotMsg>,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert(
        "overall_state",
        FieldValue::String(state_name(assessment.overall).to_owned()),
    );
    insert_state(&mut fields, "host_state", assessment.host_delivery.state);
    insert_state(
        &mut fields,
        "client_state",
        assessment.client_experience.state,
    );
    insert_u32(&mut fields, "fps_actual", sample.fps_actual);
    insert_u32(&mut fields, "fps_target", sample.fps_target);
    insert_u32(&mut fields, "fps_content", sample.fps_content);
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
    // No `unpresented_basis_points` here: `HEALTH_SNAPSHOT` has a closed field
    // schema and an undeclared key would drop the whole snapshot. Client
    // supersession stays visible through the transition event's
    // `dominant_cause=unpresented` and the Deck's own telemetry.
    insert_client_network_fields(&mut fields, client_network);
    fields
}

/// Builds the fields for the Info-level 10-second window summary: five
/// 2-second ticks' window (not cumulative) frame and input counts plus the
/// worst overall state seen in that window. Distinct from, and never a
/// substitute for, the 60-second [`snapshot_fields`].
#[must_use]
pub fn window_summary_fields(
    window_ticks: u32,
    frames_sent: u64,
    frames_dropped: u64,
    input_events: u64,
    worst_overall: Option<HealthState>,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("window_ticks", FieldValue::Integer(i64::from(window_ticks)));
    let _ = fields.insert("frames_sent", FieldValue::Integer(saturating(frames_sent)));
    let _ = fields.insert(
        "frames_dropped",
        FieldValue::Integer(saturating(frames_dropped)),
    );
    let _ = fields.insert(
        "input_events",
        FieldValue::Integer(saturating(input_events)),
    );
    let _ = fields.insert(
        "worst_overall_state",
        FieldValue::String(state_name(worst_overall).to_owned()),
    );
    fields
}

/// Inserts normalized, bounded client network-path facts (never SSID or
/// RSSI) into any lifecycle event's fields: `HEALTH_SNAPSHOT` and the
/// session summary.
pub fn insert_client_network_fields(
    fields: &mut StructuredFields,
    client_network: Option<&ClientNetworkSnapshotMsg>,
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

const fn client_interface_kind_name(kind: NetworkInterfaceKind) -> &'static str {
    match kind {
        NetworkInterfaceKind::Ethernet => "ethernet",
        NetworkInterfaceKind::Wifi => "wifi",
        NetworkInterfaceKind::Cellular => "cellular",
        NetworkInterfaceKind::Vpn => "vpn",
        NetworkInterfaceKind::Loopback => "loopback",
        NetworkInterfaceKind::Other => "other",
    }
}

const fn client_scope_name(scope: NetworkScopeMsg) -> &'static str {
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
    (selected, side): (HealthSideKind, HealthSide),
    targets: &QosTargets,
    unhealthy_since_ms: &mut Option<u64>,
) -> StructuredFields {
    let mut fields = StructuredFields::default();
    if kind == LifecycleEventKind::HealthOk {
        let _ = fields.insert(
            "previous_state",
            FieldValue::String(state_name(previous).to_owned()),
        );
        let duration = unhealthy_since_ms
            .take()
            .map_or(0, |started| timestamp_ms.saturating_sub(started));
        let _ = fields.insert(
            "degraded_duration_ms",
            FieldValue::Integer(saturating(duration)),
        );
        return fields;
    }
    unhealthy_since_ms.get_or_insert(timestamp_ms);
    let cause = side.dominant_cause.unwrap_or(HealthCause::Heartbeat);
    let (value, threshold) = cause_value_threshold(cause, selected, current, sample, targets);
    // Deliberately no `dominant_side` field: `HEALTH_DEGRADED`/`HEALTH_CRITICAL`
    // carry a closed field schema (`arcen_telemetry::lifecycle`, mirrored in
    // `scripts/observability_event_definitions.py`), and an undeclared field
    // makes `ValidatedLifecycleEvent::new` reject the whole event. The cause
    // name already carries the side: only the client side produces
    // `unpresented`, and only host delivery produces `loss`.
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
/// [`HealthCause`] is shared by both sides, so the cause alone does not say
/// which counters produced it. Losing that distinction is what once let a
/// host print a client presentation problem using host delivery numbers.
/// A change of a session's committed state, with the window of the side
/// that holds the new state.
struct CommittedChange {
    previous: Option<HealthState>,
    current: HealthState,
    kind: HealthSideKind,
    side: HealthSide,
    sample: QosSample,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthSideKind {
    HostDelivery,
    ClientExperience,
}

/// Fraction of decoded frames that never reached the screen, in basis points.
///
/// **This is not transport loss**, despite arriving as [`HealthCause::Loss`]:
/// the client-side assessment reuses that cause for supersession. Every frame
/// counted here arrived intact and decoded before a newer frame replaced it.
/// The Deck reports exactly this as `unpresented`; computing the value from
/// host `frames_sent`/`frames_dropped` instead once logged
/// `dominant_cause=loss value=0` for a grading session with 13.98 %
/// supersession and no packet drops.
fn unpresented_basis_points(sample: &QosSample) -> u32 {
    arcen_telemetry::unpresented_basis_points(sample).unwrap_or(0)
}

fn cause_value_threshold(
    cause: HealthCause,
    side: HealthSideKind,
    state: HealthState,
    sample: &QosSample,
    targets: &QosTargets,
) -> (u32, Option<u32>) {
    let critical = state == HealthState::Critical;
    match cause {
        HealthCause::Fps => (
            sample.fps_actual.unwrap_or(0),
            arcen_telemetry::fps_threshold(sample, targets, state),
        ),
        HealthCause::Rtt => (
            sample.rtt_ms.unwrap_or(0),
            Some(if critical {
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
                    dropped
                        .saturating_mul(10_000)
                        .checked_div(total)
                        .map_or(0, |points| u32::try_from(points).unwrap_or(u32::MAX))
                }
            };
            (
                value,
                Some(u32::from(if critical {
                    targets.drop_critical_basis_points()
                } else {
                    targets.drop_degraded_basis_points()
                })),
            )
        }
        HealthCause::InputLatency => (
            sample.input_latency_ms.unwrap_or(0),
            Some(if critical {
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

const fn health_cause_name(cause: HealthCause, side: HealthSideKind) -> &'static str {
    match cause {
        HealthCause::Fps => "fps",
        HealthCause::Rtt => "rtt",
        // As the Deck names it: on the client side this cause carries
        // presentation supersession, not transport loss. Real packet loss
        // is reported as loss epochs and keeps the name `loss`.
        HealthCause::Loss => match side {
            HealthSideKind::HostDelivery => "loss",
            HealthSideKind::ClientExperience => "unpresented",
        },
        HealthCause::InputLatency => "input_latency",
        HealthCause::Heartbeat => "heartbeat",
    }
}

/// A state's canonical name, or `unavailable` when there is none.
#[must_use]
pub const fn state_name(state: Option<HealthState>) -> &'static str {
    match state {
        Some(state) => state.as_str(),
        None => "unavailable",
    }
}

fn insert_state(fields: &mut StructuredFields, key: &str, state: Option<HealthState>) {
    if let Some(state) = state {
        let _ = fields.insert(key, FieldValue::String(state.as_str().to_owned()));
    }
}

fn insert_u32(fields: &mut StructuredFields, key: &str, value: Option<u32>) {
    if let Some(value) = value {
        let _ = fields.insert(key, FieldValue::Integer(i64::from(value)));
    }
}

fn saturating(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub mod monitor_rates;

#[cfg(test)]
mod tests;
