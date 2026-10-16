//! Host-side authorization of a Hard USB pen tablet and runtime proof that the
//! bridged device keeps telling the truth about itself.
//!
//! A Pier decides whether to create a virtual USB device from facts the Deck
//! supplied: the device class and the HID report descriptor of every HID
//! interface. [`authorize_hard_usb_tablet`] classifies those claims with
//! [`crate::classify_pen_tablet`] before anything reaches the host kernel.
//!
//! Bridging forwards *every* collection of *every* interface, which is how a
//! tablet's express keys (keyboard or keypad collections, boot-keyboard
//! interfaces), touch strips and mouse mode reach the host's own driver.
//! Those are allowed: the authenticated Deck can already send any keystroke
//! through the ordinary input channel, so forwarding a composite tablet's
//! keyboard grants it nothing new. What is enforced: the device must classify
//! as a pen tablet (a plain keyboard never does); no interface may carry a
//! joystick, gamepad or multi-axis controller collection, which are not
//! tablet functions ([`HardUsbTabletDenial::ForwardsNonPenInput`]); the
//! claimed descriptors together must fit the helper handshake
//! ([`MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES`]); and, at runtime, every
//! interface must be HID with interrupt endpoints only and bound to these
//! claims ([`crate::BridgedDeviceGuard`]).
//!
//! The claims are only claims, so the host also checks them against what the
//! device actually returns: [`crate::BridgedDeviceGuard`] validates every
//! device, configuration and report descriptor the device serves before the
//! host kernel sees it.

use crate::{
    MAX_HID_REPORT_DESCRIPTOR_BYTES, MAX_INTERFACES, NotPenTablet, PenTabletProfile,
    classify_pen_tablet, disqualifying_collection,
};
use std::fmt::{Display, Formatter};

/// Most report-descriptor bytes, summed over all interfaces, that one bridged
/// device may claim.
///
/// The macOS USB helper hands the descriptors to the Deck in one handshake
/// frame whose bound (`MAX_TRANSFER_BYTES + 1024` = 17,408 bytes) older Decks
/// also enforce, so the frame is not raised; 12 KiB plus three bytes of
/// framing per interface and the nine-byte identity leaves more than 5 KiB of
/// headroom. Real tablets publish a few hundred bytes per interface.
pub const MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES: usize = 12 * 1024;

/// One HID interface's report descriptor, as claimed for a bridged device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceReportDescriptor {
    /// USB interface number.
    pub interface: u8,
    /// Verbatim HID report descriptor bytes.
    pub descriptor: Vec<u8>,
}

/// Why a Hard USB device was not authorized as a pen tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardUsbTabletDenial {
    /// The device-level class is neither per-interface (`0x00`) nor HID.
    DeviceClass(u8),
    /// The Deck supplied no report descriptors (a Deck older than 0.16.1, or
    /// a device it could not read).
    NoDescriptors,
    /// More descriptors than the bridge accepts interfaces.
    TooManyDescriptors,
    /// Two descriptors claim the same interface.
    DuplicateInterface(u8),
    /// A descriptor exceeds [`MAX_HID_REPORT_DESCRIPTOR_BYTES`].
    DescriptorTooLarge(u8),
    /// The descriptors together exceed [`MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES`].
    DescriptorsTooLarge {
        /// Sum of all claimed descriptor lengths.
        total: usize,
    },
    /// The descriptors do not describe a pen tablet.
    NotPenTablet(NotPenTablet),
    /// An interface carries a collection that is not a tablet function and is
    /// never forwarded with one (Generic Desktop joystick, gamepad or
    /// multi-axis controller).
    ForwardsNonPenInput {
        /// Interface whose descriptor carries it.
        interface: u8,
        /// Generic Desktop usage of the collection.
        usage: u16,
    },
}

impl Display for HardUsbTabletDenial {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceClass(class) => write!(
                formatter,
                "device class {class:#04x} is not a HID or per-interface device"
            ),
            Self::NoDescriptors => formatter.write_str(
                "the Deck supplied no HID report descriptors; Deck 0.16.1 or newer is required",
            ),
            Self::TooManyDescriptors => {
                formatter.write_str("more HID report descriptors than the bridge accepts")
            }
            Self::DuplicateInterface(interface) => write!(
                formatter,
                "interface {interface} has more than one report descriptor"
            ),
            Self::DescriptorTooLarge(interface) => write!(
                formatter,
                "interface {interface} report descriptor exceeds {MAX_HID_REPORT_DESCRIPTOR_BYTES} bytes"
            ),
            Self::DescriptorsTooLarge { total } => write!(
                formatter,
                "the report descriptors total {total} bytes; at most \
                 {MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES} can be bridged"
            ),
            Self::NotPenTablet(reason) => write!(formatter, "not a pen tablet: {reason}"),
            Self::ForwardsNonPenInput { interface, usage } => write!(
                formatter,
                "interface {interface} carries a joystick, gamepad or multi-axis controller \
                 collection (Generic Desktop usage {usage:#04x}), which is not a tablet \
                 function and is never bridged; use Tablet support for this device"
            ),
        }
    }
}

impl std::error::Error for HardUsbTabletDenial {}

/// Decides, from claimed facts alone, whether a device may be bridged as a
/// pen tablet. Vendor and product ids play no part.
///
/// # Errors
///
/// Returns [`HardUsbTabletDenial`] when the device class, descriptor set, or
/// classification fails. Every failure is final; nothing is partially
/// admitted.
pub fn authorize_hard_usb_tablet(
    device_class: u8,
    descriptors: &[InterfaceReportDescriptor],
) -> Result<PenTabletProfile, HardUsbTabletDenial> {
    if device_class != 0x00 && device_class != 0x03 {
        return Err(HardUsbTabletDenial::DeviceClass(device_class));
    }
    if descriptors.is_empty() {
        return Err(HardUsbTabletDenial::NoDescriptors);
    }
    if descriptors.len() > MAX_INTERFACES {
        return Err(HardUsbTabletDenial::TooManyDescriptors);
    }
    let total = descriptors
        .iter()
        .map(|entry| entry.descriptor.len())
        .fold(0_usize, usize::saturating_add);
    if total > MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES {
        return Err(HardUsbTabletDenial::DescriptorsTooLarge { total });
    }
    for (index, entry) in descriptors.iter().enumerate() {
        if entry.descriptor.len() > MAX_HID_REPORT_DESCRIPTOR_BYTES {
            return Err(HardUsbTabletDenial::DescriptorTooLarge(entry.interface));
        }
        if descriptors[..index]
            .iter()
            .any(|earlier| earlier.interface == entry.interface)
        {
            return Err(HardUsbTabletDenial::DuplicateInterface(entry.interface));
        }
    }
    let profile = classify_pen_tablet(descriptors.iter().map(|entry| entry.descriptor.as_slice()))
        .map_err(HardUsbTabletDenial::NotPenTablet)?;
    // Everything below is forwarded verbatim. Express keys (keyboard/keypad)
    // and mouse mode are tablet functions and pass; a game controller is not.
    for entry in descriptors {
        if let Some(usage) = disqualifying_collection(&entry.descriptor)
            .map_err(|error| HardUsbTabletDenial::NotPenTablet(NotPenTablet::Malformed(error)))?
        {
            return Err(HardUsbTabletDenial::ForwardsNonPenInput {
                interface: entry.interface,
                usage,
            });
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SYNTHETIC_TABLET_REPORT_DESCRIPTOR;
    use crate::descriptor_fixtures::{
        BOOT_KEYBOARD, GAMEPAD, INTUOS5_STYLE_MOUSE_INTERFACE, INTUOS5_STYLE_PEN_INTERFACE,
        INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE, TOUCH_PAD_ONLY,
    };

    fn claim(interface: u8, descriptor: &[u8]) -> InterfaceReportDescriptor {
        InterfaceReportDescriptor {
            interface,
            descriptor: descriptor.to_vec(),
        }
    }

    #[test]
    fn a_classified_pen_is_authorized_whatever_its_vendor() {
        let profile =
            authorize_hard_usb_tablet(0x00, &[claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR)])
                .unwrap();
        assert_eq!(profile.capabilities.pressure_levels(), Some(1024));
    }

    #[test]
    fn a_legacy_deck_without_descriptors_is_refused() {
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[]),
            Err(HardUsbTabletDenial::NoDescriptors)
        );
    }

    #[test]
    fn non_hid_device_classes_are_refused_before_classification() {
        for class in [0x08_u8, 0x09, 0xEF, 0xFF] {
            assert_eq!(
                authorize_hard_usb_tablet(class, &[claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR)]),
                Err(HardUsbTabletDenial::DeviceClass(class))
            );
        }
    }

    #[test]
    fn duplicate_and_oversized_claims_are_refused() {
        assert_eq!(
            authorize_hard_usb_tablet(
                0x00,
                &[
                    claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
                    claim(0, INTUOS5_STYLE_MOUSE_INTERFACE)
                ]
            ),
            Err(HardUsbTabletDenial::DuplicateInterface(0))
        );
        let oversized = vec![0_u8; MAX_HID_REPORT_DESCRIPTOR_BYTES + 1];
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(1, &oversized)]),
            Err(HardUsbTabletDenial::DescriptorTooLarge(1))
        );
        let many: Vec<_> = (0..=u8::try_from(MAX_INTERFACES).unwrap())
            .map(|interface| claim(interface, SYNTHETIC_TABLET_REPORT_DESCRIPTOR))
            .collect();
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &many),
            Err(HardUsbTabletDenial::TooManyDescriptors)
        );
    }

    /// Five individually valid, large descriptors overflowed the helper
    /// handshake frame after the device had already been seized.
    #[test]
    fn the_aggregate_descriptor_size_is_bounded_to_fit_the_handshake() {
        let mut large = SYNTHETIC_TABLET_REPORT_DESCRIPTOR.to_vec();
        large.resize(4000, 0x00); // trailing zero bytes are short Main items
        let five: Vec<_> = (0..5).map(|interface| claim(interface, &large)).collect();
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &five),
            Err(HardUsbTabletDenial::DescriptorsTooLarge { total: 20_000 })
        );
        let three: Vec<_> = (0..3).map(|interface| claim(interface, &large)).collect();
        assert!(authorize_hard_usb_tablet(0x00, &three).is_ok());
    }

    #[test]
    fn a_keyboard_is_refused() {
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, BOOT_KEYBOARD)]),
            Err(HardUsbTabletDenial::NotPenTablet(NotPenTablet::NoDigitizer))
        );
    }

    /// Express keys commonly enumerate as a keyboard interface, or as a
    /// keyboard collection in the pen's own descriptor. They are tablet
    /// functions and are bridged with it.
    #[test]
    fn a_pen_with_an_express_key_keyboard_is_bridged() {
        assert!(
            authorize_hard_usb_tablet(
                0x00,
                &[
                    claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
                    claim(1, BOOT_KEYBOARD)
                ]
            )
            .is_ok()
        );
        let mut combined = SYNTHETIC_TABLET_REPORT_DESCRIPTOR.to_vec();
        combined.extend_from_slice(BOOT_KEYBOARD);
        assert!(authorize_hard_usb_tablet(0x00, &[claim(0, &combined)]).is_ok());
    }

    #[test]
    fn a_keyboard_without_a_pen_is_never_bridged() {
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, BOOT_KEYBOARD), claim(1, BOOT_KEYBOARD)]),
            Err(HardUsbTabletDenial::NotPenTablet(NotPenTablet::NoDigitizer))
        );
        // A keyboard that spoofs a digitizer with a touch pad is still no pen.
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, BOOT_KEYBOARD), claim(1, TOUCH_PAD_ONLY)]),
            Err(HardUsbTabletDenial::NotPenTablet(NotPenTablet::TouchOnly))
        );
        let mut combined = BOOT_KEYBOARD.to_vec();
        combined.extend_from_slice(TOUCH_PAD_ONLY);
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, &combined)]),
            Err(HardUsbTabletDenial::NotPenTablet(NotPenTablet::TouchOnly))
        );
    }

    #[test]
    fn a_pen_beside_a_game_controller_is_never_bridged() {
        assert_eq!(
            authorize_hard_usb_tablet(
                0x00,
                &[
                    claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
                    claim(1, GAMEPAD)
                ]
            ),
            Err(HardUsbTabletDenial::ForwardsNonPenInput {
                interface: 1,
                usage: 0x05
            })
        );
        let mut combined = SYNTHETIC_TABLET_REPORT_DESCRIPTOR.to_vec();
        combined.extend_from_slice(GAMEPAD);
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, &combined)]),
            Err(HardUsbTabletDenial::ForwardsNonPenInput {
                interface: 0,
                usage: 0x05
            })
        );
    }

    /// The reviewed bypass: a gamepad application nested inside a pen
    /// application on the same interface.
    #[test]
    fn a_gamepad_nested_inside_a_pen_interface_is_never_bridged() {
        let mut pen = SYNTHETIC_TABLET_REPORT_DESCRIPTOR
            [..SYNTHETIC_TABLET_REPORT_DESCRIPTOR.len() - 1]
            .to_vec();
        pen.extend_from_slice(GAMEPAD);
        pen.push(0xC0);
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, &pen)]),
            Err(HardUsbTabletDenial::ForwardsNonPenInput {
                interface: 0,
                usage: 0x05
            })
        );
        let mut vendor_wrapped = vec![0x06, 0x0D, 0xFF, 0x09, 0x02, 0xA1, 0x01];
        vendor_wrapped.extend_from_slice(GAMEPAD);
        vendor_wrapped.push(0xC0);
        assert_eq!(
            authorize_hard_usb_tablet(
                0x00,
                &[
                    claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
                    claim(1, &vendor_wrapped)
                ]
            ),
            Err(HardUsbTabletDenial::ForwardsNonPenInput {
                interface: 1,
                usage: 0x05
            })
        );
    }

    #[test]
    fn a_keyboard_wrapped_in_a_vendor_pen_application_is_never_bridged() {
        let mut wrapped = vec![0x06, 0x0D, 0xFF, 0x09, 0x02, 0xA1, 0x01];
        wrapped.extend_from_slice(BOOT_KEYBOARD);
        wrapped.push(0xC0);
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, &wrapped)]),
            Err(HardUsbTabletDenial::NotPenTablet(NotPenTablet::NoDigitizer))
        );
    }

    #[test]
    fn a_tablets_boot_mouse_interface_is_still_allowed() {
        assert!(
            authorize_hard_usb_tablet(
                0x00,
                &[
                    claim(0, SYNTHETIC_TABLET_REPORT_DESCRIPTOR),
                    claim(1, INTUOS5_STYLE_MOUSE_INTERFACE)
                ]
            )
            .is_ok()
        );
    }

    #[test]
    fn an_intuos5_style_tablet_is_authorized_and_a_lone_vendor_device_is_not() {
        let profile = authorize_hard_usb_tablet(
            0x00,
            &[
                claim(0, INTUOS5_STYLE_PEN_INTERFACE),
                claim(1, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE),
                claim(2, INTUOS5_STYLE_MOUSE_INTERFACE),
            ],
        )
        .unwrap();
        assert_eq!(
            profile.evidence,
            crate::PenTabletEvidence::VendorDigitizerPen
        );
        assert_eq!(profile.capabilities.pressure_levels(), None);
        assert_eq!(
            authorize_hard_usb_tablet(0x00, &[claim(0, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE)]),
            Err(HardUsbTabletDenial::NotPenTablet(NotPenTablet::NoDigitizer))
        );
        assert!(
            authorize_hard_usb_tablet(
                0x00,
                &[
                    claim(0, INTUOS5_STYLE_PEN_INTERFACE),
                    claim(1, BOOT_KEYBOARD)
                ]
            )
            .is_ok(),
            "an opaque vendor-page pen with express keys is a tablet like any other"
        );
        assert_eq!(
            authorize_hard_usb_tablet(
                0x00,
                &[claim(0, INTUOS5_STYLE_PEN_INTERFACE), claim(1, GAMEPAD)]
            ),
            Err(HardUsbTabletDenial::NotPenTablet(
                NotPenTablet::DisqualifyingCollection
            ))
        );
    }
}
