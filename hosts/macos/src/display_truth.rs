// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

#![allow(unsafe_code)]

//! The macOS Pier's reader for the shared display truth
//! (`arcen_outputs::display_truth`, ADR 0025 §14), and the proof that a
//! session's virtual displays left the Mac's own displays as they were.
//!
//! It reads every online display with CoreGraphics: its place in the global
//! desktop (points), its mode in pixels and refresh, its rotation, whether it
//! is the main display, and whether it can show EDR (`colour = hdr` when its
//! potential headroom is above SDR white). A display is identified by its
//! vendor, model and serial numbers, which survive a reboot and a new display
//! id; the id itself is only the label. macOS has no per-display HDR switch
//! and CoreGraphics reports no EDID or link depth, so those read as unread
//! and are not compared.
//!
//! The Pier changes no physical display's mode. It adds virtual displays
//! and places them, which can make macOS move the physical ones and the main
//! display; [`RestoreCheck`] reads the displays before a virtual display
//! exists and after it is gone, puts back a physical display's place (and so
//! the main display) when macOS did not, and reports what still differs.

use arcen_outputs::display_truth::{
    ColourMode, DisplayField, DisplayTruth, DisplayTruthReason, ModeTruth, OutputTruth,
    RestoreDifference, restore_differences,
};

const MEDIA: &str = arcen_telemetry::names::target::MEDIA;

/// The shared display truth's source name on macOS.
pub const SOURCE: &str = "macos-coregraphics";

/// How long a released virtual display may take to leave, and macOS to lay
/// the remaining displays out again.
const RESTORE_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);
const RESTORE_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// Every online display as CoreGraphics reads it now.
///
/// # Errors
///
/// When CoreGraphics does not answer (no window server session).
pub fn read() -> Result<DisplayTruth, String> {
    #[cfg(target_os = "macos")]
    {
        native::read()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("display truth is read on macOS only".to_owned())
    }
}

/// The identity of a display from its vendor, model and serial numbers;
/// `occurrence` tells apart identical displays that report no serial.
#[must_use]
pub fn display_identity(vendor: u32, model: u32, serial: u32, occurrence: usize) -> String {
    let base = format!("{vendor:08x}:{model:08x}:{serial:08x}");
    if occurrence == 0 {
        base
    } else {
        format!("{base}#{occurrence}")
    }
}

/// The Mac's displays before a session's virtual displays, checked again
/// once they are gone (contract part 1).
#[derive(Debug)]
pub struct RestoreCheck {
    before: DisplayTruth,
}

impl RestoreCheck {
    /// Reads the displays now. `None` (logged) when CoreGraphics does not
    /// answer: the session still runs, and its restore is not proved.
    #[must_use]
    pub fn capture() -> Option<Self> {
        match read() {
            Ok(before) => {
                for output in before.to_text().lines().skip(1) {
                    tracing::info!(
                        target: MEDIA,
                        output,
                        "displays before the session's virtual displays"
                    );
                }
                Some(Self { before })
            }
            Err(error) => {
                tracing::warn!(
                    target: MEDIA,
                    %error,
                    "the displays could not be read before the session; their restore cannot be proved"
                );
                None
            }
        }
    }

    /// Waits for the session's virtual displays to leave, puts a physical
    /// display macOS moved back in its place, and reports what still
    /// differs (`display_restore_incomplete`).
    ///
    /// # Errors
    ///
    /// The typed message when the displays do not read back as before.
    pub fn verify(self) -> Result<(), String> {
        let settled = settle(&self.before, read, RESTORE_SETTLE, RESTORE_POLL);
        let left = match settled {
            Ok(left) if left.is_empty() => {
                tracing::info!(
                    target: MEDIA,
                    "the displays are back exactly as they were before the session"
                );
                return Ok(());
            }
            Ok(left) => left,
            Err(error) => return Err(report(&format!("could not be read: {error}"))),
        };
        if let Some(origins) = origins_to_put_back(&self.before, &left) {
            tracing::warn!(
                target: MEDIA,
                left = %join(&left),
                "macOS did not put the displays back in their places; restoring them"
            );
            if let Err(error) = crate::virtual_display::arrange_origins(&origins) {
                return Err(report(&format!(
                    "{}; putting them back failed: {error}",
                    join(&left)
                )));
            }
            return match settle(&self.before, read, RESTORE_SETTLE, RESTORE_POLL) {
                Ok(left) if left.is_empty() => {
                    tracing::info!(
                        target: MEDIA,
                        "the displays are back exactly as they were before the session"
                    );
                    Ok(())
                }
                Ok(left) => Err(report(&join(&left))),
                Err(error) => Err(report(&format!("could not be read: {error}"))),
            };
        }
        Err(report(&join(&left)))
    }
}

fn report(detail: &str) -> String {
    tracing::error!(
        target: MEDIA,
        reason_code = DisplayTruthReason::RestoreIncomplete.code(),
        left = %detail,
        "the displays are not back as they were before the session"
    );
    for difference in detail.split("; ") {
        tracing::error!(target: MEDIA, difference, "display restore difference");
    }
    DisplayTruthReason::RestoreIncomplete.message(&format!(
        "the displays differ from before the session: {detail}"
    ))
}

/// Reads until the displays equal `before` or `settle` passes; the provable
/// differences left.
fn settle(
    before: &DisplayTruth,
    mut read: impl FnMut() -> Result<DisplayTruth, String>,
    settle: std::time::Duration,
    poll: std::time::Duration,
) -> Result<Vec<RestoreDifference>, String> {
    let deadline = std::time::Instant::now() + settle;
    loop {
        let after = read()?;
        let left = restore_differences(before, &after)
            .into_iter()
            .filter(|difference| difference.provable)
            .collect::<Vec<_>>();
        if left.is_empty() || std::time::Instant::now() >= deadline {
            return Ok(left);
        }
        std::thread::sleep(poll);
    }
}

/// The places to give back when only places (and so the main display, the
/// one at the origin) differ, for every display that was active before and
/// is online now: `(display id, x, y)`. `None` when anything else differs,
/// which putting places back cannot mend.
fn origins_to_put_back(
    before: &DisplayTruth,
    left: &[RestoreDifference],
) -> Option<Vec<(u32, i32, i32)>> {
    let only_places = !left.is_empty()
        && left.iter().all(|difference| {
            matches!(
                difference.field,
                DisplayField::Position | DisplayField::Primary
            )
        });
    if !only_places {
        return None;
    }
    let now = read().ok()?;
    before
        .outputs
        .iter()
        .filter(|output| output.active)
        .map(|output| {
            let display = now.output(&output.id)?.label.parse::<u32>().ok()?;
            Some((display, output.x, output.y))
        })
        .collect()
}

fn join(differences: &[RestoreDifference]) -> String {
    differences
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// One display's facts as CoreGraphics reports them, before identity.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayFacts {
    pub display_id: u32,
    pub vendor: u32,
    pub model: u32,
    pub serial: u32,
    pub active: bool,
    pub main: bool,
    pub origin: (f64, f64),
    pub pixels: Option<(u32, u32)>,
    pub refresh_hz: f64,
    pub rotation_degrees: f64,
    pub headroom: Option<f32>,
}

/// The shared snapshot of `displays`, in the order CoreGraphics listed them.
#[must_use]
pub fn truth_of(displays: &[DisplayFacts]) -> DisplayTruth {
    let mut truth = DisplayTruth::new(SOURCE);
    for (index, display) in displays.iter().enumerate() {
        let occurrence = displays[..index]
            .iter()
            .filter(|earlier| {
                (earlier.vendor, earlier.model, earlier.serial)
                    == (display.vendor, display.model, display.serial)
            })
            .count();
        let mut output = OutputTruth::new(
            display_identity(display.vendor, display.model, display.serial, occurrence),
            display.display_id.to_string(),
        );
        if display.active {
            output.active = true;
            output.primary = display.main;
            #[allow(clippy::cast_possible_truncation, reason = "desktop points")]
            {
                output.x = display.origin.0.round() as i32;
                output.y = display.origin.1.round() as i32;
            }
            output.mode = display.pixels.map(|(width, height)| ModeTruth {
                width,
                height,
                refresh_mhz: refresh_mhz(display.refresh_hz),
            });
            output.rotation = match display.rotation_degrees.round().rem_euclid(360.0) {
                angle if (angle - 90.0).abs() < 1.0 => arcen_media::Rotation::Degrees90,
                angle if (angle - 180.0).abs() < 1.0 => arcen_media::Rotation::Degrees180,
                angle if (angle - 270.0).abs() < 1.0 => arcen_media::Rotation::Degrees270,
                _ => arcen_media::Rotation::Degrees0,
            };
            output.colour = match display.headroom {
                Some(headroom) if headroom > 1.0 => ColourMode::Hdr,
                Some(_) => ColourMode::Sdr,
                None => ColourMode::Unknown,
            };
        }
        truth.outputs.push(output);
    }
    truth
}

fn refresh_mhz(hz: f64) -> u32 {
    if hz.is_finite() && hz > 0.0 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a display refresh in millihertz"
        )]
        let mhz = (hz * 1000.0).round().min(f64::from(u32::MAX)) as u32;
        mhz
    } else {
        0
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::DisplayFacts;
    use arcen_outputs::display_truth::DisplayTruth;

    const MAX_DISPLAYS: u32 = 32;

    pub(super) fn read() -> Result<DisplayTruth, String> {
        let mut ids = [0_u32; MAX_DISPLAYS as usize];
        let mut count = 0_u32;
        // SAFETY: the buffer holds `MAX_DISPLAYS` ids and CoreGraphics writes
        // at most that many, and the count into `count`.
        let status =
            unsafe { CGGetOnlineDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &raw mut count) };
        if status != 0 {
            return Err(format!("CGGetOnlineDisplayList failed: {status}"));
        }
        let count = usize::try_from(count).unwrap_or(0).min(ids.len());
        let displays = ids[..count]
            .iter()
            .map(|&display_id| facts(display_id))
            .collect::<Vec<_>>();
        Ok(super::truth_of(&displays))
    }

    fn facts(display_id: u32) -> DisplayFacts {
        // SAFETY: every call below takes a display id by value and returns
        // plain values; an id that went offline yields zeros.
        let (vendor, model, serial, active, main, rotation) = unsafe {
            (
                CGDisplayVendorNumber(display_id),
                CGDisplayModelNumber(display_id),
                CGDisplaySerialNumber(display_id),
                CGDisplayIsActive(display_id) != 0,
                CGDisplayIsMain(display_id) != 0,
                CGDisplayRotation(display_id),
            )
        };
        let (pixels, refresh_hz) = mode(display_id);
        DisplayFacts {
            display_id,
            vendor,
            model,
            serial,
            active,
            main,
            origin: crate::displays::point_bounds(display_id)
                .map_or((0.0, 0.0), |(x, y, _, _)| (x, y)),
            pixels,
            refresh_hz,
            rotation_degrees: rotation,
            headroom: crate::displays::potential_headroom(display_id),
        }
    }

    fn mode(display_id: u32) -> (Option<(u32, u32)>, f64) {
        // SAFETY: returns a retained mode or null; released below.
        let mode = unsafe { CGDisplayCopyDisplayMode(display_id) };
        if mode.is_null() {
            return (None, 0.0);
        }
        // SAFETY: `mode` is the live mode returned above.
        let (width, height, refresh) = unsafe {
            (
                CGDisplayModeGetPixelWidth(mode),
                CGDisplayModeGetPixelHeight(mode),
                CGDisplayModeGetRefreshRate(mode),
            )
        };
        // SAFETY: releases the +1 reference taken by the copy.
        unsafe { CGDisplayModeRelease(mode) };
        let pixels = u32::try_from(width).ok().zip(u32::try_from(height).ok());
        (
            pixels.filter(|(width, height)| *width > 0 && *height > 0),
            refresh,
        )
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGGetOnlineDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
        fn CGDisplayVendorNumber(display: u32) -> u32;
        fn CGDisplayModelNumber(display: u32) -> u32;
        fn CGDisplaySerialNumber(display: u32) -> u32;
        fn CGDisplayIsActive(display: u32) -> u32;
        fn CGDisplayIsMain(display: u32) -> u32;
        fn CGDisplayRotation(display: u32) -> f64;
        fn CGDisplayCopyDisplayMode(display: u32) -> *mut std::ffi::c_void;
        fn CGDisplayModeGetPixelWidth(mode: *const std::ffi::c_void) -> usize;
        fn CGDisplayModeGetPixelHeight(mode: *const std::ffi::c_void) -> usize;
        fn CGDisplayModeGetRefreshRate(mode: *mut std::ffi::c_void) -> f64;
        fn CGDisplayModeRelease(mode: *mut std::ffi::c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(display_id: u32, serial: u32, main: bool, x: f64) -> DisplayFacts {
        DisplayFacts {
            display_id,
            vendor: 0x610,
            model: 0xa050,
            serial,
            active: true,
            main,
            origin: (x, 0.0),
            pixels: Some((3024, 1964)),
            refresh_hz: 120.0,
            rotation_degrees: 0.0,
            headroom: Some(16.0),
        }
    }

    #[test]
    fn displays_are_identified_by_vendor_model_and_serial_not_their_id() {
        let truth = truth_of(&[panel(1, 0, true, 0.0), panel(7, 0, false, 3024.0)]);
        assert_eq!(truth.outputs[0].id, "00000610:0000a050:00000000");
        assert_eq!(truth.outputs[1].id, "00000610:0000a050:00000000#1");
        assert_eq!(truth.outputs[1].label, "7");
        let output = &truth.outputs[0];
        assert!(output.active && output.primary);
        assert_eq!(output.mode, Some(ModeTruth::at_hz(3024, 1964, 120)));
        assert_eq!(output.colour, ColourMode::Hdr);
        assert_eq!(output.edid, None);

        let mut renumbered = panel(42, 0, true, 0.0);
        renumbered.display_id = 42;
        assert!(
            restore_differences(
                &truth_of(&[panel(1, 0, true, 0.0)]),
                &truth_of(&[renumbered])
            )
            .is_empty()
        );
    }

    // The audit finding: nothing read the Mac's displays after a session's
    // virtual displays left, so a main display or a place macOS did not put
    // back went unseen.
    #[test]
    fn only_a_moved_place_or_main_display_is_put_back() {
        let before = truth_of(&[panel(1, 5, true, 0.0)]);
        let moved = truth_of(&[panel(1, 5, false, 1920.0)]);
        let left = restore_differences(&before, &moved);
        assert!(left.iter().all(|difference| matches!(
            difference.field,
            DisplayField::Position | DisplayField::Primary
        )));

        let mut slower = panel(1, 5, true, 0.0);
        slower.refresh_hz = 60.0;
        let left = restore_differences(&before, &truth_of(&[slower]));
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].field, DisplayField::Mode);
        assert_eq!(origins_to_put_back(&before, &left), None);
        assert_eq!(origins_to_put_back(&before, &[]), None);
    }

    #[test]
    fn a_virtual_display_still_there_after_the_session_is_a_difference() {
        let before = truth_of(&[panel(1, 5, true, 0.0)]);
        let mut virtual_panel = panel(9, 77, false, 3024.0);
        virtual_panel.vendor = 0x3456;
        let mut reads = vec![
            Ok(truth_of(&[panel(1, 5, true, 0.0), virtual_panel.clone()])),
            Ok(truth_of(&[panel(1, 5, true, 0.0)])),
        ]
        .into_iter();
        let instant = std::time::Duration::ZERO;
        assert_eq!(
            settle(
                &before,
                || reads.next().unwrap_or_else(|| Err("no more reads".into())),
                std::time::Duration::from_secs(5),
                instant
            ),
            Ok(Vec::new())
        );
        let left = settle(
            &before,
            || Ok(truth_of(&[panel(1, 5, true, 0.0), virtual_panel.clone()])),
            instant,
            instant,
        );
        let Ok(left) = left else {
            panic!("the displays were read: {left:?}");
        };
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].field, DisplayField::Present);
        assert_eq!(left[0].output, "00003456:0000a050:0000004d");
    }
}
