//! Platform-neutral policy, state, and test-device support for Arcen Hard USB
//! bridging.
//!
//! This crate deliberately contains no OS APIs, async runtime, transport,
//! foreign handles, or unsafe code. Product adapters own physical-device and
//! virtual-host-controller I/O; `arcen-protocol` owns serialization.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod descriptor;
mod ledger;
mod policy;
mod state;
mod synthetic;
mod types;

pub use descriptor::{
    DescriptorError, EndpointDescriptor, InterfaceDescriptor, ParsedConfiguration,
    parse_configuration_descriptor,
};
pub use ledger::{InFlightLedger, LedgerError, UrbRecord};
pub use policy::{
    DeviceProfile, DeviceSnapshot, InterfaceRule, PolicyDecision, PolicyDenial, evaluate_profile,
    is_permanently_prohibited_class,
};
pub use state::{AttachmentMachine, AttachmentState, StateError};
pub use synthetic::{
    ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID, ControlResponse, PenSample, PenSwitch, PenSwitches,
    SyntheticTabletDevice,
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
