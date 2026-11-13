// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Platform-neutral policy, state, and test-device support for Arcen Hard USB
//! bridging, including the vendor-neutral pen-tablet classifier that decides
//! which devices may be bridged at all.
//!
//! This crate deliberately contains no OS APIs, async runtime, transport,
//! foreign handles, or unsafe code. Product adapters own physical-device and
//! virtual-host-controller I/O; `arcen-protocol` owns serialization.
//!
//! The exporter side of a bridge — URB admission, cancellation, locally
//! answered standard requests, status mapping and release accounting — is the
//! portable [`UsbExporter`]; a platform supplies only a [`CapturedUsbDevice`]
//! (ADR 0019).

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod backend;
pub mod bridge_seat;
mod bridged_device_guard;
mod descriptor;
pub mod descriptor_fixtures;
mod exporter;
pub mod hid_device_ipc;
mod hid_digitizer;
mod hid_host;
mod hid_publish;
mod kernel_package;
mod ledger;
mod policy;
mod raw_hid;
mod session_seat;
mod state;
mod state_replay;
mod synthetic;
mod synthetic_device;
mod tablet_claim;
mod types;
mod usbip;
mod wacom_intuos5;
mod wacom_intuos_pen;

pub use backend::{
    BackendRefusal, BackendStatus, CapturedDeviceFacts, CapturedUsbDevice, CompletionSink,
    ControlOperation, InterruptPoll, PendingCompletion, ReleaseOutcome, ReleaseReason,
    hold_until_report,
};
pub use bridge_seat::{
    BridgeSeat, BridgeSeatError, BridgedInputNode, BridgedNodeRole, MAX_BRIDGED_INPUT_NODES,
};
pub use bridged_device_guard::{
    BridgedDeviceGuard, BridgedDeviceViolation, ConfigurationViolation, DeviceDescriptorViolation,
    validate_configuration_descriptor, validate_device_descriptor, validate_device_qualifier,
};
pub use descriptor::{
    DescriptorError, EndpointDescriptor, InterfaceDescriptor, ParsedConfiguration,
    parse_configuration_descriptor,
};
pub use exporter::{
    CaptureRefusal, ControlDispatch, EndSignal, ExportedCompletion, ExporterError, SHUTDOWN_DRAIN,
    USB_CLASS_HID, UrbSubmission, UsbExporter, admissible_tablet, control_dispatch,
    tablet_profile_for, verify_captured_tablet,
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
pub use hid_host::{
    BridgedHidDevice, DEFAULT_PRODUCT_NAME, HID_HOST_CONTROL_DEADLINE, HID_HOST_CONTROL_TIMEOUT_MS,
    HID_HOST_ENUMERATION_DEADLINE, HidHostAdapter, HidHostError, HidHostOutput, HidHostPhase,
    HidHostStats, HidHostUrb, HidReportFailure, HidReportRequest, INTERRUPT_IDLE_RESUBMIT,
    LOCATION_ID_BASE, MAX_INTERRUPT_ERRORS, MAX_PROPERTY_CHARS, MAX_QUEUED_REPORT_REQUESTS,
    hid_country_codes, is_bridged_location_id, location_id, string_descriptor,
};
pub use hid_publish::{APPLE_EVENT_PAGES, HidPublishPolicy, PublishRefusal, publishable};
pub use kernel_package::{
    VhciHcdFacts, el_point_release, kernel_arch, kernel_modules_internal_command,
    kernel_modules_internal_package, missing_vhci_hcd_reason, os_release_is_rhel_family,
    rocky_package_urls, valid_kernel_release,
};
pub use ledger::{InFlightLedger, LedgerError, UrbRecord};
pub use policy::{
    DeviceProfile, DeviceSnapshot, InterfaceRule, PolicyDecision, PolicyDenial, evaluate_profile,
    is_permanently_prohibited_class,
};
pub use raw_hid::{AdmittedRawHidDevice, RawHidAdmission, RawHidRefusal};
pub use session_seat::{HardUsbSessionDenial, HardUsbSessionSeat, admit_hard_usb_session};
pub use state::{AttachmentMachine, AttachmentState, StateError};
pub use state_replay::{
    DeviceStateReplay, MAX_REPLAYED_PAYLOAD, MAX_REPLAYED_REQUESTS, is_state_setting,
};
pub use synthetic::{
    ARCEN_LAB_PRODUCT_ID, ARCEN_LAB_VENDOR_ID, ControlResponse, PenSample, PenSwitch, PenSwitches,
    SYNTHETIC_TABLET_REPORT_DESCRIPTOR, SyntheticTabletDevice,
};
pub use synthetic_device::{
    EmulatedUsbFunction, MAX_QUEUED_REPORTS, SYNTHETIC_INTERRUPT_IN, SyntheticDevice,
    SyntheticHandle, SyntheticOp,
};
pub use tablet_claim::{
    HardUsbTabletDenial, InterfaceReportDescriptor, MAX_CLAIMED_REPORT_DESCRIPTOR_BYTES,
    authorize_hard_usb_tablet,
};
pub use types::{
    AlternateSetting, AttachmentGeneration, EndpointAddress, InterfaceNumber, SetupPacket,
    TransferDirection, TransferKind, UrbId, UrbStatus, UsbDeviceId, UsbSpeed,
};
pub use usbip::{
    MAX_VHCI_STATUS_ROWS, USBIP_HEADER_BYTES, UnlinkAction, UsbipCommand, UsbipError,
    UsbipServerState, UsbipSubmit, VhciHub, VhciPortRow, VhciStatus, decode_command, devpath_under,
    encode_ret_submit, encode_ret_unlink, port_devpath_prefix,
};

pub use wacom_intuos_pen::{
    HOST_DECODED_INTUOS_MODELS, HostPenSample, INTUOS_MODE_SWITCH, INTUOS_TOUCH_THRESHOLD,
    IntuosPenModel, IntuosPenPacket, IntuosPenReader, decode_intuos_pen_report,
    host_decoded_intuos_model, intuos_tool_is_eraser,
};
pub use wacom_intuos5::{
    HidReportKind, INTUOS_PRO_GRIP_PEN_TOOL_ID, INTUOS5_CONFIGURATION_DESCRIPTOR,
    INTUOS5_DEVICE_DESCRIPTOR, INTUOS5_GRIP_PEN_TOOL_ID, INTUOS5_MAX_DISTANCE,
    INTUOS5_MAX_PRESSURE, INTUOS5_MAX_X, INTUOS5_MAX_Y, INTUOS5_MODE_REPORT_ID, INTUOS5_MOUSE_IN,
    INTUOS5_MOUSE_REPORT_DESCRIPTOR, INTUOS5_PEN_IN, INTUOS5_PEN_REPORT_DESCRIPTOR,
    INTUOS5_PEN_REPORT_ID, INTUOS5_PEN_REPORT_LENGTH, INTUOS5_TOUCH_IN, INTUOS5_TOUCH_L_PRODUCT_ID,
    INTUOS5_TOUCH_REPORT_DESCRIPTOR, INTUOS5_WACOM_MODE, Intuos5PenReport, Intuos5PenState,
    RESTING_HOLD, SCRIPTED_CORNER, SCRIPTED_END, SCRIPTED_HOVER_DISTANCE, SCRIPTED_LEG_STEPS,
    SCRIPTED_MINIMUM_INTERVAL, SCRIPTED_MOVE_PRESSURE, SCRIPTED_PEN_SERIAL, SCRIPTED_RAMP_STEPS,
    SCRIPTED_START, ScriptedReport, WACOM_VENDOR_ID, WacomIntuos5, declared_report_lengths,
    intuos5_landing_stroke, intuos5_realistic_stroke, intuos5_resting_stroke,
    intuos5_scripted_stroke, with_upper_switch_held,
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
