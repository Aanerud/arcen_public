use std::time::Duration;

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
