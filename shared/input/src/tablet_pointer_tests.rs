// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
use std::time::{Duration, Instant};

use super::*;

const BTN_TOUCH: u16 = 0x14a;
const BTN_TOOL_BRUSH: u16 = 0x142;

fn range(maximum: i32) -> AxisRange {
    AxisRange {
        minimum: 0,
        maximum,
    }
}

fn frames() -> EvdevPenFrames {
    EvdevPenFrames::new(range(1999), range(999), 0, 0).unwrap()
}

fn position(sample: Option<PenSample>) -> Option<(Option<PenTool>, (f64, f64))> {
    sample.map(|sample| (sample.tool, sample.fraction()))
}

#[test]
fn a_tool_in_proximity_is_sampled_once_per_changed_frame_and_once_when_it_leaves() {
    let at = Instant::now();
    let mut pen = frames();
    assert_eq!(pen.apply(EV_ABS, ABS_X, 500, at), None);
    assert_eq!(pen.apply(EV_SYN, SYN_REPORT, 0, at), None, "no tool yet");
    pen.apply(EV_KEY, BTN_TOOL_PEN, 1, at);
    pen.apply(EV_ABS, ABS_Y, 250, at);
    let first = pen.apply(EV_SYN, SYN_REPORT, 0, at).unwrap();
    assert_eq!(
        (first.tool, first.x, first.y, first.at),
        (Some(PenTool::Pen), 500, 250, at)
    );
    assert_eq!(first.fraction(), (0.25, 0.25));
    pen.apply(EV_KEY, BTN_TOUCH, 1, at);
    assert_eq!(pen.apply(EV_SYN, SYN_REPORT, 0, at), None, "unmoved");
    pen.apply(EV_ABS, ABS_X, 1000, at);
    assert_eq!(
        position(pen.apply(EV_SYN, SYN_REPORT, 0, at)),
        Some((Some(PenTool::Pen), (0.5, 0.25)))
    );
    // Flipping to the eraser is a new tool at the same place.
    pen.apply(EV_KEY, BTN_TOOL_PEN, 0, at);
    pen.apply(EV_KEY, BTN_TOOL_RUBBER, 1, at);
    assert_eq!(
        position(pen.apply(EV_SYN, SYN_REPORT, 0, at)),
        Some((Some(PenTool::Eraser), (0.5, 0.25)))
    );
    pen.apply(EV_KEY, BTN_TOOL_RUBBER, 0, at);
    let left = pen.apply(EV_SYN, SYN_REPORT, 0, at).unwrap();
    assert!(!left.in_proximity());
    assert_eq!(pen.apply(EV_SYN, SYN_REPORT, 0, at), None);
    pen.apply(EV_KEY, BTN_TOOL_BRUSH, 1, at);
    assert_eq!(
        pen.apply(EV_SYN, SYN_REPORT, 0, at).unwrap().tool,
        Some(PenTool::Other)
    );
}

#[test]
fn a_tool_already_down_when_opened_counts_and_dropped_frames_are_skipped() {
    let at = Instant::now();
    let mut pen = frames();
    pen.set_tool_down(BTN_TOOL_PEN);
    pen.apply(EV_SYN, SYN_DROPPED, 0, at);
    pen.apply(EV_ABS, ABS_X, 1500, at);
    assert_eq!(pen.apply(EV_SYN, SYN_REPORT, 0, at), None, "lost frame");
    assert!(pen.needs_resync());
    assert_eq!(
        position(pen.resync([BTN_TOOL_PEN], 1000, 0, at)),
        Some((Some(PenTool::Pen), (0.5, 0.0)))
    );
    assert!(
        EvdevPenFrames::new(
            AxisRange {
                minimum: 5,
                maximum: 5
            },
            range(1),
            0,
            0
        )
        .is_none()
    );
}

#[test]
fn a_drop_is_resynced_from_the_device_state_before_it_is_trusted() {
    let at = Instant::now();
    let mut pen = frames();
    pen.apply(EV_KEY, BTN_TOOL_PEN, 1, at);
    pen.apply(EV_ABS, ABS_X, 500, at);
    pen.apply(EV_SYN, SYN_REPORT, 0, at);
    assert!(pen.in_proximity());
    // The pen left and came back during the drop; its leaving was lost.
    pen.apply(EV_SYN, SYN_DROPPED, 0, at);
    pen.apply(EV_KEY, BTN_TOOL_PEN, 0, at);
    assert_eq!(pen.apply(EV_SYN, SYN_REPORT, 0, at), None);
    assert!(pen.needs_resync());
    // The device says: no tool down. The pen leaves.
    let left = pen.resync([], 1500, 500, at).unwrap();
    assert!(!left.in_proximity());
    assert!(!pen.needs_resync() && !pen.in_proximity());
    // A later drop that ends with the pen down elsewhere.
    pen.apply(EV_SYN, SYN_DROPPED, 0, at);
    pen.apply(EV_SYN, SYN_REPORT, 0, at);
    assert_eq!(
        position(pen.resync([BTN_TOOL_PEN], 1000, 500, at)),
        Some((Some(PenTool::Pen), (0.5, 0.5)))
    );
    // Events after a resync apply normally again.
    pen.apply(EV_ABS, ABS_X, 1500, at);
    assert_eq!(
        position(pen.apply(EV_SYN, SYN_REPORT, 0, at)),
        Some((Some(PenTool::Pen), (0.75, 0.5)))
    );
}

#[test]
fn the_whole_tablet_maps_to_the_whole_desktop_as_the_x_server_scales_it() {
    assert_eq!(map_to_output(0.0, 0.0, 1800, 1130), (0, 0));
    assert_eq!(map_to_output(0.5, 0.25, 1800, 1130), (900, 282));
    assert_eq!(map_to_output(1.0, 1.0, 1800, 1130), (1799, 1129));
    // The X server: (value - min) * width / (max - min + 1), truncated.
    let axis = range(65_024);
    for value in [0, 1, 16_000, 32_512, 65_023, 65_024] {
        let x_server = i64::from(value) * 2560 / 65_025;
        let (x, _) = map_to_output(axis.fraction(value), 0.0, 2560, 1440);
        assert_eq!(i64::from(x), x_server, "value {value}");
    }
}

fn wacom(area: [i64; 4]) -> X11WacomTool {
    X11WacomTool {
        tablet_area: Some(area),
        rotation: Some(0),
        transform: Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]),
        absolute: true,
    }
}

#[test]
fn an_xorg_pen_leads_only_at_xf86_input_wacom_defaults() {
    let (x, y) = (range(65_024), range(40_640));
    let default = wacom([0, 0, 65_024, 40_640]);
    assert_eq!(x11_wacom_mapping(&[], x, y), PenMapping::Unknown);
    assert_eq!(
        x11_wacom_mapping(&[default.clone(), default.clone()], x, y),
        PenMapping::WholeArea
    );
    let remapped = [
        (
            wacom([0, 0, 32_512, 40_640]),
            PenMapping::Remapped(PenRemap::TabletArea),
        ),
        (
            X11WacomTool {
                rotation: Some(3),
                ..default.clone()
            },
            PenMapping::Remapped(PenRemap::Rotation),
        ),
        (
            X11WacomTool {
                transform: Some([0.5, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]),
                ..default.clone()
            },
            PenMapping::Remapped(PenRemap::Transform),
        ),
        (
            X11WacomTool {
                absolute: false,
                ..default.clone()
            },
            PenMapping::Remapped(PenRemap::Relative),
        ),
    ];
    for (tool, expected) in remapped {
        // One remapped tool (the eraser, say) is enough.
        assert_eq!(x11_wacom_mapping(&[default.clone(), tool], x, y), expected);
    }
    // A property that could not be read proves nothing.
    for unreadable in [
        X11WacomTool {
            rotation: None,
            ..default.clone()
        },
        X11WacomTool {
            tablet_area: None,
            ..default.clone()
        },
        X11WacomTool {
            transform: None,
            ..default
        },
    ] {
        assert_eq!(x11_wacom_mapping(&[unreadable], x, y), PenMapping::Unknown);
    }
}

fn sample(tool: Option<PenTool>, x: i32, y: i32, at: Instant) -> PenSample {
    PenSample {
        tool,
        x,
        y,
        x_range: range(1999),
        y_range: range(999),
        at,
    }
}

fn platform(x: i32, y: i32, at: Instant) -> DesktopPointerSample {
    DesktopPointerSample {
        x,
        y,
        desktop_width: 1000,
        desktop_height: 500,
        at,
    }
}

fn xy(sample: Option<DesktopPointerSample>) -> Option<(i32, i32)> {
    sample.map(|sample| (sample.x, sample.y))
}

#[test]
fn the_pen_leads_while_in_proximity_and_the_platform_pointer_otherwise() {
    let start = Instant::now();
    let later = start + Duration::from_millis(3);
    let mut lead = PenPointerLead::new(PenMapping::WholeArea);
    // No desktop size yet: the pen cannot be placed.
    assert_eq!(lead.pen(sample(Some(PenTool::Pen), 500, 250, start)), None);
    assert_eq!(xy(lead.platform(platform(10, 10, start))), Some((10, 10)));
    assert!(!lead.pen_leads());
    // The next pen sample leads, at its own read time.
    let led = lead
        .pen(sample(Some(PenTool::Pen), 1000, 250, later))
        .unwrap();
    assert_eq!((led.x, led.y, led.at), (500, 125, later));
    assert!(lead.pen_leads());
    // The platform's pointer is only remembered while the pen leads.
    assert_eq!(lead.platform(platform(400, 100, later)), None);
    assert_eq!(
        lead.pen(sample(Some(PenTool::Pen), 1000, 250, later)),
        None,
        "unmoved"
    );
    // Leaving publishes nothing: the cursor stays where the pen left it.
    assert_eq!(lead.pen(sample(None, 1000, 250, later)), None);
    assert!(!lead.pen_leads());
    // The platform reads the same place: not published twice.
    assert_eq!(lead.platform(platform(500, 125, later)), None);
    // An application warp is.
    assert_eq!(xy(lead.platform(platform(20, 30, later))), Some((20, 30)));
    // A puck or lens never leads: the platform's pointer follows it.
    assert_eq!(lead.pen(sample(Some(PenTool::Other), 0, 0, later)), None);
    assert!(!lead.pen_leads());
    assert_eq!(xy(lead.platform(platform(21, 30, later))), Some((21, 30)));
}

#[test]
fn a_remapped_or_unknown_mapping_leaves_the_pointer_to_the_platform() {
    let at = Instant::now();
    let mut lead = PenPointerLead::new(PenMapping::Unknown);
    lead.set_desktop_size(1000, 500);
    assert_eq!(lead.pen(sample(Some(PenTool::Pen), 1000, 250, at)), None);
    assert_eq!(lead.pen_ranges(), Some((range(1999), range(999))));
    assert_eq!(xy(lead.platform(platform(7, 8, at))), Some((7, 8)));
    // The desktop proves the default mapping: the pen in proximity leads now.
    assert_eq!(
        xy(lead.set_mapping(PenMapping::WholeArea)),
        Some((500, 125))
    );
    assert!(lead.pen_leads());
    lead.platform(platform(9, 9, at));
    // A remap hands the pointer back to the platform at once.
    assert_eq!(
        xy(lead.set_mapping(PenMapping::Remapped(PenRemap::Transform))),
        Some((9, 9))
    );
    assert!(!lead.pen_leads());
    assert_eq!(lead.pen(sample(Some(PenTool::Pen), 0, 0, at)), None);
    // A resize re-places a leading pen.
    let mut lead = PenPointerLead::new(PenMapping::WholeArea);
    lead.set_desktop_size(1000, 500);
    lead.pen(sample(Some(PenTool::Pen), 1000, 250, at));
    assert_eq!(xy(lead.set_desktop_size(2000, 1000)), Some((1000, 250)));
    // A reader that ended stops leading without publishing.
    lead.pen_ended();
    assert!(!lead.pen_leads() && lead.pen_ranges().is_none());
    assert_eq!(xy(lead.platform(platform(1, 2, at))), Some((1, 2)));
}

#[test]
fn the_hub_publishes_both_sources_through_one_lead_and_ignores_a_stale_reader() {
    use std::sync::{Arc, Mutex};
    let published = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&published);
    let hub = PenPointerHub::new(
        PenMapping::WholeArea,
        move |sample: DesktopPointerSample| {
            sink.lock().unwrap().push((sample.x, sample.y));
        },
    );
    let at = Instant::now();
    hub.platform(platform(1, 2, at));
    assert!(hub.pen(sample(Some(PenTool::Pen), 1000, 250, at)).is_some());
    assert!(hub.pen_leads());
    hub.platform(platform(3, 3, at));
    let device = PenDevice {
        device_number: (13, 70),
        x_range: range(1999),
        y_range: range(999),
    };
    hub.note_pen_device(device);
    hub.pen_ended((13, 71));
    assert!(hub.pen_leads(), "another reader's end");
    hub.pen_ended((13, 70));
    assert!(!hub.pen_leads() && hub.pen_device().is_none());
    assert_eq!(*published.lock().unwrap(), vec![(1, 2), (500, 125)]);
}
