// SPDX-License-Identifier: AGPL-3.0-only
//! Session-bound microphone-v1 ingress and Mach delivery to the HAL plug-in.

#![allow(unsafe_code)]
#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::expect_used,
    clippy::unwrap_used
)]

use std::ffi::c_void;
use std::ffi::{CStr, CString};
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use arcen_media::audio::{
    MICROPHONE_V1_FRAME_SAMPLES, MicrophoneDecodeError, MicrophoneDecoder, MicrophoneFrameOutput,
    MicrophoneIngestOutcome, MicrophoneStats, MicrophoneStatsTracker, ResolvedMicrophoneStream,
};
use arcen_protocol::decode_microphone_frame;
use arcen_telemetry::CorrelationId;
use mach2::bootstrap::{bootstrap_check_in, bootstrap_look_up, bootstrap_port};
use mach2::message::{
    MACH_MSG_SUCCESS, MACH_MSG_TRAILER_FORMAT_0, MACH_MSG_TYPE_COPY_SEND,
    MACH_MSG_TYPE_MAKE_SEND_ONCE, MACH_MSG_TYPE_MOVE_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE,
    MACH_MSGH_BITS, MACH_RCV_MSG, MACH_RCV_TIMEOUT, MACH_RCV_TRAILER_AUDIT, MACH_SEND_MSG,
    MACH_SEND_TIMEOUT, audit_token_t, mach_msg, mach_msg_audit_trailer_t, mach_msg_destroy,
    mach_msg_header_t,
};
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use zeroize::Zeroize;

pub const MACH_SERVICE: &str = "tech.arcen.microphone";
const DRIVER_BUNDLE: &str = "/Library/Audio/Plug-Ins/HAL/ArcenMicrophone.driver";
const MSG_REGISTER_DRIVER: i32 = 0x4152_4301;
const MSG_FRAME: i32 = 0x4152_4302;
const MSG_CLEAR: i32 = 0x4152_4303;
const MSG_PROBE: i32 = 0x4152_4304;
const MSG_ADMIT_SESSION: i32 = 0x4152_4305;
const MSG_HEARTBEAT: i32 = 0x4152_4306;
const DEVICE_LIFECYCLE_DEADLINE: Duration = Duration::from_millis(250);
const MACH_SEND_DEADLINE_MS: u32 = 10;
const MACH_PROBE_DEADLINE_MS: u32 = 100;
const MACH_RCV_TRAILER_AUDIT_OPTIONS: i32 =
    ((MACH_MSG_TRAILER_FORMAT_0 as i32) << 28) | ((MACH_RCV_TRAILER_AUDIT as i32) << 24);
const MAILBOX_FRAMES: usize = 2;
const CONTROL_DEADLINE: Duration = Duration::from_millis(250);
const SESSION_LEASE: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
// Must match packaging/macos/validate_release_inputs.py and the Pier signing profile.
const ARCEN_TEAM_ID: &str = "NWR7ZH8L7U";
const PIER_BUNDLE_ID: &str = "pier.arcen.tech";
const AGENT_BUNDLE_ID: &str = "pier.arcen.tech.agent";
static TOKEN_COUNTER: AtomicU32 = AtomicU32::new(1);

unsafe extern "C" {
    fn audit_token_to_euid(token: audit_token_t) -> libc::uid_t;
    fn audit_token_to_pid(token: audit_token_t) -> libc::pid_t;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
    #[cfg(test)]
    fn mach_port_get_refs(
        task: mach2::mach_types::ipc_space_t,
        name: mach_port_t,
        right: mach2::port::mach_port_right_t,
        refs: *mut u32,
    ) -> mach2::kern_return::kern_return_t;
}

type OSStatus = libc::c_int;
type SecCSFlags = u32;
type CFIndex = isize;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDataRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type SecCodeRef = *const c_void;
type SecRequirementRef = *const c_void;
type AudioObjectID = u32;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_AUDIO_OBJECT_SYSTEM_OBJECT: AudioObjectID = 1;
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = 0x676c_6f62;
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_AUDIO_HARDWARE_PROPERTY_DEVICE_FOR_UID: u32 = 0x6475_6964;
const K_AUDIO_OBJECT_UNKNOWN: AudioObjectID = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
struct AudioValueTranslation {
    input_data: *mut c_void,
    input_data_size: u32,
    output_data: *mut c_void,
    output_data_size: u32,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;

    fn CFDataCreate(allocator: CFAllocatorRef, bytes: *const u8, length: CFIndex) -> CFDataRef;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *mut *const c_void,
        values: *mut *const c_void,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
    fn CFRelease(value: CFTypeRef);
    fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        c_str: *const libc::c_char,
        encoding: u32,
    ) -> CFStringRef;
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecGuestAttributeAudit: CFStringRef;

    fn SecCodeCheckValidity(
        code: SecCodeRef,
        flags: SecCSFlags,
        requirement: SecRequirementRef,
    ) -> OSStatus;
    #[cfg(debug_assertions)]
    fn SecCodeCopyDesignatedRequirement(
        code: SecCodeRef,
        flags: SecCSFlags,
        requirement: *mut SecRequirementRef,
    ) -> OSStatus;
    fn SecCodeCopyGuestWithAttributes(
        host: SecCodeRef,
        attributes: CFDictionaryRef,
        flags: SecCSFlags,
        guest: *mut SecCodeRef,
    ) -> OSStatus;
    #[cfg(debug_assertions)]
    fn SecCodeCopySelf(flags: SecCSFlags, code: *mut SecCodeRef) -> OSStatus;
    fn SecRequirementCreateWithString(
        text: CFStringRef,
        flags: SecCSFlags,
        requirement: *mut SecRequirementRef,
    ) -> OSStatus;
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object_id: AudioObjectID,
        address: *const AudioObjectPropertyAddress,
        qualifier_data_size: u32,
        qualifier_data: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> OSStatus;
}

struct CfRef {
    ptr: NonNull<c_void>,
}

impl CfRef {
    fn new(ptr: *const c_void) -> Option<Self> {
        NonNull::new(ptr.cast_mut()).map(|ptr| Self { ptr })
    }

    fn as_ptr(&self) -> *const c_void {
        self.ptr.as_ptr().cast_const()
    }
}

impl Drop for CfRef {
    fn drop(&mut self) {
        // SAFETY: CfRef is constructed only from Create/Copy-rule CoreFoundation objects.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

#[derive(Debug)]
struct MachSendRight {
    name: mach_port_t,
}

impl MachSendRight {
    fn new(name: mach_port_t) -> Option<Self> {
        (name != MACH_PORT_NULL).then_some(Self { name })
    }

    fn as_name(&self) -> mach_port_t {
        self.name
    }
}

impl Drop for MachSendRight {
    fn drop(&mut self) {
        if self.name != MACH_PORT_NULL {
            // SAFETY: name is a send right owned by this RAII wrapper.
            unsafe {
                let _ = mach2::mach_port::mach_port_deallocate(
                    mach2::traps::mach_task_self(),
                    self.name,
                );
            }
            self.name = MACH_PORT_NULL;
        }
    }
}

#[derive(Debug)]
struct MachReceiveRight {
    name: mach_port_t,
}

impl MachReceiveRight {
    fn allocate() -> Result<Self, MicrophoneDeviceError> {
        let mut name = MACH_PORT_NULL;
        // SAFETY: mach_task_self is the current task port; name points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach2::traps::mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &mut name,
            )
        };
        if allocated == mach2::kern_return::KERN_SUCCESS {
            Ok(Self { name })
        } else {
            Err(MicrophoneDeviceError::DeviceUnavailable)
        }
    }

    fn as_name(&self) -> mach_port_t {
        self.name
    }
}

impl Drop for MachReceiveRight {
    fn drop(&mut self) {
        if self.name != MACH_PORT_NULL {
            // SAFETY: name is a receive right owned by this RAII wrapper.
            unsafe {
                let _ =
                    mach2::mach_port::mach_port_destroy(mach2::traps::mach_task_self(), self.name);
            }
            self.name = MACH_PORT_NULL;
        }
    }
}

#[cfg(test)]
fn send_refs(port: mach_port_t) -> u32 {
    port_refs(port, mach2::port::MACH_PORT_RIGHT_SEND)
}

#[cfg(test)]
fn send_once_refs(port: mach_port_t) -> u32 {
    port_refs(port, mach2::port::MACH_PORT_RIGHT_SEND_ONCE)
}

#[cfg(test)]
fn port_refs(port: mach_port_t, right: mach2::port::mach_port_right_t) -> u32 {
    let mut refs = 0;
    // SAFETY: refs points to writable storage and port is a name in the current task.
    let status =
        unsafe { mach_port_get_refs(mach2::traps::mach_task_self(), port, right, &mut refs) };
    if status == mach2::kern_return::KERN_SUCCESS {
        refs
    } else {
        0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophoneSessionBinding {
    pub generation: u32,
}

impl MicrophoneSessionBinding {
    pub fn new(generation: u32) -> Result<Self, MicrophoneDeviceError> {
        if generation == 0 {
            return Err(MicrophoneDeviceError::InvalidBinding);
        }
        Ok(Self { generation })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneDeviceError {
    InvalidBinding,
    AccessDenied,
    StaleGeneration,
    Backpressure,
    Timeout,
    WorkerFailed,
    DeviceUnavailable,
    DeviceRemoved,
    FatalCleanup,
}

impl MicrophoneDeviceError {
    pub const fn is_fatal_cleanup(self) -> bool {
        matches!(self, Self::WorkerFailed | Self::FatalCleanup)
    }
}

pub trait MicrophoneDevice {
    fn write_frame(
        &mut self,
        binding: &MicrophoneSessionBinding,
        frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> Result<(), MicrophoneDeviceError>;

    fn clear(&mut self, binding: &MicrophoneSessionBinding) -> Result<(), MicrophoneDeviceError>;
}

#[must_use]
pub fn backend_available() -> bool {
    std::path::Path::new(DRIVER_BUNDLE).exists()
        && hal_device_published()
        && probe_service().is_ok()
}

pub async fn backend_available_if_enabled(
    operator_enabled: bool,
) -> Result<bool, MicrophoneDeviceError> {
    if !operator_enabled {
        return Ok(false);
    }
    Ok(backend_available())
}

#[derive(Debug)]
pub struct NativeMicrophoneDevice {
    sender: mpsc::SyncSender<DeviceCommand>,
    control: mpsc::SyncSender<ControlCommand>,
    sequence: AtomicU32,
    accepting: Arc<AtomicBool>,
    token: u64,
}

#[derive(Debug)]
enum DeviceCommand {
    Frame(FrameMessage),
}

#[derive(Debug)]
enum ControlCommand {
    Clear(
        FrameMessage,
        mpsc::SyncSender<Result<(), MicrophoneDeviceError>>,
    ),
    Stop(mpsc::SyncSender<Result<(), MicrophoneDeviceError>>),
}

impl NativeMicrophoneDevice {
    pub fn open(binding: &MicrophoneSessionBinding) -> Result<Self, MicrophoneDeviceError> {
        let token = admit_session(binding.generation)?;
        let service_port = lookup_service()?;
        let (sender, receiver) = mpsc::sync_channel(MAILBOX_FRAMES);
        let (control, control_rx) = mpsc::sync_channel(1);
        let accepting = Arc::new(AtomicBool::new(true));
        let worker_accepting = Arc::clone(&accepting);
        let generation = binding.generation;
        std::thread::Builder::new()
            .name("arcen-microphone-sender".to_owned())
            .spawn(move || {
                device_sender(
                    service_port,
                    receiver,
                    control_rx,
                    worker_accepting,
                    generation,
                    token,
                )
            })
            .map_err(|_| MicrophoneDeviceError::WorkerFailed)?;
        Ok(Self {
            sender,
            control,
            sequence: AtomicU32::new(1),
            accepting,
            token,
        })
    }

    pub async fn shutdown_wait(&mut self) -> Result<(), MicrophoneDeviceError> {
        self.accepting.store(false, Ordering::Release);
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.control
            .send(ControlCommand::Stop(ack_tx))
            .map_err(|_| MicrophoneDeviceError::DeviceRemoved)?;
        wait_for_ack(ack_rx, DEVICE_LIFECYCLE_DEADLINE).await
    }
}

impl MicrophoneDevice for NativeMicrophoneDevice {
    fn write_frame(
        &mut self,
        binding: &MicrophoneSessionBinding,
        frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> Result<(), MicrophoneDeviceError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(MicrophoneDeviceError::DeviceRemoved);
        }
        let sequence = self.sequence.fetch_add(1, Ordering::AcqRel);
        let message = frame_message(
            MACH_PORT_NULL,
            MSG_FRAME,
            sequence,
            binding.generation,
            self.token,
            frame,
        );
        self.sender
            .try_send(DeviceCommand::Frame(message))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => MicrophoneDeviceError::Backpressure,
                mpsc::TrySendError::Disconnected(_) => MicrophoneDeviceError::DeviceRemoved,
            })
    }

    fn clear(&mut self, binding: &MicrophoneSessionBinding) -> Result<(), MicrophoneDeviceError> {
        self.accepting.store(false, Ordering::Release);
        let silence = [0_i16; MICROPHONE_V1_FRAME_SAMPLES];
        let message = frame_message(
            MACH_PORT_NULL,
            MSG_CLEAR,
            0,
            binding.generation,
            self.token,
            &silence,
        );
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.control
            .send(ControlCommand::Clear(message, ack_tx))
            .map_err(|_| MicrophoneDeviceError::DeviceRemoved)?;
        ack_rx
            .recv_timeout(CONTROL_DEADLINE)
            .map_err(|_| MicrophoneDeviceError::Timeout)?
    }
}

fn device_sender(
    service_port: MachSendRight,
    receiver: mpsc::Receiver<DeviceCommand>,
    control: mpsc::Receiver<ControlCommand>,
    accepting: Arc<AtomicBool>,
    generation: u32,
    token: u64,
) {
    let mut last_heartbeat = Instant::now();
    loop {
        while let Ok(command) = control.try_recv() {
            match command {
                ControlCommand::Clear(mut message, ack) => {
                    let result =
                        send_clear_message(service_port.as_name(), &receiver, &mut message);
                    let _ = ack.send(result);
                }
                ControlCommand::Stop(ack) => {
                    accepting.store(false, Ordering::Release);
                    drain_queued_frames(&receiver);
                    let _ = ack.send(Ok(()));
                    return;
                }
            }
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(DeviceCommand::Frame(mut message)) => {
                if !accepting.load(Ordering::Acquire) {
                    continue;
                }
                message.header.msgh_remote_port = service_port.as_name();
                let _ = send_frame_message(&mut message);
                last_heartbeat = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if accepting.load(Ordering::Acquire)
                    && last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL
                {
                    let mut heartbeat = frame_message(
                        service_port.as_name(),
                        MSG_HEARTBEAT,
                        0,
                        generation,
                        token,
                        &[0; MICROPHONE_V1_FRAME_SAMPLES],
                    );
                    let _ = send_frame_message(&mut heartbeat);
                    last_heartbeat = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn drain_queued_frames(receiver: &mpsc::Receiver<DeviceCommand>) {
    while receiver.try_recv().is_ok() {}
}

fn send_clear_message(
    service_port: mach_port_t,
    receiver: &mpsc::Receiver<DeviceCommand>,
    message: &mut FrameMessage,
) -> Result<(), MicrophoneDeviceError> {
    drain_queued_frames(receiver);
    let reply_port = MachReceiveRight::allocate()?;
    message.header.msgh_remote_port = service_port;
    message.header.msgh_local_port = reply_port.as_name();
    message.header.msgh_bits =
        MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE);
    send_frame_message(message)?;
    let reply = receive_control_reply(reply_port.as_name(), MSG_CLEAR)?;
    if reply.generation == message.generation && reply.token == message.token {
        Ok(())
    } else {
        Err(MicrophoneDeviceError::FatalCleanup)
    }
}

async fn wait_for_ack(
    receiver: mpsc::Receiver<Result<(), MicrophoneDeviceError>>,
    deadline: Duration,
) -> Result<(), MicrophoneDeviceError> {
    let until = Instant::now() + deadline;
    loop {
        match receiver.try_recv() {
            Ok(result) => return result,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(MicrophoneDeviceError::DeviceRemoved);
            }
            Err(mpsc::TryRecvError::Empty) => {
                if Instant::now() >= until {
                    return Err(MicrophoneDeviceError::Timeout);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct FrameMessage {
    header: mach_msg_header_t,
    sequence: u32,
    generation: u32,
    token: u64,
    samples: [i16; MICROPHONE_V1_FRAME_SAMPLES],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrameEnvelope {
    message: FrameMessage,
    trailer: mach_msg_audit_trailer_t,
}

fn lookup_service() -> Result<MachSendRight, MicrophoneDeviceError> {
    let service = CString::new(MACH_SERVICE).expect("static service name has no nul");
    let mut service_port = MACH_PORT_NULL;
    // SAFETY: bootstrap_port is provided by libSystem; service is a valid C string.
    let status = unsafe { bootstrap_look_up(bootstrap_port, service.as_ptr(), &mut service_port) };
    if status == mach2::kern_return::KERN_SUCCESS && service_port != MACH_PORT_NULL {
        MachSendRight::new(service_port).ok_or(MicrophoneDeviceError::DeviceUnavailable)
    } else {
        Err(MicrophoneDeviceError::DeviceUnavailable)
    }
}

fn hal_device_published() -> bool {
    let uid = match cf_string("tech.arcen.microphone.input") {
        Some(uid) => uid,
        None => return false,
    };
    let mut uid_ref = uid.as_ptr();
    let mut device = K_AUDIO_OBJECT_UNKNOWN;
    let mut translation = AudioValueTranslation {
        input_data: (&raw mut uid_ref).cast::<c_void>(),
        input_data_size: u32::try_from(size_of::<CFStringRef>()).unwrap_or(u32::MAX),
        output_data: (&raw mut device).cast::<c_void>(),
        output_data_size: u32::try_from(size_of::<AudioObjectID>()).unwrap_or(u32::MAX),
    };
    let mut size = u32::try_from(size_of::<AudioValueTranslation>()).unwrap_or(u32::MAX);
    let address = AudioObjectPropertyAddress {
        selector: K_AUDIO_HARDWARE_PROPERTY_DEVICE_FOR_UID,
        scope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    };
    // SAFETY: arguments point to valid storage and the qualifier is a retained CFStringRef.
    let status = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            (&raw mut translation).cast::<c_void>(),
        )
    };
    status == 0 && device != K_AUDIO_OBJECT_UNKNOWN
}

fn frame_message(
    remote: mach_port_t,
    id: i32,
    sequence: u32,
    generation: u32,
    token: u64,
    samples: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
) -> FrameMessage {
    FrameMessage {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0),
            msgh_size: u32::try_from(size_of::<FrameMessage>()).unwrap_or(u32::MAX),
            msgh_remote_port: remote,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: id,
        },
        sequence,
        generation,
        token,
        samples: *samples,
    }
}

fn send_frame_message(message: &mut FrameMessage) -> Result<(), MicrophoneDeviceError> {
    // SAFETY: message is a fixed-size initialized Mach message with a send right destination.
    let status = unsafe {
        mach_msg(
            &mut message.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            message.header.msgh_size,
            0,
            MACH_PORT_NULL,
            MACH_SEND_DEADLINE_MS,
            MACH_PORT_NULL,
        )
    };
    if status == MACH_MSG_SUCCESS {
        mark_moved_rights_consumed(&mut message.header);
        Ok(())
    } else if status == mach2::message::MACH_SEND_TIMED_OUT {
        destroy_returned_rights(&mut message.header);
        Err(MicrophoneDeviceError::Backpressure)
    } else {
        destroy_returned_rights(&mut message.header);
        Err(MicrophoneDeviceError::DeviceRemoved)
    }
}

fn remote_disposition(bits: u32) -> u32 {
    bits & 0xff
}

fn local_disposition(bits: u32) -> u32 {
    (bits >> 8) & 0xff
}

fn mark_moved_rights_consumed(header: &mut mach_msg_header_t) {
    let remote = remote_disposition(header.msgh_bits);
    if remote == u32::try_from(MACH_MSG_TYPE_MOVE_SEND_ONCE).unwrap()
        || remote == u32::try_from(MACH_MSG_TYPE_MOVE_SEND).unwrap()
    {
        header.msgh_remote_port = MACH_PORT_NULL;
    }
    let local = local_disposition(header.msgh_bits);
    if local == u32::try_from(MACH_MSG_TYPE_MOVE_SEND_ONCE).unwrap()
        || local == u32::try_from(MACH_MSG_TYPE_MOVE_SEND).unwrap()
    {
        header.msgh_local_port = MACH_PORT_NULL;
    }
}

fn destroy_returned_rights(header: &mut mach_msg_header_t) {
    release_returned_local_right(header);
    // SAFETY: on send failure the kernel leaves any returned rights represented by the
    // message header/body. mach_msg_destroy understands the returned dispositions.
    unsafe { mach_msg_destroy(header) };
    header.msgh_remote_port = MACH_PORT_NULL;
    header.msgh_local_port = MACH_PORT_NULL;
}

fn release_returned_local_right(header: &mut mach_msg_header_t) {
    let local = header.msgh_local_port;
    if local == MACH_PORT_NULL {
        return;
    }
    let disposition = local_disposition(header.msgh_bits);
    let owns_local = disposition == u32::try_from(MACH_MSG_TYPE_MOVE_SEND).unwrap()
        || disposition == u32::try_from(MACH_MSG_TYPE_MOVE_SEND_ONCE).unwrap();
    if owns_local {
        // SAFETY: the failed send returned an owned local send/send-once right.
        unsafe {
            let _ = mach2::mach_port::mach_port_deallocate(mach2::traps::mach_task_self(), local);
        }
        header.msgh_local_port = MACH_PORT_NULL;
    }
}

fn receive_control_reply(
    receive_port: mach_port_t,
    expected_id: i32,
) -> Result<FrameMessage, MicrophoneDeviceError> {
    let mut reply = FrameEnvelope {
        message: FrameMessage {
            header: mach_msg_header_t::default(),
            sequence: 0,
            generation: 0,
            token: 0,
            samples: [0; MICROPHONE_V1_FRAME_SAMPLES],
        },
        trailer: mach_msg_audit_trailer_t::default(),
    };
    // SAFETY: reply points to writable storage large enough for message plus trailer.
    let received = unsafe {
        mach_msg(
            &mut reply.message.header,
            MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT_OPTIONS,
            0,
            u32::try_from(size_of::<FrameEnvelope>()).unwrap_or(u32::MAX),
            receive_port,
            MACH_PROBE_DEADLINE_MS,
            MACH_PORT_NULL,
        )
    };
    if received == MACH_MSG_SUCCESS && reply.message.header.msgh_id == expected_id {
        Ok(reply.message)
    } else {
        Err(MicrophoneDeviceError::DeviceUnavailable)
    }
}

fn request_control_message(
    id: i32,
    generation: u32,
) -> Result<FrameMessage, MicrophoneDeviceError> {
    let service_port = lookup_service()?;
    let receive_port = MachReceiveRight::allocate()?;
    let mut request = FrameMessage {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE),
            msgh_size: if id == MSG_ADMIT_SESSION {
                u32::try_from(size_of::<FrameMessage>()).unwrap_or(u32::MAX)
            } else {
                u32::try_from(size_of::<mach_msg_header_t>()).unwrap_or(u32::MAX)
            },
            msgh_remote_port: service_port.as_name(),
            msgh_local_port: receive_port.as_name(),
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: id,
        },
        sequence: 0,
        generation,
        token: 0,
        samples: [0; MICROPHONE_V1_FRAME_SAMPLES],
    };
    // SAFETY: request is initialized and carries a send-once reply port.
    let sent = unsafe {
        mach_msg(
            &mut request.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            request.header.msgh_size,
            0,
            MACH_PORT_NULL,
            MACH_PROBE_DEADLINE_MS,
            MACH_PORT_NULL,
        )
    };
    if sent != MACH_MSG_SUCCESS {
        return Err(MicrophoneDeviceError::DeviceUnavailable);
    }
    receive_control_reply(receive_port.as_name(), id)
}

fn probe_service() -> Result<(), MicrophoneDeviceError> {
    let reply = request_control_message(MSG_PROBE, 0)?;
    if reply.generation == 1 {
        Ok(())
    } else {
        Err(MicrophoneDeviceError::DeviceUnavailable)
    }
}

fn admit_session(generation: u32) -> Result<u64, MicrophoneDeviceError> {
    if generation == 0 {
        return Err(MicrophoneDeviceError::InvalidBinding);
    }
    let reply = request_control_message(MSG_ADMIT_SESSION, generation)?;
    if reply.generation == generation && reply.token != 0 {
        Ok(reply.token)
    } else {
        Err(MicrophoneDeviceError::AccessDenied)
    }
}

#[derive(Debug)]
pub struct MicrophoneHub {
    driver_port: Arc<Mutex<Option<MachSendRight>>>,
}

impl MicrophoneHub {
    pub fn start() -> Result<Self, MicrophoneDeviceError> {
        let service = CString::new(MACH_SERVICE).expect("static service name has no nul");
        let mut receive_port = MACH_PORT_NULL;
        // SAFETY: bootstrap check-in is the documented LaunchDaemon MachServices receive-port path.
        let status =
            unsafe { bootstrap_check_in(bootstrap_port, service.as_ptr(), &mut receive_port) };
        if status != mach2::kern_return::KERN_SUCCESS || receive_port == MACH_PORT_NULL {
            return Err(MicrophoneDeviceError::DeviceUnavailable);
        }
        let driver_port = Arc::new(Mutex::new(None));
        let worker_driver = Arc::clone(&driver_port);
        std::thread::Builder::new()
            .name("arcen-microphone-hub".to_owned())
            .spawn(move || hub_thread(receive_port, worker_driver))
            .map_err(|_| MicrophoneDeviceError::WorkerFailed)?;
        Ok(Self { driver_port })
    }

    #[must_use]
    pub fn driver_connected(&self) -> bool {
        self.driver_port.lock().is_ok_and(|guard| guard.is_some())
    }
}

fn mach_round_msg(size: usize) -> usize {
    (size + 3) & !3
}

#[derive(Clone, Copy)]
struct ReceivedFrame {
    message: FrameMessage,
    audit: audit_token_t,
}

fn dispose_received_rights(message: &mut FrameMessage) {
    // SAFETY: message was received from mach_msg and may still own carried rights.
    unsafe { mach_msg_destroy(&mut message.header) };
    message.header.msgh_remote_port = MACH_PORT_NULL;
    message.header.msgh_local_port = MACH_PORT_NULL;
}

fn dispose_received_buffer(buffer: &mut [u8]) {
    if buffer.len() < size_of::<mach_msg_header_t>() {
        return;
    }
    // SAFETY: buffer starts with a mach_msg_header_t written by mach_msg.
    let header = unsafe { &mut *buffer.as_mut_ptr().cast::<mach_msg_header_t>() };
    // SAFETY: header points into the original received message buffer, including any descriptors.
    unsafe { mach_msg_destroy(header) };
    header.msgh_remote_port = MACH_PORT_NULL;
    header.msgh_local_port = MACH_PORT_NULL;
}

fn take_remote_send_right(message: &mut FrameMessage) -> Option<MachSendRight> {
    let port = message.header.msgh_remote_port;
    message.header.msgh_remote_port = MACH_PORT_NULL;
    MachSendRight::new(port)
}

fn replace_driver_port(
    driver_port: &Arc<Mutex<Option<MachSendRight>>>,
    replacement: MachSendRight,
) {
    if let Ok(mut guard) = driver_port.lock() {
        *guard = Some(replacement);
    }
}

fn parse_received_frame(buffer: &[u8]) -> Option<ReceivedFrame> {
    if buffer.len() < size_of::<mach_msg_header_t>() {
        return None;
    }
    // SAFETY: buffer has enough bytes for a Mach header; unaligned read avoids alignment assumptions.
    let header = unsafe { buffer.as_ptr().cast::<mach_msg_header_t>().read_unaligned() };
    let msg_size = usize::try_from(header.msgh_size).ok()?;
    let header_size = size_of::<mach_msg_header_t>();
    let frame_size = size_of::<FrameMessage>();
    if msg_size != header_size && msg_size != frame_size {
        return None;
    }
    let trailer_offset = mach_round_msg(msg_size);
    let trailer_end = trailer_offset.checked_add(size_of::<mach_msg_audit_trailer_t>())?;
    if trailer_end > buffer.len() {
        return None;
    }
    // SAFETY: bounds checked above and unaligned read avoids stack buffer alignment assumptions.
    let trailer = unsafe {
        buffer
            .as_ptr()
            .add(trailer_offset)
            .cast::<mach_msg_audit_trailer_t>()
            .read_unaligned()
    };
    if usize::try_from(trailer.msgh_trailer_size).ok()? < size_of::<mach_msg_audit_trailer_t>() {
        return None;
    }
    let mut message = FrameMessage {
        header,
        sequence: 0,
        generation: 0,
        token: 0,
        samples: [0; MICROPHONE_V1_FRAME_SAMPLES],
    };
    if msg_size == frame_size {
        // SAFETY: frame-sized message bounds checked above.
        message = unsafe { buffer.as_ptr().cast::<FrameMessage>().read_unaligned() };
    }
    Some(ReceivedFrame {
        message,
        audit: trailer.msgh_audit,
    })
}

fn valid_hub_message(message: &FrameMessage) -> bool {
    let header_size = u32::try_from(size_of::<mach_msg_header_t>()).unwrap_or(u32::MAX);
    let frame_size = u32::try_from(size_of::<FrameMessage>()).unwrap_or(u32::MAX);
    if (message.header.msgh_bits & mach2::message::MACH_MSGH_BITS_COMPLEX) != 0 {
        return false;
    }
    match message.header.msgh_id {
        MSG_REGISTER_DRIVER | MSG_PROBE => message.header.msgh_size == header_size,
        MSG_FRAME | MSG_CLEAR | MSG_ADMIT_SESSION | MSG_HEARTBEAT => {
            message.header.msgh_size == frame_size
        }
        _ => false,
    }
}

fn audit_euid(token: audit_token_t) -> libc::uid_t {
    // SAFETY: audit_token_to_euid accepts an audit token value copied from a Mach trailer.
    unsafe { audit_token_to_euid(token) }
}

fn audit_pid(token: audit_token_t) -> libc::pid_t {
    // SAFETY: audit_token_to_pid accepts an audit token value copied from a Mach trailer.
    unsafe { audit_token_to_pid(token) }
}

fn pid_path(pid: libc::pid_t) -> Option<String> {
    let mut buffer = [0_i8; 4096];
    // SAFETY: buffer is valid writable memory and proc_pidpath writes at most its size.
    let length = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    if length <= 0 {
        return None;
    }
    // SAFETY: proc_pidpath returned a positive length and NUL-terminates the buffer on success.
    Some(
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

fn authorize_driver(token: audit_token_t) -> bool {
    let euid = audit_euid(token);
    let pid = audit_pid(token);
    let coreaudiod_uid = user_id("_coreaudiod");
    if euid == coreaudiod_uid {
        return true;
    }
    if euid != 0 {
        return false;
    }
    pid_path(pid)
        .is_some_and(|path| path == "/usr/sbin/coreaudiod" || path.ends_with("/coreaudiod"))
}

fn cf_string(value: &str) -> Option<CfRef> {
    let value = CString::new(value).ok()?;
    // SAFETY: value is a valid NUL-terminated UTF-8 C string.
    CfRef::new(unsafe {
        CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    })
}

fn sec_requirement(requirement: &str) -> Option<CfRef> {
    let text = cf_string(requirement)?;
    let mut raw = std::ptr::null();
    // SAFETY: text is a CFString and raw points to writable storage for a retained requirement.
    let status = unsafe { SecRequirementCreateWithString(text.as_ptr(), 0, &mut raw) };
    (status == 0).then(|| CfRef::new(raw)).flatten()
}

fn release_requirement(identifier: &str) -> Option<CfRef> {
    let requirement = format!(
        "identifier \"{identifier}\" and anchor apple generic and \
         certificate leaf[subject.OU] = \"{ARCEN_TEAM_ID}\" and \
         certificate 1[field.1.2.840.113635.100.6.2.6] exists and \
         certificate leaf[field.1.2.840.113635.100.6.1.13] exists"
    );
    sec_requirement(&requirement)
}

fn sec_code_for_audit_token(token: audit_token_t) -> Option<CfRef> {
    let audit_bytes = &token as *const audit_token_t;
    // SAFETY: audit_bytes points to a live audit_token_t for the duration of CFDataCreate.
    let audit_data = CfRef::new(unsafe {
        CFDataCreate(
            std::ptr::null(),
            audit_bytes.cast::<u8>(),
            CFIndex::try_from(size_of::<audit_token_t>()).ok()?,
        )
    })?;
    let mut keys = [unsafe { kSecGuestAttributeAudit }.cast::<c_void>()];
    let mut values = [audit_data.as_ptr()];
    // SAFETY: key/value arrays hold one valid CF key and one CFData value.
    let attributes = CfRef::new(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            1,
            (&raw const kCFTypeDictionaryKeyCallBacks).cast::<c_void>(),
            (&raw const kCFTypeDictionaryValueCallBacks).cast::<c_void>(),
        )
    })?;
    let mut code = std::ptr::null();
    // SAFETY: attributes is a CFDictionary containing kSecGuestAttributeAudit; code is writable.
    let status = unsafe {
        SecCodeCopyGuestWithAttributes(std::ptr::null(), attributes.as_ptr(), 0, &mut code)
    };
    (status == 0).then(|| CfRef::new(code)).flatten()
}

fn code_satisfies(code: &CfRef, requirement: &CfRef) -> bool {
    // SAFETY: both arguments are valid Security framework objects retained by CfRef.
    unsafe { SecCodeCheckValidity(code.as_ptr(), 0, requirement.as_ptr()) == 0 }
}

fn release_authorized_code(code: &CfRef) -> bool {
    [PIER_BUNDLE_ID, AGENT_BUNDLE_ID]
        .into_iter()
        .filter_map(release_requirement)
        .any(|requirement| code_satisfies(code, &requirement))
}

#[cfg(debug_assertions)]
fn debug_authorized_current_build(code: &CfRef) -> bool {
    let mut current = std::ptr::null();
    // SAFETY: current points to writable storage for a retained SecCodeRef.
    if unsafe { SecCodeCopySelf(0, &mut current) } != 0 {
        return false;
    }
    let Some(current) = CfRef::new(current) else {
        return false;
    };
    let mut requirement = std::ptr::null();
    // SAFETY: current is a valid SecCodeRef and requirement is writable.
    if unsafe { SecCodeCopyDesignatedRequirement(current.as_ptr(), 0, &mut requirement) } != 0 {
        return false;
    }
    let Some(requirement) = CfRef::new(requirement) else {
        return false;
    };
    code_satisfies(code, &requirement)
}

#[cfg(not(debug_assertions))]
fn debug_authorized_current_build(_code: &CfRef) -> bool {
    false
}

fn authorize_pier(token: audit_token_t) -> bool {
    sec_code_for_audit_token(token)
        .is_some_and(|code| release_authorized_code(&code) || debug_authorized_current_build(&code))
}

fn user_id(name: &str) -> libc::uid_t {
    let Ok(name) = CString::new(name) else {
        return u32::MAX;
    };
    // SAFETY: getpwnam accepts a NUL-terminated username and returns process-global static storage.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return u32::MAX;
    }
    // SAFETY: entry is non-null and points to a passwd struct owned by libc.
    unsafe { (*entry).pw_uid }
}

fn mint_token() -> u64 {
    let mut bytes = [0_u8; 8];
    if getrandom::getrandom(&mut bytes).is_ok() {
        let token = u64::from_le_bytes(bytes);
        if token != 0 {
            return token;
        }
    }
    u64::from(TOKEN_COUNTER.fetch_add(1, Ordering::AcqRel)).max(1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionOwner {
    generation: u32,
    audit: audit_token_t,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveSession {
    owner: SessionOwner,
    token: u64,
    last_seen: Instant,
}

impl ActiveSession {
    fn admits(self, owner: SessionOwner) -> bool {
        self.owner.audit == owner.audit
    }

    fn owns(self, owner: SessionOwner, token: u64) -> bool {
        self.owner == owner && self.token != 0 && self.token == token
    }
}

fn admit_active_session(
    active: &mut Option<ActiveSession>,
    owner: SessionOwner,
    now: Instant,
) -> Option<u64> {
    revoke_expired_session(active, now);
    if owner.generation == 0 {
        return None;
    }
    if active.is_some_and(|session| !session.admits(owner)) {
        return None;
    }
    let token = mint_token();
    *active = Some(ActiveSession {
        owner,
        token,
        last_seen: now,
    });
    Some(token)
}

fn active_session_allows(
    active: &mut Option<ActiveSession>,
    owner: SessionOwner,
    token: u64,
    now: Instant,
) -> bool {
    revoke_expired_session(active, now);
    let Some(session) = active.as_mut() else {
        return false;
    };
    if session.owns(owner, token) {
        session.last_seen = now;
        true
    } else {
        false
    }
}

fn revoke_active_session(active: &mut Option<ActiveSession>, owner: SessionOwner, token: u64) {
    if active.is_some_and(|session| session.owns(owner, token)) {
        *active = None;
    }
}

fn revoke_expired_session(active: &mut Option<ActiveSession>, now: Instant) -> bool {
    if active.is_some_and(|session| now.duration_since(session.last_seen) >= SESSION_LEASE) {
        *active = None;
        true
    } else {
        false
    }
}

fn control_reply(
    request: &FrameMessage,
    id: i32,
    generation: u32,
    token: u64,
) -> Option<FrameMessage> {
    let reply_port = request.header.msgh_remote_port;
    if reply_port == MACH_PORT_NULL {
        return None;
    }
    let mut reply = frame_message(
        reply_port,
        id,
        0,
        generation,
        token,
        &[0; MICROPHONE_V1_FRAME_SAMPLES],
    );
    reply.header.msgh_bits = MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0);
    reply.header.msgh_size = u32::try_from(size_of::<FrameMessage>()).unwrap_or(u32::MAX);
    Some(reply)
}

fn clear_driver_without_ack(driver_port: &Arc<Mutex<Option<MachSendRight>>>) -> bool {
    let Ok(mut guard) = driver_port.lock() else {
        return false;
    };
    let Some(destination) = guard.as_ref().map(MachSendRight::as_name) else {
        return false;
    };
    let mut clear = frame_message(
        destination,
        MSG_CLEAR,
        0,
        0,
        0,
        &[0; MICROPHONE_V1_FRAME_SAMPLES],
    );
    if send_frame_message(&mut clear).is_ok() {
        true
    } else {
        *guard = None;
        false
    }
}

fn hub_thread(receive_port: mach_port_t, driver_port: Arc<Mutex<Option<MachSendRight>>>) {
    let mut active_session: Option<ActiveSession> = None;
    loop {
        if revoke_expired_session(&mut active_session, Instant::now()) {
            let _ = clear_driver_without_ack(&driver_port);
        }
        let mut buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        // SAFETY: buffer is writable and large enough for the largest message plus audit trailer.
        let status = unsafe {
            mach_msg(
                buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                receive_port,
                0,
                MACH_PORT_NULL,
            )
        };
        let Some(received) = parse_received_frame(&buffer) else {
            if status == MACH_MSG_SUCCESS {
                dispose_received_buffer(&mut buffer);
            }
            continue;
        };
        if status != MACH_MSG_SUCCESS || !valid_hub_message(&received.message) {
            dispose_received_buffer(&mut buffer);
            continue;
        }
        let audit = received.audit;
        let mut message = received.message;
        match message.header.msgh_id {
            MSG_REGISTER_DRIVER => {
                if authorize_driver(audit) {
                    if let Some(replacement) = take_remote_send_right(&mut message) {
                        replace_driver_port(&driver_port, replacement);
                    }
                } else {
                    dispose_received_rights(&mut message);
                }
            }
            MSG_FRAME | MSG_CLEAR | MSG_HEARTBEAT => {
                if !authorize_pier(audit) {
                    dispose_received_rights(&mut message);
                    continue;
                }
                let owner = SessionOwner {
                    generation: message.generation,
                    audit,
                };
                if !active_session_allows(&mut active_session, owner, message.token, Instant::now())
                {
                    dispose_received_rights(&mut message);
                    continue;
                }
                if message.header.msgh_id == MSG_HEARTBEAT {
                    dispose_received_rights(&mut message);
                    continue;
                }
                if message.header.msgh_id == MSG_CLEAR {
                    revoke_active_session(&mut active_session, owner, message.token);
                }
                let mut forwarded = false;
                if let Ok(mut guard) = driver_port.lock()
                    && let Some(destination) = guard.as_ref().map(MachSendRight::as_name)
                {
                    let reply_port = message.header.msgh_remote_port;
                    message.header.msgh_bits =
                        if message.header.msgh_id == MSG_CLEAR && reply_port != MACH_PORT_NULL {
                            MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE)
                        } else {
                            MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0)
                        };
                    message.header.msgh_remote_port = destination;
                    message.header.msgh_local_port = if message.header.msgh_id == MSG_CLEAR {
                        reply_port
                    } else {
                        MACH_PORT_NULL
                    };
                    forwarded = send_frame_message(&mut message).is_ok();
                    if !forwarded {
                        message.header.msgh_remote_port = MACH_PORT_NULL;
                        *guard = None;
                    }
                }
                if !forwarded {
                    dispose_received_rights(&mut message);
                }
            }
            MSG_PROBE => {
                if !authorize_pier(audit) {
                    dispose_received_rights(&mut message);
                    continue;
                }
                let live = driver_port.lock().is_ok_and(|guard| guard.is_some());
                if let Some(mut reply) = control_reply(&message, MSG_PROBE, u32::from(live), 0) {
                    let _ = send_frame_message(&mut reply);
                }
            }
            MSG_ADMIT_SESSION => {
                if !authorize_pier(audit) {
                    dispose_received_rights(&mut message);
                    continue;
                }
                if revoke_expired_session(&mut active_session, Instant::now()) {
                    let _ = clear_driver_without_ack(&driver_port);
                }
                let live = driver_port.lock().is_ok_and(|guard| guard.is_some());
                let token = if live {
                    admit_active_session(
                        &mut active_session,
                        SessionOwner {
                            generation: message.generation,
                            audit,
                        },
                        Instant::now(),
                    )
                    .unwrap_or(0)
                } else {
                    0
                };
                let generation = if token == 0 { 0 } else { message.generation };
                if let Some(mut reply) =
                    control_reply(&message, MSG_ADMIT_SESSION, generation, token)
                {
                    let _ = send_frame_message(&mut reply);
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneIngressError {
    MalformedFrame,
    Decode(MicrophoneDecodeError),
    Device(MicrophoneDeviceError),
}

#[derive(Debug)]
pub struct MicrophoneIngress<D: MicrophoneDevice> {
    binding: MicrophoneSessionBinding,
    decoder: MicrophoneDecoder,
    device: D,
    output: [i16; MICROPHONE_V1_FRAME_SAMPLES],
    stats: MicrophoneStatsTracker,
    started_at: std::time::Instant,
    session_log_id: Option<CorrelationId>,
    active: bool,
    terminal_device_error_recorded: bool,
}

impl<D: MicrophoneDevice> MicrophoneIngress<D> {
    pub fn new(
        binding: MicrophoneSessionBinding,
        stream: ResolvedMicrophoneStream,
        device: D,
    ) -> Result<Self, MicrophoneIngressError> {
        if stream.generation != binding.generation {
            return Err(MicrophoneIngressError::Device(
                MicrophoneDeviceError::StaleGeneration,
            ));
        }
        let decoder = MicrophoneDecoder::new(stream).map_err(MicrophoneIngressError::Decode)?;
        Ok(Self {
            binding,
            decoder,
            device,
            output: [0; MICROPHONE_V1_FRAME_SAMPLES],
            stats: MicrophoneStatsTracker::default(),
            started_at: std::time::Instant::now(),
            session_log_id: None,
            active: true,
            terminal_device_error_recorded: false,
        })
    }

    pub fn with_session_log_id(mut self, session_log_id: CorrelationId) -> Self {
        self.session_log_id = Some(session_log_id);
        self
    }

    pub fn ingest(
        &mut self,
        bytes: &[u8],
    ) -> Result<MicrophoneIngestOutcome, MicrophoneIngressError> {
        self.stats.record_received(bytes.len());
        let (header, payload) = decode_microphone_frame(bytes).map_err(|_| {
            self.stats.record_decoder_error();
            MicrophoneIngressError::MalformedFrame
        })?;
        let outcome = self.decoder.ingest(header, payload).map_err(|error| {
            self.stats.record_decoder_error();
            MicrophoneIngressError::Decode(error)
        })?;
        self.stats.record_ingest(outcome, bytes.len());
        Ok(outcome)
    }

    pub fn playout_tick(&mut self) -> Result<MicrophoneFrameOutput, MicrophoneIngressError> {
        let output = self.decoder.pop_into(&mut self.output).map_err(|error| {
            self.stats.record_decoder_error();
            MicrophoneIngressError::Decode(error)
        })?;
        self.stats.record_output(output);
        let write_result = self.device.write_frame(&self.binding, &self.output);
        self.output.zeroize();
        if let Err(error) = write_result {
            self.record_device_error(error);
            if error == MicrophoneDeviceError::Backpressure {
                return Ok(output);
            }
            return Err(MicrophoneIngressError::Device(error));
        }
        Ok(output)
    }

    pub fn shutdown(&mut self) -> Result<(), MicrophoneIngressError> {
        self.clear_once()
    }

    pub fn take_interval_stats(&mut self) -> MicrophoneStats {
        self.stats.take_interval()
    }

    fn clear_once(&mut self) -> Result<(), MicrophoneIngressError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.decoder.clear();
        self.output.zeroize();
        self.device
            .clear(&self.binding)
            .map_err(MicrophoneIngressError::Device)
    }

    fn record_device_error(&mut self, error: MicrophoneDeviceError) {
        if error == MicrophoneDeviceError::Backpressure {
            self.stats.record_transport_backpressure_drop();
            return;
        }
        if self.terminal_device_error_recorded {
            return;
        }
        self.terminal_device_error_recorded = true;
        match error {
            MicrophoneDeviceError::Timeout => self.stats.record_backend_timeout(),
            MicrophoneDeviceError::DeviceUnavailable | MicrophoneDeviceError::DeviceRemoved => {
                self.stats.record_backend_failure();
            }
            _ => {}
        }
    }
}

impl<D: MicrophoneDevice> Drop for MicrophoneIngress<D> {
    fn drop(&mut self) {
        let _ = self.clear_once();
    }
}

impl MicrophoneIngress<NativeMicrophoneDevice> {
    pub async fn shutdown_wait(
        &mut self,
        stop_reason: &'static str,
    ) -> Result<(), MicrophoneIngressError> {
        let jitter_depth = self.decoder.queued_frames();
        let clear_result = self.clear_once();
        let stop_result = self
            .device
            .shutdown_wait()
            .await
            .map_err(MicrophoneIngressError::Device);
        let stats = self.stats.total();
        let duration_ms = self
            .started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let sid = self
            .session_log_id
            .as_ref()
            .map_or("unavailable", CorrelationId::as_str);
        tracing::info!(
            target: arcen_telemetry::names::target::MEDIA,
            event = "mic_macos_feeder_stopped",
            sid,
            generation = self.binding.generation,
            duration_ms,
            sample_rate_hz = 48_000u32,
            channels = 1u8,
            frame_duration_ms = 20u16,
            received_frames = stats.received_frames,
            received_bytes = stats.received_bytes,
            accepted_frames = stats.accepted_frames,
            duplicate_frames = stats.duplicate_frames,
            late_frames = stats.late_frames,
            wrong_generation_frames = stats.wrong_generation_frames,
            discontinuities = stats.discontinuities,
            silence_frames = stats.silence_frames,
            underflow_frames = stats.underflow_frames,
            decoder_resets = stats.decoder_resets,
            decoder_errors = stats.decoder_errors,
            jitter_depth,
            jitter_target = arcen_media::audio::MICROPHONE_JITTER_TARGET_FRAMES,
            jitter_max = arcen_media::audio::MICROPHONE_JITTER_MAX_FRAMES,
            feeder_mailbox_drops = stats.transport_backpressure_drops,
            feeder_timeouts = stats.backend_timeouts,
            device_failures = stats.backend_failures,
            stop_reason,
            "macOS microphone feeder stopped"
        );
        clear_result.and(stop_result)
    }
}

#[cfg(test)]
#[derive(Debug)]
pub struct SessionAudioRing {
    binding: MicrophoneSessionBinding,
    frames: [[i16; MICROPHONE_V1_FRAME_SAMPLES]; 10],
    read: usize,
    len: usize,
}

#[cfg(test)]
impl SessionAudioRing {
    pub fn new(binding: MicrophoneSessionBinding) -> Self {
        Self {
            binding,
            frames: [[0; MICROPHONE_V1_FRAME_SAMPLES]; 10],
            read: 0,
            len: 0,
        }
    }
    pub fn read_frame(&mut self, output: &mut [i16; MICROPHONE_V1_FRAME_SAMPLES]) {
        if self.len == 0 {
            output.zeroize();
            return;
        }
        output.copy_from_slice(&self.frames[self.read]);
        self.frames[self.read].zeroize();
        self.read = (self.read + 1) % self.frames.len();
        self.len -= 1;
    }
    fn authorize(&self, candidate: &MicrophoneSessionBinding) -> Result<(), MicrophoneDeviceError> {
        if candidate.generation != self.binding.generation {
            return Err(MicrophoneDeviceError::StaleGeneration);
        }
        Ok(())
    }
}

#[cfg(test)]
impl MicrophoneDevice for SessionAudioRing {
    fn write_frame(
        &mut self,
        binding: &MicrophoneSessionBinding,
        frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> Result<(), MicrophoneDeviceError> {
        self.authorize(binding)?;
        if self.len == self.frames.len() {
            self.frames[self.read].zeroize();
            self.read = (self.read + 1) % self.frames.len();
            self.len -= 1;
        }
        let write = (self.read + self.len) % self.frames.len();
        self.frames[write].copy_from_slice(frame);
        self.len += 1;
        Ok(())
    }
    fn clear(&mut self, binding: &MicrophoneSessionBinding) -> Result<(), MicrophoneDeviceError> {
        self.authorize(binding)?;
        self.frames.iter_mut().for_each(Zeroize::zeroize);
        self.read = 0;
        self.len = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(generation: u32) -> MicrophoneSessionBinding {
        MicrophoneSessionBinding::new(generation).unwrap()
    }

    fn receive_with_audit(port: mach_port_t, buffer: &mut [u8]) {
        // SAFETY: buffer is writable and large enough for the expected message plus audit trailer.
        let received = unsafe {
            mach_msg(
                buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(buffer.len()).unwrap(),
                port,
                MACH_PROBE_DEADLINE_MS,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(received, MACH_MSG_SUCCESS);
    }

    fn fill_receive_queue(port: mach_port_t) {
        for sequence in 0..64 {
            let mut frame = frame_message(
                port,
                MSG_FRAME,
                sequence,
                1,
                1,
                &[1; MICROPHONE_V1_FRAME_SAMPLES],
            );
            if send_frame_message(&mut frame).is_err() {
                return;
            }
        }
        panic!("Mach queue did not fill");
    }

    #[test]
    fn binding_rejects_stale_writers() {
        let owner = binding(7);
        let mut ring = SessionAudioRing::new(owner.clone());
        let frame = [5; MICROPHONE_V1_FRAME_SAMPLES];
        assert_eq!(
            ring.write_frame(&binding(8), &frame),
            Err(MicrophoneDeviceError::StaleGeneration)
        );
    }

    #[test]
    fn ring_wrap_is_bounded_and_drops_oldest() {
        let owner = binding(7);
        let mut ring = SessionAudioRing::new(owner.clone());
        for value in 1..=12 {
            ring.write_frame(&owner, &[value; MICROPHONE_V1_FRAME_SAMPLES])
                .unwrap();
        }
        let mut output = [0; MICROPHONE_V1_FRAME_SAMPLES];
        ring.read_frame(&mut output);
        assert_eq!(output[0], 3);
    }

    #[test]
    fn underrun_and_clear_are_exact_silence() {
        let owner = binding(7);
        let mut ring = SessionAudioRing::new(owner.clone());
        let mut output = [9; MICROPHONE_V1_FRAME_SAMPLES];
        ring.read_frame(&mut output);
        assert!(output.iter().all(|sample| *sample == 0));
        ring.write_frame(&owner, &[6; MICROPHONE_V1_FRAME_SAMPLES])
            .unwrap();
        ring.clear(&owner).unwrap();
        output.fill(9);
        ring.read_frame(&mut output);
        assert!(output.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn active_session_is_explicit_and_owner_bound() {
        let owner = SessionOwner {
            generation: 9,
            audit: audit_token_t { val: [1; 8] },
        };
        let other_generation = SessionOwner {
            generation: 10,
            audit: owner.audit,
        };
        let other_sender = SessionOwner {
            generation: 9,
            audit: audit_token_t { val: [2; 8] },
        };
        let mut active = None;
        let now = Instant::now();

        assert!(!active_session_allows(&mut active, owner, 7, now));
        let token = admit_active_session(&mut active, owner, now).expect("token");
        assert_ne!(token, 0);
        assert!(active_session_allows(&mut active, owner, token, now));
        assert!(!active_session_allows(
            &mut active,
            owner,
            token.wrapping_add(1),
            now
        ));
        assert!(!active_session_allows(
            &mut active,
            other_generation,
            token,
            now
        ));
        assert_eq!(admit_active_session(&mut active, other_sender, now), None);

        let replacement = admit_active_session(&mut active, owner, now).expect("replacement token");
        assert_ne!(replacement, token);
        assert!(!active_session_allows(&mut active, owner, token, now));
        assert!(active_session_allows(&mut active, owner, replacement, now));
        revoke_active_session(&mut active, other_sender, replacement);
        assert!(active_session_allows(&mut active, owner, replacement, now));
        revoke_active_session(&mut active, owner, replacement);
        assert!(!active_session_allows(&mut active, owner, replacement, now));
    }

    #[test]
    fn expired_session_lease_admits_restarted_owner() {
        let owner = SessionOwner {
            generation: 9,
            audit: audit_token_t { val: [1; 8] },
        };
        let restarted = SessionOwner {
            generation: 10,
            audit: audit_token_t { val: [2; 8] },
        };
        let now = Instant::now();
        let mut active = None;
        let first = admit_active_session(&mut active, owner, now).expect("first token");
        assert_eq!(admit_active_session(&mut active, restarted, now), None);
        assert!(active_session_allows(&mut active, owner, first, now));

        let later = now + SESSION_LEASE + Duration::from_millis(1);
        assert!(!active_session_allows(&mut active, owner, first, later));
        let second = admit_active_session(&mut active, restarted, later).expect("second token");
        assert_ne!(second, 0);
        assert!(active_session_allows(&mut active, restarted, second, later));
    }

    #[test]
    fn replacing_driver_registration_keeps_send_refs_flat() {
        let receive = MachReceiveRight::allocate().expect("receive right");
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                receive.as_name(),
                receive.as_name(),
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        let driver = Arc::new(Mutex::new(None));
        replace_driver_port(
            &driver,
            MachSendRight::new(receive.as_name()).expect("send right"),
        );
        assert_eq!(send_refs(receive.as_name()), 1);

        for _ in 0..3 {
            // SAFETY: creates one additional send right to model a new driver registration.
            let inserted = unsafe {
                mach2::mach_port::mach_port_insert_right(
                    mach2::traps::mach_task_self(),
                    receive.as_name(),
                    receive.as_name(),
                    mach2::message::MACH_MSG_TYPE_MAKE_SEND,
                )
            };
            assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
            assert_eq!(send_refs(receive.as_name()), 2);
            replace_driver_port(
                &driver,
                MachSendRight::new(receive.as_name()).expect("replacement send right"),
            );
            assert_eq!(send_refs(receive.as_name()), 1);
        }

        driver.lock().unwrap().take();
        assert_eq!(send_refs(receive.as_name()), 0);
    }

    #[test]
    fn parser_rejection_destroys_received_send_once_right() {
        let receive = MachReceiveRight::allocate().expect("receive right");
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                receive.as_name(),
                receive.as_name(),
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        let reply = MachReceiveRight::allocate().expect("reply receive right");
        let mut send_once = MACH_PORT_NULL;
        let mut send_once_type = 0;
        // SAFETY: extracts a send-once right from the reply receive right.
        let extracted = unsafe {
            mach2::mach_port::mach_port_extract_right(
                mach2::traps::mach_task_self(),
                reply.as_name(),
                MACH_MSG_TYPE_MAKE_SEND_ONCE,
                &mut send_once,
                &mut send_once_type,
            )
        };
        assert_eq!(extracted, mach2::kern_return::KERN_SUCCESS);
        assert_eq!(send_once_type, MACH_MSG_TYPE_MOVE_SEND_ONCE);

        #[repr(C)]
        struct MalformedMessage {
            header: mach_msg_header_t,
            pad: u32,
        }
        let mut malformed = MalformedMessage {
            header: mach_msg_header_t {
                msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE),
                msgh_size: u32::try_from(size_of::<MalformedMessage>()).unwrap(),
                msgh_remote_port: receive.as_name(),
                msgh_local_port: send_once,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: MSG_PROBE,
            },
            pad: 0,
        };
        // SAFETY: malformed is an initialized fixed-size Mach message.
        let sent = unsafe {
            mach_msg(
                &mut malformed.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                malformed.header.msgh_size,
                0,
                MACH_PORT_NULL,
                MACH_PROBE_DEADLINE_MS,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(sent, MACH_MSG_SUCCESS);
        let mut buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        receive_with_audit(receive.as_name(), &mut buffer);
        let header = unsafe { buffer.as_ptr().cast::<mach_msg_header_t>().read_unaligned() };
        assert!(parse_received_frame(&buffer).is_none());
        assert_eq!(send_once_refs(header.msgh_remote_port), 1);
        dispose_received_buffer(&mut buffer);
        assert_eq!(send_once_refs(header.msgh_remote_port), 0);
    }

    #[test]
    fn complex_rejection_destroys_body_port_rights() {
        let receive = MachReceiveRight::allocate().expect("receive right");
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                receive.as_name(),
                receive.as_name(),
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        let carried = MachReceiveRight::allocate().expect("carried receive right");
        // SAFETY: inserts a send right for the carried receive right.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                carried.as_name(),
                carried.as_name(),
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        assert_eq!(send_refs(carried.as_name()), 1);

        #[repr(C)]
        struct ComplexMessage {
            header: mach_msg_header_t,
            body: mach2::message::mach_msg_body_t,
            descriptor: mach2::message::mach_msg_port_descriptor_t,
        }
        let mut complex = ComplexMessage {
            header: mach_msg_header_t {
                msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0)
                    | mach2::message::MACH_MSGH_BITS_COMPLEX,
                msgh_size: u32::try_from(size_of::<ComplexMessage>()).unwrap(),
                msgh_remote_port: receive.as_name(),
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: MSG_PROBE,
            },
            body: mach2::message::mach_msg_body_t {
                msgh_descriptor_count: 1,
            },
            descriptor: mach2::message::mach_msg_port_descriptor_t::new(
                carried.as_name(),
                MACH_MSG_TYPE_COPY_SEND,
            ),
        };
        // SAFETY: complex is an initialized complex Mach message carrying one port descriptor.
        let sent = unsafe {
            mach_msg(
                &mut complex.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                complex.header.msgh_size,
                0,
                MACH_PORT_NULL,
                MACH_PROBE_DEADLINE_MS,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(sent, MACH_MSG_SUCCESS);
        let mut buffer =
            [0_u8; size_of::<ComplexMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        receive_with_audit(receive.as_name(), &mut buffer);
        assert!(parse_received_frame(&buffer).is_none());
        assert_eq!(send_refs(carried.as_name()), 2);
        dispose_received_buffer(&mut buffer);
        assert_eq!(send_refs(carried.as_name()), 1);
    }

    #[test]
    fn timed_out_copy_send_frame_keeps_remote_refs_flat() {
        let receive = MachReceiveRight::allocate().expect("receive right");
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                receive.as_name(),
                receive.as_name(),
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        fill_receive_queue(receive.as_name());
        let refs_after_fill = send_refs(receive.as_name());
        for sequence in 100..103 {
            let mut frame = frame_message(
                receive.as_name(),
                MSG_FRAME,
                sequence,
                1,
                1,
                &[1; MICROPHONE_V1_FRAME_SAMPLES],
            );
            assert_eq!(
                send_frame_message(&mut frame),
                Err(MicrophoneDeviceError::Backpressure)
            );
            assert_eq!(send_refs(receive.as_name()), refs_after_fill);
        }
    }

    #[test]
    fn timed_out_forwarded_clear_releases_local_send_once() {
        let receive = MachReceiveRight::allocate().expect("receive right");
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                receive.as_name(),
                receive.as_name(),
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        fill_receive_queue(receive.as_name());
        let refs_after_fill = send_refs(receive.as_name());
        let reply = MachReceiveRight::allocate().expect("reply receive right");
        let mut send_once = MACH_PORT_NULL;
        let mut send_once_type = 0;
        // SAFETY: extracts a send-once right from the reply receive right.
        let extracted = unsafe {
            mach2::mach_port::mach_port_extract_right(
                mach2::traps::mach_task_self(),
                reply.as_name(),
                MACH_MSG_TYPE_MAKE_SEND_ONCE,
                &mut send_once,
                &mut send_once_type,
            )
        };
        assert_eq!(extracted, mach2::kern_return::KERN_SUCCESS);
        assert_eq!(send_once_type, MACH_MSG_TYPE_MOVE_SEND_ONCE);
        assert_eq!(send_once_refs(send_once), 1);
        let mut clear = frame_message(
            receive.as_name(),
            MSG_CLEAR,
            0,
            1,
            1,
            &[0; MICROPHONE_V1_FRAME_SAMPLES],
        );
        clear.header.msgh_bits =
            MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE);
        clear.header.msgh_local_port = send_once;
        // SAFETY: clear is an initialized fixed-size Mach message with a full destination queue.
        let sent = unsafe {
            mach_msg(
                &mut clear.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                clear.header.msgh_size,
                0,
                MACH_PORT_NULL,
                MACH_SEND_DEADLINE_MS,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(sent, mach2::message::MACH_SEND_TIMED_OUT);
        let returned_local = clear.header.msgh_local_port;
        assert_ne!(returned_local, MACH_PORT_NULL);
        assert_eq!(send_once_refs(returned_local), 1);
        destroy_returned_rights(&mut clear.header);
        assert_eq!(send_refs(receive.as_name()), refs_after_fill);
        assert_eq!(send_once_refs(returned_local), 0);
    }

    #[test]
    fn probe_reply_uses_received_remote_reply_port() {
        let mut reply_port = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and reply_port points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach2::traps::mach_task_self(),
                mach2::port::MACH_PORT_RIGHT_RECEIVE,
                &mut reply_port,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);
        let mut send_once = MACH_PORT_NULL;
        let mut send_once_type = 0;
        // SAFETY: extracts a send-once right from the receive right for this task.
        let extracted = unsafe {
            mach2::mach_port::mach_port_extract_right(
                mach2::traps::mach_task_self(),
                reply_port,
                MACH_MSG_TYPE_MAKE_SEND_ONCE,
                &mut send_once,
                &mut send_once_type,
            )
        };
        assert_eq!(extracted, mach2::kern_return::KERN_SUCCESS);
        assert_eq!(send_once_type, MACH_MSG_TYPE_MOVE_SEND_ONCE);
        let request = FrameMessage {
            header: mach_msg_header_t {
                msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE),
                msgh_size: u32::try_from(size_of::<mach_msg_header_t>()).unwrap(),
                msgh_remote_port: send_once,
                msgh_local_port: 999_999,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: MSG_PROBE,
            },
            sequence: 0,
            generation: 0,
            token: 0,
            samples: [0; MICROPHONE_V1_FRAME_SAMPLES],
        };
        let mut reply = control_reply(&request, MSG_PROBE, 1, 0).expect("reply");
        assert_eq!(reply.header.msgh_remote_port, send_once);
        assert_ne!(
            reply.header.msgh_remote_port,
            request.header.msgh_local_port
        );
        assert_eq!(
            reply.header.msgh_bits,
            MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0)
        );
        send_frame_message(&mut reply).expect("send reply");

        let mut reply_buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        // SAFETY: buffer is writable and large enough for the reply plus audit trailer.
        let received = unsafe {
            mach_msg(
                reply_buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(reply_buffer.len()).unwrap(),
                reply_port,
                0,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(received, MACH_MSG_SUCCESS);
        let parsed = parse_received_frame(&reply_buffer).expect("reply with audit trailer");
        assert_eq!(parsed.message.header.msgh_id, MSG_PROBE);
        assert_eq!(parsed.message.generation, 1);
        assert_eq!(parsed.message.token, 0);
    }

    #[test]
    fn clear_drains_pending_frames_before_acknowledgement() {
        let service = MachReceiveRight::allocate().expect("service receive right");
        let port = service.as_name();
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                port,
                port,
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        let (tx, rx) = mpsc::sync_channel(2);
        tx.try_send(DeviceCommand::Frame(frame_message(
            MACH_PORT_NULL,
            MSG_FRAME,
            1,
            42,
            9,
            &[5; MICROPHONE_V1_FRAME_SAMPLES],
        )))
        .unwrap();
        let mut clear = frame_message(
            MACH_PORT_NULL,
            MSG_CLEAR,
            0,
            42,
            9,
            &[0; MICROPHONE_V1_FRAME_SAMPLES],
        );
        let driver_received = Arc::new(AtomicBool::new(false));
        let worker_received = Arc::clone(&driver_received);
        let worker = std::thread::spawn(move || {
            let _service = service;
            let mut buffer =
                [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
            // SAFETY: buffer is writable and large enough for the clear plus audit trailer.
            let received = unsafe {
                mach_msg(
                    buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                    MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                    0,
                    u32::try_from(buffer.len()).unwrap(),
                    port,
                    MACH_PROBE_DEADLINE_MS,
                    MACH_PORT_NULL,
                )
            };
            assert_eq!(received, MACH_MSG_SUCCESS);
            let parsed = parse_received_frame(&buffer).expect("clear with audit trailer");
            assert_eq!(parsed.message.header.msgh_id, MSG_CLEAR);
            assert_eq!(parsed.message.generation, 42);
            assert_eq!(parsed.message.token, 9);
            worker_received.store(true, Ordering::Release);
            let mut reply = control_reply(
                &parsed.message,
                MSG_CLEAR,
                parsed.message.generation,
                parsed.message.token,
            )
            .expect("clear reply");
            send_frame_message(&mut reply).expect("clear ack");
        });

        send_clear_message(port, &rx, &mut clear).expect("clear send");
        worker.join().unwrap();
        assert!(driver_received.load(Ordering::Acquire));
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn clear_without_driver_ack_is_not_successful_teardown() {
        let service = MachReceiveRight::allocate().expect("service receive right");
        let port = service.as_name();
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                port,
                port,
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        let (_tx, rx) = mpsc::sync_channel(1);
        let mut clear = frame_message(
            MACH_PORT_NULL,
            MSG_CLEAR,
            0,
            42,
            9,
            &[0; MICROPHONE_V1_FRAME_SAMPLES],
        );
        assert_eq!(
            send_clear_message(port, &rx, &mut clear),
            Err(MicrophoneDeviceError::DeviceUnavailable)
        );
    }

    #[test]
    fn mach_trailer_parser_handles_small_and_full_messages() {
        let mut port = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and port points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach2::traps::mach_task_self(),
                mach2::port::MACH_PORT_RIGHT_RECEIVE,
                &mut port,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                port,
                port,
                mach2::message::MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);

        let mut small = mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0),
            msgh_size: u32::try_from(size_of::<mach_msg_header_t>()).unwrap(),
            msgh_remote_port: port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: MSG_PROBE,
        };
        // SAFETY: small is an initialized header-only Mach message with a valid send right.
        let sent = unsafe {
            mach_msg(
                &mut small,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                small.msgh_size,
                0,
                MACH_PORT_NULL,
                MACH_SEND_DEADLINE_MS,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(sent, MACH_MSG_SUCCESS);
        let mut small_buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        // SAFETY: buffer is writable and large enough for the small message plus audit trailer.
        let received = unsafe {
            mach_msg(
                small_buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(small_buffer.len()).unwrap(),
                port,
                0,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(received, MACH_MSG_SUCCESS);
        let parsed = parse_received_frame(&small_buffer).expect("small message trailer");
        assert_eq!(parsed.message.header.msgh_id, MSG_PROBE);
        assert_eq!(
            parsed.message.header.msgh_size,
            size_of::<mach_msg_header_t>() as u32
        );

        let mut full = frame_message(
            port,
            MSG_FRAME,
            7,
            11,
            0x1234,
            &[4; MICROPHONE_V1_FRAME_SAMPLES],
        );
        let sent = send_frame_message(&mut full).expect("full send");
        assert_eq!(sent, ());
        let mut full_buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        // SAFETY: buffer is writable and large enough for the full message plus audit trailer.
        let received = unsafe {
            mach_msg(
                full_buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(full_buffer.len()).unwrap(),
                port,
                0,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(received, MACH_MSG_SUCCESS);
        let parsed = parse_received_frame(&full_buffer).expect("full message trailer");
        assert_eq!(parsed.message.sequence, 7);
        assert_eq!(parsed.message.generation, 11);
        assert_eq!(parsed.message.token, 0x1234);
        assert_eq!(parsed.message.samples[0], 4);
    }
}
