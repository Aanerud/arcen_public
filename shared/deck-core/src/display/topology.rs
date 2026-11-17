// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The local multi-monitor-v1 requested topology, built from the displays a
//! Deck enumerated (moved from the macOS Deck so both Decks build it once).
//!
//! The platform shell gathers each active display's legacy roster entry and
//! its one [`DisplayMetrics`] reading; [`build_requested_topology_from`]
//! turns them into the domain-level [`arcen_media::RequestedMonitorTopology`]
//! the Deck negotiates with a host that offers multi-monitor-v1 and hashes
//! into reconnect identity.

#![cfg_attr(
    test,
    allow(clippy::float_cmp, reason = "exact scale values are the assertions")
)]

use std::fmt;

use arcen_media::{
    MAX_MULTI_MONITOR_COUNT, MediaContractError, Monitor, MonitorIdentity, RequestedMonitor,
    RequestedMonitorTopology,
};
use arcen_protocol::messages::ClientMonitor;

use crate::display::metrics::DisplayMetrics;

/// Failure building a validated local requested multi-monitor-v1 topology.
#[derive(Debug, Clone, PartialEq)]
pub enum TopologyPreflightError {
    /// No active local display could be enumerated.
    NoActiveDisplays,
    /// More displays are active than the first multi-monitor-v1 tranche
    /// supports. `arcen_media::MAX_MULTI_MONITOR_COUNT` is the exact bound.
    TooManyDisplays(usize),
    /// A display was enumerated but its geometry could not be read.
    DisplayUnavailable(u32),
    /// The gathered per-display facts failed a domain invariant.
    InvalidTopology(MediaContractError),
}

impl fmt::Display for TopologyPreflightError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoActiveDisplays => formatter.write_str("no active local displays were found"),
            Self::TooManyDisplays(count) => write!(
                formatter,
                "{count} active displays exceed the {MAX_MULTI_MONITOR_COUNT}-display \
                 multi-monitor-v1 limit; disconnect a display or use Primary Display Only"
            ),
            Self::DisplayUnavailable(id) => {
                write!(formatter, "display {id} geometry could not be read")
            }
            Self::InvalidTopology(error) => {
                write!(formatter, "local display layout is invalid: {error}")
            }
        }
    }
}

impl std::error::Error for TopologyPreflightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidTopology(error) => Some(error),
            Self::NoActiveDisplays | Self::TooManyDisplays(_) | Self::DisplayUnavailable(_) => None,
        }
    }
}

/// Assembles the validated requested topology from each display's legacy
/// roster entry paired with its single [`DisplayMetrics`] reading.
///
/// Pure and platform-independent: each Deck's live builder only gathers the
/// pairs, so every mixed-DPI, rotated, non-integer-scale and negative-origin
/// case is directly testable without real hardware.
///
/// Every geometric fact comes from the metrics -- the top-left/y-down
/// logical arrangement rectangle, the exact backing scale, the rotation and
/// the stream extent -- so a monitor's advertised `scale` and its advertised
/// `width_px`/`height_px` always describe the same reading. Identity,
/// refresh and physical size come from the legacy roster entry for the same
/// display, which is what the primary-only `ClientHello` path advertises.
///
/// # Errors
///
/// [`TopologyPreflightError::NoActiveDisplays`] for an empty layout,
/// [`TopologyPreflightError::TooManyDisplays`] beyond
/// [`MAX_MULTI_MONITOR_COUNT`], and
/// [`TopologyPreflightError::InvalidTopology`] when the gathered facts fail a
/// domain invariant.
pub fn build_requested_topology_from(
    displays: &[(ClientMonitor, DisplayMetrics)],
    use_notch_area: bool,
    hidpi_mask: u8,
) -> Result<RequestedMonitorTopology, TopologyPreflightError> {
    if displays.is_empty() {
        return Err(TopologyPreflightError::NoActiveDisplays);
    }
    if displays.len() > MAX_MULTI_MONITOR_COUNT {
        return Err(TopologyPreflightError::TooManyDisplays(displays.len()));
    }

    let mut requested = Vec::with_capacity(displays.len());
    for (index, (client_monitor, metrics)) in displays.iter().enumerate() {
        // Per display, primary-first, matching `full_color_display_mask`'s
        // own slot convention. A monitor outside the mask's four slots is
        // always point-sized, never silently promoted.
        let hidpi = index < 4 && hidpi_mask & (1u8 << index) != 0;
        // Same pinned refresh + EDID/HiDPI fallback policy `ClientHello`
        // applies to a primary-only session (`primary_presentation_size`)
        // -- every monitor in a multi-monitor-v1 topology goes through
        // the exact same shared decision, per-monitor, so a host can
        // never see a different stream size or refresh for the same
        // physical display depending on whether Deck negotiated
        // multi-monitor or fell back to legacy primary-only.
        let refresh_hz = crate::display::pinned_refresh_hz(client_monitor.refresh_hz);
        let stream = crate::display::presentation_extent_with_edid_fallback(
            metrics,
            refresh_hz,
            use_notch_area,
            hidpi,
        );
        let arrangement = metrics.arrangement();
        let monitor = Monitor {
            identity: MonitorIdentity {
                id: client_monitor.id.to_string(),
                name: client_monitor.name.clone(),
                vendor: client_monitor.vendor,
                model: client_monitor.model,
                serial: client_monitor.serial,
            },
            x: arrangement.x(),
            y: arrangement.y(),
            width_px: stream.width(),
            height_px: stream.height(),
            scale: metrics.scale().get(),
            refresh_hz,
            rotation: metrics.rotation(),
            primary: client_monitor.is_primary,
            width_mm: client_monitor.width_mm,
            height_mm: client_monitor.height_mm,
            color: client_monitor.color,
        };
        let requested_monitor =
            RequestedMonitor::new(monitor, arrangement.width(), arrangement.height())
                .map_err(TopologyPreflightError::InvalidTopology)?;
        requested.push(requested_monitor);
    }
    RequestedMonitorTopology::new(requested).map_err(TopologyPreflightError::InvalidTopology)
}

#[cfg(test)]
mod tests {
    use super::*;

    use arcen_media::Rotation;

    use crate::display::metrics::{LogicalRect, SafeAreaInsets};

    /// One display's legacy roster entry, as a Deck's display enumeration
    /// builds it from the same metrics the pair carries.
    fn client_monitor(
        id: u32,
        metrics: &DisplayMetrics,
        refresh_hz: u32,
        is_primary: bool,
    ) -> ClientMonitor {
        let stream = metrics.native_stream_extent();
        ClientMonitor {
            id,
            x: metrics.arrangement().x(),
            y: metrics.arrangement().y(),
            width_px: stream.width(),
            height_px: stream.height(),
            scale: metrics.scale().get(),
            refresh_hz,
            is_primary,
            name: format!("Display {id}"),
            width_mm: 302.0,
            height_mm: 189.0,
            vendor: 1552,
            model: 40_968,
            serial: 0,
            edid: String::new(),
            color: None,
            rotation_degrees: 0,
        }
    }

    fn display(
        id: u32,
        arrangement: LogicalRect,
        backing: (u32, u32),
        rotation: Rotation,
        insets: SafeAreaInsets,
        refresh_hz: u32,
        is_primary: bool,
    ) -> (ClientMonitor, DisplayMetrics) {
        let metrics = DisplayMetrics::new(id, arrangement, backing.0, backing.1, rotation, insets)
            .expect("valid metrics");
        (
            client_monitor(id, &metrics, refresh_hz, is_primary),
            metrics,
        )
    }

    /// The measured 14" MacBook Pro built-in panel: a 2x Retina "More Space"
    /// mode with a 38 pt notch inset, at the desktop origin.
    fn retina_builtin(is_primary: bool) -> (ClientMonitor, DisplayMetrics) {
        display(
            1,
            LogicalRect::new(0, 0, 1800, 1169).expect("valid arrangement"),
            (3600, 2338),
            Rotation::Degrees0,
            SafeAreaInsets {
                top: 38,
                ..SafeAreaInsets::ZERO
            },
            120,
            is_primary,
        )
    }

    /// A second 2x Retina panel, so a per-display test can vary only the
    /// mask rather than the panels' own scales.
    fn retina_builtin_secondary() -> (ClientMonitor, DisplayMetrics) {
        display(
            2,
            LogicalRect::new(1800, 0, 1800, 1169).expect("valid arrangement"),
            (3600, 2338),
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
            60,
            false,
        )
    }

    /// An ordinary 1x 2560x1440 external panel to the right of the built-in.
    fn external_1x() -> (ClientMonitor, DisplayMetrics) {
        display(
            2,
            LogicalRect::new(1800, 0, 2560, 1440).expect("valid arrangement"),
            (2560, 1440),
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
            60,
            false,
        )
    }

    fn monitor_named(topology: &RequestedMonitorTopology, id: &str) -> Monitor {
        topology
            .monitors()
            .iter()
            .map(RequestedMonitor::monitor)
            .find(|monitor| monitor.identity.id == id)
            .cloned()
            .unwrap_or_else(|| panic!("monitor {id} must be in the topology"))
    }

    #[test]
    fn too_many_displays_error_names_the_exact_count() {
        let error = TopologyPreflightError::TooManyDisplays(5);
        assert!(error.to_string().contains('5'));
        assert!(error.to_string().contains('4'));
    }

    /// Full-resolution streaming is chosen per display, not session-wide.
    ///
    /// Streaming a Retina panel at point size sends its *logical* width, so a
    /// large physical screen showing 1800 points becomes an 1800-pixel remote
    /// desktop the remote OS then renders at 100% -- which is why the Philips
    /// looked oversized on 2026-08-11. Opting in costs roughly four times the
    /// pixels on a 2x panel, so it has to be affordable one display at a time
    /// and must default to off.
    #[test]
    fn full_resolution_streaming_is_selected_per_display_and_defaults_to_off() {
        // Two 2x Retina panels, so the mask is the only thing that differs.
        let displays = [retina_builtin(true), retina_builtin_secondary()];

        let none = build_requested_topology_from(&displays, false, 0)
            .expect("a two-display layout is valid");
        let first_only = build_requested_topology_from(&displays, false, 0b0001)
            .expect("a two-display layout is valid");
        let both = build_requested_topology_from(&displays, false, 0b0011)
            .expect("a two-display layout is valid");

        let widths = |topology: &RequestedMonitorTopology| -> Vec<u32> {
            topology
                .monitors()
                .iter()
                .map(|monitor| monitor.monitor().width_px)
                .collect()
        };

        let point = widths(&none);
        let mixed = widths(&first_only);
        let all = widths(&both);

        assert!(
            mixed[0] > point[0],
            "the opted-in display must stream its backing pixels: {mixed:?} vs {point:?}",
        );
        assert_eq!(
            mixed[1], point[1],
            "a display outside the mask must stay point-sized",
        );
        assert!(
            all[0] > point[0] && all[1] > point[1],
            "both displays opt in independently: {all:?} vs {point:?}",
        );
    }

    /// A 2x Retina built-in beside a 1x external panel: every monitor's
    /// advertised scale, logical arrangement and stream size come from its
    /// own metrics, never from the primary's.
    #[test]
    fn mixed_dpi_topology_advertises_each_displays_own_scale_and_stream() {
        let displays = [retina_builtin(true), external_1x()];

        let point_size = build_requested_topology_from(&displays, false, 0)
            .expect("a two-display mixed-DPI layout is valid");
        let builtin = monitor_named(&point_size, "1");
        let external = monitor_named(&point_size, "2");
        assert_eq!(builtin.scale, 2.0);
        assert_eq!(external.scale, 1.0);
        // Logical arrangement always stays in CG point space, whatever the
        // stream size policy is.
        assert_eq!((builtin.x, builtin.y), (0, 0));
        assert_eq!((external.x, external.y), (1800, 0));
        let rects: Vec<_> = point_size
            .monitors()
            .iter()
            .map(|monitor| {
                monitor
                    .logical_arrangement_rect()
                    .expect("valid logical rect")
            })
            .collect();
        assert_eq!((rects[0].width, rects[0].height), (1800, 1169));
        assert_eq!((rects[1].width, rects[1].height), (2560, 1440));
        // Point-size streams: the notched built-in loses its menu-bar strip.
        assert_eq!((builtin.width_px, builtin.height_px), (1800, 1130));
        assert_eq!((external.width_px, external.height_px), (2560, 1440));
        // Both refreshes are pinned to the shared ceiling.
        assert_eq!(builtin.refresh_hz, crate::display::PINNED_MAX_REFRESH_HZ);
        assert_eq!(external.refresh_hz, 60);

        let hidpi = build_requested_topology_from(&displays, false, 0b1111)
            .expect("a two-display mixed-DPI layout is valid");
        let builtin = monitor_named(&hidpi, "1");
        let external = monitor_named(&hidpi, "2");
        // Only the Retina panel doubles; the 1x external panel is untouched
        // by its neighbour's scale. 2262 px is the exact double of the
        // 1131 pt strip below the 38 pt notch inset -- the point-size stream
        // above lost a row to encoder alignment, the pixel stream does not.
        assert_eq!((builtin.width_px, builtin.height_px), (3600, 2262));
        assert_eq!((external.width_px, external.height_px), (2560, 1440));
        assert_eq!(builtin.scale, 2.0);
        assert_eq!(external.scale, 1.0);
    }

    /// A 1.5x scaled Retina mode used to advertise `scale = 1.5` while the
    /// stream carried the point-size pixels an integer scale produced. The
    /// shared metrics make the two agree.
    #[test]
    fn non_integer_scale_topology_streams_the_scale_it_advertises() {
        let scaled = display(
            3,
            LogicalRect::new(0, 0, 2000, 1333).expect("valid arrangement"),
            (3000, 2000),
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
            60,
            true,
        );
        let topology = build_requested_topology_from(&[scaled], false, 0b1111)
            .expect("a single scaled display is a valid layout");
        let monitor = monitor_named(&topology, "3");
        assert_eq!(monitor.scale, 1.5);
        assert_eq!((monitor.width_px, monitor.height_px), (3000, 2000));
        let rect = topology.monitors()[0]
            .logical_arrangement_rect()
            .expect("valid logical rect");
        assert_eq!((rect.width, rect.height), (2000, 1333));
        // The advertised stream is exactly the advertised scale times the
        // advertised logical arrangement, on both axes.
        assert!(
            (f64::from(monitor.width_px) - f64::from(rect.width) * f64::from(monitor.scale)).abs()
                < 4.0
        );
        assert!(
            (f64::from(monitor.height_px) - f64::from(rect.height) * f64::from(monitor.scale))
                .abs()
                < 2.0
        );
    }

    /// A rotated secondary at a negative origin: `CGDisplayBounds` reports
    /// the portrait rectangle the desktop is arranged in, so the topology
    /// advertises portrait geometry and the rotation that produced it.
    #[test]
    fn rotated_secondary_at_a_negative_origin_keeps_cg_top_left_geometry() {
        let primary = retina_builtin(true);
        // 2160x3840 backing pixels for a 1080x1920 portrait rectangle: the
        // panel's landscape framebuffer transposed with its own bounds.
        let rotated = display(
            4,
            LogicalRect::new(-1080, -400, 1080, 1920).expect("valid arrangement"),
            (2160, 3840),
            Rotation::Degrees90,
            SafeAreaInsets::ZERO,
            60,
            false,
        );
        let topology = build_requested_topology_from(&[primary, rotated], false, 0b1111)
            .expect("a rotated secondary is a valid layout");

        let secondary = monitor_named(&topology, "4");
        assert_eq!(secondary.rotation, Rotation::Degrees90);
        assert_eq!((secondary.x, secondary.y), (-1080, -400));
        assert_eq!(secondary.scale, 2.0);
        assert_eq!((secondary.width_px, secondary.height_px), (2160, 3840));
        let rect = topology
            .monitors()
            .iter()
            .find(|monitor| monitor.monitor().identity.id == "4")
            .expect("the rotated secondary must be in the topology")
            .logical_arrangement_rect()
            .expect("valid logical rect");
        assert_eq!((rect.x, rect.y), (-1080, -400));
        assert_eq!((rect.width, rect.height), (1080, 1920));
        // Negative origins are ordinary CG arrangement coordinates: y grows
        // downward, so a display above the primary is negative.
        assert_eq!(rect.y + i32::try_from(rect.height).expect("in range"), 1520);
        // The unrotated primary is unaffected by its neighbour's rotation.
        assert_eq!(monitor_named(&topology, "1").rotation, Rotation::Degrees0);
    }

    /// Whatever the layout, no monitor may advertise a scale its own stream
    /// size disagrees with -- the invariant the split integer/float scale
    /// derivations used to break.
    #[test]
    fn every_advertised_scale_matches_its_own_stream_extent() {
        let displays = [
            retina_builtin(true),
            external_1x(),
            display(
                3,
                LogicalRect::new(-2000, -1333, 2000, 1333).expect("valid arrangement"),
                (3000, 2000),
                Rotation::Degrees0,
                SafeAreaInsets::ZERO,
                60,
                false,
            ),
            display(
                4,
                LogicalRect::new(4360, -400, 1080, 1920).expect("valid arrangement"),
                (2160, 3840),
                Rotation::Degrees270,
                SafeAreaInsets::ZERO,
                60,
                false,
            ),
        ];
        for use_notch_area in [false, true] {
            for hidpi in [false, true] {
                let topology = build_requested_topology_from(
                    &displays,
                    use_notch_area,
                    if hidpi { 0b1111 } else { 0 },
                )
                .expect("a four-display layout is valid");
                assert_eq!(topology.monitors().len(), displays.len());
                for requested in topology.monitors() {
                    let monitor = requested.monitor();
                    let rect = requested
                        .logical_arrangement_rect()
                        .expect("valid logical rect");
                    let metrics = displays
                        .iter()
                        .find(|(client, _)| client.id.to_string() == monitor.identity.id)
                        .map(|(_, metrics)| *metrics)
                        .expect("every advertised monitor comes from a real display");
                    assert_eq!(monitor.scale, metrics.scale().get());
                    assert!(monitor.scale.is_finite() && monitor.scale > 0.0);
                    let (presentable_width, presentable_height) =
                        metrics.presentable_size(use_notch_area);
                    let stream_scale = f64::from(metrics.stream_scale(hidpi).get());
                    assert!(
                        (f64::from(monitor.width_px) - f64::from(presentable_width) * stream_scale)
                            .abs()
                            < 4.0,
                        "monitor {} advertises {}px for {presentable_width}pt at {stream_scale}x",
                        monitor.identity.id,
                        monitor.width_px,
                    );
                    assert!(
                        (f64::from(monitor.height_px)
                            - f64::from(presentable_height) * stream_scale)
                            .abs()
                            < 2.0,
                        "monitor {} advertises {}px for {presentable_height}pt at {stream_scale}x",
                        monitor.identity.id,
                        monitor.height_px,
                    );
                    assert_eq!(monitor.width_px % 4, 0, "encoder-aligned width");
                    assert_eq!(monitor.height_px % 2, 0, "even height");
                    // The logical arrangement never changes with the stream
                    // policy: it is always the display's CG point rectangle.
                    assert_eq!(rect.x, metrics.arrangement().x());
                    assert_eq!(rect.y, metrics.arrangement().y());
                    assert_eq!(rect.width, metrics.arrangement().width());
                    assert_eq!(rect.height, metrics.arrangement().height());
                }
            }
        }
    }

    #[test]
    fn an_empty_or_oversized_layout_is_refused_before_it_reaches_a_host() {
        assert_eq!(
            build_requested_topology_from(&[], false, 0),
            Err(TopologyPreflightError::NoActiveDisplays)
        );
        let mut displays = vec![retina_builtin(true)];
        for id in 2..=(MAX_MULTI_MONITOR_COUNT as u32 + 1) {
            displays.push(display(
                id,
                LogicalRect::new(1800 * i32::try_from(id).expect("in range"), 0, 1920, 1080)
                    .expect("valid arrangement"),
                (1920, 1080),
                Rotation::Degrees0,
                SafeAreaInsets::ZERO,
                60,
                false,
            ));
        }
        assert_eq!(
            build_requested_topology_from(&displays, false, 0),
            Err(TopologyPreflightError::TooManyDisplays(displays.len()))
        );
    }
}
