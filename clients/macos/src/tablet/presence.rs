//! Vendor-neutral pen-tablet presence for this Mac.
//!
//! **Typed pen input (Tablet support) does not depend on presence.** Its
//! source is AppKit's tablet events, which every tablet driver produces and
//! which carry no vendor gate, so the Deck advertises it whenever typed pen
//! capture is possible and enabled ([`typed_pen_capability`]). A tablet
//! plugged in after connecting, or one whose descriptors the registry does
//! not publish, therefore still gets pressure without a reconnect.
//!
//! Presence answers a narrower question — is a pen tablet attached? — for the
//! Tablet Monitor and diagnostics. Two independent signals answer it, and
//! neither one is a vendor id:
//!
//! 1. **What the HID devices say they are.** Every HID device macOS knows
//!    about, USB or Bluetooth, publishes its report descriptor in the I/O
//!    Registry. A device whose descriptor declares a pen (the Digitizers page
//!    pen collection, or a vendor-mirrored digitizer page) is a pen tablet;
//!    the classification is shared code in `arcen_usb_bridge`. Reading the
//!    registry needs no permission and opens nothing.
//! 2. **What AppKit has already delivered.** Once a pen or eraser sample has
//!    arrived this run, a tablet is demonstrably present, whatever its
//!    descriptor looks like.
//!
//! Presence only decides whether to *offer* typed pen input; the operating
//! system's tablet driver still produces every sample. Bridging a device to a
//! host (Hard USB) uses the stricter `arcen_usb_bridge::classify_pen_tablet`,
//! which is evaluated again by the privileged helper and by the host.
#![forbid(unsafe_code)]

use arcen_input::CapabilityAvailability;
use std::sync::atomic::{AtomicBool, Ordering};

static PEN_OBSERVED: AtomicBool = AtomicBool::new(false);

/// Records that AppKit delivered a pen or eraser sample this run.
pub fn note_pen_observed() {
    PEN_OBSERVED.store(true, Ordering::Relaxed);
}

/// Whether AppKit has delivered a pen or eraser sample this run.
#[must_use]
pub fn pen_observed() -> bool {
    PEN_OBSERVED.load(Ordering::Relaxed)
}

/// One HID device as the I/O Registry describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidDeviceFacts {
    /// Informational USB/Bluetooth vendor id (0 when not published).
    pub vendor_id: u16,
    /// Informational product id (0 when not published).
    pub product_id: u16,
    /// Verbatim HID report descriptor.
    pub report_descriptor: Vec<u8>,
}

/// Decides presence from an inventory read and the AppKit signal.
///
/// `Available` when any device declares a pen or vendor digitizer, or a pen
/// sample was already observed; `Unavailable` when the inventory was read and
/// none does; `Unknown` when the inventory could not be read at all, so a
/// registry failure is never reported as "no tablet".
#[must_use]
pub fn presence_from_inventory(
    inventory: Option<&[HidDeviceFacts]>,
    observed: bool,
) -> CapabilityAvailability {
    if observed {
        return CapabilityAvailability::Available;
    }
    match inventory {
        Some(devices)
            if devices.iter().any(|device| {
                arcen_usb_bridge::declares_pen_or_vendor_digitizer(&device.report_descriptor)
            }) =>
        {
            CapabilityAvailability::Available
        }
        Some(_) => CapabilityAvailability::Unavailable,
        None => CapabilityAvailability::Unknown,
    }
}

/// Groups an inventory by device identity and returns every identity whose
/// descriptors classify as a pen tablet under the strict shared rules.
#[must_use]
pub fn pen_tablets_in(inventory: &[HidDeviceFacts]) -> Vec<(u16, u16)> {
    let mut identities: Vec<(u16, u16)> = inventory
        .iter()
        .map(|device| (device.vendor_id, device.product_id))
        .collect();
    identities.sort_unstable();
    identities.dedup();
    identities
        .into_iter()
        .filter(|&(vendor_id, product_id)| {
            arcen_usb_bridge::classify_pen_tablet(
                inventory
                    .iter()
                    .filter(|device| {
                        device.vendor_id == vendor_id && device.product_id == product_id
                    })
                    .map(|device| device.report_descriptor.as_slice()),
            )
            .is_ok()
        })
        .collect()
}

/// Whether typed pen input (local termination) can be offered: the user left
/// it enabled and this build captures AppKit tablet events (macOS).
///
/// Deliberately independent of [`tablet_presence`]. Advertising it with no
/// tablet attached costs nothing — no pen sample is sent until AppKit
/// delivers one — while tying it to presence left a tablet that was
/// connected late, or that the registry does not describe, without pressure
/// until the next reconnect, because the hello cannot be renegotiated
/// mid-session.
#[must_use]
pub const fn typed_pen_capability(enabled: bool) -> CapabilityAvailability {
    if enabled && cfg!(target_os = "macos") {
        CapabilityAvailability::Available
    } else {
        CapabilityAvailability::Unavailable
    }
}

/// Whether a pen tablet of any make is attached to this Mac, or has already
/// been used this run. Informational (Tablet Monitor, diagnostics); it does
/// not gate typed pen input.
#[must_use]
pub fn tablet_presence() -> CapabilityAvailability {
    let inventory = super::hid_inventory::hid_devices();
    presence_from_inventory(inventory.as_deref(), pen_observed())
}

/// Identities of attached devices that classify as pen tablets.
#[must_use]
pub fn attached_pen_tablets() -> Vec<(u16, u16)> {
    super::hid_inventory::hid_devices()
        .map(|inventory| pen_tablets_in(&inventory))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEN: &[u8] = arcen_usb_bridge::SYNTHETIC_TABLET_REPORT_DESCRIPTOR;
    const KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0,
    ];
    const TRACKPAD: &[u8] = &[
        0x05, 0x0D, 0x09, 0x05, 0xA1, 0x01, 0x09, 0x22, 0xA1, 0x02, 0x09, 0x42, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30,
        0x09, 0x31, 0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0, 0xC0,
    ];

    fn device(vendor_id: u16, product_id: u16, descriptor: &[u8]) -> HidDeviceFacts {
        HidDeviceFacts {
            vendor_id,
            product_id,
            report_descriptor: descriptor.to_vec(),
        }
    }

    #[test]
    fn any_vendors_pen_makes_a_tablet_present() {
        for vendor_id in [0x056a, 0x256c, 0x28bd, 0x0b57, 0x1234, 0] {
            let inventory = [
                device(0x05ac, 0x0001, KEYBOARD),
                device(vendor_id, 0x0042, PEN),
            ];
            assert_eq!(
                presence_from_inventory(Some(&inventory), false),
                CapabilityAvailability::Available,
                "vendor {vendor_id:#06x}"
            );
        }
    }

    #[test]
    fn keyboards_and_trackpads_are_not_tablets() {
        let inventory = [
            device(0x05ac, 0x0001, KEYBOARD),
            device(0x05ac, 0x0002, TRACKPAD),
            device(0x056a, 0x0003, KEYBOARD),
        ];
        assert_eq!(
            presence_from_inventory(Some(&inventory), false),
            CapabilityAvailability::Unavailable
        );
        assert_eq!(
            presence_from_inventory(Some(&[]), false),
            CapabilityAvailability::Unavailable
        );
    }

    #[test]
    fn an_unreadable_registry_is_unknown_not_absent() {
        assert_eq!(
            presence_from_inventory(None, false),
            CapabilityAvailability::Unknown
        );
    }

    #[test]
    fn an_observed_pen_is_present_whatever_the_inventory_says() {
        assert_eq!(
            presence_from_inventory(None, true),
            CapabilityAvailability::Available
        );
        assert_eq!(
            presence_from_inventory(Some(&[device(1, 1, KEYBOARD)]), true),
            CapabilityAvailability::Available
        );
    }

    /// The reviewed regression: with no descriptor in the registry and no pen
    /// sample yet, presence is `Unavailable`; typed pen must still be offered
    /// so that the first pen sample has a negotiated path.
    #[test]
    fn typed_pen_is_offered_without_any_presence_evidence() {
        assert_eq!(
            presence_from_inventory(Some(&[]), false),
            CapabilityAvailability::Unavailable
        );
        let expected = if cfg!(target_os = "macos") {
            CapabilityAvailability::Available
        } else {
            CapabilityAvailability::Unavailable
        };
        assert_eq!(typed_pen_capability(true), expected);
        assert_eq!(
            typed_pen_capability(false),
            CapabilityAvailability::Unavailable
        );
    }

    #[test]
    fn strict_classification_groups_interfaces_by_device() {
        let inventory = [
            device(0x1234, 0x0001, PEN),
            device(0x1234, 0x0001, KEYBOARD),
            device(0x05ac, 0x0002, TRACKPAD),
            device(0x9999, 0x0003, KEYBOARD),
        ];
        assert_eq!(pen_tablets_in(&inventory), vec![(0x1234, 0x0001)]);
    }
}
