// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A tablet bound to Microsoft's in-box `WinUSB`, as a shared
//! [`CapturedUsbDevice`]. Every transfer runs overlapped on its own thread
//! and completes exactly once; cancellation and deadlines use `CancelIoEx`
//! on that one transfer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use arcen_usb_bridge::captured_facts::{
    DEVICE_DESCRIPTOR_LENGTH, captured_facts_from_descriptors, configuration_total_length,
    hid_report_lengths,
};
use arcen_usb_bridge::{
    BackendRefusal, BackendStatus, CapturedDeviceFacts, CapturedUsbDevice, ControlOperation,
    EndpointAddress, InterfaceReportDescriptor, MAX_HID_REPORT_DESCRIPTOR_BYTES, PendingCompletion,
    ReleaseOutcome, ReleaseReason, UrbId, UsbSpeed,
};
use windows::Win32::Devices::Usb::{
    ALLOW_PARTIAL_READS, DEVICE_SPEED, PIPE_TRANSFER_TIMEOUT, USB_CONFIGURATION_DESCRIPTOR_TYPE,
    USB_DEVICE_DESCRIPTOR_TYPE, USB_INTERFACE_DESCRIPTOR, WINUSB_INTERFACE_HANDLE,
    WINUSB_SETUP_PACKET, WinUsb_ControlTransfer, WinUsb_Free, WinUsb_GetAssociatedInterface,
    WinUsb_GetDescriptor, WinUsb_GetOverlappedResult, WinUsb_Initialize,
    WinUsb_QueryDeviceInformation, WinUsb_QueryInterfaceSettings, WinUsb_ReadPipe,
    WinUsb_ResetPipe, WinUsb_SetCurrentAlternateSetting, WinUsb_SetPipePolicy, WinUsb_WritePipe,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_BAD_COMMAND, ERROR_DEVICE_NOT_CONNECTED, ERROR_FILE_NOT_FOUND,
    ERROR_GEN_FAILURE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED, ERROR_SEM_TIMEOUT, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{GUID, PCWSTR};

use super::wide;

/// The device interface the Arcen `WinUSB` INF registers.
pub(crate) const INTERFACE_GUID: GUID = GUID::from_u128(0x0becb574_029e_4ab9_89dd_df6903e0c352);

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
/// How long release waits for transfer threads to let go of the handles.
const RELEASE_DRAIN: Duration = Duration::from_secs(2);
/// How often (ms) a waiting transfer looks at its cancellation flag.
const POLL_MILLIS: u32 = 50;

/// A raw handle shared between transfer threads. `WinUSB` allows concurrent
/// overlapped transfers on one interface handle.
#[derive(Clone, Copy)]
struct Interface(WINUSB_INTERFACE_HANDLE);
// SAFETY: WinUSB interface handles may be used from any thread; their
// lifetime is owned by `Handles`, which outlives every transfer thread.
unsafe impl Send for Interface {}
// SAFETY: as above; WinUSB serialises what it must internally.
unsafe impl Sync for Interface {}

#[derive(Clone, Copy)]
struct File(HANDLE);
// SAFETY: a file handle is a kernel object reference usable from any thread.
unsafe impl Send for File {}
// SAFETY: as above.
unsafe impl Sync for File {}

/// The open device and its interface handles, closed when the last transfer
/// thread and the device let go.
struct Handles {
    file: File,
    /// By interface number; the first is the one `WinUsb_Initialize` gave.
    interfaces: Vec<(u8, Interface)>,
}

impl Drop for Handles {
    fn drop(&mut self) {
        for (_, interface) in self.interfaces.iter().rev() {
            // SAFETY: each handle came from WinUsb_Initialize or
            // WinUsb_GetAssociatedInterface and is freed once.
            let _ = unsafe { WinUsb_Free(interface.0) };
        }
        // SAFETY: opened by CreateFileW, closed once.
        let _ = unsafe { CloseHandle(self.file.0) };
    }
}

impl Handles {
    fn interface(&self, number: u8) -> Option<Interface> {
        self.interfaces
            .iter()
            .find(|(candidate, _)| *candidate == number)
            .map(|(_, handle)| *handle)
    }

    fn first(&self) -> Interface {
        self.interfaces[0].1
    }
}

/// What runs when the device is released: the claim's driver restore.
pub(crate) type ReleaseHook = Box<dyn FnOnce(ReleaseReason) -> Result<(), String> + Send>;

/// One tablet captured through `WinUSB`.
pub(crate) struct WinUsbDevice {
    handles: Option<Arc<Handles>>,
    facts: CapturedDeviceFacts,
    /// Endpoint address → owning interface number.
    endpoints: HashMap<u8, u8>,
    release_hook: Mutex<Option<ReleaseHook>>,
}

impl std::fmt::Debug for WinUsbDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WinUsbDevice")
            .field("id", &self.facts.snapshot.id)
            .finish_non_exhaustive()
    }
}

fn status_of(error: &windows::core::Error) -> BackendStatus {
    let code = error.code();
    if code == ERROR_OPERATION_ABORTED.to_hresult() {
        BackendStatus::Cancelled
    } else if code == ERROR_SEM_TIMEOUT.to_hresult() {
        BackendStatus::TimedOut
    } else if code == ERROR_GEN_FAILURE.to_hresult() {
        // WinUSB reports a STALL handshake as a general failure.
        BackendStatus::Stall
    } else if code == ERROR_DEVICE_NOT_CONNECTED.to_hresult()
        || code == ERROR_BAD_COMMAND.to_hresult()
        || code == ERROR_FILE_NOT_FOUND.to_hresult()
    {
        BackendStatus::Disconnected
    } else {
        BackendStatus::Io
    }
}

impl WinUsbDevice {
    /// Opens the `WinUSB` device at `path`, reads its descriptors itself and
    /// builds the facts the shared engine will verify. `expected` is the
    /// prepared model's (vendor, product).
    pub(crate) fn open(
        path: &str,
        expected: (u16, u16),
        release_hook: ReleaseHook,
    ) -> Result<Self, String> {
        let path_w = wide(path);
        // SAFETY: opens the device interface path for overlapped I/O.
        let file = unsafe {
            CreateFileW(
                PCWSTR(path_w.as_ptr()),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                HANDLE::default(),
            )
        }
        .map_err(|error| format!("open the WinUSB device: {error}"))?;
        let mut first = WINUSB_INTERFACE_HANDLE::default();
        // SAFETY: initialises WinUSB on the open device.
        if let Err(error) = unsafe { WinUsb_Initialize(file, &raw mut first) } {
            // SAFETY: opened above.
            let _ = unsafe { CloseHandle(file) };
            return Err(format!("WinUsb_Initialize: {error}"));
        }
        let mut handles = Handles {
            file: File(file),
            interfaces: vec![(interface_number(first).unwrap_or(0), Interface(first))],
        };
        for index in 0..=u8::MAX {
            let mut associated = WINUSB_INTERFACE_HANDLE::default();
            // SAFETY: asks for the next associated interface of the device.
            if unsafe { WinUsb_GetAssociatedInterface(first, index, &raw mut associated) }.is_err()
            {
                break;
            }
            let number = interface_number(associated).unwrap_or(index.saturating_add(1));
            handles.interfaces.push((number, Interface(associated)));
        }
        let handles = Arc::new(handles);
        let first = handles.first();

        let mut device = [0_u8; DEVICE_DESCRIPTOR_LENGTH];
        read_descriptor(first, USB_DEVICE_DESCRIPTOR_TYPE, &mut device)?;
        let mut header = [0_u8; 9];
        read_descriptor(first, USB_CONFIGURATION_DESCRIPTOR_TYPE, &mut header)?;
        let total = configuration_total_length(&header)?;
        let mut configuration = vec![0_u8; total];
        read_descriptor(first, USB_CONFIGURATION_DESCRIPTOR_TYPE, &mut configuration)?;

        let report_descriptors = read_report_descriptors(&handles, &configuration)?;
        let facts = captured_facts_from_descriptors(
            &device,
            &configuration,
            speed(first),
            expected,
            report_descriptors,
        )?;
        let endpoints = endpoint_owners(&handles, &facts);
        Ok(Self {
            handles: Some(handles),
            facts,
            endpoints,
            release_hook: Mutex::new(Some(release_hook)),
        })
    }
    fn handle_for_endpoint(&self, endpoint: EndpointAddress) -> Option<(Arc<Handles>, Interface)> {
        let handles = self.handles.as_ref()?;
        let interface = *self.endpoints.get(&endpoint.0)?;
        Some((Arc::clone(handles), handles.interface(interface)?))
    }

    fn release_once(&mut self, reason: ReleaseReason) -> ReleaseOutcome {
        // Close the handles first: the driver swap needs WinUSB unbound.
        // Transfer threads hold them until their I/O ends, so cancel all of
        // it and give them a moment to let go.
        if let Some(handles) = self.handles.take() {
            // SAFETY: cancels every transfer this process issued on the file.
            let _ = unsafe { CancelIoEx(handles.file.0, None) };
            let started = Instant::now();
            while Arc::strong_count(&handles) > 1 && started.elapsed() < RELEASE_DRAIN {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(handles);
        }
        let hook = self
            .release_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match hook.map(|hook| hook(reason)) {
            None | Some(Ok(())) => ReleaseOutcome::Restored,
            Some(Err(error)) => ReleaseOutcome::Failed(error),
        }
    }
}

fn read_report_descriptors(
    handles: &Handles,
    configuration: &[u8],
) -> Result<Vec<InterfaceReportDescriptor>, String> {
    let first = handles.first();
    let mut report_descriptors = Vec::new();
    for (interface, length) in hid_report_lengths(configuration) {
        let length = usize::from(length).min(MAX_HID_REPORT_DESCRIPTOR_BYTES);
        let handle = handles.interface(interface).unwrap_or(first);
        let mut buffer = vec![0_u8; length];
        let setup = WINUSB_SETUP_PACKET {
            RequestType: 0x81,
            Request: 0x06,
            Value: 0x2200,
            Index: u16::from(interface),
            Length: u16::try_from(length).unwrap_or(u16::MAX),
        };
        let mut transferred = 0_u32;
        // SAFETY: a synchronous control transfer into `buffer`.
        unsafe {
            WinUsb_ControlTransfer(
                handle.0,
                setup,
                Some(&mut buffer),
                Some(&raw mut transferred),
                None,
            )
        }
        .map_err(|error| format!("read the report descriptor of interface {interface}: {error}"))?;
        buffer.truncate(transferred as usize);
        report_descriptors.push(InterfaceReportDescriptor {
            interface,
            descriptor: buffer,
        });
    }
    Ok(report_descriptors)
}

/// Endpoint address → owning interface, with each interrupt pipe set to wait
/// without a deadline and to accept short reads.
fn endpoint_owners(handles: &Handles, facts: &CapturedDeviceFacts) -> HashMap<u8, u8> {
    let mut endpoints = HashMap::new();
    for interface in &facts.snapshot.configuration.interfaces {
        for endpoint in &interface.endpoints {
            endpoints.insert(endpoint.address.0, interface.number.0);
            if let Some(handle) = handles.interface(interface.number.0) {
                let infinite = 0_u32;
                let partial = 1_u8;
                // SAFETY: plain pipe policies on a pipe this interface owns.
                unsafe {
                    let _ = WinUsb_SetPipePolicy(
                        handle.0,
                        endpoint.address.0,
                        PIPE_TRANSFER_TIMEOUT,
                        4,
                        (&raw const infinite).cast(),
                    );
                    let _ = WinUsb_SetPipePolicy(
                        handle.0,
                        endpoint.address.0,
                        ALLOW_PARTIAL_READS,
                        1,
                        (&raw const partial).cast(),
                    );
                }
            }
        }
    }
    endpoints
}

fn interface_number(handle: WINUSB_INTERFACE_HANDLE) -> Option<u8> {
    let mut descriptor = USB_INTERFACE_DESCRIPTOR::default();
    // SAFETY: reads alternate setting 0's interface descriptor.
    unsafe { WinUsb_QueryInterfaceSettings(handle, 0, &raw mut descriptor) }.ok()?;
    Some(descriptor.bInterfaceNumber)
}

fn read_descriptor(interface: Interface, kind: u32, buffer: &mut [u8]) -> Result<(), String> {
    let mut transferred = 0_u32;
    // SAFETY: a synchronous descriptor read into `buffer`.
    unsafe {
        WinUsb_GetDescriptor(
            interface.0,
            u8::try_from(kind).unwrap_or(0),
            0,
            0,
            Some(buffer),
            &raw mut transferred,
        )
    }
    .map_err(|error| format!("read descriptor {kind}: {error}"))?;
    if (transferred as usize) < buffer.len() {
        return Err(format!("descriptor {kind} was short ({transferred} bytes)"));
    }
    Ok(())
}

fn speed(interface: Interface) -> Option<UsbSpeed> {
    let mut value = 0_u8;
    let mut length = 1_u32;
    // SAFETY: DEVICE_SPEED returns one byte.
    unsafe {
        WinUsb_QueryDeviceInformation(
            interface.0,
            DEVICE_SPEED,
            &raw mut length,
            (&raw mut value).cast(),
        )
    }
    .ok()?;
    match value {
        // usb.h: LowSpeed 1, FullSpeed 2, HighSpeed 3.
        1 => Some(UsbSpeed::Low),
        2 => Some(UsbSpeed::Full),
        3 => Some(UsbSpeed::High),
        _ => None,
    }
}

/// One overlapped transfer: starts it, waits for it while honouring
/// cancellation and an optional deadline, and returns the outcome.
fn overlapped(
    handles: &Handles,
    interface: Interface,
    completion: &PendingCompletion,
    deadline: Option<Instant>,
    start: impl FnOnce(*const OVERLAPPED) -> windows::core::Result<()>,
) -> (BackendStatus, u32) {
    // SAFETY: a manual-reset, initially unsignalled event.
    let Ok(event) = (unsafe { CreateEventW(None, true, false, PCWSTR::null()) }) else {
        return (BackendStatus::Io, 0);
    };
    let overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    let started = start(&raw const overlapped);
    let outcome = match started {
        Err(error) if error.code() != ERROR_IO_PENDING.to_hresult() => (status_of(&error), 0),
        _ => {
            let mut cancelled_by = None;
            loop {
                // SAFETY: waits on our own event.
                if unsafe { WaitForSingleObject(event, POLL_MILLIS) } == WAIT_OBJECT_0 {
                    break;
                }
                let reason = if completion.is_cancelled() {
                    Some(BackendStatus::Cancelled)
                } else if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    Some(BackendStatus::TimedOut)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    if cancelled_by.is_none() {
                        cancelled_by = Some(reason);
                        // SAFETY: cancels exactly this transfer.
                        let _ = unsafe { CancelIoEx(handles.file.0, Some(&raw const overlapped)) };
                    }
                }
            }
            let mut transferred = 0_u32;
            // SAFETY: the transfer has completed; collect its result.
            match unsafe {
                WinUsb_GetOverlappedResult(
                    interface.0,
                    &raw const overlapped,
                    &raw mut transferred,
                    true,
                )
            } {
                Ok(()) => (BackendStatus::Completed, transferred),
                Err(error) => (
                    cancelled_by
                        .filter(|_| error.code() == ERROR_OPERATION_ABORTED.to_hresult())
                        .unwrap_or_else(|| status_of(&error)),
                    0,
                ),
            }
        }
    };
    // SAFETY: created above; the transfer is no longer in flight.
    let _ = unsafe { CloseHandle(event) };
    outcome
}

impl CapturedUsbDevice for WinUsbDevice {
    fn facts(&self) -> &CapturedDeviceFacts {
        &self.facts
    }

    fn submit_control(
        &self,
        operation: ControlOperation,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        let Some(handles) = self.handles.clone() else {
            completion.refuse(BackendRefusal::Disconnected);
            return;
        };
        match operation {
            ControlOperation::Transfer {
                setup,
                data,
                in_length,
            } => {
                // WinUSB rewrites wIndex for interface and endpoint
                // recipients to the handle's own: use that handle.
                let interface = match setup.request_type & 0x1f {
                    1 => handles.interface((setup.index & 0xff) as u8),
                    2 => self
                        .endpoints
                        .get(&((setup.index & 0xff) as u8))
                        .and_then(|number| handles.interface(*number)),
                    _ => Some(handles.first()),
                };
                let Some(interface) = interface else {
                    completion.refuse(BackendRefusal::UnknownTarget);
                    return;
                };
                std::thread::spawn(move || {
                    let packet = WINUSB_SETUP_PACKET {
                        RequestType: setup.request_type,
                        Request: setup.request,
                        Value: setup.value,
                        Index: setup.index,
                        Length: setup.length,
                    };
                    let is_in = setup.request_type & 0x80 != 0;
                    let mut buffer = if is_in { vec![0_u8; in_length] } else { data };
                    let deadline = Some(Instant::now() + timeout.max(Duration::from_millis(1)));
                    let (status, transferred) =
                        overlapped(&handles, interface, &completion, deadline, |ov| {
                            // SAFETY: `buffer` outlives the transfer, which
                            // `overlapped` waits for before returning.
                            unsafe {
                                WinUsb_ControlTransfer(
                                    interface.0,
                                    packet,
                                    (!buffer.is_empty()).then_some(buffer.as_mut_slice()),
                                    None,
                                    Some(ov),
                                )
                            }
                        });
                    let data = if is_in && status == BackendStatus::Completed {
                        buffer.truncate(transferred as usize);
                        buffer
                    } else {
                        Vec::new()
                    };
                    completion.complete(status, data);
                });
            }
            ControlOperation::SelectAlternateSetting {
                interface,
                alternate,
            } => {
                let Some(handle) = handles.interface(interface.0) else {
                    completion.refuse(BackendRefusal::UnknownTarget);
                    return;
                };
                // SAFETY: SET_INTERFACE through WinUSB on that interface.
                let status =
                    match unsafe { WinUsb_SetCurrentAlternateSetting(handle.0, alternate.0) } {
                        Ok(()) => BackendStatus::Completed,
                        Err(error) => status_of(&error),
                    };
                completion.complete(status, Vec::new());
            }
            ControlOperation::ClearHalt { endpoint } => {
                let Some((_, handle)) = self.handle_for_endpoint(endpoint) else {
                    completion.refuse(BackendRefusal::UnknownTarget);
                    return;
                };
                // SAFETY: CLEAR_FEATURE(ENDPOINT_HALT) + data toggle reset.
                let status = match unsafe { WinUsb_ResetPipe(handle.0, endpoint.0) } {
                    Ok(()) => BackendStatus::Completed,
                    Err(error) => status_of(&error),
                };
                completion.complete(status, Vec::new());
            }
        }
    }

    fn submit_interrupt_in(
        &self,
        endpoint: EndpointAddress,
        length: usize,
        completion: PendingCompletion,
    ) {
        let Some((handles, interface)) = self.handle_for_endpoint(endpoint) else {
            completion.refuse(BackendRefusal::UnknownTarget);
            return;
        };
        std::thread::spawn(move || {
            let mut buffer = vec![0_u8; length];
            // No deadline: the transfer stays pending until the tablet
            // reports, it is cancelled, or the device goes away.
            let (status, transferred) = overlapped(&handles, interface, &completion, None, |ov| {
                // SAFETY: `buffer` outlives the transfer (see `overlapped`).
                unsafe {
                    WinUsb_ReadPipe(
                        interface.0,
                        endpoint.0,
                        Some(buffer.as_mut_slice()),
                        None,
                        Some(ov),
                    )
                }
            });
            buffer.truncate(transferred as usize);
            completion.complete(
                status,
                if status == BackendStatus::Completed {
                    buffer
                } else {
                    Vec::new()
                },
            );
        });
    }

    fn submit_interrupt_out(
        &self,
        endpoint: EndpointAddress,
        data: Vec<u8>,
        timeout: Duration,
        completion: PendingCompletion,
    ) {
        let Some((handles, interface)) = self.handle_for_endpoint(endpoint) else {
            completion.refuse(BackendRefusal::UnknownTarget);
            return;
        };
        std::thread::spawn(move || {
            let deadline = Some(Instant::now() + timeout.max(Duration::from_millis(1)));
            let (status, _) = overlapped(&handles, interface, &completion, deadline, |ov| {
                // SAFETY: `data` outlives the transfer (see `overlapped`).
                unsafe { WinUsb_WritePipe(interface.0, endpoint.0, &data, None, Some(ov)) }
            });
            completion.complete(status, Vec::new());
        });
    }

    fn cancel(&self, _urb: UrbId) {
        // Each transfer thread polls its completion's cancellation flag,
        // which the exporter has already raised, and cancels its own I/O.
    }

    fn release(mut self: Box<Self>, reason: ReleaseReason) -> ReleaseOutcome {
        self.release_once(reason)
    }
}

impl Drop for WinUsbDevice {
    fn drop(&mut self) {
        let _ = self.release_once(ReleaseReason::Error);
    }
}
