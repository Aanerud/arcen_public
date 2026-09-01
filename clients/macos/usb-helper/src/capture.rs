//! Physical USB capture. This is the only reason the helper needs root.
//!
//! Ported from the Deck's `usb_bridge.rs` physical path, with tokio removed:
//! the root process runs no async runtime, only std threads. Policy is
//! evaluated here, from descriptors this process reads itself, so a compromised
//! Deck cannot talk the helper into capturing an unapproved device.

use arcen_protocol::{encode_usb_urb_complete, UsbUrbCompletionHeader, UsbUrbSubmitHeader};
use arcen_usb_bridge::{
    evaluate_profile, AlternateSetting, AttachmentGeneration, DeviceProfile, DeviceSnapshot,
    EndpointAddress, EndpointDescriptor, InterfaceDescriptor, InterfaceNumber, InterfaceRule,
    ParsedConfiguration, TransferDirection, TransferKind, UrbId, UrbStatus, UsbDeviceId, UsbSpeed,
    MAX_ENDPOINTS, MAX_INTERFACES, MAX_IN_FLIGHT_URBS,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WACOM_VENDOR_ID: u16 = 0x056a;
/// Interface class every bridged interface must report.
///
/// This is the invariant that replaced pinning one product id. A vendor id is
/// only a claim -- anything can assert Wacom's -- so admissibility rests on
/// what the device *is*: every interface must be HID. Combined with
/// `evaluate_profile`'s permanently-prohibited class list, that keeps storage,
/// hubs, smartcard and wireless-controller devices out of the bridge no matter
/// what identity they present.
const USB_CLASS_HID: u8 = 0x03;
/// Reported when no Wacom-vendor device is present at all.
///
/// Distinct from an inadmissible device, because absence is the ordinary case
/// for a user who left the tablet on another desk, and the client turns it into
/// a quiet downgrade rather than a failure.
pub const NO_TABLET_ATTACHED: &str = "no Wacom tablet is attached";
const USB_REQUEST_CLEAR_FEATURE: u8 = 1;
const USB_REQUEST_SET_ADDRESS: u8 = 5;
const USB_REQUEST_SET_CONFIGURATION: u8 = 9;
const USB_REQUEST_SET_INTERFACE: u8 = 11;
const USB_REQUEST_TYPE_STANDARD: u8 = 0;
const USB_REQUEST_TYPE_MASK: u8 = 0x60;
const USB_RECIPIENT_DEVICE: u8 = 0;
const USB_RECIPIENT_ENDPOINT: u8 = 2;
const USB_RECIPIENT_INTERFACE: u8 = 1;
const USB_RECIPIENT_MASK: u8 = 0x1f;
const USB_FEATURE_ENDPOINT_HALT: u16 = 0;

/// Identity of the captured device, reported to Deck at handshake.
#[derive(Debug, Clone, Copy)]
pub struct DeviceIdentity {
    pub vendor_id: u16,
    pub product_id: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub speed: UsbSpeed,
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

struct CompletedUrb {
    header: UsbUrbSubmitHeader,
    status: UrbStatus,
    data: Vec<u8>,
}

struct InFlightUrb {
    generation: AttachmentGeneration,
    cancelled: Arc<AtomicBool>,
}

struct CapturedPhysicalUsb {
    handle: rusb::DeviceHandle<rusb::Context>,
    claimed_interfaces: Vec<u8>,
    configuration_value: u8,
    control_lock: Mutex<()>,
}

impl Drop for CapturedPhysicalUsb {
    fn drop(&mut self) {
        // Ownership restoration must happen on every exit path, including
        // panic and connection loss, or the tablet stays dead to macOS.
        //
        // Logged unconditionally: a device left captured is indistinguishable
        // from a healthy one at a glance — it still enumerates and `ioreg`
        // still lists it — so silence here made the difference between
        // "teardown ran and failed" and "teardown never ran" impossible to
        // tell apart from the outside.
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
            Ok(()) => eprintln!("arcen-usb-helper: device released back to macOS"),
            Err(error) => eprintln!("arcen-usb-helper: restore kernel driver failed: {error}"),
        }
    }
}

/// One captured physical device plus its in-flight URB bookkeeping.
pub struct PhysicalDevice {
    device: Arc<CapturedPhysicalUsb>,
    identity: DeviceIdentity,
    completions: Mutex<Receiver<CompletedUrb>>,
    completion_sender: SyncSender<CompletedUrb>,
    /// Set by `shutdown` so the completion pump stops instead of blocking.
    ///
    /// `Receiver::recv` only returns once every sender is dropped, and this
    /// struct owns `completion_sender` for its whole life, so the pump could
    /// never observe the end of the stream on its own: `handle_client` joined
    /// a thread that would never return, `Drop` never ran, and the device was
    /// left captured after the session ended.
    stopping: Arc<AtomicBool>,
    state: Mutex<UrbState>,
}

/// In-flight bookkeeping, shared between the request loop and the completion
/// pump. Both hold the lock only briefly; the blocking wait for a completion
/// happens on the channel, never under this lock.
#[derive(Default)]
struct UrbState {
    generation: Option<AttachmentGeneration>,
    in_flight: BTreeMap<UrbId, InFlightUrb>,
    cancelled: BTreeSet<UrbId>,
}

impl PhysicalDevice {
    /// Captures an attached Wacom tablet.
    ///
    /// Any device presenting Wacom's vendor id is considered, rather than one
    /// hard-coded model: the tablet on a user's desk is not the tablet the
    /// bridge happened to be developed against. Admissibility is decided by
    /// [`admissible_tablet`] -- every interface must be HID -- and by
    /// `evaluate_profile`'s prohibited-class rules, both evaluated here in the
    /// privileged process from descriptors it read itself. Deck is not trusted
    /// to assert any of it.
    ///
    /// When several Wacom devices are attached, the first admissible one wins,
    /// so an inadmissible companion device does not mask a usable tablet.
    ///
    /// # Errors
    ///
    /// Returns a message when no Wacom device is attached, when none of the
    /// attached ones is admissible, or when the process lacks the privilege to
    /// capture it.
    pub fn capture() -> Result<Self, String> {
        use rusb::UsbContext;

        let context = rusb::Context::new().map_err(|error| error.to_string())?;
        let devices = context.devices().map_err(|error| error.to_string())?;
        let candidates = devices
            .iter()
            .filter(|device| {
                device
                    .device_descriptor()
                    .is_ok_and(|descriptor| descriptor.vendor_id() == WACOM_VENDOR_ID)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(NO_TABLET_ATTACHED.to_owned());
        }

        let mut rejections: Vec<String> = Vec::new();
        let mut admitted: Option<(rusb::Device<rusb::Context>, DeviceSnapshot, UsbSpeed)> = None;
        for device in candidates {
            match examine_candidate(&device) {
                Ok((snapshot, speed)) => {
                    admitted = Some((device, snapshot, speed));
                    break;
                }
                Err(reason) => rejections.push(reason),
            }
        }
        let Some((device, snapshot, speed)) = admitted else {
            return Err(format!(
                "no attached Wacom device is admissible: {}",
                rejections.join("; ")
            ));
        };

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

        let identity = DeviceIdentity {
            vendor_id: snapshot.id.vendor_id,
            product_id: snapshot.id.product_id,
            bcd_device: snapshot.id.bcd_device,
            device_class: snapshot.device_class,
            speed,
        };
        // Bounded, because this is a root process and the queue is fed by
        // worker threads a connected client can start. At most
        // `MAX_IN_FLIGHT_URBS` live workers plus that many cancelled-but-still-
        // running ones can exist, and each sends exactly one completion, so
        // this capacity is never reached in correct operation and no worker can
        // block on it. It exists so that a client which stops draining cannot
        // grow the queue without limit.
        let (completion_sender, completions) =
            std::sync::mpsc::sync_channel(MAX_IN_FLIGHT_URBS.saturating_mul(2));
        Ok(Self {
            device: Arc::new(CapturedPhysicalUsb {
                handle,
                claimed_interfaces,
                configuration_value: snapshot.configuration.configuration_value,
                control_lock: Mutex::new(()),
            }),
            identity,
            completions: Mutex::new(completions),
            completion_sender,
            stopping: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(UrbState::default()),
        })
    }

    #[must_use]
    pub const fn identity(&self) -> DeviceIdentity {
        self.identity
    }

    /// Admits one URB and starts it on a worker thread.
    ///
    /// # Errors
    ///
    /// Returns a message on generation change, duplicate URB id, or when the
    /// in-flight bound is reached.
    pub fn submit(&self, header: UsbUrbSubmitHeader, payload: &[u8]) -> Result<(), String> {
        let cancelled = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.lock_state()?;
            admit_submission(&mut state, header, &cancelled)?;
        }
        let device = Arc::clone(&self.device);
        let sender = self.completion_sender.clone();
        let payload = payload.to_vec();
        std::thread::spawn(move || {
            let (status, data) = perform_urb(&device, header, &payload, &cancelled);
            let _ = sender.send(CompletedUrb {
                header,
                status,
                data,
            });
        });
        Ok(())
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
        {
            let mut state = self.lock_state()?;
            record_cancellation(&mut state, generation, urb_id)?;
        }
        encode_cancelled(generation, urb_id)
    }

    /// Blocks for the next completion, skipping tombstoned URBs.
    ///
    /// # Errors
    ///
    /// Returns a message when every worker has ended or the encoding overflows.
    pub fn next_completion(&self) -> Result<Vec<u8>, String> {
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Err("physical USB completion pump stopped".to_owned());
            }
            let completion = {
                let receiver = self
                    .completions
                    .lock()
                    .map_err(|_| "physical USB completion lock poisoned".to_owned())?;
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(completion) => completion,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("physical USB completion worker ended".to_owned());
                    }
                }
            };
            {
                let mut state = self.lock_state()?;
                if state.in_flight.remove(&completion.header.urb_id).is_none() {
                    state.cancelled.remove(&completion.header.urb_id);
                    continue;
                }
            }
            let actual_length = u32::try_from(completion.data.len())
                .map_err(|_| "physical USB completion length overflow".to_owned())?;
            return encode_usb_urb_complete(
                UsbUrbCompletionHeader {
                    generation: completion.header.generation,
                    urb_id: completion.header.urb_id,
                    status: completion.status,
                    actual_length,
                },
                &completion.data,
            )
            .map_err(|error| format!("encode physical USB completion: {error:?}"));
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, UrbState>, String> {
        self.state
            .lock()
            .map_err(|_| "physical USB state lock poisoned".to_owned())
    }

    /// Signals every worker to stop and waits briefly for them to drain.
    pub fn shutdown(&self) {
        // Release the pump before waiting for workers, or joining it deadlocks.
        self.stopping.store(true, Ordering::Release);
        if let Ok(mut state) = self.state.lock() {
            for in_flight in state.in_flight.values() {
                in_flight.cancelled.store(true, Ordering::Release);
            }
            state.in_flight.clear();
        }
        let deadline = std::time::Instant::now() + Duration::from_millis(1_500);
        while Arc::strong_count(&self.device) > 1 && std::time::Instant::now() < deadline {
            if let Ok(receiver) = self.completions.lock() {
                let _ = receiver.recv_timeout(Duration::from_millis(50));
            }
        }
    }
}

fn bind_generation(state: &mut UrbState, generation: AttachmentGeneration) -> Result<(), String> {
    match state.generation {
        None => {
            state.generation = Some(generation);
            Ok(())
        }
        Some(expected) if expected == generation => Ok(()),
        Some(_) => Err("physical USB attachment generation changed".to_owned()),
    }
}

/// Decides whether one submission may start a worker, and records it if so.
///
/// Split out of [`PhysicalDevice::submit`] so the admission rules can be tested
/// without a captured device. This is the only thing standing between a
/// connected client and unbounded thread creation inside a root process, so it
/// is worth testing directly.
fn admit_submission(
    state: &mut UrbState,
    header: UsbUrbSubmitHeader,
    cancelled: &Arc<AtomicBool>,
) -> Result<(), String> {
    bind_generation(state, header.generation)?;
    if state.in_flight.contains_key(&header.urb_id) {
        return Err("physical USB submit reused an in-flight URB id".to_owned());
    }
    // A cancelled URB keeps a tombstone until its worker's late completion
    // arrives and clears it. Accepting the same id again inside that window is
    // what let a submit/cancel loop spawn workers without bound: cancellation
    // drops the id from `in_flight`, so the ceiling below stopped constraining
    // anything while every cancelled worker was still running.
    if state.cancelled.contains(&header.urb_id) {
        return Err("physical USB submit reused a cancelled URB id".to_owned());
    }
    if state.in_flight.len() >= MAX_IN_FLIGHT_URBS {
        return Err("physical USB in-flight URB limit exceeded".to_owned());
    }
    state.in_flight.insert(
        header.urb_id,
        InFlightUrb {
            generation: header.generation,
            cancelled: Arc::clone(cancelled),
        },
    );
    Ok(())
}

/// Records one cancellation against the URB ledger.
///
/// Split out alongside [`admit_submission`] for the same reason: together these
/// two are what bound how many worker threads a connected client can have
/// running inside a root process, and that bound should be provable in a test
/// rather than argued about in a comment.
fn record_cancellation(
    state: &mut UrbState,
    generation: AttachmentGeneration,
    urb_id: UrbId,
) -> Result<(), String> {
    bind_generation(state, generation)?;
    if let Some(in_flight) = state.in_flight.remove(&urb_id) {
        if in_flight.generation != generation {
            return Err("physical USB cancellation generation mismatch".to_owned());
        }
        in_flight.cancelled.store(true, Ordering::Release);
        if state.cancelled.len() >= MAX_IN_FLIGHT_URBS {
            return Err("physical USB cancellation tombstone limit exceeded".to_owned());
        }
        state.cancelled.insert(urb_id);
    }
    Ok(())
}

fn encode_cancelled(generation: AttachmentGeneration, urb_id: UrbId) -> Result<Vec<u8>, String> {
    encode_usb_urb_complete(
        UsbUrbCompletionHeader {
            generation,
            urb_id,
            status: UrbStatus::Cancelled,
            actual_length: 0,
        },
        &[],
    )
    .map_err(|error| format!("Hard USB cancellation failed: {error:?}"))
}

fn perform_urb(
    device: &CapturedPhysicalUsb,
    header: UsbUrbSubmitHeader,
    payload: &[u8],
    cancelled: &AtomicBool,
) -> (UrbStatus, Vec<u8>) {
    let timeout = Duration::from_millis(u64::from(header.timeout_ms.max(1)));
    let result = match header.transfer_kind {
        TransferKind::Control => {
            let Some(setup) = header.setup else {
                return (UrbStatus::Protocol, Vec::new());
            };
            let Ok(_control) = device.control_lock.lock() else {
                return (UrbStatus::Io, Vec::new());
            };
            perform_control(device, setup, payload, header.declared_length, timeout)
        }
        TransferKind::Interrupt if header.endpoint.direction() == TransferDirection::In => {
            let Ok(length) = usize::try_from(header.declared_length) else {
                return (UrbStatus::Protocol, Vec::new());
            };
            read_interrupt_until_report(device, header.endpoint.0, length, cancelled)
        }
        TransferKind::Interrupt => device
            .handle
            .write_interrupt(header.endpoint.0, payload, timeout)
            .map(|_| Vec::new()),
    };
    match result {
        Ok(data) => (UrbStatus::Success, data),
        Err(error) => (map_rusb_error(error), Vec::new()),
    }
}

/// Keeps an interrupt-IN URB pending across ordinary polling timeouts.
///
/// Returning `ETIMEDOUT` on every idle poll made Linux reset an otherwise
/// correctly enumerated tablet after a few seconds, so a timeout is retried
/// rather than reported until a report arrives or the URB is cancelled.
fn read_interrupt_until_report(
    device: &CapturedPhysicalUsb,
    endpoint: u8,
    length: usize,
    cancelled: &AtomicBool,
) -> rusb::Result<Vec<u8>> {
    let mut data = vec![0_u8; length];
    while !cancelled.load(Ordering::Acquire) {
        match device
            .handle
            .read_interrupt(endpoint, &mut data, Duration::from_millis(100))
        {
            Ok(actual) => {
                data.truncate(actual);
                return Ok(data);
            }
            Err(rusb::Error::Timeout) => {}
            Err(error) => return Err(error),
        }
    }
    Err(rusb::Error::Interrupted)
}

fn perform_control(
    device: &CapturedPhysicalUsb,
    setup: arcen_usb_bridge::SetupPacket,
    payload: &[u8],
    declared_length: u32,
    timeout: Duration,
) -> rusb::Result<Vec<u8>> {
    match control_dispatch(setup, device.configuration_value) {
        ControlDispatch::Ack => Ok(Vec::new()),
        ControlDispatch::SetInterface {
            interface,
            alternate,
        } => device
            .handle
            .set_alternate_setting(interface, alternate)
            .map(|()| Vec::new()),
        ControlDispatch::ClearHalt(endpoint) => {
            device.handle.clear_halt(endpoint).map(|()| Vec::new())
        }
        ControlDispatch::Stall => Err(rusb::Error::Pipe),
        ControlDispatch::Transfer if setup.request_type & 0x80 != 0 => {
            let length = usize::try_from(declared_length).map_err(|_| rusb::Error::InvalidParam)?;
            let mut data = vec![0_u8; length];
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
        ControlDispatch::Transfer => device
            .handle
            .write_control(
                setup.request_type,
                setup.request,
                setup.value,
                setup.index,
                payload,
                timeout,
            )
            .map(|_| Vec::new()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlDispatch {
    Ack,
    SetInterface { interface: u8, alternate: u8 },
    ClearHalt(u8),
    Stall,
    Transfer,
}

const fn control_dispatch(
    setup: arcen_usb_bridge::SetupPacket,
    configuration_value: u8,
) -> ControlDispatch {
    let standard = setup.request_type & USB_REQUEST_TYPE_MASK == USB_REQUEST_TYPE_STANDARD;
    let recipient = setup.request_type & USB_RECIPIENT_MASK;
    if standard && recipient == USB_RECIPIENT_DEVICE && setup.request == USB_REQUEST_SET_ADDRESS {
        return ControlDispatch::Ack;
    }
    if standard
        && recipient == USB_RECIPIENT_DEVICE
        && setup.request == USB_REQUEST_SET_CONFIGURATION
    {
        return if setup.value == configuration_value as u16 {
            ControlDispatch::Ack
        } else {
            ControlDispatch::Stall
        };
    }
    if standard
        && recipient == USB_RECIPIENT_INTERFACE
        && setup.request == USB_REQUEST_SET_INTERFACE
    {
        return ControlDispatch::SetInterface {
            interface: setup.index.to_le_bytes()[0],
            alternate: setup.value.to_le_bytes()[0],
        };
    }
    if standard
        && recipient == USB_RECIPIENT_ENDPOINT
        && setup.request == USB_REQUEST_CLEAR_FEATURE
        && setup.value == USB_FEATURE_ENDPOINT_HALT
    {
        return ControlDispatch::ClearHalt(setup.index.to_le_bytes()[0]);
    }
    ControlDispatch::Transfer
}

const fn map_rusb_error(error: rusb::Error) -> UrbStatus {
    match error {
        rusb::Error::Timeout => UrbStatus::TimedOut,
        rusb::Error::Pipe => UrbStatus::Stall,
        rusb::Error::NoDevice => UrbStatus::Disconnected,
        rusb::Error::InvalidParam
        | rusb::Error::Overflow
        | rusb::Error::NotSupported
        | rusb::Error::BadDescriptor => UrbStatus::Protocol,
        rusb::Error::Interrupted => UrbStatus::Cancelled,
        rusb::Error::Io
        | rusb::Error::Access
        | rusb::Error::NotFound
        | rusb::Error::Busy
        | rusb::Error::NoMem
        | rusb::Error::Other => UrbStatus::Io,
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

/// Reads one candidate's descriptors and decides whether it may be bridged.
///
/// Every check runs against descriptors read here, in the privileged process.
fn examine_candidate(
    device: &rusb::Device<rusb::Context>,
) -> Result<(DeviceSnapshot, UsbSpeed), String> {
    let descriptor = device
        .device_descriptor()
        .map_err(|error| format!("read physical USB device descriptor: {error}"))?;
    let identity = format!(
        "{:04x}:{:04x}",
        descriptor.vendor_id(),
        descriptor.product_id()
    );
    let speed = map_speed(device.speed()).map_err(|error| format!("{identity}: {error}"))?;
    let configuration = device
        .active_config_descriptor()
        .map_err(|error| format!("{identity}: read physical USB configuration: {error}"))?;
    let parsed =
        parsed_configuration(&configuration).map_err(|error| format!("{identity}: {error}"))?;
    let bcd_device = version_to_bcd(descriptor.device_version())
        .map_err(|error| format!("{identity}: {error}"))?;
    let snapshot = DeviceSnapshot {
        id: UsbDeviceId {
            vendor_id: descriptor.vendor_id(),
            product_id: descriptor.product_id(),
            bcd_device,
        },
        device_class: descriptor.class_code(),
        configuration: parsed,
    };
    admissible_tablet(&snapshot).map_err(|error| format!("{identity}: {error}"))?;
    evaluate_profile(true, &wacom_profile_for(&snapshot), &snapshot)
        .map_err(|error| format!("{identity}: denied by profile: {error}"))?;
    Ok((snapshot, speed))
}

fn wacom_profile_for(snapshot: &DeviceSnapshot) -> DeviceProfile {
    DeviceProfile {
        name: format!(
            "wacom-{:04x}-{:04x}",
            snapshot.id.vendor_id, snapshot.id.product_id
        ),
        vendor_id: snapshot.id.vendor_id,
        product_id: snapshot.id.product_id,
        minimum_bcd_device: snapshot.id.bcd_device,
        maximum_bcd_device: snapshot.id.bcd_device,
        interfaces: snapshot
            .configuration
            .interfaces
            .iter()
            .map(|interface| InterfaceRule {
                number: interface.number,
                alternate_setting: interface.alternate_setting,
                class: interface.class,
                subclass: interface.subclass,
                protocol: interface.protocol,
            })
            .collect(),
    }
}

/// Rejects a Wacom-vendor device that is not wholly a HID device.
///
/// `wacom_profile_for` is derived from the device being examined, so it cannot
/// by itself reject anything -- it exists to keep `evaluate_profile`'s
/// prohibited-class and internal-consistency checks in the path. This function
/// is the part that can say no, and it is deliberately the only place where
/// admissibility is decided.
fn admissible_tablet(snapshot: &DeviceSnapshot) -> Result<(), String> {
    if snapshot.configuration.interfaces.is_empty() {
        return Err("device exposes no interfaces".to_owned());
    }
    // 0x00 means "class is declared per interface", which the loop below then
    // checks. Any other device-level class must itself be HID.
    if snapshot.device_class != 0x00 && snapshot.device_class != USB_CLASS_HID {
        return Err(format!(
            "device class {:#04x} is not a HID device",
            snapshot.device_class
        ));
    }
    for interface in &snapshot.configuration.interfaces {
        if interface.class != USB_CLASS_HID {
            return Err(format!(
                "interface {} class {:#04x} is not HID",
                interface.number.0, interface.class
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_usb_bridge::SetupPacket;

    #[test]
    fn set_address_is_acknowledged_locally() {
        let setup = SetupPacket {
            request_type: 0,
            request: USB_REQUEST_SET_ADDRESS,
            value: 3,
            index: 0,
            length: 0,
        };
        assert_eq!(control_dispatch(setup, 1), ControlDispatch::Ack);
    }

    #[test]
    fn mismatched_configuration_stalls_rather_than_reconfiguring() {
        let setup = SetupPacket {
            request_type: 0,
            request: USB_REQUEST_SET_CONFIGURATION,
            value: 7,
            index: 0,
            length: 0,
        };
        assert_eq!(control_dispatch(setup, 1), ControlDispatch::Stall);
    }

    #[test]
    fn endpoint_halt_maps_to_clear_halt() {
        let setup = SetupPacket {
            request_type: 0x02,
            request: USB_REQUEST_CLEAR_FEATURE,
            value: USB_FEATURE_ENDPOINT_HALT,
            index: 0x81,
            length: 0,
        };
        assert_eq!(control_dispatch(setup, 1), ControlDispatch::ClearHalt(0x81));
    }

    #[test]
    fn cancellation_maps_to_cancelled_not_success() {
        assert_eq!(
            map_rusb_error(rusb::Error::Interrupted),
            UrbStatus::Cancelled
        );
        assert_eq!(map_rusb_error(rusb::Error::Timeout), UrbStatus::TimedOut);
        assert_eq!(map_rusb_error(rusb::Error::Pipe), UrbStatus::Stall);
    }

    #[test]
    fn superspeed_is_outside_v1() {
        assert!(map_speed(rusb::Speed::Super).is_err());
        assert_eq!(map_speed(rusb::Speed::Full), Ok(UsbSpeed::Full));
    }

    #[test]
    fn version_to_bcd_matches_the_measured_wacom_revision() {
        assert_eq!(version_to_bcd(rusb::Version(1, 0, 0)), Ok(0x0100));
        assert!(version_to_bcd(rusb::Version(100, 0, 0)).is_err());
    }

    fn snapshot_with(
        product_id: u16,
        device_class: u8,
        interfaces: &[(u8, u8, u8, u8)],
    ) -> DeviceSnapshot {
        DeviceSnapshot {
            id: UsbDeviceId {
                vendor_id: WACOM_VENDOR_ID,
                product_id,
                bcd_device: 0x0100,
            },
            device_class,
            configuration: ParsedConfiguration {
                configuration_value: 1,
                interfaces: interfaces
                    .iter()
                    .map(|&(number, class, subclass, protocol)| InterfaceDescriptor {
                        number: InterfaceNumber(number),
                        alternate_setting: AlternateSetting(0),
                        class,
                        subclass,
                        protocol,
                        endpoints: Vec::new(),
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn the_measured_three_interface_tablet_is_still_admissible() {
        // The device the bridge was developed against, kept as a regression.
        let snapshot = snapshot_with(
            0x0317,
            0x00,
            &[
                (0, 0x03, 0x00, 0x00),
                (1, 0x03, 0x00, 0x00),
                (2, 0x03, 0x01, 0x02),
            ],
        );
        assert_eq!(admissible_tablet(&snapshot), Ok(()));
        assert!(evaluate_profile(true, &wacom_profile_for(&snapshot), &snapshot).is_ok());
    }

    #[test]
    fn a_different_wacom_model_is_admitted_rather_than_pinned_out() {
        // The point of the change: another product id, another interface count,
        // another revision. Pinning one model rejected every one of these.
        for (product_id, interfaces) in [
            (0x033c_u16, &[(0_u8, 0x03_u8, 0x00_u8, 0x00_u8)][..]),
            (0x0357, &[(0, 0x03, 0x00, 0x00), (1, 0x03, 0x00, 0x00)][..]),
            (0x03aa, &[(0, 0x03, 0x01, 0x02)][..]),
        ] {
            let snapshot = snapshot_with(product_id, 0x00, interfaces);
            assert_eq!(
                admissible_tablet(&snapshot),
                Ok(()),
                "product {product_id:#06x} should be admissible"
            );
            assert!(evaluate_profile(true, &wacom_profile_for(&snapshot), &snapshot).is_ok());
        }
    }

    #[test]
    fn a_non_hid_interface_is_refused_even_under_wacoms_vendor_id() {
        // A vendor id is a claim, not a credential. Mass storage, a hub and a
        // smartcard reader must not become bridgeable by asserting 056a.
        for class in [0x08_u8, 0x09, 0x0b, 0x01, 0xe0] {
            let snapshot = snapshot_with(
                0x0317,
                0x00,
                &[(0, 0x03, 0x00, 0x00), (1, class, 0x00, 0x00)],
            );
            assert!(
                admissible_tablet(&snapshot).is_err(),
                "interface class {class:#04x} should be refused"
            );
        }
    }

    #[test]
    fn a_non_hid_device_class_is_refused() {
        let snapshot = snapshot_with(0x0317, 0x08, &[(0, 0x03, 0x00, 0x00)]);
        assert!(admissible_tablet(&snapshot).is_err());
    }

    #[test]
    fn a_device_without_interfaces_is_refused() {
        let snapshot = snapshot_with(0x0317, 0x00, &[]);
        assert!(admissible_tablet(&snapshot).is_err());
    }

    #[test]
    fn prohibited_classes_are_still_refused_by_the_shared_policy() {
        // `wacom_profile_for` is derived from the device, so this proves the
        // shared prohibited-class rules remain in the path and are not made
        // vacuous by deriving the profile.
        let snapshot = snapshot_with(0x0317, 0x09, &[(0, 0x03, 0x00, 0x00)]);
        assert!(evaluate_profile(true, &wacom_profile_for(&snapshot), &snapshot).is_err());
    }

    #[test]
    fn speed_codes_are_stable() {
        assert_eq!(speed_code(UsbSpeed::Low), 0);
        assert_eq!(speed_code(UsbSpeed::Full), 1);
        assert_eq!(speed_code(UsbSpeed::High), 2);
    }

    fn generation() -> AttachmentGeneration {
        AttachmentGeneration::new(std::num::NonZeroU64::new(7).expect("nonzero"))
    }

    fn urb(urb_id: u32) -> UrbId {
        UrbId::new(std::num::NonZeroU32::new(urb_id).expect("nonzero urb id"))
    }

    fn submit_header(urb_id: u32) -> UsbUrbSubmitHeader {
        UsbUrbSubmitHeader {
            generation: generation(),
            urb_id: urb(urb_id),
            endpoint: EndpointAddress(0x81),
            transfer_kind: TransferKind::Interrupt,
            timeout_ms: 100,
            declared_length: 0,
            setup: None,
        }
    }

    #[test]
    fn an_in_flight_urb_id_is_not_admitted_twice() {
        let mut state = UrbState::default();
        let flag = Arc::new(AtomicBool::new(false));
        assert!(admit_submission(&mut state, submit_header(1), &flag).is_ok());
        assert!(admit_submission(&mut state, submit_header(1), &flag).is_err());
    }

    /// The submit/cancel loop that used to spawn workers without bound. After a
    /// cancellation the id is gone from `in_flight`, so the in-flight ceiling
    /// alone would have re-admitted it immediately while its worker was still
    /// running. The tombstone must refuse it until that worker reports back.
    #[test]
    fn a_cancelled_urb_id_is_not_readmitted_while_its_worker_runs() {
        let mut state = UrbState::default();
        let flag = Arc::new(AtomicBool::new(false));
        admit_submission(&mut state, submit_header(1), &flag).expect("first submit");
        record_cancellation(&mut state, generation(), urb(1)).expect("cancel");

        assert!(
            admit_submission(&mut state, submit_header(1), &flag).is_err(),
            "a tombstoned id must not start a second worker"
        );

        // The late completion clears the tombstone, and only then may the id
        // be used again — which is what keeps long sessions working.
        state.cancelled.remove(&urb(1));
        assert!(admit_submission(&mut state, submit_header(1), &flag).is_ok());
    }

    /// The real bound, driven through the real admission and cancellation
    /// paths rather than by editing the ledger directly.
    ///
    /// A client that submits and immediately cancels, with a fresh id every
    /// time, is not stopped by the tombstone — each id is new. It is stopped by
    /// the tombstone *ceiling*, because every cancelled worker keeps its
    /// tombstone until it reports back. So the number of workers such a loop
    /// can leave running is bounded, and that is the property worth pinning:
    /// unbounded thread creation inside a root process is the thing being
    /// prevented.
    #[test]
    fn a_submit_cancel_loop_cannot_start_unbounded_workers() {
        let mut state = UrbState::default();
        let flag = Arc::new(AtomicBool::new(false));
        let mut live_workers = 0_usize;
        for urb_id in 1..=(MAX_IN_FLIGHT_URBS as u32 * 8) {
            if admit_submission(&mut state, submit_header(urb_id), &flag).is_err() {
                break;
            }
            // A worker is now running. Cancel it at once, as the attack would.
            if record_cancellation(&mut state, generation(), urb(urb_id)).is_err() {
                // The session dies here; the worker still counts.
                live_workers += 1;
                break;
            }
            live_workers += 1;
        }
        assert!(
            live_workers <= MAX_IN_FLIGHT_URBS + 1,
            "a submit/cancel loop left {live_workers} workers running; \
             the ceiling is {MAX_IN_FLIGHT_URBS}"
        );
    }

    /// And the same loop reusing one id is stopped immediately, after a single
    /// worker.
    #[test]
    fn a_submit_cancel_loop_on_one_id_starts_exactly_one_worker() {
        let mut state = UrbState::default();
        let flag = Arc::new(AtomicBool::new(false));
        let mut admitted = 0_usize;
        for _ in 0..1_000 {
            if admit_submission(&mut state, submit_header(1), &flag).is_err() {
                continue;
            }
            admitted += 1;
            record_cancellation(&mut state, generation(), urb(1)).expect("cancel");
        }
        assert_eq!(
            admitted, 1,
            "one id may only ever have one worker at a time"
        );
    }

    #[test]
    fn the_in_flight_ceiling_is_enforced() {
        let mut state = UrbState::default();
        let flag = Arc::new(AtomicBool::new(false));
        for urb_id in 1..=MAX_IN_FLIGHT_URBS as u32 {
            admit_submission(&mut state, submit_header(urb_id), &flag).expect("under the ceiling");
        }
        assert!(admit_submission(&mut state, submit_header(9999), &flag).is_err());
    }

    #[test]
    fn a_second_generation_is_refused() {
        let mut state = UrbState::default();
        let flag = Arc::new(AtomicBool::new(false));
        admit_submission(&mut state, submit_header(1), &flag).expect("first generation binds");
        let mut other = submit_header(2);
        other.generation =
            AttachmentGeneration::new(std::num::NonZeroU64::new(8).expect("nonzero"));
        assert!(admit_submission(&mut state, other, &flag).is_err());
    }
}
