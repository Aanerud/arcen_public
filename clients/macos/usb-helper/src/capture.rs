// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Physical USB capture. This is the only reason the helper needs root.
//!
//! Ported from the Deck's `usb_bridge.rs` physical path, with tokio removed:
//! the root process runs no async runtime, only std threads. Policy is
//! evaluated here, from descriptors this process reads itself, so a compromised
//! Deck cannot talk the helper into capturing an unapproved device.
//!
//! Everything after capture that does not depend on libusb — URB admission,
//! cancellation tombstones, locally answered standard requests, the interrupt
//! hold, status mapping and exactly-once release — is the shared
//! `arcen_usb_bridge::UsbExporter` (ADR 0019). This file is discovery plus
//! [`RusbDevice`], the libusb backend that engine drives, and the choice of
//! [`Backend`]: a helper built with the `iousbhost` feature captures with
//! `IOUSBHost` (`crate::iousbhost`) instead.

use arcen_protocol::{encode_usb_urb_complete, UsbUrbCompletionHeader, UsbUrbSubmitHeader};
use arcen_usb_bridge::{
    admissible_tablet, authorize_hard_usb_tablet, evaluate_profile, hid_report_descriptor_length,
    hid_report_descriptor_request, hold_until_report, tablet_profile_for, AlternateSetting,
    AttachmentGeneration, BackendStatus, CapturedDeviceFacts, CapturedUsbDevice, ControlOperation,
    DeviceSnapshot, EndpointAddress, EndpointDescriptor, ExportedCompletion, HardUsbTabletDenial,
    InterfaceDescriptor, InterfaceNumber, InterfaceReportDescriptor, InterruptPoll,
    ParsedConfiguration, PenTabletProfile, PendingCompletion, ReleaseOutcome, ReleaseReason,
    TransferKind, UrbId, UrbStatus, UrbSubmission, UsbDeviceId, UsbExporter, UsbSpeed,
    MAX_ENDPOINTS, MAX_INTERFACES, USB_CLASS_HID,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long one report-descriptor read may take before the device is skipped.
const REPORT_DESCRIPTOR_TIMEOUT: Duration = Duration::from_millis(500);
/// How long one libusb interrupt read waits before the hold rechecks
/// cancellation.
const INTERRUPT_POLL: Duration = Duration::from_millis(100);
/// Reported when no attached device classifies as a pen tablet.
///
/// Distinct from an inadmissible device, because absence is the ordinary case
/// for a user who left the tablet on another desk, and the client turns it into
/// a quiet downgrade rather than a failure.
pub const NO_TABLET_ATTACHED: &str = "no pen tablet is attached";

/// The capture backend a helper session uses.
///
/// A default (release) build has libusb only. A build with the `iousbhost`
/// feature captures with `IOUSBHost` by default (ADR 0019)
/// and keeps libusb selectable with `--backend libusb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Libusb,
    #[cfg(feature = "iousbhost")]
    IoUsbHost,
}

impl Backend {
    /// What this build captures with unless told otherwise.
    #[cfg(feature = "iousbhost")]
    pub const COMPILED_DEFAULT: Self = Self::IoUsbHost;
    /// What this build captures with unless told otherwise.
    #[cfg(not(feature = "iousbhost"))]
    pub const COMPILED_DEFAULT: Self = Self::Libusb;

    /// The stable name used on the command line, in the helper log and in
    /// the handshake's info frame.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Libusb => "libusb",
            #[cfg(feature = "iousbhost")]
            Self::IoUsbHost => "iousbhost",
        }
    }

    /// Parses a `--backend` value, refusing one this build does not contain.
    ///
    /// # Errors
    ///
    /// Returns a message naming the backends this build has.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "libusb" => Ok(Self::Libusb),
            #[cfg(feature = "iousbhost")]
            "iousbhost" => Ok(Self::IoUsbHost),
            other => Err(format!(
                "unknown capture backend {other}; this build has: {}",
                Self::available().join(", ")
            )),
        }
    }

    /// The backend names this build contains.
    #[must_use]
    pub fn available() -> Vec<&'static str> {
        vec![
            Self::Libusb.name(),
            #[cfg(feature = "iousbhost")]
            Self::IoUsbHost.name(),
        ]
    }
}

/// Identity of the captured device, reported to Deck at handshake.
///
/// Vendor and product ids are informational. The report descriptors are what
/// made the device admissible, and the host classifies them again itself.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub speed: UsbSpeed,
    pub report_descriptors: Vec<InterfaceReportDescriptor>,
}

impl DeviceIdentity {
    fn from_facts(facts: &CapturedDeviceFacts) -> Self {
        Self {
            vendor_id: facts.snapshot.id.vendor_id,
            product_id: facts.snapshot.id.product_id,
            bcd_device: facts.snapshot.id.bcd_device,
            device_class: facts.snapshot.device_class,
            speed: facts.speed,
            report_descriptors: facts.report_descriptors.clone(),
        }
    }
}

/// Numeric encoding of [`UsbSpeed`] for the handshake frame.
#[must_use]
pub const fn speed_code(speed: UsbSpeed) -> u8 {
    match speed {
        UsbSpeed::Low => 0,
        UsbSpeed::Full => 1,
        UsbSpeed::High => 2,
    }
}

/// The libusb handle of a captured device, and the obligation to give it back.
struct CapturedPhysicalUsb {
    handle: rusb::DeviceHandle<rusb::Context>,
    claimed_interfaces: Vec<u8>,
    control_lock: Mutex<()>,
    restored: bool,
}

impl CapturedPhysicalUsb {
    /// Releases every claimed interface and returns the device to macOS.
    ///
    /// Logged unconditionally: a device left captured is indistinguishable
    /// from a healthy one at a glance — it still enumerates and `ioreg` still
    /// lists it — so silence here made the difference between "teardown ran
    /// and failed" and "teardown never ran" impossible to tell apart from the
    /// outside.
    fn restore(&mut self) -> Result<(), String> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        eprintln!(
            "arcen-usb-helper: releasing device, interfaces={:?}",
            self.claimed_interfaces
        );
        for interface in self.claimed_interfaces.iter().copied().rev() {
            if let Err(error) = self.handle.release_interface(interface) {
                eprintln!("arcen-usb-helper: release interface {interface} failed: {error}");
            }
        }
        // Reattachment must mirror capture exactly. Capture calls
        // `detach_kernel_driver(0)` once, which libusb's macOS backend
        // implements as *whole-device* capture (`USBDeviceReEnumerate` with the
        // capture bit) rather than a per-interface detach, so exactly one
        // reattach releases the whole device.
        match self.handle.attach_kernel_driver(0) {
            Ok(()) => {
                eprintln!("arcen-usb-helper: device released back to macOS");
                Ok(())
            }
            Err(error) => {
                eprintln!("arcen-usb-helper: restore kernel driver failed: {error}");
                Err(format!("restore kernel driver: {error}"))
            }
        }
    }
}

impl Drop for CapturedPhysicalUsb {
    fn drop(&mut self) {
        // Ownership restoration must happen on every exit path, including
        // panic and connection loss, or the tablet stays dead to macOS. When a
        // transfer thread outlives the release, its drop of the last handle
        // reference lands here.
        let _ = self.restore();
    }
}

/// The libusb backend: one thread per transfer, synchronous libusb calls.
///
/// Kept only until the IOUSBHost backend replaces it (ADR 0019 M2/M3).
struct RusbDevice {
    device: Arc<CapturedPhysicalUsb>,
    facts: CapturedDeviceFacts,
}

impl RusbDevice {
    /// Runs one transfer on its own thread. The thread drops its handle
    /// reference before completing, so once every completion has been
    /// delivered the release can restore the device at once.
    fn spawn(
        &self,
        completion: PendingCompletion,
        transfer: impl FnOnce(&CapturedPhysicalUsb, &PendingCompletion) -> (BackendStatus, Vec<u8>)
            + Send
            + 'static,
    ) {
        let device = Arc::clone(&self.device);
        std::thread::spawn(move || {
            let (status, data) = transfer(&device, &completion);
            drop(device);
            completion.complete(status, data);
        });
    }
}

impl CapturedUsbDevice for RusbDevice {
    fn facts(&self) -> &CapturedDeviceFacts {
        &self.facts
    }

    fn submit_control(
        &self,
        operation: ControlOperation,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        self.spawn(completion, move |device, _| {
            let Ok(_control) = device.control_lock.lock() else {
                return (BackendStatus::Io, Vec::new());
            };
            match perform_control(device, operation, timeout) {
                Ok(data) => (BackendStatus::Completed, data),
                Err(error) => (map_rusb_error(error), Vec::new()),
            }
        });
    }

    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    ) {
        self.spawn(completion, move |device, completion| {
            let mut data = vec![0_u8; length];
            hold_until_report(completion, || {
                match device
                    .handle
                    .read_interrupt(endpoint.0, &mut data, INTERRUPT_POLL)
                {
                    Ok(actual) => InterruptPoll::Report(data[..actual].to_vec()),
                    Err(rusb::Error::Timeout) => InterruptPoll::Idle,
                    Err(error) => InterruptPoll::Failed(map_rusb_error(error)),
                }
            })
        });
    }

    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        self.spawn(completion, move |device, _| {
            match device.handle.write_interrupt(endpoint.0, &data, timeout) {
                Ok(_) => (BackendStatus::Completed, Vec::new()),
                Err(error) => (map_rusb_error(error), Vec::new()),
            }
        });
    }

    /// libusb's synchronous calls cannot be aborted one by one; the transfer
    /// thread sees the cancellation flag at its next poll.
    fn cancel(&self, _urb: UrbId) {}

    fn release(self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome {
        eprintln!("arcen-usb-helper: release requested, reason={reason}");
        match Arc::try_unwrap(self.device) {
            Ok(mut device) => match device.restore() {
                Ok(()) => ReleaseOutcome::Restored,
                Err(error) => ReleaseOutcome::Failed(error),
            },
            Err(shared) => {
                eprintln!(
                    "arcen-usb-helper: a transfer is still running; \
                     the device is released when it ends"
                );
                drop(shared);
                ReleaseOutcome::Deferred
            }
        }
    }
}

fn perform_control(
    device: &CapturedPhysicalUsb,
    operation: ControlOperation,
    timeout: Duration,
) -> rusb::Result<Vec<u8>> {
    match operation {
        ControlOperation::SelectAlternateSetting {
            interface,
            alternate,
        } => device
            .handle
            .set_alternate_setting(interface.0, alternate.0)
            .map(|()| Vec::new()),
        ControlOperation::ClearHalt { endpoint } => {
            device.handle.clear_halt(endpoint.0).map(|()| Vec::new())
        }
        ControlOperation::Transfer {
            setup, in_length, ..
        } if setup.request_type & 0x80 != 0 => {
            let mut data = vec![0_u8; in_length];
            let actual = device.handle.read_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                &mut data,
                timeout,
            )?;
            data.truncate(actual);
            Ok(data)
        }
        ControlOperation::Transfer { setup, data, .. } => device
            .handle
            .write_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                &data,
                timeout,
            )
            .map(|_| Vec::new()),
    }
}

/// One captured physical device, bridged through the shared exporter engine.
pub struct PhysicalDevice {
    exporter: UsbExporter,
    identity: DeviceIdentity,
}

impl PhysicalDevice {
    /// Captures an attached pen tablet with `backend`.
    ///
    /// Both backends apply the same admission: the I/O Registry's report
    /// descriptors classified before capture, and the shared engine's
    /// `verify_captured_tablet` over what the backend read after it.
    ///
    /// # Errors
    ///
    /// Returns [`NO_TABLET_ATTACHED`] when no attached device classifies as a
    /// pen tablet, or a message naming why capture failed.
    pub fn capture(backend: Backend) -> Result<Self, String> {
        match backend {
            Backend::Libusb => Self::capture_libusb(),
            #[cfg(feature = "iousbhost")]
            Backend::IoUsbHost => crate::iousbhost::capture(),
        }
    }

    /// Captures an attached pen tablet with libusb, whoever made it.
    ///
    /// Every attached device whose interfaces are all HID is considered.
    /// Admissibility is decided here, in the privileged process, from
    /// descriptors it reads itself: [`admissible_tablet`] (every interface is
    /// HID), the HID report descriptors classified by
    /// `arcen_usb_bridge::classify_pen_tablet` (a pen digitizer, not a
    /// keyboard, mouse, gamepad or vendor-only device), and
    /// `evaluate_profile`'s prohibited-class rules. Deck is not trusted to
    /// assert any of it, and no vendor or product id is consulted.
    ///
    /// Report descriptors come from the I/O Registry (`IOHIDDevice`
    /// `ReportDescriptor`), which opens nothing and sends no request, so
    /// examining a keyboard or mouse does not touch it at all. A USB
    /// `GET_DESCRIPTOR(Report)` request is sent only for an interface the
    /// registry left undescribed, on a device the registry already shows to
    /// be a digitizer and that has no boot-keyboard interface
    /// ([`crate::registry::fallback_plan`]). Only the admitted tablet is
    /// captured.
    ///
    /// When several tablets are attached, the first admissible one wins, so an
    /// inadmissible companion device does not mask a usable tablet.
    ///
    /// # Errors
    ///
    /// Returns [`NO_TABLET_ATTACHED`] when no attached device classifies as a
    /// pen tablet, a message when a pen tablet is attached but inadmissible,
    /// or when the process lacks the privilege to capture it.
    fn capture_libusb() -> Result<Self, String> {
        use rusb::UsbContext;

        let context = rusb::Context::new().map_err(|error| error.to_string())?;
        let devices = context.devices().map_err(|error| error.to_string())?;
        let registry = crate::registry::hid_devices()
            .ok_or_else(|| "read HID report descriptors from the I/O Registry".to_owned())?;

        let mut rejections: Vec<String> = Vec::new();
        let mut admitted: Option<(rusb::Device<rusb::Context>, Candidate)> = None;
        for device in devices.iter() {
            // A location is needed only to tell two identical devices apart;
            // with one, identity alone is unambiguous and does not depend on
            // reconstructing macOS's location id from libusb's port path.
            let identical = devices
                .iter()
                .filter(
                    |other| match (other.device_descriptor(), device.device_descriptor()) {
                        (Ok(a), Ok(b)) => {
                            a.vendor_id() == b.vendor_id() && a.product_id() == b.product_id()
                        }
                        _ => false,
                    },
                )
                .count();
            match examine_candidate(&device, &registry, identical > 1) {
                Ok(candidate) => {
                    admitted = Some((device, candidate));
                    break;
                }
                Err(CandidateRejection::NotATablet) => {}
                Err(CandidateRejection::Inadmissible(reason)) => rejections.push(reason),
            }
        }
        let Some((device, candidate)) = admitted else {
            if rejections.is_empty() {
                return Err(NO_TABLET_ATTACHED.to_owned());
            }
            return Err(format!(
                "no attached pen tablet is admissible: {}",
                rejections.join("; ")
            ));
        };
        let Candidate {
            snapshot,
            speed,
            report_descriptors,
            profile,
        } = candidate;
        // Checked before the device is opened or detached: a handshake that
        // cannot be sent must not cost the user their tablet.
        if !crate::frame::hello_fits(
            report_descriptors
                .iter()
                .map(|entry| entry.descriptor.len()),
        ) {
            return Err(format!(
                "pen tablet {:04x}:{:04x}: its report descriptors do not fit the helper \
                 handshake; it cannot be bridged (use Tablet support)",
                snapshot.id.vendor_id, snapshot.id.product_id
            ));
        }
        eprintln!(
            "arcen-usb-helper: pen tablet {:04x}:{:04x} admitted: evidence={:?} \
             pressure_levels={:?} tilt={} twist={} eraser={} barrel_buttons={}",
            snapshot.id.vendor_id,
            snapshot.id.product_id,
            profile.evidence,
            profile.capabilities.pressure_levels(),
            profile.capabilities.has_tilt(),
            profile.capabilities.twist.is_some(),
            profile.capabilities.eraser,
            profile.capabilities.barrel_buttons,
        );

        let handle = device
            .open()
            .map_err(|error| format!("open physical USB device: {error}"))?;
        handle.detach_kernel_driver(0).map_err(|error| {
            format!("capture physical USB device: {error}; the helper must run with root privilege")
        })?;
        let mut claimed_interfaces: Vec<u8> = Vec::new();
        let mut numbers = snapshot
            .configuration
            .interfaces
            .iter()
            .map(|interface| interface.number.0)
            .collect::<Vec<_>>();
        numbers.sort_unstable();
        numbers.dedup();
        for number in numbers {
            if let Err(error) = handle.claim_interface(number) {
                for claimed in claimed_interfaces.iter().copied().rev() {
                    let _ = handle.release_interface(claimed);
                }
                // Mirror capture: one whole-device reattach, matching the
                // single `detach_kernel_driver(0)` above.
                let _ = handle.attach_kernel_driver(0);
                return Err(format!("claim physical USB interface {number}: {error}"));
            }
            claimed_interfaces.push(number);
        }

        let device = RusbDevice {
            device: Arc::new(CapturedPhysicalUsb {
                handle,
                claimed_interfaces,
                control_lock: Mutex::new(()),
                restored: false,
            }),
            facts: CapturedDeviceFacts {
                snapshot,
                speed,
                report_descriptors,
            },
        };
        Self::bridge(Box::new(device))
    }

    /// Bridges a device captured with `IOUSBHost` through the same shared
    /// engine, and lets the backend report an ending the operating system
    /// started (a requesting-close, an unplug).
    ///
    /// # Errors
    ///
    /// Returns a message when the shared policy refuses the device; it has
    /// already been released.
    #[cfg(feature = "iousbhost")]
    pub fn bridge_iousbhost(device: arcen_usb_iousbhost::IoUsbHostDevice) -> Result<Self, String> {
        let link = device.end_link();
        let bridged = Self::bridge(Box::new(device))?;
        link.connect(bridged.exporter.end_signal());
        Ok(bridged)
    }

    /// Hands a captured device to the shared exporter engine, which re-runs
    /// the shared admission policy on the descriptors the backend read.
    ///
    /// # Errors
    ///
    /// Returns a message when the shared policy refuses the device; it has
    /// already been released.
    fn bridge(device: Box<dyn CapturedUsbDevice>) -> Result<Self, String> {
        let exporter = UsbExporter::new(device).map_err(|(refusal, outcome)| {
            format!(
                "captured device refused by the shared policy: {refusal} (release: {outcome:?})"
            )
        })?;
        let identity = DeviceIdentity::from_facts(exporter.facts());
        Ok(Self { exporter, identity })
    }

    #[must_use]
    pub const fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Admits one URB and starts it.
    ///
    /// # Errors
    ///
    /// Returns a message on generation change, duplicate URB id, or when the
    /// in-flight bound is reached.
    pub fn submit(&self, header: UsbUrbSubmitHeader, payload: &[u8]) -> Result<(), String> {
        self.exporter
            .submit(
                UrbSubmission {
                    generation: header.generation,
                    urb_id: header.urb_id,
                    endpoint: header.endpoint,
                    transfer_kind: header.transfer_kind,
                    timeout_ms: header.timeout_ms,
                    declared_length: header.declared_length,
                    setup: header.setup,
                },
                payload,
            )
            .map_err(|error| error.to_string())
    }

    /// Cancels one URB and returns the encoded terminal completion.
    ///
    /// # Errors
    ///
    /// Returns a message on generation mismatch or tombstone exhaustion.
    pub fn cancel(
        &self,
        generation: AttachmentGeneration,
        urb_id: UrbId,
    ) -> Result<Vec<u8>, String> {
        let cancelled = self
            .exporter
            .cancel(generation, urb_id)
            .map_err(|error| error.to_string())?;
        encode_usb_urb_complete(
            UsbUrbCompletionHeader {
                generation: cancelled.generation,
                urb_id: cancelled.urb_id,
                status: UrbStatus::Cancelled,
                actual_length: 0,
            },
            &[],
        )
        .map_err(|error| format!("Hard USB cancellation failed: {error:?}"))
    }

    /// Blocks for the next completion, skipping tombstoned URBs.
    ///
    /// # Errors
    ///
    /// Returns a message once the session is shutting down, or when the
    /// encoding overflows.
    pub fn next_completion(&self) -> Result<Vec<u8>, String> {
        let completion = self
            .exporter
            .next_completion()
            .map_err(|error| error.to_string())?;
        encode_completion(&completion)
    }

    /// Signals every transfer to stop and waits briefly for them to drain.
    pub fn shutdown(&self) {
        self.exporter.shutdown();
    }
}

fn encode_completion(completion: &ExportedCompletion) -> Result<Vec<u8>, String> {
    let actual_length = u32::try_from(completion.data.len())
        .map_err(|_| "physical USB completion length overflow".to_owned())?;
    encode_usb_urb_complete(
        UsbUrbCompletionHeader {
            generation: completion.generation,
            urb_id: completion.urb_id,
            status: completion.status,
            actual_length,
        },
        &completion.data,
    )
    .map_err(|error| format!("encode physical USB completion: {error:?}"))
}

const fn map_rusb_error(error: rusb::Error) -> BackendStatus {
    match error {
        rusb::Error::Timeout => BackendStatus::TimedOut,
        rusb::Error::Pipe => BackendStatus::Stall,
        rusb::Error::NoDevice => BackendStatus::Disconnected,
        rusb::Error::InvalidParam
        | rusb::Error::Overflow
        | rusb::Error::NotSupported
        | rusb::Error::BadDescriptor => BackendStatus::Protocol,
        rusb::Error::Interrupted => BackendStatus::Cancelled,
        rusb::Error::Io
        | rusb::Error::Access
        | rusb::Error::NotFound
        | rusb::Error::Busy
        | rusb::Error::NoMem
        | rusb::Error::Other => BackendStatus::Io,
    }
}

fn map_speed(speed: rusb::Speed) -> Result<UsbSpeed, String> {
    match speed {
        rusb::Speed::Low => Ok(UsbSpeed::Low),
        rusb::Speed::Full => Ok(UsbSpeed::Full),
        rusb::Speed::High => Ok(UsbSpeed::High),
        _ => Err(format!("USB speed {speed:?} is outside input bridge v1")),
    }
}

fn version_to_bcd(version: rusb::Version) -> Result<u16, String> {
    let major = version.major();
    let minor = version.minor();
    let sub_minor = version.sub_minor();
    if major > 99 || minor > 9 || sub_minor > 9 {
        return Err(format!("USB device version {version} is not valid BCD"));
    }
    Ok((u16::from(major / 10) << 12)
        | (u16::from(major % 10) << 8)
        | (u16::from(minor) << 4)
        | u16::from(sub_minor))
}

fn parsed_configuration(
    configuration: &rusb::ConfigDescriptor,
) -> Result<ParsedConfiguration, String> {
    let mut interfaces = Vec::new();
    let mut endpoint_count = 0_usize;
    for interface in configuration.interfaces() {
        for descriptor in interface.descriptors() {
            if interfaces.len() >= MAX_INTERFACES {
                return Err("physical USB interface count exceeds bridge limit".to_owned());
            }
            let mut endpoints = Vec::new();
            for endpoint in descriptor.endpoint_descriptors() {
                endpoint_count = endpoint_count.saturating_add(1);
                if endpoint_count > MAX_ENDPOINTS {
                    return Err("physical USB endpoint count exceeds bridge limit".to_owned());
                }
                if endpoint.transfer_type() != rusb::TransferType::Interrupt {
                    return Err(format!(
                        "physical USB endpoint {:#04x} is not interrupt",
                        endpoint.address()
                    ));
                }
                endpoints.push(EndpointDescriptor {
                    address: EndpointAddress(endpoint.address()),
                    transfer_kind: TransferKind::Interrupt,
                    max_packet_size: endpoint.max_packet_size(),
                    interval: endpoint.interval(),
                });
            }
            interfaces.push(InterfaceDescriptor {
                number: InterfaceNumber(descriptor.interface_number()),
                alternate_setting: AlternateSetting(descriptor.setting_number()),
                class: descriptor.class_code(),
                subclass: descriptor.sub_class_code(),
                protocol: descriptor.protocol_code(),
                endpoints,
            });
        }
    }
    Ok(ParsedConfiguration {
        configuration_value: configuration.number(),
        interfaces,
    })
}

/// One device that passed every admissibility check.
struct Candidate {
    snapshot: DeviceSnapshot,
    speed: UsbSpeed,
    report_descriptors: Vec<InterfaceReportDescriptor>,
    profile: PenTabletProfile,
}

/// Why a device was not admitted.
enum CandidateRejection {
    /// Not a pen tablet at all: skipped silently, like any other peripheral.
    NotATablet,
    /// A pen tablet that failed a bridge invariant: reported to the user.
    Inadmissible(String),
}

/// Reads one candidate's descriptors and decides whether it may be bridged.
///
/// Every check runs against descriptors read here, in the privileged process:
/// the device and configuration descriptors libusb caches, and the HID report
/// descriptors macOS publishes in the I/O Registry.
fn examine_candidate(
    device: &rusb::Device<rusb::Context>,
    registry: &[crate::registry::RegistryHid],
    disambiguate_by_location: bool,
) -> Result<Candidate, CandidateRejection> {
    let descriptor = device
        .device_descriptor()
        .map_err(|_| CandidateRejection::NotATablet)?;
    if descriptor.class_code() != 0x00 && descriptor.class_code() != USB_CLASS_HID {
        return Err(CandidateRejection::NotATablet);
    }
    let configuration = device
        .active_config_descriptor()
        .map_err(|_| CandidateRejection::NotATablet)?;
    // Cheap structural screen: a composite with storage, audio or a hub is
    // never a bridge candidate.
    if !configuration.interfaces().all(|interface| {
        interface
            .descriptors()
            .all(|alternate| alternate.class_code() == USB_CLASS_HID)
    }) {
        return Err(CandidateRejection::NotATablet);
    }
    let hid_interfaces = hid_interfaces(&configuration);
    let location = if disambiguate_by_location {
        let location = device
            .port_numbers()
            .ok()
            .and_then(|ports| crate::registry::location_id(device.bus_number(), &ports));
        // Two identical devices and no way to tell them apart: refuse rather
        // than risk pairing one device's descriptors with the other.
        if location.is_none() {
            return Err(CandidateRejection::NotATablet);
        }
        location
    } else {
        None
    };
    let mut described = match crate::registry::descriptors_for(
        registry,
        descriptor.vendor_id(),
        descriptor.product_id(),
        location,
    ) {
        Ok(described) => described,
        Err(ambiguity) => {
            return Err(CandidateRejection::Inadmissible(format!(
                "{:04x}:{:04x}: the I/O Registry cannot tell which descriptors belong to \
                 this device ({ambiguity:?})",
                descriptor.vendor_id(),
                descriptor.product_id()
            )));
        }
    };
    described.retain(|number, _| {
        hid_interfaces
            .iter()
            .any(|interface| interface.number == *number)
    });
    // The registry alone decides whether this device is worth a closer look:
    // without a digitizer collection there, nothing is ever sent to it.
    let registry_declares_digitizer = described
        .values()
        .any(|bytes| arcen_usb_bridge::declares_any_digitizer(bytes));
    if !registry_declares_digitizer {
        return Err(CandidateRejection::NotATablet);
    }

    // From here on the registry shows a digitizer; every refusal is reported.
    let identity = format!(
        "{:04x}:{:04x}",
        descriptor.vendor_id(),
        descriptor.product_id()
    );
    let inadmissible =
        |error: String| CandidateRejection::Inadmissible(format!("{identity}: {error}"));
    let missing =
        crate::registry::fallback_plan(&hid_interfaces, &described, registry_declares_digitizer)
            .map_err(|refusal| {
                inadmissible(format!(
                    "a HID interface has no report descriptor in the I/O Registry \
                     and the helper will not ask the device for it ({refusal:?})"
                ))
            })?;
    if !missing.is_empty() {
        for (number, bytes) in
            read_report_descriptors(device, &configuration, &missing).map_err(&inadmissible)?
        {
            described.insert(number, bytes);
        }
    }
    let report_descriptors: Vec<InterfaceReportDescriptor> = described
        .into_iter()
        .map(|(interface, descriptor)| InterfaceReportDescriptor {
            interface,
            descriptor,
        })
        .collect();
    // The same shared authorization the host applies: a pen tablet, nothing
    // forwarded beside it that is not (no keyboard, keypad, game controller),
    // and descriptors that fit the handshake.
    let profile = match authorize_hard_usb_tablet(descriptor.class_code(), &report_descriptors) {
        Ok(profile) => profile,
        Err(HardUsbTabletDenial::NotPenTablet(_)) => return Err(CandidateRejection::NotATablet),
        Err(denial) => return Err(inadmissible(denial.to_string())),
    };

    let speed = map_speed(device.speed()).map_err(&inadmissible)?;
    let parsed = parsed_configuration(&configuration).map_err(&inadmissible)?;
    let bcd_device = version_to_bcd(descriptor.device_version()).map_err(&inadmissible)?;
    let snapshot = DeviceSnapshot {
        id: UsbDeviceId {
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
            bcd_device,
        },
        device_class: descriptor.class_code(),
        configuration: parsed,
    };
    admissible_tablet(&snapshot).map_err(&inadmissible)?;
    evaluate_profile(true, &tablet_profile_for(&snapshot), &snapshot)
        .map_err(|error| inadmissible(format!("denied by profile: {error}")))?;
    Ok(Candidate {
        snapshot,
        speed,
        report_descriptors,
        profile,
    })
}

/// The HID interfaces (alternate setting 0) of a configuration.
fn hid_interfaces(configuration: &rusb::ConfigDescriptor) -> Vec<crate::registry::HidInterface> {
    let mut interfaces: Vec<crate::registry::HidInterface> = Vec::new();
    for interface in configuration.interfaces() {
        for alternate in interface.descriptors() {
            if alternate.class_code() == USB_CLASS_HID
                && alternate.setting_number() == 0
                && !interfaces
                    .iter()
                    .any(|known| known.number == alternate.interface_number())
            {
                interfaces.push(crate::registry::HidInterface {
                    number: alternate.interface_number(),
                    subclass: alternate.sub_class_code(),
                    protocol: alternate.protocol_code(),
                });
            }
        }
    }
    interfaces
}

/// Reads the HID report descriptor of each interface in `only` (alternate 0)
/// with `GET_DESCRIPTOR(Report)` on the default pipe.
///
/// Called only for a device the registry already shows to be a digitizer,
/// with no boot-keyboard interface, for interfaces the registry left
/// undescribed. The device is opened but not captured: no kernel driver is
/// detached and no interface is claimed.
fn read_report_descriptors(
    device: &rusb::Device<rusb::Context>,
    configuration: &rusb::ConfigDescriptor,
    only: &[u8],
) -> Result<Vec<(u8, Vec<u8>)>, String> {
    let handle = device
        .open()
        .map_err(|error| format!("open for report descriptors: {error}"))?;
    let mut descriptors: Vec<(u8, Vec<u8>)> = Vec::new();
    for interface in configuration.interfaces() {
        for alternate in interface.descriptors() {
            let number = alternate.interface_number();
            if alternate.class_code() != USB_CLASS_HID
                || alternate.setting_number() != 0
                || !only.contains(&number)
                || descriptors.iter().any(|(known, _)| *known == number)
            {
                continue;
            }
            if descriptors.len() >= MAX_INTERFACES {
                return Err("HID interface count exceeds bridge limit".to_owned());
            }
            let length = hid_report_descriptor_length(alternate.extra())
                .ok_or_else(|| format!("interface {number} has no usable HID descriptor"))?;
            let setup = hid_report_descriptor_request(number, length);
            let mut buffer = vec![0_u8; usize::from(length)];
            let read = handle
                .read_control(
                    setup.request_type,
                    setup.request,
                    setup.value,
                    setup.index,
                    &mut buffer,
                    REPORT_DESCRIPTOR_TIMEOUT,
                )
                .map_err(|error| format!("read interface {number} report descriptor: {error}"))?;
            buffer.truncate(read);
            descriptors.push((number, buffer));
        }
    }
    Ok(descriptors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::decode_usb_urb_complete;
    use arcen_usb_bridge::{
        SetupPacket, SyntheticDevice, SyntheticHandle, SyntheticOp, SyntheticTabletDevice,
    };
    use std::num::{NonZeroU32, NonZeroU64};

    #[test]
    fn cancellation_maps_to_cancelled_not_success() {
        assert_eq!(
            map_rusb_error(rusb::Error::Interrupted),
            BackendStatus::Cancelled
        );
        assert_eq!(
            map_rusb_error(rusb::Error::Timeout),
            BackendStatus::TimedOut
        );
        assert_eq!(map_rusb_error(rusb::Error::Pipe), BackendStatus::Stall);
        assert_eq!(
            map_rusb_error(rusb::Error::NoDevice).urb_status(),
            UrbStatus::Disconnected
        );
    }

    /// The libusb error table, end to end to the wire status, exactly as the
    /// helper mapped it before the engine moved into `arcen-usb-bridge`.
    #[test]
    fn every_libusb_error_keeps_its_wire_status() {
        use rusb::Error;
        for (error, status) in [
            (Error::Timeout, UrbStatus::TimedOut),
            (Error::Pipe, UrbStatus::Stall),
            (Error::NoDevice, UrbStatus::Disconnected),
            (Error::InvalidParam, UrbStatus::Protocol),
            (Error::Overflow, UrbStatus::Protocol),
            (Error::NotSupported, UrbStatus::Protocol),
            (Error::BadDescriptor, UrbStatus::Protocol),
            (Error::Interrupted, UrbStatus::Cancelled),
            (Error::Io, UrbStatus::Io),
            (Error::Access, UrbStatus::Io),
            (Error::NotFound, UrbStatus::Io),
            (Error::Busy, UrbStatus::Io),
            (Error::NoMem, UrbStatus::Io),
            (Error::Other, UrbStatus::Io),
        ] {
            assert_eq!(map_rusb_error(error).urb_status(), status, "{error:?}");
        }
    }

    #[test]
    fn superspeed_is_outside_v1() {
        assert!(map_speed(rusb::Speed::Super).is_err());
        assert_eq!(map_speed(rusb::Speed::Full), Ok(UsbSpeed::Full));
    }

    #[test]
    fn version_to_bcd_matches_the_measured_tablet_revision() {
        assert_eq!(version_to_bcd(rusb::Version(1, 0, 0)), Ok(0x0100));
        assert!(version_to_bcd(rusb::Version(100, 0, 0)).is_err());
    }

    #[test]
    fn speed_codes_are_stable() {
        assert_eq!(speed_code(UsbSpeed::Low), 0);
        assert_eq!(speed_code(UsbSpeed::Full), 1);
        assert_eq!(speed_code(UsbSpeed::High), 2);
    }

    // The helper's wire path over the shared engine, with the synthetic
    // tablet standing in for libusb. No physical device is involved.

    fn synthetic() -> (PhysicalDevice, SyntheticHandle) {
        let (device, handle) = SyntheticDevice::capture().expect("synthetic tablet");
        (
            PhysicalDevice::bridge(Box::new(device)).expect("admitted"),
            handle,
        )
    }

    fn generation() -> AttachmentGeneration {
        AttachmentGeneration::new(NonZeroU64::new(7).expect("nonzero"))
    }

    fn urb(urb_id: u32) -> UrbId {
        UrbId::new(NonZeroU32::new(urb_id).expect("nonzero urb id"))
    }

    fn get_device_descriptor(urb_id: u32) -> UsbUrbSubmitHeader {
        UsbUrbSubmitHeader {
            generation: generation(),
            urb_id: urb(urb_id),
            endpoint: EndpointAddress(0x80),
            transfer_kind: TransferKind::Control,
            timeout_ms: 500,
            declared_length: 18,
            setup: Some(SetupPacket {
                request_type: 0x80,
                request: 6,
                value: 0x0100,
                index: 0,
                length: 18,
            }),
        }
    }

    fn poll(urb_id: u32) -> UsbUrbSubmitHeader {
        UsbUrbSubmitHeader {
            generation: generation(),
            urb_id: urb(urb_id),
            endpoint: EndpointAddress(0x81),
            transfer_kind: TransferKind::Interrupt,
            timeout_ms: 100,
            declared_length: 16,
            setup: None,
        }
    }

    #[test]
    fn the_handshake_identity_comes_from_what_the_backend_read() {
        let (device, _handle) = synthetic();
        let identity = device.identity();
        let expected = SyntheticTabletDevice::identity();
        assert_eq!(
            (identity.vendor_id, identity.product_id, identity.bcd_device),
            (expected.vendor_id, expected.product_id, expected.bcd_device)
        );
        assert_eq!(identity.device_class, 0);
        assert_eq!(speed_code(identity.speed), 1);
        assert_eq!(identity.report_descriptors.len(), 1);
        assert_eq!(identity.report_descriptors[0].interface, 0);
    }

    #[test]
    fn a_submitted_urb_completes_as_a_wire_frame() {
        let (device, handle) = synthetic();
        device
            .submit(get_device_descriptor(1), &[])
            .expect("admitted");
        let frame = device.next_completion().expect("completion");
        let (header, payload) = decode_usb_urb_complete(&frame).expect("valid frame");
        assert_eq!(header.urb_id, urb(1));
        assert_eq!(header.generation, generation());
        assert_eq!(header.status, UrbStatus::Success);
        assert_eq!(payload, SyntheticTabletDevice::device_descriptor());
        assert_eq!(handle.ops().len(), 1);
    }

    #[test]
    fn a_cancel_answers_with_one_cancelled_frame_and_swallows_the_late_one() {
        let (device, handle) = synthetic();
        device.submit(poll(1), &[]).expect("admitted");
        let frame = device.cancel(generation(), urb(1)).expect("cancelled");
        let (header, payload) = decode_usb_urb_complete(&frame).expect("valid frame");
        assert_eq!(
            (header.urb_id, header.status, payload.len()),
            (urb(1), UrbStatus::Cancelled, 0)
        );
        assert!(handle.ops().contains(&SyntheticOp::Cancel(urb(1))));
        // The id stays refused until the backend's late completion is drained.
        assert_eq!(
            device.submit(poll(1), &[]),
            Err("physical USB submit reused a cancelled URB id".to_owned())
        );
        device
            .submit(get_device_descriptor(2), &[])
            .expect("admitted");
        let (header, _) = decode_usb_urb_complete(&device.next_completion().expect("completion"))
            .expect("valid frame");
        assert_eq!(header.urb_id, urb(2));
        device.submit(poll(1), &[]).expect("tombstone cleared");
    }

    #[test]
    fn the_session_errors_sent_to_deck_keep_their_text() {
        let (device, _handle) = synthetic();
        device.submit(poll(1), &[]).expect("admitted");
        assert_eq!(
            device.submit(poll(1), &[]),
            Err("physical USB submit reused an in-flight URB id".to_owned())
        );
        let mut other = poll(2);
        other.generation = AttachmentGeneration::new(NonZeroU64::new(8).expect("nonzero"));
        assert_eq!(
            device.submit(other, &[]),
            Err("physical USB attachment generation changed".to_owned())
        );
    }

    #[test]
    fn shutting_down_stops_the_pump_and_dropping_releases_once() {
        let (device, handle) = synthetic();
        device.submit(poll(1), &[]).expect("admitted");
        device.shutdown();
        assert_eq!(
            device.next_completion(),
            Err("physical USB completion pump stopped".to_owned())
        );
        assert_eq!(handle.releases(), 0);
        drop(device);
        assert_eq!(handle.releases(), 1);
        assert_eq!(
            handle.ops().last(),
            Some(&SyntheticOp::Release(ReleaseReason::SessionEnded))
        );
    }
}
