// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Local display geometry and the stream sizes a Deck asks for, shared by
//! both Decks (moved from the macOS Deck's `display` module).
//!
//! A platform shell reads its displays (`CGDisplay`/`NSScreen` on the Mac,
//! `EnumDisplayMonitors` and the effective DPI on Windows) into one
//! validated [`DisplayMetrics`] per display; everything after that -- the
//! fullscreen presentation size, the `HiDPI` choice and its EDID-1.4
//! fallback, and the multi-monitor-v1 requested topology
//! ([`topology::build_requested_topology_from`]) -- is decided here.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    reason = "pixel clocks and display sizes, the macOS Deck's arithmetic kept exactly"
)]

pub mod metrics;
pub mod topology;

pub use metrics::{
    BackingScale, DisplayMetrics, DisplayMetricsError, LogicalRect, SafeAreaInsets,
    ScreenSpaceFlip, StreamExtent,
};

/// Highest refresh a pinned session asks the host to synthesise.
///
/// The stream is capped at 60 fps by every performance mode, so requesting a
/// 120 Hz virtual display buys nothing and doubles the pixel-clock budget — on
/// a `HiDPI` Retina request that is the difference between a working session and
/// `calculated 1116.72MHz pixel clock exceeds EDID 1.4 limits`.
pub const PINNED_MAX_REFRESH_HZ: u32 = 60;

/// The refresh a pinned session should request for a display running at
/// `monitor_hz`.
#[must_use]
pub fn pinned_refresh_hz(monitor_hz: u32) -> u32 {
    monitor_hz.clamp(1, PINNED_MAX_REFRESH_HZ)
}

/// EDID 1.4 detailed timings store dimensions in 12 bits and the pixel clock in
/// 10 kHz units in 16 bits.
const MAX_DTD_VALUE: u32 = 4095;
const MAX_DTD_PIXEL_CLOCK_10_KHZ: u32 = u16::MAX as u32;
const CVT_RB_HORIZONTAL_BLANK: u32 = 160;
const CVT_RB_MIN_VERTICAL_BLANK_US: f64 = 460.0;
/// Upper bound on the host's `CVT_RB_VERTICAL_FRONT_PORCH + vsync + 6` floor.
/// The computed minimum blanking dominates at every size Arcen streams, so a
/// bound is enough and avoids duplicating the aspect-ratio sync table.
const CVT_RB_MIN_VERTICAL_BLANK_LINES: u32 = 17;

/// Whether the host can express `width`x`height`@`refresh_hz` as an EDID 1.4
/// detailed timing.
///
/// Mirrors `cvt_reduced_blanking` in `hosts/windows/src/edid.rs`. The client
/// checks this so an impossible request is downgraded here instead of failing
/// authentication with a mode the host was never able to present.
#[must_use]
pub fn edid_feasible(width: u32, height: u32, refresh_hz: u32) -> bool {
    cvt_rb_pixel_clock_10khz(width, height, refresh_hz)
        .is_some_and(|clock| clock <= MAX_DTD_PIXEL_CLOCK_10_KHZ)
}

fn cvt_rb_pixel_clock_10khz(width: u32, height: u32, refresh_hz: u32) -> Option<u32> {
    if width == 0 || height == 0 || refresh_hz == 0 {
        return None;
    }
    if width > MAX_DTD_VALUE || height > MAX_DTD_VALUE {
        return None;
    }
    let frame_period_us = 1_000_000.0 / f64::from(refresh_hz);
    let estimated_line_us = (frame_period_us - CVT_RB_MIN_VERTICAL_BLANK_US) / f64::from(height);
    if estimated_line_us <= 0.0 {
        return None;
    }
    let minimum_vertical_blank = (CVT_RB_MIN_VERTICAL_BLANK_US / estimated_line_us).ceil() as u32;
    let vertical_blank = minimum_vertical_blank.max(CVT_RB_MIN_VERTICAL_BLANK_LINES);
    if vertical_blank > MAX_DTD_VALUE {
        return None;
    }
    let horizontal_total = f64::from(width + CVT_RB_HORIZONTAL_BLANK);
    let vertical_total = f64::from(height + vertical_blank);
    let ideal_clock_mhz = horizontal_total * vertical_total * f64::from(refresh_hz) / 1_000_000.0;
    Some((ideal_clock_mhz * 100.0).ceil() as u32)
}

/// The stream size a pinned session should request for one display: what it
/// can present in fullscreen ([`DisplayMetrics::presentation_stream_extent`]),
/// downgraded from a `HiDPI` (backing-pixel) request to the point-size stream
/// when the host could not express it as an EDID 1.4 detailed timing at
/// `refresh_hz`.
///
/// This is the single, shared encoder/EDID/refresh/`HiDPI` fallback policy of
/// both Decks: a primary-only pinned session and every entry of a
/// multi-monitor-v1 topology for the same physical display go through it.
/// `refresh_hz` must already be the *pinned* refresh
/// ([`pinned_refresh_hz`]) the caller advertises.
///
/// This is the form every caller that has already read a display's metrics
/// must use -- the multi-monitor-v1 topology builder reads each display once
/// and calls this, so a primary-only `ClientHello` and a topology entry for
/// the same physical display share not just the same decision but the same
/// metrics reading.
#[must_use]
pub fn presentation_extent_with_edid_fallback(
    metrics: &DisplayMetrics,
    refresh_hz: u32,
    use_notch_area: bool,
    hidpi: bool,
) -> StreamExtent {
    let requested = metrics.presentation_stream_extent(use_notch_area, hidpi);
    if !hidpi || requested_size_is_edid_feasible(Some(requested.as_array()), refresh_hz) {
        return requested;
    }
    // HiDPI multiplies the pixel count, which can push the synthesised mode
    // past what an EDID 1.4 detailed timing can express. Downgrade to the
    // point-size stream here rather than letting the host refuse the whole
    // session.
    let fallback = metrics.presentation_stream_extent(use_notch_area, false);
    tracing::warn!(
        target: "arcen::display",
        display_id = metrics.display_id(),
        requested = ?requested.as_array(),
        fallback = ?fallback.as_array(),
        refresh_hz,
        scale = metrics.scale().get(),
        "HiDPI stream cannot be expressed as an EDID 1.4 timing; using the point-size stream",
    );
    fallback
}

/// The pure "may we keep this HiDPI-requested size, or must we downgrade"
/// decision at the heart of [`presentation_extent_with_edid_fallback`]. `None` (the display's own
/// facts could not be read at all) is never feasible.
fn requested_size_is_edid_feasible(requested: Option<[u32; 2]>, refresh_hz: u32) -> bool {
    match requested {
        Some([width, height]) => edid_feasible(width, height, refresh_hz),
        None => false,
    }
}

/// The stream size a pinned session (Primary Display Only, or Match My
/// Layout on one display) asks for: the primary display's fullscreen
/// presentation size through [`presentation_extent_with_edid_fallback`] at
/// its pinned refresh. `displays` pairs each roster entry with its metrics
/// (the primary is the one marked primary, else the first); `None` with no
/// displays.
#[must_use]
pub fn primary_presentation_size(
    displays: &[(arcen_protocol::messages::ClientMonitor, DisplayMetrics)],
    use_notch_area: bool,
    hidpi: bool,
) -> Option<[u32; 2]> {
    let (monitor, metrics) = displays
        .iter()
        .find(|(monitor, _)| monitor.is_primary)
        .or_else(|| displays.first())?;
    Some(
        presentation_extent_with_edid_fallback(
            metrics,
            pinned_refresh_hz(monitor.refresh_hz),
            use_notch_area,
            hidpi,
        )
        .as_array(),
    )
}

/// A change a Deck makes to its session window's fullscreen state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFullscreenStep {
    /// A pinned session started: show it fullscreen.
    Enter,
    /// The pinned session ended: go back to what the window was before.
    Leave {
        /// Whether the window was fullscreen before the session.
        restore: bool,
    },
}

/// The rule both Decks follow: a pinned session (Primary Display Only,
/// Match My Layout) is fullscreen for its whole life, and the window goes
/// back to its own state when it ends; a Windowed session leaves the window
/// alone (the user toggles fullscreen there).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionFullscreen {
    restore: Option<bool>,
}

impl SessionFullscreen {
    /// One pass: whether a pinned session is on screen now, and whether the
    /// window is fullscreen. Returns the change to make, once per edge.
    pub fn step(
        &mut self,
        pinned_session: bool,
        fullscreen: bool,
    ) -> Option<SessionFullscreenStep> {
        if pinned_session {
            if self.restore.is_none() {
                self.restore = Some(fullscreen);
                return Some(SessionFullscreenStep::Enter);
            }
            return None;
        }
        self.restore
            .take()
            .map(|restore| SessionFullscreenStep::Leave { restore })
    }

    /// Whether a pinned session holds the window fullscreen now.
    #[must_use]
    pub const fn holding(self) -> bool {
        self.restore.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::Rotation;

    /// Metrics for one display at the desktop origin: `logical` points backed
    /// by `backing` pixels, with `insets` reserved by the system.
    fn metrics_of(
        logical: (u32, u32),
        backing: (u32, u32),
        insets: SafeAreaInsets,
    ) -> DisplayMetrics {
        DisplayMetrics::new(
            1,
            LogicalRect::new(0, 0, logical.0, logical.1).expect("valid arrangement"),
            backing.0,
            backing.1,
            Rotation::Degrees0,
            insets,
        )
        .expect("valid metrics")
    }

    /// The measured 14" MacBook Pro "More Space" Retina panel.
    fn retina_1800x1169(insets: SafeAreaInsets) -> DisplayMetrics {
        metrics_of((1800, 1169), (3600, 2338), insets)
    }

    /// Reproduces the exact figure the Windows host reported when it refused a
    /// HiDPI Retina request on 2026-08-03:
    /// "calculated 1116.72MHz pixel clock exceeds EDID 1.4 limits".
    #[test]
    fn a_pinned_session_holds_fullscreen_and_gives_the_window_back() {
        let mut rule = SessionFullscreen::default();
        assert_eq!(rule.step(false, false), None);
        assert_eq!(rule.step(true, false), Some(SessionFullscreenStep::Enter));
        // Once per session, even while the window is still switching.
        assert_eq!(rule.step(true, false), None);
        assert_eq!(rule.step(true, true), None);
        assert_eq!(
            rule.step(false, true),
            Some(SessionFullscreenStep::Leave { restore: false })
        );
        assert_eq!(rule.step(false, false), None);
        // A window already fullscreen before the session stays so.
        assert_eq!(rule.step(true, true), Some(SessionFullscreenStep::Enter));
        assert_eq!(
            rule.step(false, true),
            Some(SessionFullscreenStep::Leave { restore: true })
        );
    }

    #[test]
    fn the_pinned_stream_is_the_primary_displays_fullscreen_size() {
        let monitor = |id: u32, primary: bool| arcen_protocol::messages::ClientMonitor {
            id,
            is_primary: primary,
            refresh_hz: 120,
            ..arcen_protocol::messages::ClientMonitor::default()
        };
        let scaled = metrics_of((2560, 1440), (3840, 2160), SafeAreaInsets::ZERO);
        let plain = metrics_of((1920, 1080), (1920, 1080), SafeAreaInsets::ZERO);
        let displays = [(monitor(2, false), plain), (monitor(1, true), scaled)];
        assert_eq!(
            primary_presentation_size(&displays, false, false),
            Some([2560, 1440])
        );
        // HiDPI streams the panel's pixels, at the pinned 60 Hz.
        assert_eq!(
            primary_presentation_size(&displays, false, true),
            Some([3840, 2160])
        );
        assert_eq!(primary_presentation_size(&[], false, true), None);
    }

    #[test]
    fn cvt_estimate_matches_the_host_rejection() {
        assert_eq!(cvt_rb_pixel_clock_10khz(3600, 2338, 120), Some(111_672));
        assert!(!edid_feasible(3600, 2338, 120));
    }

    #[test]
    fn capping_the_refresh_makes_a_retina_stream_presentable() {
        assert_eq!(pinned_refresh_hz(120), 60);
        assert_eq!(pinned_refresh_hz(144), 60);
        assert_eq!(pinned_refresh_hz(60), 60);
        assert_eq!(pinned_refresh_hz(50), 50);
        assert_eq!(pinned_refresh_hz(0), 1);
        assert!(
            edid_feasible(3600, 2338, 60),
            "the 60 Hz HiDPI mode the Deck now asks for must fit EDID 1.4"
        );
    }

    #[test]
    fn requested_size_is_edid_feasible_is_the_single_decision_every_display_shares() {
        // The exact decision `presentation_extent_with_edid_fallback`
        // applies for *every* display -- a primary-only pinned session and
        // every entry in a multi-monitor-v1 topology alike -- proven here
        // without a live display, mirroring
        // `cvt_estimate_matches_the_host_rejection`/
        // `capping_the_refresh_makes_a_retina_stream_presentable`'s own
        // pure-math coverage of the underlying `edid_feasible` formula.
        assert!(
            !requested_size_is_edid_feasible(None, 60),
            "a display whose own facts could not be read is never feasible",
        );
        // A 120 Hz Retina panel's native HiDPI stream ("calculated
        // 1116.72MHz pixel clock exceeds EDID 1.4 limits") is infeasible at
        // its own unclamped refresh, but the exact same size becomes
        // feasible once pinned to 60 Hz -- proving the decision depends on
        // whatever refresh the caller already pinned, not a value it
        // silently reclamps itself.
        assert!(!requested_size_is_edid_feasible(Some([3600, 2338]), 120));
        assert!(requested_size_is_edid_feasible(Some([3600, 2338]), 60));
        // An ordinary non-Retina 1080p panel at 60 Hz never needed a
        // downgrade in the first place.
        assert!(requested_size_is_edid_feasible(Some([1920, 1080]), 60));
    }

    #[test]
    fn oversized_modes_are_refused_before_they_reach_the_host() {
        // 5K backing pixels exceed the 12-bit detailed-timing field.
        assert!(!edid_feasible(5120, 2880, 60));
        assert!(!edid_feasible(0, 1080, 60));
        assert!(!edid_feasible(1920, 1080, 0));
    }

    #[test]
    fn ordinary_point_size_modes_stay_feasible() {
        assert!(edid_feasible(1800, 1130, 60));
        assert!(edid_feasible(1800, 1168, 60));
        assert!(edid_feasible(2560, 1440, 60));
    }

    /// The EDID fallback applies the same decision to an already-read metrics
    /// value as it does to a display id, so a topology entry and a
    /// primary-only `ClientHello` for one physical display share both the
    /// decision and the reading behind it.
    #[test]
    fn edid_fallback_downgrades_a_hidpi_stream_only_when_it_must() {
        let retina = retina_1800x1169(SafeAreaInsets {
            top: 39,
            ..SafeAreaInsets::ZERO
        });
        // 60 Hz: the HiDPI stream fits an EDID 1.4 detailed timing.
        assert_eq!(
            presentation_extent_with_edid_fallback(&retina, 60, false, true).as_array(),
            [3600, 2260]
        );
        // 120 Hz: it does not, so the point-size stream is used instead --
        // never a silently different refresh.
        assert_eq!(
            presentation_extent_with_edid_fallback(&retina, 120, false, true).as_array(),
            [1800, 1130]
        );
        // A point-size request is never downgraded, feasible or not.
        assert_eq!(
            presentation_extent_with_edid_fallback(&retina, 120, false, false).as_array(),
            [1800, 1130]
        );
        // A 1.5x scaled panel's HiDPI stream is its true backing size, and it
        // still fits at 60 Hz.
        let scaled = metrics_of((2000, 1333), (3000, 2000), SafeAreaInsets::ZERO);
        assert_eq!(
            presentation_extent_with_edid_fallback(&scaled, 60, false, true).as_array(),
            [3000, 2000]
        );
    }
}
