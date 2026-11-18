// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Each monitor's own frame rate in a multi-monitor session, and whether it
//! keeps up (ADR 0026: a Pier that cannot sustain a monitor degrades
//! visibly). The session's health judges the monitors together; this says
//! which one fell behind and why, for the Deck (`HealthStatsMsg.monitors`)
//! and the Pier's log.
//!
//! A monitor keeps up when it sent what its content owed: its served rate,
//! or less when its capture proved the content changed less often (a still
//! monitor owes only its keepalive). Below the degraded share of that for
//! two windows running, it is limited by the host keeping up
//! ([`MONITOR_LIMITED_HOST_SHORTFALL`]); one window that keeps up clears it.

use std::collections::{BTreeMap, BTreeSet};

use arcen_protocol::messages::MonitorRateMsg;
use arcen_telemetry::{HOST_FPS_WARMUP_MS, QosTargets};

use super::HOST_SAMPLE_WINDOW_SECS;

/// The `limited_by` token of a monitor whose host encoded or sent less than
/// its content owed (the frame-rate basis `host_shortfall`).
pub const MONITOR_LIMITED_HOST_SHORTFALL: &str = "host_shortfall";

/// Short windows in a row before a monitor reads as limited.
const LIMITED_AFTER_WINDOWS: u8 = 2;

/// Whole frames per second a monitor may fall short by before it counts
/// (frames are counted per window, content and sends not at the same
/// instant).
const FPS_SLACK: f64 = 1.0;

/// One monitor's counters for [`MonitorRates::observe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorCounters {
    pub session_monitor_id: u16,
    /// The rate this monitor is served at.
    pub fps_target: u32,
    /// Frames it sent since the session started.
    pub frames_sent: u64,
    /// The content rate its capture proved over the window, if any.
    pub content_fps: Option<u32>,
}

/// A monitor that became limited or recovered, for the Pier's log.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorRateChange {
    pub session_monitor_id: u16,
    pub fps_target: u32,
    /// What its content owed over the window.
    pub owed_fps: u32,
    pub fps_actual: f64,
    /// The reason it is now limited; `None` once it keeps up again.
    pub limited_by: Option<&'static str>,
}

/// Per-monitor windows over cumulative counters.
#[derive(Debug, Clone, Default)]
pub struct MonitorRates {
    previous: BTreeMap<u16, u64>,
    previous_ms: Option<u64>,
    first_ms: Option<u64>,
    short_windows: BTreeMap<u16, u8>,
    limited: BTreeSet<u16>,
}

impl MonitorRates {
    /// One window of `monitors` at the caller's monotonic `timestamp_ms`:
    /// each monitor's line for `HealthStatsMsg.monitors`, and the monitors
    /// whose limit changed. Rates are not judged in the first
    /// [`HOST_FPS_WARMUP_MS`] (a stream ramps up).
    pub fn observe(
        &mut self,
        timestamp_ms: u64,
        monitors: &[MonitorCounters],
        targets: &QosTargets,
    ) -> (Vec<MonitorRateMsg>, Vec<MonitorRateChange>) {
        let elapsed_ms = self
            .previous_ms
            .map_or(u64::from(HOST_SAMPLE_WINDOW_SECS) * 1_000, |previous| {
                timestamp_ms.saturating_sub(previous)
            })
            .max(1);
        let first = *self.first_ms.get_or_insert(timestamp_ms);
        let warming_up = timestamp_ms.saturating_sub(first) < HOST_FPS_WARMUP_MS;
        self.previous_ms = Some(timestamp_ms);
        let mut lines = Vec::with_capacity(monitors.len());
        let mut changes = Vec::new();
        let mut seen = BTreeSet::new();
        for monitor in monitors {
            let id = monitor.session_monitor_id;
            seen.insert(id);
            let previous = self.previous.insert(id, monitor.frames_sent).unwrap_or(0);
            let sent = monitor.frames_sent.saturating_sub(previous);
            #[allow(
                clippy::cast_precision_loss,
                reason = "frames and milliseconds in one health window"
            )]
            let fps_actual = sent as f64 * 1_000.0 / elapsed_ms as f64;
            let target = monitor.fps_target.max(1);
            // Silence is a stall, never idleness: every Pier sends a
            // keepalive on a still monitor.
            let owed = if sent == 0 {
                target
            } else {
                monitor
                    .content_fps
                    .map_or(target, |content| content.min(target))
            };
            let short = !warming_up
                && fps_actual + FPS_SLACK
                    < f64::from(owed) * f64::from(targets.fps_degraded_percent()) / 100.0;
            let streak = self.short_windows.entry(id).or_default();
            *streak = if short { streak.saturating_add(1) } else { 0 };
            let now_limited =
                *streak >= LIMITED_AFTER_WINDOWS || (short && self.limited.contains(&id));
            let was_limited = if now_limited {
                !self.limited.insert(id)
            } else {
                self.limited.remove(&id)
            };
            let limited_by = now_limited.then_some(MONITOR_LIMITED_HOST_SHORTFALL);
            if now_limited != was_limited {
                changes.push(MonitorRateChange {
                    session_monitor_id: id,
                    fps_target: monitor.fps_target,
                    owed_fps: owed,
                    fps_actual,
                    limited_by,
                });
            }
            lines.push(MonitorRateMsg {
                session_monitor_id: id,
                fps_target: monitor.fps_target,
                fps_actual,
                limited_by: limited_by.map(str::to_owned),
            });
        }
        // A monitor that left the topology starts afresh if it returns.
        self.previous.retain(|id, _| seen.contains(id));
        self.short_windows.retain(|id, _| seen.contains(id));
        self.limited.retain(|id| seen.contains(id));
        (lines, changes)
    }
}

/// Logs a monitor that became limited (WARN, with what it sent against
/// what it owed) or kept up again (INFO). A display slower than its rate is
/// never silent.
pub fn log_monitor_rate_change(change: &MonitorRateChange) {
    if let Some(reason) = change.limited_by {
        tracing::warn!(
            target: arcen_telemetry::names::target::MEDIA,
            session_monitor_id = change.session_monitor_id,
            fps_target = change.fps_target,
            owed_fps = change.owed_fps,
            fps_actual = change.fps_actual,
            fps_basis = reason,
            "a monitor delivers below what its content owed; the Deck is told why"
        );
    } else {
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            session_monitor_id = change.session_monitor_id,
            fps_target = change.fps_target,
            fps_actual = change.fps_actual,
            "a monitor keeps up with its content again"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(
        id: u16,
        fps_target: u32,
        frames_sent: u64,
        content: Option<u32>,
    ) -> MonitorCounters {
        MonitorCounters {
            session_monitor_id: id,
            fps_target,
            frames_sent,
            content_fps: content,
        }
    }

    #[test]
    fn a_monitor_behind_its_moving_content_is_named_after_two_windows() {
        let mut rates = MonitorRates::default();
        let targets = QosTargets::default();
        let mut sent = (0_u64, 0_u64);
        let mut at = 0;
        // Warm-up, then two monitors: one keeps up at 60, one sends 41 of
        // the 60 its content changed at.
        let mut last = None;
        for _ in 0..8 {
            at += 2_000;
            sent.0 += 120;
            sent.1 += 82;
            last = Some(rates.observe(
                at,
                &[
                    monitor(1, 60, sent.0, Some(60)),
                    monitor(2, 60, sent.1, Some(60)),
                ],
                &targets,
            ));
        }
        let (lines, changes) = last.expect("observed");
        assert!(changes.is_empty(), "named once, when it changed");
        assert_eq!(lines[0].limited_by, None);
        assert_eq!(
            lines[1].limited_by.as_deref(),
            Some(MONITOR_LIMITED_HOST_SHORTFALL)
        );
        assert!((lines[1].fps_actual - 41.0).abs() < 1e-9);
        // It catches up: cleared in one window, and said so.
        at += 2_000;
        sent.0 += 120;
        sent.1 += 120;
        let (lines, changes) = rates.observe(
            at,
            &[
                monitor(1, 60, sent.0, Some(60)),
                monitor(2, 60, sent.1, Some(60)),
            ],
            &targets,
        );
        assert_eq!(lines[1].limited_by, None);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].limited_by, None);
    }

    #[test]
    fn a_still_monitor_owes_its_keepalive_and_a_silent_one_its_rate() {
        let mut rates = MonitorRates::default();
        let targets = QosTargets::default();
        rates.observe(0, &[monitor(1, 120, 0, None)], &targets);
        let mut at = HOST_FPS_WARMUP_MS;
        let mut sent = 0;
        for _ in 0..3 {
            at += 2_000;
            sent += 2;
            let (lines, _) = rates.observe(at, &[monitor(1, 120, sent, Some(1))], &targets);
            assert_eq!(lines[0].limited_by, None, "a still monitor keeps up");
        }
        let mut limited = false;
        for _ in 0..2 {
            at += 2_000;
            let (lines, _) = rates.observe(at, &[monitor(1, 120, sent, Some(0))], &targets);
            limited = lines[0].limited_by.is_some();
        }
        assert!(limited, "sending nothing at all is a stall");
    }

    #[test]
    fn nothing_is_judged_while_the_streams_warm_up() {
        let mut rates = MonitorRates::default();
        let targets = QosTargets::default();
        for at in [2_000, 4_000, 6_000] {
            let (lines, changes) = rates.observe(at, &[monitor(1, 60, 0, None)], &targets);
            assert_eq!(lines[0].limited_by, None);
            assert!(changes.is_empty());
        }
    }
}
