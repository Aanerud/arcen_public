//! Host-side admission for the quarantined experimental raw-HID path.
//!
//! The raw-HID path forwards one HID device (one macOS `IOHIDDevice`, which
//! is one USB interface) at a time, each with its own report descriptor, and a
//! host turns admitted ones into `/dev/uhid` devices. Admission is decided by
//! what the descriptors say, never by vendor id:
//!
//! * only an interface that itself declares a pen or a vendor-mirrored
//!   digitizer becomes a virtual device, so a tablet's keyboard or touch
//!   interface is never recreated on the host;
//! * the interfaces received so far for the same device identity are
//!   classified together with [`crate::classify_pen_tablet`], so a pen on the
//!   vendor-mirrored digitizer page is admitted only once a standard digitizer
//!   from the same device corroborates it, whichever arrives first.
//!
//! * an admitted interface forwards *every* collection in its descriptor.
//!   Express-key keyboard/keypad collections beside a pen are tablet functions
//!   and are admitted (the Deck can already send any keystroke); a joystick,
//!   gamepad or multi-axis controller collection is not, and refuses the
//!   descriptor outright.
//!
//! Every offer is bounded by the session's device limit, and a malformed
//! descriptor is refused outright.

use crate::{
    CollectionClass, HidDescriptorError, classify_pen_tablet, disqualifying_collection,
    parse_report_descriptor,
};
use std::fmt::{Display, Formatter};

/// One device a host should now create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedRawHidDevice {
    /// Peer-assigned device id.
    pub device_id: u8,
    /// Informational vendor id (used only for naming).
    pub vendor_id: u16,
    /// Informational product id (used only for naming).
    pub product_id: u16,
    /// Verbatim report descriptor to hand to the virtual device.
    pub descriptor: Vec<u8>,
}

/// Why an offered raw-HID device was refused outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawHidRefusal {
    /// The session already holds the maximum number of offered devices.
    DeviceLimit,
    /// The device id is already in use.
    DuplicateDeviceId,
    /// The descriptor is malformed or oversized.
    Malformed(HidDescriptorError),
    /// The descriptor declares no digitizer at all: keyboard, mouse,
    /// gamepad, vendor-only.
    NotADigitizer,
    /// The descriptor also carries a collection that is not a tablet function
    /// and is never forwarded (Generic Desktop joystick, gamepad, multi-axis
    /// controller); the usage is attached.
    NonPenCollection(u16),
}

impl Display for RawHidRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeviceLimit => formatter.write_str("per-session raw-HID device limit reached"),
            Self::DuplicateDeviceId => formatter.write_str("raw-HID device id already in use"),
            Self::Malformed(error) => Display::fmt(error, formatter),
            Self::NotADigitizer => formatter.write_str("HID device declares no digitizer"),
            Self::NonPenCollection(usage) => write!(
                formatter,
                "HID device also carries a joystick, gamepad or multi-axis controller \
                 collection (Generic Desktop usage {usage:#04x})"
            ),
        }
    }
}

impl std::error::Error for RawHidRefusal {}

#[derive(Debug, Clone)]
struct Offered {
    device: AdmittedRawHidDevice,
    pen_bearing: bool,
    admitted: bool,
}

/// Per-session raw-HID admission state.
#[derive(Debug, Clone)]
pub struct RawHidAdmission {
    max_devices: usize,
    offered: Vec<Offered>,
}

impl RawHidAdmission {
    /// Admission bounded to `max_devices` offered devices per session.
    #[must_use]
    pub const fn new(max_devices: usize) -> Self {
        Self {
            max_devices,
            offered: Vec::new(),
        }
    }

    /// Offers one device and returns every device that is admissible now,
    /// which may include an earlier offer this one corroborates.
    ///
    /// An `Ok` with an empty list means the offer is held: either it is not a
    /// pen interface (it may still corroborate another), or it is a
    /// vendor-page pen still waiting for a standard digitizer.
    ///
    /// # Errors
    ///
    /// Returns [`RawHidRefusal`] when the offer can never be admitted.
    pub fn offer(
        &mut self,
        device_id: u8,
        vendor_id: u16,
        product_id: u16,
        descriptor: &[u8],
    ) -> Result<Vec<AdmittedRawHidDevice>, RawHidRefusal> {
        if self
            .offered
            .iter()
            .any(|offered| offered.device.device_id == device_id)
        {
            return Err(RawHidRefusal::DuplicateDeviceId);
        }
        if self.offered.len() >= self.max_devices {
            return Err(RawHidRefusal::DeviceLimit);
        }
        let summary = parse_report_descriptor(descriptor).map_err(RawHidRefusal::Malformed)?;
        let digitizer = summary
            .collections
            .iter()
            .any(|collection| !matches!(collection.class, CollectionClass::Other));
        if !digitizer {
            return Err(RawHidRefusal::NotADigitizer);
        }
        if let Some(usage) =
            disqualifying_collection(descriptor).map_err(RawHidRefusal::Malformed)?
        {
            return Err(RawHidRefusal::NonPenCollection(usage));
        }
        let pen_bearing = summary.collections.iter().any(|collection| {
            matches!(
                collection.class,
                CollectionClass::Pen | CollectionClass::VendorDigitizer
            )
        });
        self.offered.push(Offered {
            device: AdmittedRawHidDevice {
                device_id,
                vendor_id,
                product_id,
                descriptor: descriptor.to_vec(),
            },
            pen_bearing,
            admitted: false,
        });
        Ok(self.newly_admissible(vendor_id, product_id))
    }

    fn newly_admissible(&mut self, vendor_id: u16, product_id: u16) -> Vec<AdmittedRawHidDevice> {
        let same_device = |offered: &Offered| {
            offered.device.vendor_id == vendor_id && offered.device.product_id == product_id
        };
        let tablet = classify_pen_tablet(
            self.offered
                .iter()
                .filter(|offered| same_device(offered))
                .map(|offered| offered.device.descriptor.as_slice()),
        )
        .is_ok();
        if !tablet {
            return Vec::new();
        }
        let mut admitted = Vec::new();
        for offered in &mut self.offered {
            if same_device(offered) && offered.pen_bearing && !offered.admitted {
                offered.admitted = true;
                admitted.push(offered.device.clone());
            }
        }
        admitted
    }

    /// Forgets a removed device. Returns whether it had been admitted.
    pub fn remove(&mut self, device_id: u8) -> bool {
        let Some(index) = self
            .offered
            .iter()
            .position(|offered| offered.device.device_id == device_id)
        else {
            return false;
        };
        self.offered.remove(index).admitted
    }

    /// Whether `device_id` has been admitted.
    #[must_use]
    pub fn is_admitted(&self, device_id: u8) -> bool {
        self.offered
            .iter()
            .any(|offered| offered.device.device_id == device_id && offered.admitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SYNTHETIC_TABLET_REPORT_DESCRIPTOR as PEN;

    /// A vendor-page digitizer collection that names no Digitizer or Pen:
    /// admissible only once a standard digitizer corroborates it.
    const VENDOR_PEN: &[u8] = &[
        0x06, 0x0D, 0xFF, 0x09, 0x00, 0xA1, 0x01, 0x06, 0x00, 0xFF, 0x09, 0x01, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x09, 0x81, 0x02, 0xC0,
    ];
    const TOUCH: &[u8] = &[
        0x05, 0x0D, 0x09, 0x05, 0xA1, 0x01, 0x09, 0x22, 0xA1, 0x02, 0x09, 0x42, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30,
        0x09, 0x31, 0x26, 0xFF, 0x0F, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0xC0, 0xC0,
    ];
    const KEYBOARD: &[u8] = &[
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0,
    ];

    fn ids(admitted: &[AdmittedRawHidDevice]) -> Vec<u8> {
        admitted.iter().map(|device| device.device_id).collect()
    }

    #[test]
    fn a_standard_pen_from_any_vendor_is_admitted_at_once() {
        let mut admission = RawHidAdmission::new(8);
        let admitted = admission.offer(1, 0x1234, 0x0001, PEN).unwrap();
        assert_eq!(ids(&admitted), vec![1]);
        assert_eq!(admitted[0].descriptor, PEN);
        assert!(admission.is_admitted(1));
    }

    #[test]
    fn a_vendor_page_pen_waits_for_a_standard_digitizer_in_either_order() {
        let mut first = RawHidAdmission::new(8);
        assert!(
            first
                .offer(1, 0x1234, 0x0002, VENDOR_PEN)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(&first.offer(2, 0x1234, 0x0002, TOUCH).unwrap()),
            vec![1]
        );
        assert!(
            !first.is_admitted(2),
            "touch is corroboration, not a device"
        );

        let mut second = RawHidAdmission::new(8);
        assert!(second.offer(2, 0x1234, 0x0002, TOUCH).unwrap().is_empty());
        assert_eq!(
            ids(&second.offer(1, 0x1234, 0x0002, VENDOR_PEN).unwrap()),
            vec![1]
        );
    }

    #[test]
    fn an_intuos5_style_pen_interface_is_admitted_and_its_other_interfaces_are_not() {
        use crate::descriptor_fixtures::{
            BOOT_KEYBOARD, INTUOS5_STYLE_MOUSE_INTERFACE, INTUOS5_STYLE_PEN_INTERFACE,
            INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE,
        };
        let mut admission = RawHidAdmission::new(8);
        let admitted = admission
            .offer(4, 0x056a, 0x0317, INTUOS5_STYLE_PEN_INTERFACE)
            .unwrap();
        assert_eq!(ids(&admitted), vec![4]);
        assert_eq!(
            admission.offer(5, 0x056a, 0x0317, INTUOS5_STYLE_VENDOR_CONTROL_INTERFACE),
            Err(RawHidRefusal::NotADigitizer)
        );
        assert_eq!(
            admission.offer(6, 0x056a, 0x0317, INTUOS5_STYLE_MOUSE_INTERFACE),
            Err(RawHidRefusal::NotADigitizer)
        );

        // Express keys sharing the pen's descriptor are part of the tablet.
        let mut combined = INTUOS5_STYLE_PEN_INTERFACE.to_vec();
        combined.extend_from_slice(BOOT_KEYBOARD);
        let mut other = RawHidAdmission::new(8);
        assert_eq!(ids(&other.offer(1, 0x1234, 1, &combined).unwrap()), vec![1]);
    }

    /// Express keys commonly share the pen's descriptor as a keyboard
    /// collection: admitted with the pen. A game controller is not a tablet
    /// function and refuses the descriptor.
    #[test]
    fn express_key_keyboards_are_admitted_and_game_controllers_are_not() {
        use crate::descriptor_fixtures::{BOOT_KEYBOARD, GAMEPAD, TOUCH_PAD_ONLY};
        let mut combined = PEN.to_vec();
        combined.extend_from_slice(BOOT_KEYBOARD);
        let mut admission = RawHidAdmission::new(8);
        assert_eq!(
            ids(&admission.offer(1, 0x1234, 1, &combined).unwrap()),
            vec![1]
        );
        let mut with_gamepad = PEN.to_vec();
        with_gamepad.extend_from_slice(GAMEPAD);
        assert_eq!(
            admission.offer(3, 0x1234, 1, &with_gamepad),
            Err(RawHidRefusal::NonPenCollection(0x05))
        );
        // A keyboard alone is not a digitizer; a keyboard with a touch pad
        // has no pen, so it is held and never admitted.
        let mut spoof = RawHidAdmission::new(8);
        assert_eq!(
            spoof.offer(1, 0x9999, 1, BOOT_KEYBOARD),
            Err(RawHidRefusal::NotADigitizer)
        );
        let mut keyboard_touch = BOOT_KEYBOARD.to_vec();
        keyboard_touch.extend_from_slice(TOUCH_PAD_ONLY);
        assert!(
            spoof
                .offer(2, 0x9999, 1, &keyboard_touch)
                .unwrap()
                .is_empty()
        );
        assert!(!spoof.is_admitted(2));
        // A pen with a mouse collection (tablet mouse mode) is still fine.
        let mut with_mouse = PEN.to_vec();
        with_mouse.extend_from_slice(crate::descriptor_fixtures::INTUOS5_STYLE_MOUSE_INTERFACE);
        assert_eq!(
            ids(&admission.offer(2, 0x1234, 1, &with_mouse).unwrap()),
            vec![2]
        );
    }

    /// The reviewed bypass for raw HID: a gamepad application nested inside
    /// a pen application, and a keyboard wrapped in a vendor "pen".
    #[test]
    fn nested_game_controllers_and_wrapped_keyboards_are_refused() {
        use crate::descriptor_fixtures::{BOOT_KEYBOARD, GAMEPAD};
        let mut pen_with_gamepad = PEN[..PEN.len() - 1].to_vec();
        pen_with_gamepad.extend_from_slice(GAMEPAD);
        pen_with_gamepad.push(0xC0);
        let mut admission = RawHidAdmission::new(8);
        assert_eq!(
            admission.offer(1, 0x1234, 1, &pen_with_gamepad),
            Err(RawHidRefusal::NonPenCollection(0x05))
        );
        let mut wrapped_keyboard = vec![0x06, 0x0D, 0xFF, 0x09, 0x02, 0xA1, 0x01];
        wrapped_keyboard.extend_from_slice(BOOT_KEYBOARD);
        wrapped_keyboard.push(0xC0);
        assert_eq!(
            admission.offer(2, 0x1234, 2, &wrapped_keyboard),
            Err(RawHidRefusal::NotADigitizer)
        );
        let mut wrapped_gamepad = vec![0x06, 0x0D, 0xFF, 0x09, 0x02, 0xA1, 0x01];
        wrapped_gamepad.extend_from_slice(GAMEPAD);
        wrapped_gamepad.push(0xC0);
        assert!(admission.offer(3, 0x1234, 3, &wrapped_gamepad).is_err());
        assert!(
            !admission.is_admitted(1) && !admission.is_admitted(2) && !admission.is_admitted(3)
        );
    }

    #[test]
    fn corroboration_does_not_cross_device_identities() {
        let mut admission = RawHidAdmission::new(8);
        assert!(
            admission
                .offer(1, 0x1234, 0x0002, VENDOR_PEN)
                .unwrap()
                .is_empty()
        );
        assert!(
            admission
                .offer(2, 0x9999, 0x0002, TOUCH)
                .unwrap()
                .is_empty()
        );
        assert!(!admission.is_admitted(1));
    }

    #[test]
    fn non_digitizers_malformed_duplicates_and_overflow_are_refused() {
        let mut admission = RawHidAdmission::new(2);
        assert_eq!(
            admission.offer(1, 0x1234, 1, KEYBOARD),
            Err(RawHidRefusal::NotADigitizer)
        );
        assert!(matches!(
            admission.offer(1, 0x1234, 1, &[0xC0]),
            Err(RawHidRefusal::Malformed(_))
        ));
        admission.offer(1, 0x1234, 1, PEN).unwrap();
        assert_eq!(
            admission.offer(1, 0x1234, 1, PEN),
            Err(RawHidRefusal::DuplicateDeviceId)
        );
        admission.offer(2, 0x1234, 1, TOUCH).unwrap();
        assert_eq!(
            admission.offer(3, 0x1234, 1, PEN),
            Err(RawHidRefusal::DeviceLimit)
        );
        assert!(admission.remove(1));
        assert!(!admission.remove(1));
        assert_eq!(ids(&admission.offer(3, 0x1234, 1, PEN).unwrap()), vec![3]);
    }
}
