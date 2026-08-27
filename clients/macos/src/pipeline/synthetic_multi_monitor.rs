//! Synthetic multi-monitor producer/test harness.
//!
//! Drives [`crate::pipeline::monitor_router::MonitorFrameRouter`] with 1, 2,
//! or 4 distinct deterministic per-monitor patterns and no real host,
//! decoder, or network connection, proving no cross-routing between monitor
//! slots. This is exercised both by unit tests in this module and by the
//! `multi-monitor-harness` CLI subcommand (`crate::main`) for manual/CI
//! diagnostic use.
//!
//! The harness's negotiated roster always uses session monitor ids
//! `1..=monitor_count` (nonzero by construction); it also separately probes
//! [`crate::pipeline::monitor_router::MonitorRoute::LegacyPrimary`] (wire
//! `monitor_id == 0`) to prove a real negotiated-only router correctly
//! rejects it as unrouted, without ever constructing a zero
//! `SessionMonitorId` (which is statically impossible).

use arcen_media::{SessionMonitorId, TopologyGeneration};

use crate::pipeline::monitor_router::{MonitorFrameRouter, RouterAdmissionError};
use crate::pipeline::video_decoder::DecodedVideoFrame;

/// One monitor's deterministic synthetic frame pattern: `width`×`height`
/// solid-fill RGBA, colored by a stable per-monitor tag so cross-routing is
/// trivially detectable by comparing pixel content to the expected tag.
#[must_use]
pub fn synthetic_frame(monitor_id: SessionMonitorId, sequence: u32) -> DecodedVideoFrame {
    solid_fill_frame(monitor_pixel_tag(monitor_id), sequence)
}

/// The legacy single-monitor wire frame's deterministic synthetic pattern
/// (wire `monitor_id == 0`), tagged distinctly from every negotiated monitor
/// id's pattern so cross-routing between the legacy route and any negotiated
/// route is trivially detectable.
#[must_use]
pub fn legacy_primary_synthetic_frame(sequence: u32) -> DecodedVideoFrame {
    solid_fill_frame(LEGACY_PRIMARY_PIXEL_TAG, sequence)
}

/// Fixed fill byte for the legacy primary route's synthetic pattern,
/// deliberately outside the range [`monitor_pixel_tag`] produces for any
/// negotiated id in `1..=MAX_MULTI_MONITOR_COUNT`.
const LEGACY_PRIMARY_PIXEL_TAG: u8 = 0xFE;

fn solid_fill_frame(tag: u8, sequence: u32) -> DecodedVideoFrame {
    const WIDTH: usize = 4;
    const HEIGHT: usize = 4;
    let mut rgba = Vec::with_capacity(WIDTH * HEIGHT * 4);
    for _ in 0..(WIDTH * HEIGHT) {
        rgba.extend_from_slice(&[tag, tag, tag, 0xFF]);
    }
    DecodedVideoFrame {
        width: WIDTH,
        height: HEIGHT,
        rgba,
        timestamp_ms: sequence,
        pixel_format: "rgba8".to_string(),
        backend: "synthetic-harness",
        native: None,
    }
}

/// Maps a session monitor id to a stable, distinguishable fill byte so
/// misrouted frames are visibly wrong rather than accidentally matching.
///
/// `pub(crate)` (rather than private) so `crate::ui::multi_window_diagnostic`
/// can independently recompute the *expected* tag for a viewport's own
/// self-claimed monitor id and compare it against the tag byte that
/// viewport's paint callback actually reported consuming -- the isolation
/// proof's cross-check that a callback's claim and its real painted data
/// agree (see `crate::ui::multi_window_diagnostic::verify_paint_isolation`).
pub(crate) const fn monitor_pixel_tag(monitor_id: SessionMonitorId) -> u8 {
    // Spread ids across the byte range instead of using the raw id directly,
    // so adjacent ids (1, 2, 3...) still produce very different pixel tags.
    0x10_u8.wrapping_add(monitor_id.get().wrapping_mul(0x40) as u8)
}

/// Report proving no cross-routing occurred while driving `monitor_count`
/// distinct synthetic monitors through a fresh [`MonitorFrameRouter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessReport {
    pub monitor_count: usize,
    pub frames_per_monitor: u32,
    pub isolation_verified: bool,
}

/// Failure running the synthetic harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    /// `monitor_count` was zero or exceeded the router's supported roster.
    InvalidMonitorCount(usize),
    /// Routing a synthetic frame was unexpectedly rejected.
    Routing(RouterAdmissionError),
}

impl std::fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMonitorCount(count) => {
                write!(formatter, "invalid synthetic monitor count: {count}")
            }
            Self::Routing(error) => write!(formatter, "unexpected routing rejection: {error}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<RouterAdmissionError> for HarnessError {
    fn from(error: RouterAdmissionError) -> Self {
        Self::Routing(error)
    }
}

/// Builds the harness's negotiated synthetic monitor roster: session monitor
/// ids `1..=monitor_count`.
///
/// This is the harness's single, documented point of trusting that the
/// range starting at `1` is always nonzero, rather than an unwrap scattered
/// through the routing loop below.
fn synthetic_monitor_ids(monitor_count: usize) -> Vec<SessionMonitorId> {
    (1..=monitor_count as u16)
        .map(|wire_id| {
            SessionMonitorId::new(wire_id)
                .unwrap_or_else(|_| unreachable!("{wire_id} is in 1..=monitor_count, never 0"))
        })
        .collect()
}

/// Drives `monitor_count` distinct synthetic monitors (session monitor ids
/// `1..=monitor_count`) through `frames_per_monitor` rounds each, then proves
/// every monitor's final latest frame matches only its own pattern.
///
/// # Errors
///
/// Returns [`HarnessError::InvalidMonitorCount`] for `monitor_count == 0` or
/// `monitor_count > arcen_media::MAX_MULTI_MONITOR_COUNT`, and
/// [`HarnessError::Routing`] if a synthetic frame is unexpectedly rejected
/// (a router/harness roster mismatch bug).
pub fn run_isolation_harness(
    monitor_count: usize,
    frames_per_monitor: u32,
) -> Result<HarnessReport, HarnessError> {
    if monitor_count == 0 || monitor_count > arcen_media::MAX_MULTI_MONITOR_COUNT {
        return Err(HarnessError::InvalidMonitorCount(monitor_count));
    }
    if frames_per_monitor == 0 {
        return Ok(HarnessReport {
            monitor_count,
            frames_per_monitor,
            isolation_verified: false,
        });
    }
    let generation = TopologyGeneration::new(1).unwrap_or_else(|_| unreachable!("1 != 0"));
    let monitor_ids = synthetic_monitor_ids(monitor_count);
    let mut router = MonitorFrameRouter::new(generation, &monitor_ids)
        .unwrap_or_else(|_| unreachable!("harness roster is always 1..=MAX_MULTI_MONITOR_COUNT"));

    for sequence in 0..frames_per_monitor {
        for monitor_id in &monitor_ids {
            let frame = synthetic_frame(*monitor_id, sequence);
            router.route_decoded_frame(generation, *monitor_id, frame)?;
        }
    }

    // Deliberately try to misroute one extra frame at an out-of-roster
    // negotiated id, at the legacy primary route (wire id 0 -- never part of
    // a real negotiated roster), and at a stale generation; all three must
    // be rejected, and the real monitors' slots must be provably unaffected
    // by the attempts.
    let bogus_id = SessionMonitorId::new(monitor_count as u16 + 1)
        .unwrap_or_else(|_| unreachable!("monitor_count + 1 is always nonzero"));
    let misroute_unrouted =
        router.route_decoded_frame(generation, bogus_id, synthetic_frame(bogus_id, 0));
    let misroute_legacy_primary =
        router.route_decoded_frame_legacy_primary(generation, legacy_primary_synthetic_frame(0));
    let stale_generation =
        TopologyGeneration::new(generation.get() + 1).unwrap_or_else(|_| unreachable!());
    let misroute_stale = router.route_decoded_frame(
        stale_generation,
        monitor_ids[0],
        synthetic_frame(monitor_ids[0], 0),
    );

    // MEDIUM review finding: these rejections are the actual isolation proof
    // for the deliberate misroute attempts, so they must affect
    // `isolation_verified` (and therefore the CLI's exit code) in every
    // build, not only when `debug_assertions` are compiled in. A
    // `debug_assert!` here would silently compile away in a release build,
    // masking a real router regression that started accepting an
    // out-of-roster monitor id, the legacy primary route, or a stale
    // topology generation.
    let mut isolation_verified = misroutes_were_rejected(
        &misroute_unrouted,
        &misroute_stale,
        &misroute_legacy_primary,
    );
    for monitor_id in &monitor_ids {
        let Some(latest) = router.latest_frame(*monitor_id) else {
            isolation_verified = false;
            continue;
        };
        // The exact expected frame (this monitor's own tag, final sequence)
        // is the strongest available proof of isolation: any cross-routed or
        // stale byte would make this comparison fail.
        let expected = synthetic_frame(*monitor_id, frames_per_monitor - 1);
        if latest.rgba != expected.rgba || latest.timestamp_ms != expected.timestamp_ms {
            isolation_verified = false;
        }
    }
    if router.latest_frame_legacy_primary().is_some() {
        // The legacy primary route must never have accepted a frame on this
        // negotiated-only router; its presence alone is proof of a
        // cross-routing regression, independent of the rejection check
        // above.
        isolation_verified = false;
    }

    Ok(HarnessReport {
        monitor_count,
        frames_per_monitor,
        isolation_verified,
    })
}

/// Whether all three deliberate misroute probes (an out-of-roster monitor
/// id, a stale topology generation, and the legacy primary route on a
/// negotiated-only router) were correctly rejected.
///
/// Pure and independent of any router/harness state so it is directly
/// unit-testable with fabricated `Result`s standing in for a hypothetical
/// router bug that wrongly *accepted* a misroute -- the scenario this check
/// exists to catch, in both debug and release builds.
const fn misroutes_were_rejected(
    unrouted: &Result<(), RouterAdmissionError>,
    stale_generation: &Result<(), RouterAdmissionError>,
    legacy_primary: &Result<(), RouterAdmissionError>,
) -> bool {
    unrouted.is_err() && stale_generation.is_err() && legacy_primary.is_err()
}

/// Human-readable one-line summary for the `multi-monitor-harness` CLI
/// subcommand.
#[must_use]
pub fn format_report(report: &HarnessReport) -> String {
    format!(
        "monitors={} frames_per_monitor={} isolation_verified={}",
        report.monitor_count, report.frames_per_monitor, report.isolation_verified
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Validated-constructor test helper mirroring
    /// `monitor_router::tests::sid`, centralizing the one place tests trust a
    /// literal is a nonzero session monitor id.
    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("test session monitor id must be nonzero")
    }

    #[test]
    fn misroutes_were_rejected_is_true_only_when_all_three_probes_were_rejected() {
        let rejected: Result<(), RouterAdmissionError> =
            Err(RouterAdmissionError::UnroutedMonitor(99));
        let rejected_stale: Result<(), RouterAdmissionError> =
            Err(RouterAdmissionError::StaleGeneration {
                expected: 1,
                actual: 2,
            });
        let rejected_legacy: Result<(), RouterAdmissionError> =
            Err(RouterAdmissionError::UnroutedMonitor(0));
        let accepted: Result<(), RouterAdmissionError> = Ok(());

        // All three probes correctly rejected: isolation holds.
        assert!(misroutes_were_rejected(
            &rejected,
            &rejected_stale,
            &rejected_legacy
        ));

        // Simulating a hypothetical router bug that wrongly *accepted* one
        // (or more) misroutes: this is exactly what `debug_assert!` would
        // have silently missed in a release build. The plain boolean fold
        // must catch it unconditionally.
        assert!(!misroutes_were_rejected(
            &accepted,
            &rejected_stale,
            &rejected_legacy
        ));
        assert!(!misroutes_were_rejected(
            &rejected,
            &accepted,
            &rejected_legacy
        ));
        assert!(!misroutes_were_rejected(
            &rejected,
            &rejected_stale,
            &accepted
        ));
        assert!(!misroutes_were_rejected(&accepted, &accepted, &accepted));
    }

    #[test]
    fn single_monitor_harness_reports_isolated() {
        let report = run_isolation_harness(1, 3).expect("harness runs");
        assert_eq!(
            report,
            HarnessReport {
                monitor_count: 1,
                frames_per_monitor: 3,
                isolation_verified: true,
            }
        );
    }

    #[test]
    fn two_monitor_harness_reports_isolated() {
        let report = run_isolation_harness(2, 5).expect("harness runs");
        assert!(report.isolation_verified);
    }

    #[test]
    fn four_monitor_harness_reports_isolated() {
        let report = run_isolation_harness(4, 5).expect("harness runs");
        assert!(report.isolation_verified);
    }

    #[test]
    fn zero_monitors_is_rejected() {
        assert_eq!(
            run_isolation_harness(0, 1).unwrap_err(),
            HarnessError::InvalidMonitorCount(0)
        );
    }

    #[test]
    fn more_than_four_monitors_is_rejected() {
        assert_eq!(
            run_isolation_harness(5, 1).unwrap_err(),
            HarnessError::InvalidMonitorCount(5)
        );
    }

    #[test]
    fn distinct_monitor_patterns_never_collide() {
        let ids: Vec<_> = (1..=4).map(sid).collect();
        let tags: Vec<_> = ids.iter().map(|id| monitor_pixel_tag(*id)).collect();
        for i in 0..tags.len() {
            for j in (i + 1)..tags.len() {
                assert_ne!(
                    tags[i], tags[j],
                    "monitor pixel tags must be distinguishable"
                );
            }
        }
    }

    #[test]
    fn legacy_primary_pattern_is_distinct_from_every_negotiated_monitor_pattern() {
        let legacy = legacy_primary_synthetic_frame(0);
        for id in (1..=4).map(sid) {
            let negotiated = synthetic_frame(id, 0);
            assert_ne!(
                legacy.rgba, negotiated.rgba,
                "legacy primary pattern must never match a negotiated monitor's pattern"
            );
        }
    }

    #[test]
    fn negotiated_only_harness_router_rejects_the_legacy_primary_route() {
        // Separate, focused proof (beyond `run_isolation_harness`'s internal
        // probe) that a real negotiated multi-monitor router -- exactly the
        // kind `run_isolation_harness` builds -- never admits wire id 0.
        let generation = TopologyGeneration::new(1).expect("nonzero generation");
        let monitor_ids = synthetic_monitor_ids(2);
        let mut router = MonitorFrameRouter::new(generation, &monitor_ids).expect("valid roster");
        let error = router
            .route_decoded_frame_legacy_primary(generation, legacy_primary_synthetic_frame(0))
            .unwrap_err();
        assert_eq!(error, RouterAdmissionError::UnroutedMonitor(0));
        assert!(router.latest_frame_legacy_primary().is_none());
    }
}
