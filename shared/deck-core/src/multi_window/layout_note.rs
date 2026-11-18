// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How a host showed the Deck's monitors when it could not show them as
//! asked: a desktop another remote-desktop product laid out, which a Pier
//! never changes (ADR 0025 §8), shows each Deck display the host monitor in
//! its place at that monitor's own size. The Deck fits each picture into
//! its window; this is the one line that says so.

use arcen_media::{RequestedMonitorTopology, Rotation};
use arcen_protocol::messages::{AppliedMonitorTopologyMsg, ServerHelloMsg};

/// One line naming every Deck display the host shows at another size (the
/// picture is scaled, letterboxed when the shapes differ) or does not show
/// at all; `None` when every display is shown exactly as asked.
#[must_use]
pub fn applied_layout_note(
    requested: &RequestedMonitorTopology,
    applied: &AppliedMonitorTopologyMsg,
) -> Option<String> {
    let mut parts = Vec::new();
    for (index, wanted) in requested.monitors().iter().enumerate() {
        let monitor = wanted.monitor();
        let display = index + 1;
        let Some(shown) = applied
            .monitors()
            .iter()
            .find(|shown| shown.client_display_id.as_str() == monitor.identity.id)
        else {
            parts.push(format!(
                "display {display}: not shown (the host has fewer monitors)"
            ));
            continue;
        };
        let (width, height) = (shown.width_px, shown.height_px);
        // The Deck states its native (pre-rotation) size; the host states
        // the monitor's extent on its desktop.
        let (deck_width, deck_height) = match monitor.rotation {
            Rotation::Degrees90 | Rotation::Degrees270 => (monitor.height_px, monitor.width_px),
            Rotation::Degrees0 | Rotation::Degrees180 => (monitor.width_px, monitor.height_px),
        };
        if (width, height) == (deck_width, deck_height) {
            continue;
        }
        let cross = |a: u32, b: u32| u64::from(a) * u64::from(b);
        let left = cross(width, deck_height);
        let right = cross(deck_width, height);
        let letterboxed = left.abs_diff(right) * 200 > right.max(1);
        parts.push(format!(
            "display {display}: host {width}x{height} scaled to {deck_width}x{deck_height}{}",
            if letterboxed { ", letterboxed" } else { "" }
        ));
    }
    (!parts.is_empty()).then(|| format!("Host layout kept: {}", parts.join("; ")))
}

/// [`applied_layout_note`] for the layout a hello says the host applied,
/// against the layout this attempt asked for; `None` without either.
#[must_use]
pub fn hello_layout_note(
    requested: Option<&RequestedMonitorTopology>,
    hello: &ServerHelloMsg,
) -> Option<String> {
    let requested = requested?;
    let capability = hello.multi_monitor_v1().ok().flatten()?;
    applied_layout_note(requested, capability.applied_topology()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{Monitor, MonitorIdentity, RequestedMonitor};
    use arcen_protocol::messages::{
        AppliedMonitorDescriptorMsg, AppliedMonitorMediaPlanMsg, ClientDisplayId, CursorMode,
        MultiMonitorCarrierMsg, RotationMsg,
    };

    fn wanted(id: &str, x: i32, (width, height): (u32, u32), primary: bool) -> RequestedMonitor {
        RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: id.to_owned(),
                    name: id.to_owned(),
                    ..MonitorIdentity::default()
                },
                x,
                y: 0,
                width_px: width,
                height_px: height,
                scale: 1.0,
                refresh_hz: 60,
                rotation: Rotation::Degrees0,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
                color: None,
            },
            width,
            height,
        )
        .expect("requested monitor")
    }

    fn shown(
        id: &str,
        monitor: u16,
        x: i32,
        (width, height): (u32, u32),
        primary: bool,
    ) -> AppliedMonitorDescriptorMsg {
        AppliedMonitorDescriptorMsg {
            client_display_id: ClientDisplayId::new(id.to_owned()).expect("id"),
            session_monitor_id: monitor,
            x,
            y: 0,
            width_px: width,
            height_px: height,
            refresh_hz: 60,
            rotation: RotationMsg::Degrees0,
            is_primary: primary,
            media_plan: AppliedMonitorMediaPlanMsg {
                stream_epoch: 1,
                encoder_backend: "native-nvenc".to_owned(),
                encoder_class: "hardware".to_owned(),
                codec: "h265".to_owned(),
                chroma: "yuv420".to_owned(),
                width_px: width,
                height_px: height,
                fps: 60,
                bitrate_kbps: 20_000,
                cursor_mode: CursorMode::Local,
                degraded: false,
                degradation_reason: String::new(),
            },
        }
    }

    fn applied(monitors: Vec<AppliedMonitorDescriptorMsg>) -> AppliedMonitorTopologyMsg {
        let left = monitors.iter().map(|monitor| monitor.x).min().unwrap_or(0);
        let right = monitors
            .iter()
            .map(|monitor| monitor.x + i32::try_from(monitor.width_px).expect("width"))
            .max()
            .unwrap_or(0);
        let height = monitors
            .iter()
            .map(|monitor| monitor.height_px)
            .max()
            .unwrap_or(0);
        AppliedMonitorTopologyMsg::new(
            1,
            left,
            0,
            u32::try_from(right - left).expect("width"),
            height,
            0,
            0,
            MultiMonitorCarrierMsg::MuxedReliableStream,
            monitors,
        )
        .expect("applied topology")
    }

    #[test]
    fn a_layout_shown_as_asked_needs_no_note() {
        let requested = RequestedMonitorTopology::new(vec![
            wanted("a", 0, (2560, 1440), true),
            wanted("b", 2560, (1920, 1080), false),
        ])
        .expect("requested");
        let applied = applied(vec![
            shown("a", 1, 0, (2560, 1440), true),
            shown("b", 2, 2560, (1920, 1080), false),
        ]);
        assert_eq!(applied_layout_note(&requested, &applied), None);
    }

    #[test]
    fn a_rotated_display_shown_at_its_extent_needs_no_note() {
        let mut portrait = wanted("b", 2560, (1920, 1080), false);
        portrait.monitor.rotation = Rotation::Degrees90;
        let requested =
            RequestedMonitorTopology::new(vec![wanted("a", 0, (2560, 1440), true), portrait])
                .expect("requested");
        let applied = applied(vec![
            shown("a", 1, 0, (2560, 1440), true),
            shown("b", 2, 2560, (1080, 1920), false),
        ]);
        assert_eq!(applied_layout_note(&requested, &applied), None);
    }

    #[test]
    fn scaled_letterboxed_and_missing_displays_are_named() {
        let requested = RequestedMonitorTopology::new(vec![
            wanted("a", 0, (1920, 1080), true),
            wanted("b", 1920, (1920, 1080), false),
            wanted("c", 3840, (1920, 1080), false),
        ])
        .expect("requested");
        let applied = applied(vec![
            shown("a", 1, 7680, (3840, 2160), true),
            shown("b", 2, 11520, (1920, 1200), false),
        ]);
        assert_eq!(
            applied_layout_note(&requested, &applied).as_deref(),
            Some(
                "Host layout kept: display 1: host 3840x2160 scaled to 1920x1080; \
                 display 2: host 1920x1200 scaled to 1920x1080, letterboxed; \
                 display 3: not shown (the host has fewer monitors)"
            )
        );
    }
}
