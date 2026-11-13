// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `arcen-pier-macos probe-pen`: does a Deck's Tablet support pen reach
//! applications on this Mac?
//!
//! Posts a scripted pen through the session input path —
//! [`InputSession::apply`](crate::input_session::InputSession::apply), the call
//! a live session makes for every `pen_event` a Deck sends — and reads back
//! what `AppKit` made of it with a global `NSEvent` monitor: the events the
//! window server delivered to whichever application is under the pen. Counting
//! what was posted is not evidence (`hosts/macos/AGENTS.md`); this reports
//! what arrived.
//!
//! By default the pen only hovers: proximity in and out, tilt and rotation,
//! the eraser end, and nothing that clicks. `--strokes` adds a pressure stroke,
//! both barrel buttons and an eraser rub, which press buttons wherever the area
//! is, so it is for an area the operator has checked is empty.
//!
//! `--intuos5` replaces the script with the emulated Intuos5's packets
//! (`arcen_usb_bridge::intuos5_scripted_stroke`) read by the same shared
//! reader and posted by the same call as a bridged tablet's pen in a
//! Native tablet session reading a known pen (`InputSession::apply_host_pen`), so what a
//! Native tablet session reading the pen itself gives applications is measured
//! without a Deck. It presses (tip and lower barrel), like `--strokes`.
//!
//! Lab builds only (`--features lab-pen-probe`). Started inside the installed
//! Agent Helper it posts input — clicks, with `--strokes` — under the agent's
//! Accessibility approval, which any process of the same user could ask a
//! launchd job to do; so no packaging script enables the feature and a shipped
//! Pier does not contain this module.
//!
//! Run it from the Aqua session, not over SSH: posting needs the
//! Accessibility approval of the app responsible for the process, which over
//! SSH is `sshd` (measured on macOS 26.5: from a plain SSH child the pointer
//! moved and no application saw a single event).

use serde::Serialize;

use crate::input::DesktopBounds;

/// What `probe-pen` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenProbeOptions {
    /// Global points, top-left origin, that the script's normalized
    /// coordinates map onto. `None` uses the middle of the main display.
    pub area: Option<DesktopBounds>,
    /// Whether to press: tip strokes, barrel buttons, eraser rub.
    pub strokes: bool,
    /// Post the emulated Intuos5 stroke through the host pen reader.
    pub intuos5: bool,
}

/// Parses `probe-pen` arguments:
/// `[--area X,Y,WIDTH,HEIGHT] [--strokes] [--intuos5]`.
///
/// # Errors
///
/// Returns the message to show the operator when an argument is unusable.
pub fn parse_options(arguments: &[String]) -> Result<PenProbeOptions, String> {
    let mut options = PenProbeOptions {
        area: None,
        strokes: false,
        intuos5: false,
    };
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--strokes" => options.strokes = true,
            "--intuos5" => options.intuos5 = true,
            "--area" => {
                let value = arguments
                    .get(index + 1)
                    .ok_or_else(|| "--area requires X,Y,WIDTH,HEIGHT".to_owned())?;
                options.area = Some(parse_area(value)?);
                index += 1;
            }
            other => return Err(format!("probe-pen: unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(options)
}

fn parse_area(value: &str) -> Result<DesktopBounds, String> {
    let numbers: Vec<f64> = value
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("--area expects X,Y,WIDTH,HEIGHT, got '{value}'"))?;
    let [x, y, width, height] = numbers[..] else {
        return Err(format!("--area expects X,Y,WIDTH,HEIGHT, got '{value}'"));
    };
    let area = DesktopBounds::new(x, y, width, height);
    if !area.is_usable() {
        return Err(format!(
            "--area needs a finite position and a positive size, got '{value}'"
        ));
    }
    Ok(area)
}

/// One `pen_event` of the script, in the wire's units.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    x: f64,
    y: f64,
    pressure: f32,
    tilt_x: f32,
    tilt_y: f32,
    rotation: f32,
    eraser: bool,
    in_proximity: bool,
    touching: bool,
    buttons: u16,
}

impl Sample {
    const fn away(x: f64, y: f64, eraser: bool) -> Self {
        Self {
            x,
            y,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation: 0.0,
            eraser,
            in_proximity: false,
            touching: false,
            buttons: 0,
        }
    }

    const fn hover(x: f64, y: f64, eraser: bool) -> Self {
        Self {
            in_proximity: true,
            ..Self::away(x, y, eraser)
        }
    }
}

/// How long the script waits after a sample, so the window server sees a
/// tool leave before the next one arrives, as it would from a real tablet.
const fn pause_after(sample: &Sample) -> std::time::Duration {
    std::time::Duration::from_millis(if sample.in_proximity { 8 } else { 150 })
}

/// The pen the probe posts, in the order a digitizer would report it.
#[allow(
    clippy::cast_possible_truncation,
    reason = "ramps of small, bounded wire values (0..=1, degrees)"
)]
fn script(strokes: bool) -> Vec<Sample> {
    const STEPS: u32 = 40;
    let ramp = |step: u32| f64::from(step) / f64::from(STEPS);
    let mut samples = Vec::new();

    // The pen hovers left to right, tilting and turning: proximity, tilt and
    // rotation, and no button.
    for step in 0..=STEPS {
        let t = ramp(step);
        samples.push(Sample {
            tilt_x: (20.0 + 20.0 * t) as f32,
            tilt_y: -10.0,
            rotation: (90.0 * t) as f32,
            ..Sample::hover(0.2 + 0.6 * t, 0.3, false)
        });
    }
    samples.push(Sample::away(0.8, 0.3, false));

    // The eraser end hovers: a second tool type, still no button.
    for step in 0..10 {
        samples.push(Sample::hover(0.2 + 0.02 * f64::from(step), 0.4, true));
    }
    samples.push(Sample::away(0.38, 0.4, true));

    if !strokes {
        return samples;
    }

    // A tip stroke with a pressure ramp, lifted in range, then out.
    samples.push(Sample {
        tilt_x: 40.0,
        tilt_y: 25.0,
        rotation: 90.0,
        ..Sample::hover(0.2, 0.55, false)
    });
    for step in 0..=STEPS {
        let t = ramp(step);
        samples.push(Sample {
            pressure: (0.1 + 0.9 * t) as f32,
            tilt_x: 40.0,
            tilt_y: 25.0,
            rotation: 90.0,
            touching: true,
            ..Sample::hover(0.2 + 0.6 * t, 0.55, false)
        });
    }
    samples.push(Sample {
        tilt_x: 40.0,
        tilt_y: 25.0,
        rotation: 90.0,
        ..Sample::hover(0.8, 0.55, false)
    });
    samples.push(Sample::away(0.8, 0.55, false));

    // Each barrel button, pressed and released while hovering.
    for (x, buttons) in [(0.35, 0b01_u16), (0.65, 0b10)] {
        samples.push(Sample::hover(x, 0.7, false));
        samples.push(Sample {
            buttons,
            ..Sample::hover(x, 0.7, false)
        });
        samples.push(Sample::hover(x, 0.7, false));
        samples.push(Sample::away(x, 0.7, false));
    }

    // The eraser rubs.
    samples.push(Sample::hover(0.2, 0.85, true));
    for step in 0..=STEPS {
        samples.push(Sample {
            pressure: 0.7,
            touching: true,
            ..Sample::hover(0.2 + 0.6 * ramp(step), 0.85, true)
        });
    }
    samples.push(Sample::hover(0.8, 0.85, true));
    samples.push(Sample::away(0.8, 0.85, true));
    samples
}

/// The wire message for `sample`, exactly as a Deck sends it.
fn pen_event_json(sample: &Sample, sequence: u64) -> serde_json::Value {
    serde_json::json!({
        "type": arcen_protocol::messages::PEN_EVENT,
        "x": sample.x,
        "y": sample.y,
        "server_x": -1,
        "server_y": -1,
        "pressure": sample.pressure,
        "tilt_x_degrees": sample.tilt_x,
        "tilt_y_degrees": sample.tilt_y,
        "rotation_degrees": sample.rotation,
        "tool": if sample.eraser { "eraser" } else { "tip" },
        "in_proximity": sample.in_proximity,
        "touching": sample.touching,
        "buttons": sample.buttons,
        "sequence": sequence,
        "timestamp_ns": 0,
        "coalescable": true,
    })
}

/// What the probe posted and what `AppKit` delivered.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PenProbeEvidence {
    /// Why nothing could be measured, when that is the case.
    pub refusal: Option<String>,
    /// Whether the script pressed (`--strokes`).
    pub strokes: bool,
    /// Whether the script was the emulated Intuos5 read by the host pen
    /// reader (`--intuos5`).
    pub intuos5: bool,
    /// The area the script was mapped onto: x, y, width, height.
    pub area: [f64; 4],
    /// Samples the session input path injected.
    pub posted_samples: u64,
    /// Proximity edges the session input path posted.
    pub posted_proximity_edges: u64,
    /// Samples the session input path refused.
    pub posted_rejected: u64,
    /// Tablet proximity events `AppKit` delivered.
    pub proximity_events: u64,
    /// ... of them for the pen tip.
    pub pen_proximity_events: u64,
    /// ... of them for the eraser.
    pub eraser_proximity_events: u64,
    /// Union of `-[NSEvent capabilityMask]` over proximity events.
    pub capability_mask: u64,
    /// Tablet point events (or tablet-subtype mouse events) delivered.
    pub tablet_point_events: u64,
    /// Highest `-[NSEvent pressure]` seen on a tablet point.
    pub max_pressure: f32,
    /// Largest `-[NSEvent tilt]` component seen, -1..=1 scale.
    pub max_tilt: f64,
    /// Highest `-[NSEvent rotation]` seen, degrees.
    pub max_rotation: f32,
    /// Union of `-[NSEvent buttonMask]` over tablet points.
    pub button_mask: u64,
    /// Tablet-subtype button-down events (tip, barrel).
    pub button_down_events: u64,
}

impl PenProbeEvidence {
    /// Whether applications here receive the pen as a tablet: proximity with
    /// a capability mask, tilt and rotation; with `--strokes` also pressure,
    /// the tip in the button mask and both barrel buttons.
    #[must_use]
    pub fn observed(&self) -> bool {
        if self.intuos5 {
            // The Intuos5 stroke has the tip, a pressure ramp and the lower
            // barrel button; no eraser and no rotation.
            let wanted = u64::try_from(
                crate::input::pen::PEN_BUTTON_MASK_TIP
                    | crate::input::pen::PEN_BUTTON_MASK_LOWER_SIDE,
            )
            .unwrap_or(0);
            return self.refusal.is_none()
                && self.pen_proximity_events > 0
                && self.capability_mask != 0
                && self.tablet_point_events > 0
                && self.max_pressure > 0.0
                && self.button_mask & wanted == wanted;
        }
        let hover = self.pen_proximity_events > 0
            && self.eraser_proximity_events > 0
            && self.capability_mask != 0
            && self.tablet_point_events > 0
            && self.max_tilt > 0.0
            && self.max_rotation > 0.0;
        let strokes = !self.strokes
            || (self.max_pressure > 0.0
                && self.button_mask
                    & u64::try_from(
                        crate::input::pen::PEN_BUTTON_MASK_TIP
                            | crate::input::pen::PEN_BUTTON_MASK_LOWER_SIDE
                            | crate::input::pen::PEN_BUTTON_MASK_UPPER_SIDE,
                    )
                    .unwrap_or(0)
                    == 0b111);
        self.refusal.is_none() && hover && strokes
    }
}

#[cfg(target_os = "macos")]
pub use native::probe;

#[cfg(target_os = "macos")]
mod native {
    #![allow(unsafe_code)]

    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2_app_kit::{
        NSApplication, NSEvent, NSEventMask, NSEventSubtype, NSEventType, NSPointingDeviceType,
    };
    use objc2_foundation::{MainThreadMarker, NSDate, NSDefaultRunLoopMode};

    use super::{PenProbeEvidence, PenProbeOptions, pause_after, pen_event_json, script};
    use crate::input::DesktopBounds;
    use crate::input_session::InputSession;

    /// Longest the probe runs, whatever happens.
    const DEADLINE: Duration = Duration::from_secs(30);
    /// How long late deliveries are waited for after the last sample.
    const DRAIN: Duration = Duration::from_millis(700);

    /// Runs the probe. Must be called on the main thread: `AppKit` delivers
    /// monitored events there.
    #[must_use]
    pub fn probe(options: PenProbeOptions) -> PenProbeEvidence {
        let mut evidence = PenProbeEvidence {
            strokes: options.strokes || options.intuos5,
            intuos5: options.intuos5,
            ..PenProbeEvidence::default()
        };
        let Some(marker) = MainThreadMarker::new() else {
            evidence.refusal = Some("probe-pen must run on the main thread".to_owned());
            return evidence;
        };
        let area = options.area.unwrap_or_else(main_display_middle);
        evidence.area = [area.origin_x, area.origin_y, area.width, area.height];
        if !area.is_usable() {
            evidence.refusal = Some("no display to map the pen onto".to_owned());
            return evidence;
        }

        let _ = crate::cursor_probe::connect_to_window_server();
        let application = NSApplication::sharedApplication(marker);
        let shared = Arc::new(Mutex::new(evidence));
        let recorder = Arc::clone(&shared);
        let handler = RcBlock::new(move |event: NonNull<NSEvent>| {
            // SAFETY: AppKit calls a monitor with a live event, valid for the
            // duration of the call; it is only read here.
            let event = unsafe { event.as_ref() };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(mut evidence) = recorder.lock() {
                    record(&mut evidence, event);
                }
            }));
        });
        let mask = NSEventMask::TabletPoint
            | NSEventMask::TabletProximity
            | NSEventMask::MouseMoved
            | NSEventMask::LeftMouseDown
            | NSEventMask::LeftMouseUp
            | NSEventMask::LeftMouseDragged
            | NSEventMask::RightMouseDown
            | NSEventMask::RightMouseUp
            | NSEventMask::RightMouseDragged
            | NSEventMask::OtherMouseDown
            | NSEventMask::OtherMouseUp
            | NSEventMask::OtherMouseDragged;
        let Some(monitor) = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &handler)
        else {
            let mut evidence = shared.lock().map(|e| e.clone()).unwrap_or_default();
            evidence.refusal = Some("AppKit refused a global event monitor".to_owned());
            return evidence;
        };

        let before = crate::input::current_pointer_position();
        let strokes = options.strokes;
        let intuos5 = options.intuos5;
        let poster = std::thread::spawn(move || {
            if intuos5 {
                post_intuos5(area)
            } else {
                post(area, strokes)
            }
        });

        let started = Instant::now();
        let mut finished_at = None;
        while started.elapsed() < DEADLINE {
            if finished_at.is_none() && poster.is_finished() {
                finished_at = Some(Instant::now());
            }
            if finished_at.is_some_and(|at| at.elapsed() >= DRAIN) {
                break;
            }
            let until = NSDate::dateWithTimeIntervalSinceNow(0.05);
            // SAFETY: an extern constant AppKit and Foundation define.
            let mode = unsafe { NSDefaultRunLoopMode };
            if let Some(event) = application.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                mode,
                true,
            ) {
                application.sendEvent(&event);
            }
        }
        // SAFETY: `monitor` is the object `addGlobalMonitor…` returned.
        unsafe { NSEvent::removeMonitor(&monitor) };
        let outcome = if poster.is_finished() {
            poster
                .join()
                .unwrap_or_else(|_| Err("pen poster panicked".to_owned()))
        } else {
            Err("pen poster did not finish before the deadline".to_owned())
        };
        if let Some(position) = before {
            crate::input::warp_pointer(position);
        }

        let mut evidence = shared.lock().map(|e| e.clone()).unwrap_or_default();
        match outcome {
            Ok(stats) => {
                evidence.posted_samples = stats.pen_samples;
                evidence.posted_proximity_edges = stats.pen_proximity_edges;
                evidence.posted_rejected = stats.pen_rejected;
            }
            Err(error) => evidence.refusal = Some(error),
        }
        evidence
    }

    fn post(
        area: DesktopBounds,
        strokes: bool,
    ) -> Result<crate::input_session::InputStats, String> {
        let mut session =
            InputSession::new(area).map_err(|error| format!("no input session: {error}"))?;
        // Lets the monitor settle before the first proximity edge.
        std::thread::sleep(Duration::from_millis(300));
        for (index, sample) in script(strokes).iter().enumerate() {
            let sequence = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            session
                .apply(&pen_event_json(sample, sequence).to_string())
                .map_err(|error| format!("injecting sample {sequence}: {error}"))?;
            std::thread::sleep(pause_after(sample));
        }
        let _ = session.release_all();
        Ok(session.stats())
    }

    /// The emulated Intuos5 stroke, packet by packet, through the shared
    /// reader and the call a Native tablet session reading a known pen makes.
    fn post_intuos5(area: DesktopBounds) -> Result<crate::input_session::InputStats, String> {
        let model = arcen_usb_bridge::host_decoded_intuos_model(
            arcen_usb_bridge::WACOM_VENDOR_ID,
            arcen_usb_bridge::INTUOS5_TOUCH_L_PRODUCT_ID,
        )
        .ok_or_else(|| "the Intuos5 is not a host-read model".to_owned())?;
        let mut reader = arcen_usb_bridge::IntuosPenReader::new(model);
        let mut session =
            InputSession::new(area).map_err(|error| format!("no input session: {error}"))?;
        std::thread::sleep(Duration::from_millis(300));
        for step in arcen_usb_bridge::intuos5_scripted_stroke() {
            if let Some(sample) = reader.read(&step.report.encode()) {
                let event = crate::bridged_tablet::host_pen_event(&sample);
                session
                    .apply_host_pen(&event)
                    .map_err(|error| format!("injecting {}: {error}", step.phase))?;
                std::thread::sleep(if event.in_proximity {
                    Duration::from_millis(12)
                } else {
                    Duration::from_millis(150)
                });
            }
        }
        let _ = session.release_all();
        Ok(session.stats())
    }

    fn main_display_middle() -> DesktopBounds {
        let display = objc2_core_graphics::CGDisplayBounds(objc2_core_graphics::CGMainDisplayID());
        DesktopBounds::new(
            display.origin.x + display.size.width / 4.0,
            display.origin.y + display.size.height / 4.0,
            display.size.width / 2.0,
            display.size.height / 2.0,
        )
    }

    fn record(evidence: &mut PenProbeEvidence, event: &NSEvent) {
        let kind = event.r#type();
        let mouse = matches!(
            kind,
            NSEventType::MouseMoved
                | NSEventType::LeftMouseDown
                | NSEventType::LeftMouseUp
                | NSEventType::LeftMouseDragged
                | NSEventType::RightMouseDown
                | NSEventType::RightMouseUp
                | NSEventType::RightMouseDragged
                | NSEventType::OtherMouseDown
                | NSEventType::OtherMouseUp
                | NSEventType::OtherMouseDragged
        );
        // `subtype` raises for event types that have none, so it is read
        // only for the mouse types tablets annotate.
        let subtype = mouse.then(|| event.subtype());
        if kind == NSEventType::TabletProximity || subtype == Some(NSEventSubtype::TabletProximity)
        {
            evidence.proximity_events += 1;
            match event.pointingDeviceType() {
                NSPointingDeviceType::Pen => evidence.pen_proximity_events += 1,
                NSPointingDeviceType::Eraser => evidence.eraser_proximity_events += 1,
                _ => {}
            }
            evidence.capability_mask |= event.capabilityMask() as u64;
        } else if kind == NSEventType::TabletPoint || subtype == Some(NSEventSubtype::TabletPoint) {
            evidence.tablet_point_events += 1;
            evidence.max_pressure = evidence.max_pressure.max(event.pressure());
            let tilt = event.tilt();
            evidence.max_tilt = evidence.max_tilt.max(tilt.x.abs()).max(tilt.y.abs());
            evidence.max_rotation = evidence.max_rotation.max(event.rotation());
            evidence.button_mask |= event.buttonMask().0 as u64;
            if matches!(
                kind,
                NSEventType::LeftMouseDown
                    | NSEventType::RightMouseDown
                    | NSEventType::OtherMouseDown
            ) {
                evidence.button_down_events += 1;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn options_name_an_area_and_whether_to_press() {
        let options = parse_options(&arguments(&["--area", "120,200,640,420", "--strokes"]))
            .expect("valid options");
        assert!(options.strokes);
        assert_eq!(
            options.area,
            Some(DesktopBounds::new(120.0, 200.0, 640.0, 420.0))
        );
        let defaults = parse_options(&[]).expect("no options");
        assert!(!defaults.strokes && !defaults.intuos5);
        assert_eq!(defaults.area, None);
        assert!(
            parse_options(&arguments(&["--intuos5"]))
                .expect("valid")
                .intuos5
        );
    }

    #[test]
    fn an_unusable_area_or_unknown_argument_is_refused() {
        for bad in [
            &["--area"][..],
            &["--area", "1,2,3"],
            &["--area", "1,2,0,4"],
            &["--area", "a,b,c,d"],
            &["--press"],
        ] {
            assert!(parse_options(&arguments(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hovering_never_presses_anything() {
        // The default must be safe on a desktop someone is using.
        let samples = script(false);
        assert!(samples.iter().all(|s| !s.touching && s.buttons == 0));
        assert!(samples.iter().any(|s| s.in_proximity && s.eraser));
        assert!(samples.iter().any(|s| s.in_proximity && s.rotation > 0.0));
        assert!(!samples.last().expect("a script").in_proximity);
    }

    #[test]
    fn strokes_press_the_tip_both_barrel_buttons_and_the_eraser() {
        let samples = script(true);
        assert!(
            samples
                .iter()
                .any(|s| s.touching && !s.eraser && s.pressure > 0.9)
        );
        assert!(samples.iter().any(|s| s.buttons == 0b01));
        assert!(samples.iter().any(|s| s.buttons == 0b10));
        assert!(samples.iter().any(|s| s.touching && s.eraser));
        assert!(!samples.last().expect("a script").in_proximity);
    }

    #[test]
    fn every_sample_is_a_valid_pen_event_in_sequence() {
        for (index, sample) in script(true).iter().enumerate() {
            let sequence = u64::try_from(index).expect("small") + 1;
            let message: arcen_protocol::messages::PenEventMsg =
                serde_json::from_value(pen_event_json(sample, sequence)).expect("wire shape");
            message.validate().expect("in range");
            assert_eq!(message.sequence, sequence);
        }
    }

    #[test]
    fn evidence_needs_the_whole_tablet_surface_not_a_count() {
        let hover = PenProbeEvidence {
            pen_proximity_events: 2,
            eraser_proximity_events: 2,
            proximity_events: 4,
            capability_mask: 0x25c7,
            tablet_point_events: 40,
            max_tilt: 0.4,
            max_rotation: 90.0,
            ..PenProbeEvidence::default()
        };
        assert!(hover.observed());
        assert!(
            !PenProbeEvidence {
                capability_mask: 0,
                ..hover.clone()
            }
            .observed()
        );
        let strokes = PenProbeEvidence {
            strokes: true,
            max_pressure: 1.0,
            button_mask: 0b011,
            ..hover.clone()
        };
        assert!(
            !strokes.observed(),
            "the second barrel button never arrived"
        );
        assert!(
            PenProbeEvidence {
                button_mask: 0b111,
                ..strokes
            }
            .observed()
        );
        // The Intuos5 stroke has no eraser and no second barrel button, but
        // must show pressure.
        let intuos5 = PenProbeEvidence {
            intuos5: true,
            strokes: true,
            eraser_proximity_events: 0,
            max_rotation: 0.0,
            max_pressure: 0.5,
            button_mask: 0b011,
            ..hover
        };
        assert!(intuos5.observed());
        assert!(
            !PenProbeEvidence {
                max_pressure: 0.0,
                ..intuos5
            }
            .observed()
        );
    }
}
