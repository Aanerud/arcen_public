// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Pen tablet presence for the Tablet Monitor and diagnostics.
//!
//! Windows lists every pen digitizer it routes through `WM_POINTER` (a
//! Wacom with Windows Ink, a Surface pen, any HID pen) with
//! `GetPointerDevices`. Presence is informational; typed pen capability
//! never depends on it (`arcen_deck_core::typed_pen_capability`).

use std::sync::atomic::{AtomicBool, Ordering};

use arcen_input::CapabilityAvailability;
use windows::Win32::UI::Controls::{
    POINTER_DEVICE_INFO, POINTER_DEVICE_TYPE, POINTER_DEVICE_TYPE_EXTERNAL_PEN,
    POINTER_DEVICE_TYPE_INTEGRATED_PEN,
};
use windows::Win32::UI::Input::Pointer::GetPointerDevices;

static PEN_OBSERVED: AtomicBool = AtomicBool::new(false);

/// Records that a pen sample arrived this run (the `input` stream calls it
/// from its `WM_POINTER` pen path), so presence stays `Available` even when
/// the device list cannot be read.
pub fn note_pen_observed() {
    PEN_OBSERVED.store(true, Ordering::Relaxed);
}

/// Whether a pen sample arrived this run.
#[must_use]
pub fn pen_observed() -> bool {
    PEN_OBSERVED.load(Ordering::Relaxed)
}

/// The macOS rule on Windows facts: a pen seen this run is present; else a
/// listed pen digitizer is; an empty or pen-less list is not; an unreadable
/// list is unknown.
#[must_use]
pub fn presence_from_devices(
    devices: Option<&[POINTER_DEVICE_TYPE]>,
    observed: bool,
) -> CapabilityAvailability {
    if observed {
        return CapabilityAvailability::Available;
    }
    match devices {
        Some(devices)
            if devices.iter().any(|kind| {
                *kind == POINTER_DEVICE_TYPE_INTEGRATED_PEN
                    || *kind == POINTER_DEVICE_TYPE_EXTERNAL_PEN
            }) =>
        {
            CapabilityAvailability::Available
        }
        Some(_) => CapabilityAvailability::Unavailable,
        None => CapabilityAvailability::Unknown,
    }
}

/// The pointer devices Windows lists, by type; `None` when it cannot say.
#[must_use]
pub fn pointer_device_types() -> Option<Vec<POINTER_DEVICE_TYPE>> {
    let mut count = 0_u32;
    // SAFETY: the sizing call passes no output buffer.
    unsafe { GetPointerDevices(&raw mut count, None) }.ok()?;
    if count == 0 {
        return Some(Vec::new());
    }
    let mut devices = vec![POINTER_DEVICE_INFO::default(); usize::try_from(count).ok()?.min(64)];
    let mut filled = u32::try_from(devices.len()).ok()?;
    // SAFETY: `devices` is writable for `filled` records.
    unsafe { GetPointerDevices(&raw mut filled, Some(devices.as_mut_ptr())) }.ok()?;
    devices.truncate(usize::try_from(filled).ok()?);
    Some(
        devices
            .iter()
            .map(|device| device.pointerDeviceType)
            .collect(),
    )
}

/// Whether a pen digitizer is attached as a HID device (listed by raw
/// input whether or not Windows Ink routes it); `false` when the list cannot
/// be read.
#[must_use]
pub fn digitizer_hid_present() -> bool {
    use windows::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RID_DEVICE_INFO,
        RIDI_DEVICEINFO, RIM_TYPEHID,
    };
    let entry = u32::try_from(std::mem::size_of::<RAWINPUTDEVICELIST>()).unwrap_or(0);
    let mut count = 0_u32;
    // SAFETY: the sizing call passes no output buffer.
    if unsafe { GetRawInputDeviceList(None, &raw mut count, entry) } == u32::MAX || count == 0 {
        return false;
    }
    let mut devices = vec![RAWINPUTDEVICELIST::default(); count.min(256) as usize];
    let mut filled = u32::try_from(devices.len()).unwrap_or(0);
    // SAFETY: `devices` is writable for `filled` entries.
    let read = unsafe { GetRawInputDeviceList(Some(devices.as_mut_ptr()), &raw mut filled, entry) };
    if read == u32::MAX {
        return false;
    }
    devices.truncate(read as usize);
    devices
        .iter()
        .filter(|device| device.dwType == RIM_TYPEHID)
        .any(|device| {
            let mut info = RID_DEVICE_INFO {
                cbSize: u32::try_from(std::mem::size_of::<RID_DEVICE_INFO>()).unwrap_or(0),
                ..RID_DEVICE_INFO::default()
            };
            let mut size = info.cbSize;
            // SAFETY: `info` is a RID_DEVICE_INFO of `size` bytes.
            let got = unsafe {
                GetRawInputDeviceInfoW(
                    device.hDevice,
                    RIDI_DEVICEINFO,
                    Some(std::ptr::from_mut(&mut info).cast()),
                    &raw mut size,
                )
            };
            if got == u32::MAX || info.dwType != RIM_TYPEHID {
                return false;
            }
            // SAFETY: a HID device's info carries the `hid` member.
            let hid = unsafe { info.Anonymous.hid };
            arcen_input::deck_pen::is_pen_digitizer_collection(
                hid.dwVendorId,
                hid.usUsagePage,
                hid.usUsage,
            )
        })
}

/// Whether Tablet support would receive no pen from an attached tablet
/// whose driver turns it into a mouse (Windows Ink off): the shared rule on
/// this PC's facts.
#[must_use]
pub fn tablet_sends_mouse(tablet_support: bool) -> bool {
    let pen_listed = pointer_device_types().is_some_and(|devices| {
        devices.iter().any(|kind| {
            *kind == POINTER_DEVICE_TYPE_INTEGRATED_PEN || *kind == POINTER_DEVICE_TYPE_EXTERNAL_PEN
        })
    });
    arcen_input::deck_pen::tablet_sends_mouse(arcen_input::deck_pen::TabletFacts {
        tablet_support,
        digitizer_present: tablet_support && digitizer_hid_present(),
        pen_pointer_listed: pen_listed,
        pen_observed: pen_observed(),
    })
}

/// Whether a pen tablet is attached or was used this run.
#[must_use]
pub fn tablet_presence() -> CapabilityAvailability {
    presence_from_devices(pointer_device_types().as_deref(), pen_observed())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use windows::Win32::UI::Controls::{POINTER_DEVICE_TYPE_TOUCH, POINTER_DEVICE_TYPE_TOUCH_PAD};

    #[test]
    fn presence_follows_listed_pens_and_observed_samples() {
        assert_eq!(
            presence_from_devices(Some(&[POINTER_DEVICE_TYPE_EXTERNAL_PEN]), false),
            CapabilityAvailability::Available
        );
        assert_eq!(
            presence_from_devices(Some(&[POINTER_DEVICE_TYPE_INTEGRATED_PEN]), false),
            CapabilityAvailability::Available
        );
        assert_eq!(
            presence_from_devices(
                Some(&[POINTER_DEVICE_TYPE_TOUCH, POINTER_DEVICE_TYPE_TOUCH_PAD]),
                false
            ),
            CapabilityAvailability::Unavailable
        );
        assert_eq!(
            presence_from_devices(Some(&[]), false),
            CapabilityAvailability::Unavailable
        );
        assert_eq!(
            presence_from_devices(None, false),
            CapabilityAvailability::Unknown
        );
        assert_eq!(
            presence_from_devices(None, true),
            CapabilityAvailability::Available
        );
    }

    #[test]
    fn this_machine_answers_the_device_list() {
        assert!(pointer_device_types().is_some());
    }
}
