//! Platform-neutral policy, state, and test-device support for Arcen Hard USB
//! bridging, including the vendor-neutral pen-tablet classifier that decides
//! which devices may be bridged at all.
//!
//! This crate deliberately contains no OS APIs, async runtime, transport,
//! foreign handles, or unsafe code. Product adapters own physical-device and
//! virtual-host-controller I/O; `arcen-protocol` owns serialization.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod bridged_device_guard;
mod descriptor;
pub mod descriptor_fixtures;
mod hid_digitizer;
mod ledger;
mod policy;
mod raw_hid;
mod session_seat;
mod state;
mod synthetic;
mod tablet_claim;
mod types;

pub use bridged_device_guard::{
    BridgedDeviceGuard, BridgedDeviceViolation, ConfigurationViolation, DeviceDescriptorViolation,
    validate_configuration_descriptor, validate_device_descriptor, validate_device_qualifier,
};
pub use descriptor::{
    DescriptorError, EndpointDescriptor, InterfaceDescriptor, ParsedConfiguration,
    parse_configuration_descriptor,
};
pub use hid_digitizer::{
    AxisRange, CollectionClass, DESCRIPTOR_HID, DESCRIPTOR_HID_REPORT, HidClassDescriptorError,
    HidDescriptorError, MAX_COLLECTION_DEPTH, MAX_GLOBAL_STACK_DEPTH,
    MAX_HID_REPORT_DESCRIPTOR_BYTES, MAX_HID_SUBORDINATE_DESCRIPTORS, MAX_LOCAL_USAGES,
    MAX_TOP_LEVEL_COLLECTIONS, NotPenTablet, PenCapabilities, PenTabletEvidence, PenTabletProfile,
    ReportDescriptorSummary, TopLevelCollection, USAGE_PAGE_BUTTON, USAGE_PAGE_DIGITIZER,
    USAGE_PAGE_GENERIC_DESKTOP, USAGE_PAGE_VENDOR_DEFINED_MIN, USAGE_PAGE_VENDOR_DIGITIZER,
    classify_pen_tablet, declares_any_digitizer, declares_pen_or_vendor_digitizer,
    disqualifying_collection, hid_report_descriptor_length, hid_report_descriptor_request,
    is_boot_keyboard_interface, parse_hid_class_descriptor, parse_report_descriptor,
    requested_report_descriptor_interface,
};
pub use ledger::{InFlightLedger, LedgerError, UrbRecord};
pub use policy::{
    DeviceProfile, DeviceSnapshot, InterfaceRule, PolicyDecision, PolicyDenial, evaluate_profile,
    is_permanently_prohibited_class,
};
pub use raw_hid::{AdmittedRawHidDevice, RawHidAdmission, RawHidRefusal};
pub use session_seat::{HardUsbSessionDenial, HardUsbSessionSeat, admit_hard_usb_session};
pub use state::{AttachmentMachine, AttachmentState, StateError};
pub use synthetic::{
    ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID, ControlResponse, PenSample, PenSwitch, PenSwitches,
    SYNTHETIC_TABLET_REPORT_DESCRIPTOR, SyntheticTabletDevice,
};
pub use tablet_claim::{
    HardUsbTabletDenial, InterfaceReportDescriptor, MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES,
    authorize_hard_usb_tablet,
};
pub use types::{
    AlternateSetting, AttachmentGeneration, EndpointAddress, InterfaceNumber, SetupPacket,
    TransferDirection, TransferKind, UrbId, UrbStatus, UsbDeviceId, UsbSpeed,
};

/// Maximum bytes accepted in one configuration descriptor snapshot.
pub const MAX_CONFIGURATION_DESCRIPTOR_BYTES: usize = 4 * 1024;
/// Maximum interfaces accepted for one bridged device.
pub const MAX_INTERFACES: usize = 16;
/// Maximum endpoints accepted across one bridged device.
pub const MAX_ENDPOINTS: usize = 32;
/// Maximum payload accepted for one v1 control/interrupt transfer.
pub const MAX_TRANSFER_BYTES: usize = 16 * 1024;
/// Maximum simultaneously outstanding URBs for one attachment.
pub const MAX_IN_FLIGHT_URBS: usize = 128;
