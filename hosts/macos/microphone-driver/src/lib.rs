// SPDX-License-Identifier: AGPL-3.0-only
//! Core Audio HAL `AudioServerPlugIn` for the Arcen Deck microphone.
//!
//! The plug-in exposes one input-only 48 kHz mono Float32 device named
//! "Arcen Microphone". A non-real-time receiver thread connects to the Pier's
//! declared Mach service and writes PCM into an SPSC ring; Core Audio's IO
//! thread only performs atomic loads/stores and writes silence on underrun.

#![allow(unsafe_code)]
#![allow(non_snake_case)]
#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::expect_used,
    clippy::unwrap_used
)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::{align_of, size_of};
use std::panic::AssertUnwindSafe;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, OnceLock};

use mach2::bootstrap::{bootstrap_look_up, bootstrap_port};
use mach2::message::{
    MACH_MSG_SUCCESS, MACH_MSG_TRAILER_FORMAT_0, MACH_MSG_TYPE_MAKE_SEND, MACH_MSG_TYPE_MOVE_SEND,
    MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_RCV_MSG, MACH_RCV_TIMED_OUT,
    MACH_RCV_TIMEOUT, MACH_RCV_TRAILER_AUDIT, MACH_SEND_MSG, MACH_SEND_TIMEOUT, mach_msg,
    mach_msg_audit_trailer_t, mach_msg_destroy, mach_msg_header_t, mach_msg_return_t,
};
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use mach2::traps::mach_task_self;

pub const MACH_SERVICE: &str = "tech.arcen.microphone";
pub const MACH_MSG_REGISTER_DRIVER: i32 = 0x4152_4301;
pub const MACH_MSG_FRAME: i32 = 0x4152_4302;
pub const MACH_MSG_CLEAR: i32 = 0x4152_4303;
const MACH_RCV_TRAILER_AUDIT_OPTIONS: i32 =
    ((MACH_MSG_TRAILER_FORMAT_0 as i32) << 28) | ((MACH_RCV_TRAILER_AUDIT as i32) << 24);
pub const SAMPLE_RATE_HZ: f64 = 48_000.0;
pub const CHANNELS: u32 = 1;
pub const FRAME_SAMPLES: usize = 960;
pub const RING_SAMPLES: usize = FRAME_SAMPLES * 64;

const PLUGIN_OBJECT: AudioObjectID = 1;
const DEVICE_OBJECT: AudioObjectID = 2;
const STREAM_OBJECT: AudioObjectID = 3;
const OBJECT_UNKNOWN: AudioObjectID = 0;

const PLUGIN_TYPE_UUID: CFUUIDBytes = CFUUIDBytes::new(
    0x443A_BAB8,
    0xE7B3,
    0x491A,
    [0xB9, 0x85, 0xBE, 0xB9, 0x18, 0x70, 0x30, 0xDB],
);
const DRIVER_INTERFACE_UUID: CFUUIDBytes = CFUUIDBytes::new(
    0xEEA5_773D,
    0xCC43,
    0x49F1,
    [0x8E, 0x00, 0x8F, 0x96, 0xE7, 0xD2, 0x3B, 0x17],
);
const IUNKNOWN_UUID: CFUUIDBytes = CFUUIDBytes::new(
    0x0000_0000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

const fn fourcc(bytes: &[u8; 4]) -> u32 {
    ((bytes[0] as u32) << 24)
        | ((bytes[1] as u32) << 16)
        | ((bytes[2] as u32) << 8)
        | bytes[3] as u32
}

const NO_ERR: OSStatus = 0;
const K_AUDIO_HARDWARE_BAD_OBJECT_ERROR: OSStatus = fourcc(b"!obj") as OSStatus;
const K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR: OSStatus = fourcc(b"who?") as OSStatus;
const K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR: OSStatus = fourcc(b"!siz") as OSStatus;
const K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR: OSStatus = fourcc(b"nope") as OSStatus;
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = fourcc(b"glob");
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT: u32 = fourcc(b"inpt");
pub const K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT: u32 = fourcc(b"outp");
pub const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_AUDIO_OBJECT_PROPERTY_BASE_CLASS: u32 = fourcc(b"bcls");
const K_AUDIO_OBJECT_PROPERTY_CLASS: u32 = fourcc(b"clas");
const K_AUDIO_OBJECT_PROPERTY_OWNER: u32 = fourcc(b"stdv");
const K_AUDIO_OBJECT_PROPERTY_NAME: u32 = fourcc(b"lnam");
const K_AUDIO_OBJECT_PROPERTY_MANUFACTURER: u32 = fourcc(b"lmak");
const K_AUDIO_OBJECT_PROPERTY_MODEL_NAME: u32 = fourcc(b"lmod");
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_NAME: u32 = fourcc(b"lchn");
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_CATEGORY_NAME: u32 = fourcc(b"lccn");
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_NUMBER_NAME: u32 = fourcc(b"lcnn");
const K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS: u32 = fourcc(b"ownd");
const K_AUDIO_OBJECT_PROPERTY_IDENTIFY: u32 = fourcc(b"iden");
const K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER: u32 = fourcc(b"snum");
const K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION: u32 = fourcc(b"fwvn");
const K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST: u32 = fourcc(b"cust");
const K_AUDIO_PLUGIN_PROPERTY_RESOURCE_BUNDLE: u32 = fourcc(b"rsrc");
const K_AUDIO_PLUGIN_PROPERTY_DEVICE_LIST: u32 = fourcc(b"dev#");
const K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_DEVICE: u32 = fourcc(b"uidd");
const K_AUDIO_PLUGIN_PROPERTY_BOX_LIST: u32 = fourcc(b"box#");
const K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_BOX: u32 = fourcc(b"uidb");
const K_AUDIO_PLUGIN_PROPERTY_CLOCK_DEVICE_LIST: u32 = fourcc(b"clk#");
const K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_CLOCK_DEVICE: u32 = fourcc(b"uidc");
const K_AUDIO_PLUGIN_CLASS_ID: u32 = fourcc(b"aplg");
const K_AUDIO_DEVICE_CLASS_ID: u32 = fourcc(b"adev");
const K_AUDIO_STREAM_CLASS_ID: u32 = fourcc(b"astr");
const K_AUDIO_DEVICE_PROPERTY_DEVICE_UID: u32 = fourcc(b"uid ");
const K_AUDIO_DEVICE_PROPERTY_MODEL_UID: u32 = fourcc(b"muid");
const K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE: u32 = fourcc(b"tran");
const K_AUDIO_DEVICE_PROPERTY_RELATED_DEVICES: u32 = fourcc(b"akin");
const K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN: u32 = fourcc(b"clkd");
const K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE: u32 = fourcc(b"livn");
const K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_RUNNING: u32 = fourcc(b"goin");
const K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT: u32 = fourcc(b"dflt");
const K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT_SYSTEM: u32 = fourcc(b"sflt");
const K_AUDIO_DEVICE_PROPERTY_LATENCY: u32 = fourcc(b"ltnc");
const K_AUDIO_DEVICE_PROPERTY_STREAMS: u32 = fourcc(b"stm#");
const K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET: u32 = fourcc(b"saft");
const K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE: u32 = fourcc(b"nsrt");
const K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES: u32 = fourcc(b"nsr#");
const K_AUDIO_DEVICE_PROPERTY_ICON: u32 = fourcc(b"icon");
const K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN: u32 = fourcc(b"hidn");
const K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO: u32 = fourcc(b"dch2");
const K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNEL_LAYOUT: u32 = fourcc(b"srnd");
const K_AUDIO_DEVICE_PROPERTY_WANTS_CONTROLS_RESTORED: u32 = fourcc(b"resc");
const K_AUDIO_DEVICE_PROPERTY_WANTS_STREAM_FORMATS_RESTORED: u32 = fourcc(b"resf");
const K_AUDIO_OBJECT_PROPERTY_CONTROL_LIST: u32 = fourcc(b"ctrl");
const K_AUDIO_DEVICE_PROPERTY_ZERO_TIMESTAMP_PERIOD: u32 = fourcc(b"ring");
const K_AUDIO_DEVICE_PROPERTY_CLOCK_ALGORITHM: u32 = fourcc(b"clok");
const K_AUDIO_DEVICE_PROPERTY_CLOCK_IS_STABLE: u32 = fourcc(b"cstb");
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE: u32 = fourcc(b"fsz#");
const K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION: u32 = fourcc(b"slay");
const K_AUDIO_DEVICE_TRANSPORT_TYPE_VIRTUAL: u32 = fourcc(b"virt");
const K_AUDIO_DEVICE_CLOCK_ALGORITHM_RAW: u32 = fourcc(b"raww");
const K_AUDIO_STREAM_PROPERTY_IS_ACTIVE: u32 = fourcc(b"sact");
const K_AUDIO_STREAM_PROPERTY_DIRECTION: u32 = fourcc(b"sdir");
const K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE: u32 = fourcc(b"term");
const K_AUDIO_STREAM_PROPERTY_STARTING_CHANNEL: u32 = fourcc(b"schn");
const K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT: u32 = fourcc(b"sfmt");
const K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS: u32 = fourcc(b"sfma");
const K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT: u32 = fourcc(b"pft ");
const K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS: u32 = fourcc(b"pfta");
const K_AUDIO_STREAM_TERMINAL_TYPE_MICROPHONE: u32 = fourcc(b"micr");
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 1 << 3;
const K_AUDIO_CHANNEL_LAYOUT_TAG_MONO: u32 = (100 << 16) | 1;

pub type OSStatus = i32;
type HRESULT = i32;
type ULONG = u32;
type Boolean = u8;
type UInt32 = u32;
type UInt64 = u64;
type AudioObjectID = u32;
type AudioObjectPropertySelector = u32;
type AudioObjectPropertyScope = u32;
type AudioObjectPropertyElement = u32;
type AudioServerPlugInDriverRef = *mut *const AudioServerPlugInDriverInterface;
type AudioServerPlugInHostRef = *const AudioServerPlugInHostInterface;
type CFAllocatorRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CFUUIDBytes {
    byte0: u8,
    byte1: u8,
    byte2: u8,
    byte3: u8,
    byte4: u8,
    byte5: u8,
    byte6: u8,
    byte7: u8,
    byte8: u8,
    byte9: u8,
    byte10: u8,
    byte11: u8,
    byte12: u8,
    byte13: u8,
    byte14: u8,
    byte15: u8,
}

impl CFUUIDBytes {
    const fn new(a: u32, b: u16, c: u16, tail: [u8; 8]) -> Self {
        Self {
            byte0: (a >> 24) as u8,
            byte1: (a >> 16) as u8,
            byte2: (a >> 8) as u8,
            byte3: a as u8,
            byte4: (b >> 8) as u8,
            byte5: b as u8,
            byte6: (c >> 8) as u8,
            byte7: c as u8,
            byte8: tail[0],
            byte9: tail[1],
            byte10: tail[2],
            byte11: tail[3],
            byte12: tail[4],
            byte13: tail[5],
            byte14: tail[6],
            byte15: tail[7],
        }
    }
}

unsafe extern "C" {
    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringCompare(a: CFStringRef, b: CFStringRef, options: u32) -> i64;
    fn CFURLCreateWithFileSystemPath(
        allocator: CFAllocatorRef,
        file_path: CFStringRef,
        path_style: i32,
        is_directory: Boolean,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;
    fn _os_log_debug(dso: *const c_void, log: *mut c_void, format: *const c_char, ...);
}

const K_CF_URL_POSIX_PATH_STYLE: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

const ZTS_PERIOD_FRAMES: u64 = 48_000;
const REGISTER_RETRY_MS: u32 = 1_000;

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioObjectPropertyAddress {
    pub mSelector: AudioObjectPropertySelector,
    pub mScope: AudioObjectPropertyScope,
    pub mElement: AudioObjectPropertyElement,
}

#[repr(C)]
#[derive(Debug)]
pub struct AudioServerPlugInClientInfo {
    pub mClientID: UInt32,
    pub mProcessID: libc::pid_t,
    pub mIsNativeEndian: Boolean,
    pub mBundleID: CFStringRef,
}

#[repr(C)]
#[derive(Debug)]
pub struct AudioServerPlugInIOCycleInfo {
    pub mIOCycleCounter: UInt64,
    pub mNominalIOBufferFrameSize: UInt32,
    pub mCurrentTime: AudioTimeStamp,
    pub mInputTime: AudioTimeStamp,
    pub mOutputTime: AudioTimeStamp,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AudioTimeStamp {
    pub mSampleTime: f64,
    pub mHostTime: u64,
    pub mRateScalar: f64,
    pub mWordClockTime: u64,
    pub mSMPTETime: [u8; 16],
    pub mFlags: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioStreamBasicDescription {
    pub mSampleRate: f64,
    pub mFormatID: u32,
    pub mFormatFlags: u32,
    pub mBytesPerPacket: u32,
    pub mFramesPerPacket: u32,
    pub mBytesPerFrame: u32,
    pub mChannelsPerFrame: u32,
    pub mBitsPerChannel: u32,
    pub mReserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioValueRange {
    pub mMinimum: f64,
    pub mMaximum: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioStreamRangedDescription {
    pub mFormat: AudioStreamBasicDescription,
    pub mSampleRateRange: AudioValueRange,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioChannelDescription {
    pub mChannelLabel: u32,
    pub mChannelFlags: u32,
    pub mCoordinates: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonoAudioChannelLayout {
    pub mChannelLayoutTag: u32,
    pub mChannelBitmap: u32,
    pub mNumberChannelDescriptions: u32,
    pub mChannelDescriptions: [AudioChannelDescription; 1],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioBuffer {
    pub mNumberChannels: u32,
    pub mDataByteSize: u32,
    pub mData: *mut c_void,
}

#[repr(C)]
#[derive(Debug)]
pub struct AudioBufferList {
    pub mNumberBuffers: u32,
    pub mBuffers: [AudioBuffer; 1],
}

#[repr(C)]
#[derive(Debug)]
pub struct AudioServerPlugInHostInterface {
    _private: [usize; 5],
}

#[repr(C)]
#[derive(Debug)]
pub struct AudioServerPlugInDriverInterface {
    pub _reserved: *mut c_void,
    pub QueryInterface: extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> HRESULT,
    pub AddRef: extern "C" fn(*mut c_void) -> ULONG,
    pub Release: extern "C" fn(*mut c_void) -> ULONG,
    pub Initialize: extern "C" fn(AudioServerPlugInDriverRef, AudioServerPlugInHostRef) -> OSStatus,
    pub CreateDevice: extern "C" fn(
        AudioServerPlugInDriverRef,
        CFDictionaryRef,
        *const AudioServerPlugInClientInfo,
        *mut AudioObjectID,
    ) -> OSStatus,
    pub DestroyDevice: extern "C" fn(AudioServerPlugInDriverRef, AudioObjectID) -> OSStatus,
    pub AddDeviceClient: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        *const AudioServerPlugInClientInfo,
    ) -> OSStatus,
    pub RemoveDeviceClient: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        *const AudioServerPlugInClientInfo,
    ) -> OSStatus,
    pub PerformDeviceConfigurationChange:
        extern "C" fn(AudioServerPlugInDriverRef, AudioObjectID, u64, *mut c_void) -> OSStatus,
    pub AbortDeviceConfigurationChange:
        extern "C" fn(AudioServerPlugInDriverRef, AudioObjectID, u64, *mut c_void) -> OSStatus,
    pub HasProperty: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
    ) -> Boolean,
    pub IsPropertySettable: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        *mut Boolean,
    ) -> OSStatus,
    pub GetPropertyDataSize: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        *mut u32,
    ) -> OSStatus,
    pub GetPropertyData: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        u32,
        *mut u32,
        *mut c_void,
    ) -> OSStatus,
    pub SetPropertyData: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        libc::pid_t,
        *const AudioObjectPropertyAddress,
        u32,
        *const c_void,
        u32,
        *const c_void,
    ) -> OSStatus,
    pub StartIO: extern "C" fn(AudioServerPlugInDriverRef, AudioObjectID, u32) -> OSStatus,
    pub StopIO: extern "C" fn(AudioServerPlugInDriverRef, AudioObjectID, u32) -> OSStatus,
    pub GetZeroTimeStamp: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        u32,
        *mut f64,
        *mut u64,
        *mut u64,
    ) -> OSStatus,
    pub WillDoIOOperation: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        u32,
        u32,
        *mut Boolean,
        *mut Boolean,
    ) -> OSStatus,
    pub BeginIOOperation: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
    pub DoIOOperation: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
        *mut c_void,
        *mut c_void,
    ) -> OSStatus,
    pub EndIOOperation: extern "C" fn(
        AudioServerPlugInDriverRef,
        AudioObjectID,
        u32,
        u32,
        u32,
        *const AudioServerPlugInIOCycleInfo,
    ) -> OSStatus,
}

static DRIVER_INTERFACE: AudioServerPlugInDriverInterface = AudioServerPlugInDriverInterface {
    _reserved: null_mut(),
    QueryInterface: query_interface,
    AddRef: add_ref,
    Release: release,
    Initialize: initialize,
    CreateDevice: create_device,
    DestroyDevice: destroy_device,
    AddDeviceClient: add_device_client,
    RemoveDeviceClient: remove_device_client,
    PerformDeviceConfigurationChange: perform_device_configuration_change,
    AbortDeviceConfigurationChange: abort_device_configuration_change,
    HasProperty: has_property,
    IsPropertySettable: is_property_settable,
    GetPropertyDataSize: get_property_data_size,
    GetPropertyData: get_property_data,
    SetPropertyData: set_property_data,
    StartIO: start_io,
    StopIO: stop_io,
    GetZeroTimeStamp: get_zero_time_stamp,
    WillDoIOOperation: will_do_io_operation,
    BeginIOOperation: begin_io_operation,
    DoIOOperation: do_io_operation,
    EndIOOperation: end_io_operation,
};

#[repr(transparent)]
struct DriverRef(*const AudioServerPlugInDriverInterface);

// SAFETY: immutable pointer to the static driver vtable.
unsafe impl Sync for DriverRef {}

static DRIVER_REF_PTR: DriverRef = DriverRef(&DRIVER_INTERFACE);
static REF_COUNT: AtomicU32 = AtomicU32::new(1);
static RUNNING_CLIENTS: AtomicUsize = AtomicUsize::new(0);
static SAMPLE_TIME: AtomicU64 = AtomicU64::new(0);
static TIMESTAMP_SEED: AtomicU64 = AtomicU64::new(1);
static START_HOST_TIME: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
static TIMEBASE: OnceLock<MachTimebaseInfo> = OnceLock::new();
static RECEIVER_STARTED: OnceLock<()> = OnceLock::new();
static RING: LazyLock<SpscRing> = LazyLock::new(|| SpscRing::new(RING_SAMPLES));

// SAFETY: the interface table is immutable function pointers and contains no Rust references.
unsafe impl Sync for AudioServerPlugInDriverInterface {}

#[derive(Debug)]
pub struct SpscRing {
    samples: Box<[AtomicU32]>,
    capacity: usize,
    read: AtomicUsize,
    write: AtomicUsize,
    underruns: AtomicU64,
    overruns: AtomicU64,
    clear_epoch: AtomicU64,
    consumer_epoch: AtomicU64,
}

impl SpscRing {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let samples = (0..capacity)
            .map(|_| AtomicU32::new(0))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            samples,
            capacity,
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            clear_epoch: AtomicU64::new(0),
            consumer_epoch: AtomicU64::new(0),
        }
    }

    pub fn push_samples(&self, samples: &[i16]) {
        if RUNNING_CLIENTS.load(Ordering::Acquire) == 0 {
            self.overruns.fetch_add(
                u64::try_from(samples.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            return;
        }
        let mut write = self.write.load(Ordering::Relaxed);
        for sample in samples {
            let read = self.read.load(Ordering::Acquire);
            let next = (write + 1) % self.capacity;
            if next == read {
                self.overruns.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let value = f32::from(*sample) / 32768.0;
            self.samples[write].store(value.to_bits(), Ordering::Relaxed);
            write = next;
            self.write.store(write, Ordering::Release);
        }
    }

    fn reconcile_clear(&self) {
        let wanted = self.clear_epoch.load(Ordering::Acquire);
        if self.consumer_epoch.load(Ordering::Acquire) != wanted {
            let write = self.write.load(Ordering::Acquire);
            self.read.store(write, Ordering::Release);
            self.consumer_epoch.store(wanted, Ordering::Release);
        }
    }

    pub fn pop_f32(&self) -> f32 {
        self.reconcile_clear();
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read == write {
            self.underruns.fetch_add(1, Ordering::Relaxed);
            return 0.0;
        }
        let bits = self.samples[read].load(Ordering::Relaxed);
        self.read
            .store((read + 1) % self.capacity, Ordering::Release);
        f32::from_bits(bits)
    }

    pub fn clear(&self) {
        self.clear_epoch.fetch_add(1, Ordering::AcqRel);
    }

    #[must_use]
    pub fn queued(&self) -> usize {
        self.reconcile_clear();
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);
        if write >= read {
            write - read
        } else {
            self.capacity - read + write
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DriverRegisterMessage {
    header: mach_msg_header_t,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrameMessage {
    header: mach_msg_header_t,
    sequence: u32,
    generation: u32,
    token: u64,
    samples: [i16; FRAME_SAMPLES],
}

fn catch_status(function: impl FnOnce() -> OSStatus) -> OSStatus {
    std::panic::catch_unwind(AssertUnwindSafe(function))
        .unwrap_or(K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR)
}

fn catch_bool(function: impl FnOnce() -> Boolean) -> Boolean {
    std::panic::catch_unwind(AssertUnwindSafe(function)).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn ArcenMicrophoneFactory(
    _allocator: CFAllocatorRef,
    type_uuid: CFUUIDRef,
) -> *mut c_void {
    if type_uuid.is_null() {
        return null_mut();
    }
    // SAFETY: CoreFoundation passes a valid CFUUIDRef to CFPlugIn factories.
    let requested = unsafe { CFUUIDGetUUIDBytes(type_uuid) };
    if requested != PLUGIN_TYPE_UUID {
        return null_mut();
    }
    add_ref(null_mut());
    (&raw const DRIVER_REF_PTR).cast_mut().cast::<c_void>()
}

extern "C" fn query_interface(
    _driver: *mut c_void,
    in_uuid: CFUUIDBytes,
    out_interface: *mut *mut c_void,
) -> HRESULT {
    if out_interface.is_null() {
        return -1;
    }
    // SAFETY: out_interface is checked non-null and owned by the CoreFoundation caller.
    unsafe {
        if in_uuid == DRIVER_INTERFACE_UUID || in_uuid == IUNKNOWN_UUID {
            *out_interface = (&raw const DRIVER_REF_PTR).cast_mut().cast::<c_void>();
            add_ref(null_mut());
            0
        } else {
            *out_interface = null_mut();
            -1
        }
    }
}

extern "C" fn add_ref(_driver: *mut c_void) -> ULONG {
    REF_COUNT.fetch_add(1, Ordering::AcqRel).saturating_add(1)
}

extern "C" fn release(_driver: *mut c_void) -> ULONG {
    REF_COUNT.fetch_sub(1, Ordering::AcqRel).saturating_sub(1)
}

extern "C" fn initialize(
    _driver: AudioServerPlugInDriverRef,
    _host: AudioServerPlugInHostRef,
) -> OSStatus {
    catch_status(|| {
        LazyLock::force(&RING);
        TIMEBASE.get_or_init(read_timebase);
        start_receiver_once();
        NO_ERR
    })
}

extern "C" fn create_device(
    _driver: AudioServerPlugInDriverRef,
    _description: CFDictionaryRef,
    _client: *const AudioServerPlugInClientInfo,
    out_device: *mut AudioObjectID,
) -> OSStatus {
    catch_status(|| {
        if !out_device.is_null() {
            // SAFETY: CoreAudio supplied storage for the object id.
            unsafe { *out_device = DEVICE_OBJECT };
        }
        NO_ERR
    })
}

extern "C" fn destroy_device(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
) -> OSStatus {
    NO_ERR
}
extern "C" fn add_device_client(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
    _client: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    NO_ERR
}
extern "C" fn remove_device_client(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
    _client: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    NO_ERR
}
extern "C" fn perform_device_configuration_change(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
    _action: u64,
    _info: *mut c_void,
) -> OSStatus {
    NO_ERR
}
extern "C" fn abort_device_configuration_change(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
    _action: u64,
    _info: *mut c_void,
) -> OSStatus {
    NO_ERR
}

extern "C" fn has_property(
    _driver: AudioServerPlugInDriverRef,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
) -> Boolean {
    catch_bool(|| {
        if address.is_null() {
            return 0;
        }
        // SAFETY: CoreAudio passes a valid address pointer for property callbacks.
        let address = unsafe { *address };
        property_size(object, address).is_some().into()
    })
}

extern "C" fn is_property_settable(
    _driver: AudioServerPlugInDriverRef,
    _object: AudioObjectID,
    _pid: libc::pid_t,
    _address: *const AudioObjectPropertyAddress,
    out_settable: *mut Boolean,
) -> OSStatus {
    catch_status(|| {
        if out_settable.is_null() {
            return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
        }
        // SAFETY: CoreAudio supplied writable storage for a Boolean result.
        unsafe { *out_settable = 0 };
        NO_ERR
    })
}

extern "C" fn get_property_data_size(
    _driver: AudioServerPlugInDriverRef,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    _q_size: u32,
    _q_data: *const c_void,
    out_size: *mut u32,
) -> OSStatus {
    catch_status(|| {
        if address.is_null() || out_size.is_null() {
            return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
        }
        // SAFETY: pointers checked above.
        let Some(size) = property_size(object, unsafe { *address }) else {
            // SAFETY: address is non-null and points to a CoreAudio property address.
            log_unknown_property(object, unsafe { *address });
            return K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR;
        };
        // SAFETY: CoreAudio supplied writable storage for the size result.
        unsafe { *out_size = size };
        NO_ERR
    })
}

extern "C" fn get_property_data(
    _driver: AudioServerPlugInDriverRef,
    object: AudioObjectID,
    _pid: libc::pid_t,
    address: *const AudioObjectPropertyAddress,
    q_size: u32,
    q_data: *const c_void,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    catch_status(|| {
        if address.is_null() || out_size.is_null() || out_data.is_null() {
            return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
        }
        // SAFETY: checked by caller contract and non-null guards above.
        let address = unsafe { *address };
        if property_size(object, address).is_none() {
            log_unknown_property(object, address);
        }
        write_property(
            object, address, q_size, q_data, data_size, out_size, out_data,
        )
    })
}

extern "C" fn set_property_data(
    _driver: AudioServerPlugInDriverRef,
    _object: AudioObjectID,
    _pid: libc::pid_t,
    _address: *const AudioObjectPropertyAddress,
    _q_size: u32,
    _q_data: *const c_void,
    _data_size: u32,
    _data: *const c_void,
) -> OSStatus {
    K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR
}

extern "C" fn start_io(
    _driver: AudioServerPlugInDriverRef,
    device: AudioObjectID,
    _client: u32,
) -> OSStatus {
    catch_status(|| {
        if device != DEVICE_OBJECT {
            return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR;
        }
        LazyLock::force(&RING);
        if RUNNING_CLIENTS.fetch_add(1, Ordering::AcqRel) == 0 {
            RING.clear();
            SAMPLE_TIME.store(0, Ordering::Release);
            // SAFETY: mach_absolute_time is safe to call and returns the current host tick.
            START_HOST_TIME.store(unsafe { mach_absolute_time() }, Ordering::Release);
            STARTED.store(true, Ordering::Release);
        }
        NO_ERR
    })
}

extern "C" fn stop_io(
    _driver: AudioServerPlugInDriverRef,
    device: AudioObjectID,
    _client: u32,
) -> OSStatus {
    catch_status(|| {
        if device != DEVICE_OBJECT {
            return K_AUDIO_HARDWARE_BAD_OBJECT_ERROR;
        }
        RUNNING_CLIENTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            })
            .ok();
        NO_ERR
    })
}

extern "C" fn get_zero_time_stamp(
    _driver: AudioServerPlugInDriverRef,
    device: AudioObjectID,
    _client: u32,
    out_sample_time: *mut f64,
    out_host_time: *mut u64,
    out_seed: *mut u64,
) -> OSStatus {
    catch_status(|| {
        if device != DEVICE_OBJECT
            || out_sample_time.is_null()
            || out_host_time.is_null()
            || out_seed.is_null()
        {
            return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
        }
        let (sample_time, host_time) = period_aligned_zero_timestamp();
        // SAFETY: CoreAudio supplied non-null output pointers checked above.
        unsafe {
            *out_sample_time = sample_time as f64;
            *out_host_time = host_time;
            *out_seed = TIMESTAMP_SEED.load(Ordering::Acquire);
        }
        NO_ERR
    })
}

extern "C" fn will_do_io_operation(
    _driver: AudioServerPlugInDriverRef,
    device: AudioObjectID,
    _client: u32,
    operation: u32,
    out_will_do: *mut Boolean,
    out_in_place: *mut Boolean,
) -> OSStatus {
    catch_status(|| {
        if device != DEVICE_OBJECT || out_will_do.is_null() || out_in_place.is_null() {
            return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
        }
        let will = matches!(operation, x if x == fourcc(b"read") || x == fourcc(b"thrd") || x == fourcc(b"cycl"));
        // SAFETY: CoreAudio supplied non-null Boolean output pointers.
        unsafe {
            *out_will_do = will.into();
            *out_in_place = 1;
        }
        NO_ERR
    })
}

extern "C" fn begin_io_operation(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    NO_ERR
}

extern "C" fn do_io_operation(
    _driver: AudioServerPlugInDriverRef,
    device: AudioObjectID,
    stream: AudioObjectID,
    _client: u32,
    operation: u32,
    frames: u32,
    _cycle: *const AudioServerPlugInIOCycleInfo,
    io_main: *mut c_void,
    _secondary: *mut c_void,
) -> OSStatus {
    catch_status(|| {
        if device != DEVICE_OBJECT || stream != STREAM_OBJECT || operation != fourcc(b"read") {
            return NO_ERR;
        }
        if io_main.is_null() {
            return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
        }
        let frame_count = usize::try_from(frames).unwrap_or(0);
        // SAFETY: for ReadInput CoreAudio gives a writable Float32 mono buffer of at least
        // `frames` samples matching the fixed ASBD this driver advertises.
        let out = unsafe { std::slice::from_raw_parts_mut(io_main.cast::<f32>(), frame_count) };
        for sample in out.iter_mut() {
            *sample = RING.pop_f32();
        }
        let _ = frames;
        NO_ERR
    })
}

extern "C" fn end_io_operation(
    _driver: AudioServerPlugInDriverRef,
    _device: AudioObjectID,
    _client: u32,
    _operation: u32,
    _frames: u32,
    _cycle: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    NO_ERR
}

fn property_size(object: AudioObjectID, address: AudioObjectPropertyAddress) -> Option<u32> {
    let selector = address.mSelector;
    let size = match object {
        PLUGIN_OBJECT => match selector {
            K_AUDIO_OBJECT_PROPERTY_BASE_CLASS
            | K_AUDIO_OBJECT_PROPERTY_CLASS
            | K_AUDIO_OBJECT_PROPERTY_OWNER
            | K_AUDIO_OBJECT_PROPERTY_IDENTIFY => size_of::<u32>(),
            K_AUDIO_OBJECT_PROPERTY_NAME
            | K_AUDIO_OBJECT_PROPERTY_MANUFACTURER
            | K_AUDIO_OBJECT_PROPERTY_MODEL_NAME
            | K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER
            | K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION
            | K_AUDIO_PLUGIN_PROPERTY_RESOURCE_BUNDLE => size_of::<CFStringRef>(),
            K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_PLUGIN_PROPERTY_DEVICE_LIST => {
                size_of::<AudioObjectID>()
            }
            K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST
            | K_AUDIO_PLUGIN_PROPERTY_BOX_LIST
            | K_AUDIO_PLUGIN_PROPERTY_CLOCK_DEVICE_LIST => 0,
            K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_DEVICE
            | K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_BOX
            | K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_CLOCK_DEVICE => size_of::<AudioObjectID>(),
            _ => return None,
        },
        DEVICE_OBJECT => match selector {
            K_AUDIO_OBJECT_PROPERTY_BASE_CLASS
            | K_AUDIO_OBJECT_PROPERTY_CLASS
            | K_AUDIO_OBJECT_PROPERTY_OWNER
            | K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE
            | K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_RUNNING
            | K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT
            | K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT_SYSTEM
            | K_AUDIO_DEVICE_PROPERTY_LATENCY
            | K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET
            | K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN
            | K_AUDIO_DEVICE_PROPERTY_ZERO_TIMESTAMP_PERIOD
            | K_AUDIO_DEVICE_PROPERTY_CLOCK_ALGORITHM
            | K_AUDIO_DEVICE_PROPERTY_CLOCK_IS_STABLE
            | K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE
            | K_AUDIO_DEVICE_PROPERTY_WANTS_CONTROLS_RESTORED
            | K_AUDIO_DEVICE_PROPERTY_WANTS_STREAM_FORMATS_RESTORED
            | K_AUDIO_OBJECT_PROPERTY_IDENTIFY => size_of::<u32>(),
            K_AUDIO_OBJECT_PROPERTY_NAME
            | K_AUDIO_OBJECT_PROPERTY_MANUFACTURER
            | K_AUDIO_OBJECT_PROPERTY_MODEL_NAME
            | K_AUDIO_OBJECT_PROPERTY_ELEMENT_NAME
            | K_AUDIO_OBJECT_PROPERTY_ELEMENT_CATEGORY_NAME
            | K_AUDIO_OBJECT_PROPERTY_ELEMENT_NUMBER_NAME
            | K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER
            | K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION
            | K_AUDIO_DEVICE_PROPERTY_DEVICE_UID
            | K_AUDIO_DEVICE_PROPERTY_MODEL_UID => size_of::<CFStringRef>(),
            K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_DEVICE_PROPERTY_RELATED_DEVICES => {
                size_of::<AudioObjectID>()
            }
            K_AUDIO_DEVICE_PROPERTY_STREAMS => scoped_object_array_size(address.mScope),
            K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST
            | K_AUDIO_OBJECT_PROPERTY_CONTROL_LIST => 0,
            K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE => size_of::<f64>(),
            K_AUDIO_DEVICE_PROPERTY_ICON => size_of::<*const c_void>(),
            K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO => {
                scoped_input_size(address.mScope, size_of::<[u32; 2]>())?
            }
            K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNEL_LAYOUT => {
                scoped_input_size(address.mScope, size_of::<MonoAudioChannelLayout>())?
            }
            K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES
            | K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE => size_of::<AudioValueRange>(),
            K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION => {
                stream_configuration_size(address.mScope)
            }
            _ => return None,
        },
        STREAM_OBJECT => match selector {
            K_AUDIO_OBJECT_PROPERTY_BASE_CLASS
            | K_AUDIO_OBJECT_PROPERTY_CLASS
            | K_AUDIO_OBJECT_PROPERTY_OWNER
            | K_AUDIO_STREAM_PROPERTY_IS_ACTIVE
            | K_AUDIO_STREAM_PROPERTY_DIRECTION
            | K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE
            | K_AUDIO_STREAM_PROPERTY_STARTING_CHANNEL
            | K_AUDIO_DEVICE_PROPERTY_LATENCY
            | K_AUDIO_OBJECT_PROPERTY_IDENTIFY => size_of::<u32>(),
            K_AUDIO_OBJECT_PROPERTY_NAME
            | K_AUDIO_OBJECT_PROPERTY_MANUFACTURER
            | K_AUDIO_OBJECT_PROPERTY_MODEL_NAME
            | K_AUDIO_OBJECT_PROPERTY_ELEMENT_NAME
            | K_AUDIO_OBJECT_PROPERTY_ELEMENT_CATEGORY_NAME
            | K_AUDIO_OBJECT_PROPERTY_ELEMENT_NUMBER_NAME
            | K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER
            | K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION => size_of::<CFStringRef>(),
            K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST => 0,
            K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT | K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT => {
                size_of::<AudioStreamBasicDescription>()
            }
            K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS
            | K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS => {
                size_of::<AudioStreamRangedDescription>()
            }
            _ => return None,
        },
        _ => return None,
    };
    u32::try_from(size).ok()
}

fn scoped_object_array_size(scope: AudioObjectPropertyScope) -> usize {
    if is_output_scope(scope) {
        0
    } else {
        size_of::<AudioObjectID>()
    }
}

fn scoped_input_size(scope: AudioObjectPropertyScope, size: usize) -> Option<usize> {
    (!is_output_scope(scope)).then_some(size)
}

fn stream_configuration_size(scope: AudioObjectPropertyScope) -> usize {
    if is_output_scope(scope) {
        size_of::<u32>()
    } else {
        size_of::<AudioBufferList>()
    }
}

fn is_output_scope(scope: AudioObjectPropertyScope) -> bool {
    scope == K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT
}

fn write_property(
    object: AudioObjectID,
    address: AudioObjectPropertyAddress,
    q_size: u32,
    q_data: *const c_void,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    match object {
        PLUGIN_OBJECT => {
            write_plugin_property(address, q_size, q_data, data_size, out_size, out_data)
        }
        DEVICE_OBJECT => write_device_property(address, data_size, out_size, out_data),
        STREAM_OBJECT => write_stream_property(address, data_size, out_size, out_data),
        _ => K_AUDIO_HARDWARE_BAD_OBJECT_ERROR,
    }
}

fn write_plugin_property(
    address: AudioObjectPropertyAddress,
    q_size: u32,
    q_data: *const c_void,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    match address.mSelector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS | K_AUDIO_OBJECT_PROPERTY_CLASS => {
            write_value(K_AUDIO_PLUGIN_CLASS_ID, data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_OWNER => write_value(OBJECT_UNKNOWN, data_size, out_size, out_data),
        K_AUDIO_OBJECT_PROPERTY_NAME => {
            write_cf_string("Arcen Microphone Plug-In", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_MANUFACTURER => {
            write_cf_string("Arcen", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_MODEL_NAME => {
            write_cf_string("Arcen Microphone", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_IDENTIFY => write_value(0_u32, data_size, out_size, out_data),
        K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER | K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION => {
            write_cf_string("0.15.0", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST => {
            write_array::<u8>(&[], data_size, out_size, out_data)
        }
        K_AUDIO_PLUGIN_PROPERTY_RESOURCE_BUNDLE => {
            write_cf_string("", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_PLUGIN_PROPERTY_DEVICE_LIST => {
            write_array(&[DEVICE_OBJECT], data_size, out_size, out_data)
        }
        K_AUDIO_PLUGIN_PROPERTY_BOX_LIST | K_AUDIO_PLUGIN_PROPERTY_CLOCK_DEVICE_LIST => {
            write_array::<AudioObjectID>(&[], data_size, out_size, out_data)
        }
        K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_DEVICE => {
            let object = if qualifier_is_device_uid(q_size, q_data) {
                DEVICE_OBJECT
            } else {
                OBJECT_UNKNOWN
            };
            write_value(object, data_size, out_size, out_data)
        }
        K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_BOX => {
            if q_size != u32::try_from(size_of::<CFStringRef>()).unwrap_or(u32::MAX)
                || q_data.is_null()
            {
                return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
            }
            write_value(OBJECT_UNKNOWN, data_size, out_size, out_data)
        }
        K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_CLOCK_DEVICE => {
            if q_size != u32::try_from(size_of::<CFStringRef>()).unwrap_or(u32::MAX)
                || q_data.is_null()
            {
                return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
            }
            write_value(OBJECT_UNKNOWN, data_size, out_size, out_data)
        }
        _ => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

fn qualifier_is_device_uid(q_size: u32, q_data: *const c_void) -> bool {
    if q_size != u32::try_from(size_of::<CFStringRef>()).unwrap_or(u32::MAX) || q_data.is_null() {
        return false;
    }
    // SAFETY: the qualifier is validated to contain a CFStringRef-sized value supplied by CoreAudio.
    let candidate = unsafe { q_data.cast::<CFStringRef>().read() };
    if candidate.is_null() {
        return false;
    }
    let Ok(uid) = CString::new("tech.arcen.microphone.input") else {
        return false;
    };
    // SAFETY: uid is a valid C string and CoreFoundation returns a retained string reference.
    let expected =
        unsafe { CFStringCreateWithCString(null(), uid.as_ptr(), K_CF_STRING_ENCODING_UTF8) };
    if expected.is_null() {
        return false;
    }
    // SAFETY: both values are valid CFStringRef objects for comparison; expected is released after use.
    let equal = unsafe { CFStringCompare(candidate, expected, 0) == 0 };
    // SAFETY: expected was created by CFStringCreateWithCString in this function.
    unsafe { CFRelease(expected) };
    equal
}

fn write_device_property(
    address: AudioObjectPropertyAddress,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    match address.mSelector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS | K_AUDIO_OBJECT_PROPERTY_CLASS => {
            write_value(K_AUDIO_DEVICE_CLASS_ID, data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_OWNER => write_value(PLUGIN_OBJECT, data_size, out_size, out_data),
        K_AUDIO_OBJECT_PROPERTY_NAME => {
            write_cf_string("Arcen Microphone", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_MANUFACTURER => {
            write_cf_string("Arcen", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_MODEL_NAME => {
            write_cf_string("Arcen Microphone", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_ELEMENT_NAME
        | K_AUDIO_OBJECT_PROPERTY_ELEMENT_CATEGORY_NAME
        | K_AUDIO_OBJECT_PROPERTY_ELEMENT_NUMBER_NAME => {
            write_cf_string("Microphone", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_IDENTIFY => write_value(0_u32, data_size, out_size, out_data),
        K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER | K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION => {
            write_cf_string("0.15.0", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST
        | K_AUDIO_OBJECT_PROPERTY_CONTROL_LIST => {
            write_array::<u8>(&[], data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_DEVICE_UID => {
            write_cf_string("tech.arcen.microphone.input", data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_MODEL_UID => {
            write_cf_string("tech.arcen.microphone", data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_TRANSPORT_TYPE => write_value(
            K_AUDIO_DEVICE_TRANSPORT_TYPE_VIRTUAL,
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_RELATED_DEVICES => {
            write_array(&[DEVICE_OBJECT], data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_CLOCK_DOMAIN => write_value(0_u32, data_size, out_size, out_data),
        K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_ALIVE => {
            write_value(1_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_DEVICE_IS_RUNNING => write_value(
            (RUNNING_CLIENTS.load(Ordering::Acquire) > 0) as u32,
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT => write_value(
            u32::from(!is_output_scope(address.mScope)),
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT_SYSTEM => {
            write_value(0_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_LATENCY | K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET => {
            write_value(0_u32, data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_OWNED_OBJECTS | K_AUDIO_DEVICE_PROPERTY_STREAMS => {
            if address.mSelector == K_AUDIO_DEVICE_PROPERTY_STREAMS
                && is_output_scope(address.mScope)
            {
                write_array::<AudioObjectID>(&[], data_size, out_size, out_data)
            } else {
                write_array(&[STREAM_OBJECT], data_size, out_size, out_data)
            }
        }
        K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE => {
            write_value(SAMPLE_RATE_HZ, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_AVAILABLE_NOMINAL_SAMPLE_RATES => write_value(
            AudioValueRange {
                mMinimum: SAMPLE_RATE_HZ,
                mMaximum: SAMPLE_RATE_HZ,
            },
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_ICON => write_cf_url(
            "/Library/Audio/Plug-Ins/HAL/ArcenMicrophone.driver",
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_IS_HIDDEN => write_value(0_u32, data_size, out_size, out_data),
        K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNELS_FOR_STEREO => {
            if is_output_scope(address.mScope) {
                return K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR;
            }
            write_array(&[1_u32, 1_u32], data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNEL_LAYOUT => {
            if is_output_scope(address.mScope) {
                return K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR;
            }
            write_value(mono_channel_layout(), data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_WANTS_CONTROLS_RESTORED
        | K_AUDIO_DEVICE_PROPERTY_WANTS_STREAM_FORMATS_RESTORED => {
            write_value(0_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_ZERO_TIMESTAMP_PERIOD => {
            write_value(48_000_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_CLOCK_ALGORITHM => write_value(
            K_AUDIO_DEVICE_CLOCK_ALGORITHM_RAW,
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_CLOCK_IS_STABLE => {
            write_value(1_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE => {
            write_value(960_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE => write_value(
            AudioValueRange {
                mMinimum: 128.0,
                mMaximum: 4096.0,
            },
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION => {
            write_stream_configuration(address.mScope, data_size, out_size, out_data)
        }
        _ => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

fn write_stream_property(
    address: AudioObjectPropertyAddress,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    match address.mSelector {
        K_AUDIO_OBJECT_PROPERTY_BASE_CLASS | K_AUDIO_OBJECT_PROPERTY_CLASS => {
            write_value(K_AUDIO_STREAM_CLASS_ID, data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_OWNER => write_value(DEVICE_OBJECT, data_size, out_size, out_data),
        K_AUDIO_OBJECT_PROPERTY_NAME => {
            write_cf_string("Arcen Microphone Stream", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_MANUFACTURER => {
            write_cf_string("Arcen", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_MODEL_NAME => {
            write_cf_string("Arcen Microphone", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_ELEMENT_NAME
        | K_AUDIO_OBJECT_PROPERTY_ELEMENT_CATEGORY_NAME
        | K_AUDIO_OBJECT_PROPERTY_ELEMENT_NUMBER_NAME => {
            write_cf_string("Microphone", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_IDENTIFY => write_value(0_u32, data_size, out_size, out_data),
        K_AUDIO_OBJECT_PROPERTY_SERIAL_NUMBER | K_AUDIO_OBJECT_PROPERTY_FIRMWARE_VERSION => {
            write_cf_string("0.15.0", data_size, out_size, out_data)
        }
        K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST => {
            write_array::<u8>(&[], data_size, out_size, out_data)
        }
        K_AUDIO_STREAM_PROPERTY_IS_ACTIVE => write_value(1_u32, data_size, out_size, out_data),
        K_AUDIO_STREAM_PROPERTY_DIRECTION => write_value(1_u32, data_size, out_size, out_data),
        K_AUDIO_STREAM_PROPERTY_TERMINAL_TYPE => write_value(
            K_AUDIO_STREAM_TERMINAL_TYPE_MICROPHONE,
            data_size,
            out_size,
            out_data,
        ),
        K_AUDIO_STREAM_PROPERTY_STARTING_CHANNEL => {
            write_value(1_u32, data_size, out_size, out_data)
        }
        K_AUDIO_DEVICE_PROPERTY_LATENCY => write_value(0_u32, data_size, out_size, out_data),
        K_AUDIO_STREAM_PROPERTY_VIRTUAL_FORMAT | K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT => {
            write_value(fixed_asbd(), data_size, out_size, out_data)
        }
        K_AUDIO_STREAM_PROPERTY_AVAILABLE_VIRTUAL_FORMATS
        | K_AUDIO_STREAM_PROPERTY_AVAILABLE_PHYSICAL_FORMATS => write_value(
            AudioStreamRangedDescription {
                mFormat: fixed_asbd(),
                mSampleRateRange: AudioValueRange {
                    mMinimum: SAMPLE_RATE_HZ,
                    mMaximum: SAMPLE_RATE_HZ,
                },
            },
            data_size,
            out_size,
            out_data,
        ),
        _ => K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR,
    }
}

#[must_use]
pub const fn fixed_asbd() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: SAMPLE_RATE_HZ,
        mFormatID: K_AUDIO_FORMAT_LINEAR_PCM,
        mFormatFlags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: CHANNELS,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

fn write_stream_configuration(
    scope: AudioObjectPropertyScope,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let needed = stream_configuration_size(scope);
    if usize::try_from(data_size).unwrap_or(0) < needed {
        return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
    }
    if is_output_scope(scope) {
        // SAFETY: the output buffer is large enough for the variable header field.
        unsafe {
            out_data.cast::<u32>().write(0);
            *out_size = u32::try_from(needed).unwrap_or(u32::MAX);
        }
        return NO_ERR;
    }
    // SAFETY: the output buffer is at least `needed` bytes and AudioBufferList is C-compatible.
    unsafe {
        let list = out_data.cast::<AudioBufferList>();
        (*list).mNumberBuffers = 1;
        (*list).mBuffers[0] = AudioBuffer {
            mNumberChannels: CHANNELS,
            mDataByteSize: 0,
            mData: null_mut(),
        };
        *out_size = u32::try_from(needed).unwrap_or(u32::MAX);
    }
    NO_ERR
}

fn write_cf_string(
    value: &str,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let validation = validate_value_buffer::<CFStringRef>(data_size, out_data);
    if validation != NO_ERR {
        return validation;
    }
    let Ok(c_string) = CString::new(value) else {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    };
    // SAFETY: CString is NUL-terminated and valid for the duration of the call;
    // CoreAudio owns the returned retained CFStringRef according to property conventions.
    let string =
        unsafe { CFStringCreateWithCString(null(), c_string.as_ptr(), K_CF_STRING_ENCODING_UTF8) };
    if string.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    let status = write_value(string, data_size, out_size, out_data);
    if status != NO_ERR {
        // SAFETY: ownership transfers to CoreAudio only after a successful property write.
        unsafe { CFRelease(string) };
    }
    status
}

fn write_cf_url(path: &str, data_size: u32, out_size: *mut u32, out_data: *mut c_void) -> OSStatus {
    let validation = validate_value_buffer::<*const c_void>(data_size, out_data);
    if validation != NO_ERR {
        return validation;
    }
    let Ok(c_string) = CString::new(path) else {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    };
    // SAFETY: CString is NUL-terminated and valid for this call.
    let string =
        unsafe { CFStringCreateWithCString(null(), c_string.as_ptr(), K_CF_STRING_ENCODING_UTF8) };
    if string.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    // SAFETY: string is a valid CFStringRef; CoreAudio owns the returned retained CFURLRef.
    let url =
        unsafe { CFURLCreateWithFileSystemPath(null(), string, K_CF_URL_POSIX_PATH_STYLE, 0) };
    // SAFETY: string was created by CFStringCreateWithCString above.
    unsafe { CFRelease(string) };
    if url.is_null() {
        return K_AUDIO_HARDWARE_ILLEGAL_OPERATION_ERROR;
    }
    let status = write_value(url, data_size, out_size, out_data);
    if status != NO_ERR {
        // SAFETY: ownership transfers to CoreAudio only after a successful property write.
        unsafe { CFRelease(url) };
    }
    status
}

fn mono_channel_layout() -> MonoAudioChannelLayout {
    MonoAudioChannelLayout {
        mChannelLayoutTag: K_AUDIO_CHANNEL_LAYOUT_TAG_MONO,
        mChannelBitmap: 0,
        mNumberChannelDescriptions: 1,
        mChannelDescriptions: [AudioChannelDescription {
            mChannelLabel: 42,
            mChannelFlags: 0,
            mCoordinates: [0; 3],
        }],
    }
}

fn log_unknown_property(object: AudioObjectID, address: AudioObjectPropertyAddress) {
    let Ok(subsystem) = CString::new("tech.arcen.microphone.driver") else {
        return;
    };
    let Ok(category) = CString::new("properties") else {
        return;
    };
    // SAFETY: strings are valid C strings; os_log debug entries are emitted only when debug logging is enabled.
    let log = unsafe { os_log_create(subsystem.as_ptr(), category.as_ptr()) };
    if log.is_null() {
        return;
    }
    let Ok(format) =
        CString::new("unknown CoreAudio property object=%u selector=%u scope=%u element=%u")
    else {
        return;
    };
    // SAFETY: format and argument types match the os_log format string.
    unsafe {
        _os_log_debug(
            (&raw const DRIVER_INTERFACE).cast::<c_void>(),
            log,
            format.as_ptr(),
            object,
            address.mSelector,
            address.mScope,
            address.mElement,
        )
    };
}

fn write_array<T: Copy>(
    values: &[T],
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let needed = size_of_val(values);
    if usize::try_from(data_size).unwrap_or(0) < needed {
        return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
    }
    // SAFETY: caller supplied a buffer large enough for `values` and both source/dest are non-overlapping.
    unsafe {
        std::ptr::copy_nonoverlapping(values.as_ptr().cast::<u8>(), out_data.cast::<u8>(), needed);
        *out_size = u32::try_from(needed).unwrap_or(u32::MAX);
    }
    NO_ERR
}

fn write_value<T: Copy>(
    value: T,
    data_size: u32,
    out_size: *mut u32,
    out_data: *mut c_void,
) -> OSStatus {
    let validation = validate_value_buffer::<T>(data_size, out_data);
    if validation != NO_ERR {
        return validation;
    }
    // SAFETY: caller supplied aligned, writable storage large enough for T.
    unsafe {
        out_data.cast::<T>().write(value);
        *out_size = u32::try_from(size_of::<T>()).unwrap_or(u32::MAX);
    }
    NO_ERR
}

fn validate_value_buffer<T>(data_size: u32, out_data: *mut c_void) -> OSStatus {
    let needed = size_of::<T>();
    if align_of::<T>() > 1 && !(out_data as usize).is_multiple_of(align_of::<T>()) {
        return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
    }
    if usize::try_from(data_size).unwrap_or(0) < needed {
        return K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR;
    }
    NO_ERR
}

fn read_timebase() -> MachTimebaseInfo {
    let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
    // SAFETY: info points to initialized writable storage for mach_timebase_info.
    let status = unsafe { mach_timebase_info(&mut info) };
    if status != 0 || info.numer == 0 || info.denom == 0 {
        MachTimebaseInfo { numer: 1, denom: 1 }
    } else {
        info
    }
}

fn ticks_to_nanos(ticks: u64, timebase: MachTimebaseInfo) -> u128 {
    u128::from(ticks)
        .saturating_mul(u128::from(timebase.numer))
        .checked_div(u128::from(timebase.denom).max(1))
        .unwrap_or(0)
}

fn nanos_to_ticks(nanos: u128, timebase: MachTimebaseInfo) -> u64 {
    let ticks = nanos
        .saturating_mul(u128::from(timebase.denom))
        .checked_div(u128::from(timebase.numer).max(1))
        .unwrap_or(0);
    u64::try_from(ticks).unwrap_or(u64::MAX)
}

fn period_aligned_zero_timestamp() -> (u64, u64) {
    let timebase = *TIMEBASE.get_or_init(read_timebase);
    let anchor = START_HOST_TIME.load(Ordering::Acquire);
    if anchor == 0 || !STARTED.load(Ordering::Acquire) {
        // SAFETY: mach_absolute_time is safe to call and returns the current host tick.
        return (0, unsafe { mach_absolute_time() });
    }
    // SAFETY: mach_absolute_time is safe to call and returns the current host tick.
    let now = unsafe { mach_absolute_time() };
    let elapsed_ticks = now.saturating_sub(anchor);
    let elapsed_nanos = ticks_to_nanos(elapsed_ticks, timebase);
    let elapsed_samples = elapsed_nanos
        .saturating_mul(u128::from(SAMPLE_RATE_HZ as u32))
        .checked_div(1_000_000_000)
        .unwrap_or(0);
    let aligned_samples = (elapsed_samples / u128::from(ZTS_PERIOD_FRAMES))
        .saturating_mul(u128::from(ZTS_PERIOD_FRAMES));
    let aligned_nanos = aligned_samples
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(SAMPLE_RATE_HZ as u32))
        .unwrap_or(0);
    let host = anchor.saturating_add(nanos_to_ticks(aligned_nanos, timebase));
    (u64::try_from(aligned_samples).unwrap_or(u64::MAX), host)
}

#[derive(Clone, Copy)]
struct ReceivedFrame {
    message: FrameMessage,
}

fn mach_round_msg(size: usize) -> usize {
    (size + 3) & !3
}

fn parse_received_frame(buffer: &[u8], received_size: usize) -> Option<ReceivedFrame> {
    if received_size < size_of::<mach_msg_header_t>() {
        return None;
    }
    // SAFETY: buffer has at least a mach_msg_header_t and unaligned read avoids alignment assumptions.
    let header = unsafe { buffer.as_ptr().cast::<mach_msg_header_t>().read_unaligned() };
    let msg_size = usize::try_from(header.msgh_size).ok()?;
    let header_size = size_of::<mach_msg_header_t>();
    let frame_size = size_of::<FrameMessage>();
    if msg_size != header_size && msg_size != frame_size {
        return None;
    }
    let trailer_offset = mach_round_msg(msg_size);
    let trailer_end =
        trailer_offset.checked_add(size_of::<mach2::message::mach_msg_audit_trailer_t>())?;
    if trailer_end > received_size || trailer_end > buffer.len() {
        return None;
    }
    // SAFETY: bounds checked above and unaligned reads avoid layout assumptions for stack byte storage.
    let trailer = unsafe {
        buffer
            .as_ptr()
            .add(trailer_offset)
            .cast::<mach2::message::mach_msg_audit_trailer_t>()
            .read_unaligned()
    };
    if usize::try_from(trailer.msgh_trailer_size).ok()?
        < size_of::<mach2::message::mach_msg_audit_trailer_t>()
    {
        return None;
    }
    let mut message = FrameMessage {
        header,
        sequence: 0,
        generation: 0,
        token: 0,
        samples: [0; FRAME_SAMPLES],
    };
    if msg_size == frame_size {
        // SAFETY: frame-sized message bounds were checked above.
        message = unsafe { buffer.as_ptr().cast::<FrameMessage>().read_unaligned() };
    }
    let _ = trailer.msgh_audit;
    Some(ReceivedFrame { message })
}

fn valid_driver_message(message: &FrameMessage) -> bool {
    let expected = u32::try_from(size_of::<FrameMessage>()).unwrap_or(u32::MAX);
    matches!(message.header.msgh_id, MACH_MSG_FRAME | MACH_MSG_CLEAR)
        && message.header.msgh_size == expected
        && (message.header.msgh_bits & mach2::message::MACH_MSGH_BITS_COMPLEX) == 0
}

fn send_clear_ack(request: &FrameMessage) {
    let reply_port = request.header.msgh_remote_port;
    if reply_port == MACH_PORT_NULL {
        return;
    }
    let mut reply = FrameMessage {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
            msgh_size: u32::try_from(size_of::<FrameMessage>()).unwrap_or(u32::MAX),
            msgh_remote_port: reply_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: MACH_MSG_CLEAR,
        },
        sequence: 0,
        generation: request.generation,
        token: request.token,
        samples: [0; FRAME_SAMPLES],
    };
    // SAFETY: reply is a fixed-size initialized Mach message with a send-once destination.
    let sent = unsafe {
        mach_msg(
            &mut reply.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            reply.header.msgh_size,
            0,
            MACH_PORT_NULL,
            100,
            MACH_PORT_NULL,
        )
    };
    if sent != MACH_MSG_SUCCESS {
        // SAFETY: mach_msg did not consume the move-send-once right on failure.
        unsafe { mach_msg_destroy(&mut reply.header) };
    }
}

fn start_receiver_once() {
    RECEIVER_STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("arcen-microphone-mach".to_owned())
            .spawn(receiver_thread);
    });
}

fn send_register_message(
    service_port: mach_port_t,
    receive_port: mach_port_t,
) -> mach_msg_return_t {
    let mut register = DriverRegisterMessage {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND, MACH_MSG_TYPE_MAKE_SEND),
            msgh_size: u32::try_from(size_of::<DriverRegisterMessage>()).unwrap_or(u32::MAX),
            msgh_remote_port: service_port,
            msgh_local_port: receive_port,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: MACH_MSG_REGISTER_DRIVER,
        },
    };
    // SAFETY: register is a properly initialized fixed-size Mach message.
    let sent = unsafe {
        mach_msg(
            &mut register.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            register.header.msgh_size,
            0,
            MACH_PORT_NULL,
            100,
            MACH_PORT_NULL,
        )
    };
    if sent != MACH_MSG_SUCCESS {
        destroy_failed_register_rights(&mut register.header);
    }
    sent
}

fn local_disposition(bits: u32) -> u32 {
    (bits >> 8) & 0xff
}

fn destroy_failed_register_rights(header: &mut mach_msg_header_t) {
    let local = header.msgh_local_port;
    if local != MACH_PORT_NULL {
        let disposition = local_disposition(header.msgh_bits);
        let owns_local = disposition == u32::try_from(MACH_MSG_TYPE_MOVE_SEND).unwrap()
            || disposition == u32::try_from(MACH_MSG_TYPE_MOVE_SEND_ONCE).unwrap();
        if owns_local {
            // SAFETY: the failed registration send returned an owned local send right.
            unsafe {
                let _ = mach2::mach_port::mach_port_deallocate(mach_task_self(), local);
            }
            header.msgh_local_port = MACH_PORT_NULL;
        }
    }
    // SAFETY: mach_msg did not consume all move/body rights on failure.
    unsafe { mach_msg_destroy(header) };
    header.msgh_remote_port = MACH_PORT_NULL;
    header.msgh_local_port = MACH_PORT_NULL;
}

fn receiver_thread() {
    let mut receive_port: mach_port_t = MACH_PORT_NULL;
    // SAFETY: mach_task_self is the current task port; receive_port points to writable storage.
    let allocated = unsafe {
        mach2::mach_port::mach_port_allocate(
            mach_task_self(),
            MACH_PORT_RIGHT_RECEIVE,
            &mut receive_port,
        )
    };
    if allocated != mach2::kern_return::KERN_SUCCESS {
        return;
    }
    let service = CString::new(MACH_SERVICE).expect("static service name has no nul");
    let mut registered = false;
    loop {
        if !registered {
            let mut service_port = MACH_PORT_NULL;
            // SAFETY: bootstrap_port is provided by libSystem and service is NUL-terminated.
            let looked_up =
                unsafe { bootstrap_look_up(bootstrap_port, service.as_ptr(), &mut service_port) };
            if looked_up != mach2::kern_return::KERN_SUCCESS || service_port == MACH_PORT_NULL {
                std::thread::sleep(std::time::Duration::from_millis(u64::from(
                    REGISTER_RETRY_MS,
                )));
                continue;
            }
            let sent = send_register_message(service_port, receive_port);
            if sent != MACH_MSG_SUCCESS {
                std::thread::sleep(std::time::Duration::from_millis(u64::from(
                    REGISTER_RETRY_MS,
                )));
                continue;
            }
            RING.clear();
            TIMESTAMP_SEED.fetch_add(1, Ordering::AcqRel);
            registered = true;
        }

        let mut buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        // SAFETY: buffer is writable and large enough for the largest message plus trailer.
        let received: mach_msg_return_t = unsafe {
            mach_msg(
                buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                receive_port,
                REGISTER_RETRY_MS,
                MACH_PORT_NULL,
            )
        };
        if received == MACH_RCV_TIMED_OUT {
            registered = false;
            continue;
        }
        let Some(received_frame) = parse_received_frame(&buffer, buffer.len()) else {
            continue;
        };
        let message = received_frame.message;
        if received != MACH_MSG_SUCCESS || !valid_driver_message(&message) {
            continue;
        }
        if message.header.msgh_id == MACH_MSG_FRAME {
            RING.push_samples(&message.samples);
        } else if message.header.msgh_id == MACH_MSG_CLEAR {
            RING.clear();
            TIMESTAMP_SEED.fetch_add(1, Ordering::AcqRel);
            send_clear_ack(&message);
        }
    }
}

pub fn mach_strerror(code: i32) -> String {
    // SAFETY: bootstrap_strerror returns a static C string for any kern_return_t.
    let ptr = unsafe { mach2::bootstrap::bootstrap_strerror(code) };
    if ptr.is_null() {
        return format!("mach error {code}");
    }
    // SAFETY: libSystem returns a valid NUL-terminated string pointer.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    unsafe extern "C" {
        fn mach_port_get_refs(
            task: mach2::mach_types::ipc_space_t,
            name: mach_port_t,
            right: mach2::port::mach_port_right_t,
            refs: *mut u32,
        ) -> mach2::kern_return::kern_return_t;
    }

    fn send_refs(port: mach_port_t) -> u32 {
        let mut refs = 0;
        // SAFETY: refs points to writable storage and port is a name in the current task.
        let status = unsafe {
            mach_port_get_refs(
                mach_task_self(),
                port,
                mach2::port::MACH_PORT_RIGHT_SEND,
                &mut refs,
            )
        };
        if status == mach2::kern_return::KERN_SUCCESS {
            refs
        } else {
            0
        }
    }

    fn fill_receive_queue(port: mach_port_t) {
        for sequence in 0..64 {
            let mut message = DriverRegisterMessage {
                header: mach_msg_header_t {
                    msgh_bits: MACH_MSGH_BITS(mach2::message::MACH_MSG_TYPE_COPY_SEND, 0),
                    msgh_size: u32::try_from(size_of::<DriverRegisterMessage>()).unwrap(),
                    msgh_remote_port: port,
                    msgh_local_port: MACH_PORT_NULL,
                    msgh_voucher_port: MACH_PORT_NULL,
                    msgh_id: MACH_MSG_REGISTER_DRIVER + sequence,
                },
            };
            // SAFETY: message is a fixed-size initialized Mach message.
            let sent = unsafe {
                mach_msg(
                    &mut message.header,
                    MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                    message.header.msgh_size,
                    0,
                    MACH_PORT_NULL,
                    100,
                    MACH_PORT_NULL,
                )
            };
            if sent == mach2::message::MACH_SEND_TIMED_OUT {
                return;
            }
            assert_eq!(sent, MACH_MSG_SUCCESS);
        }
        panic!("Mach queue did not fill");
    }

    #[test]
    fn ring_underrun_returns_silence() {
        let ring = SpscRing::new(8);
        assert_eq!(ring.pop_f32(), 0.0);
        assert_eq!(ring.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn ring_wrap_drops_new_when_full() {
        let _guard = TEST_LOCK.lock().unwrap();
        RUNNING_CLIENTS.store(1, Ordering::Release);
        let ring = SpscRing::new(4);
        ring.push_samples(&[32767, 16384, 8192, 4096, 2048]);
        assert_eq!(ring.queued(), 3);
        assert!((ring.pop_f32() - (32767.0 / 32768.0)).abs() < 0.001);
        assert_eq!(ring.overruns.load(Ordering::Relaxed), 2);
        RUNNING_CLIENTS.store(0, Ordering::Release);
    }

    #[test]
    fn clear_discards_queued_audio() {
        let _guard = TEST_LOCK.lock().unwrap();
        RUNNING_CLIENTS.store(1, Ordering::Release);
        let ring = SpscRing::new(8);
        ring.push_samples(&[1000, 2000, 3000]);
        assert!(ring.queued() > 0);
        ring.clear();
        assert_eq!(ring.queued(), 0);
        assert_eq!(ring.pop_f32(), 0.0);
        RUNNING_CLIENTS.store(0, Ordering::Release);
    }

    #[test]
    fn advertised_format_matches_microphone_v1() {
        let format = fixed_asbd();
        assert_eq!(format.mSampleRate, 48_000.0);
        assert_eq!(format.mChannelsPerFrame, 1);
        assert_eq!(format.mFormatID, K_AUDIO_FORMAT_LINEAR_PCM);
        assert_eq!(
            format.mFormatFlags,
            K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED
        );
        assert_eq!(format.mBitsPerChannel, 32);
    }

    #[test]
    fn pure_property_sizes_are_available() {
        let address = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION,
            mScope: K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT,
            mElement: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, address),
            Some(size_of::<AudioBufferList>() as u32)
        );
        let name = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_OBJECT_PROPERTY_NAME,
            ..address
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, name),
            Some(size_of::<CFStringRef>() as u32)
        );
        let custom = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_OBJECT_PROPERTY_CUSTOM_PROPERTY_INFO_LIST,
            ..address
        };
        assert_eq!(property_size(PLUGIN_OBJECT, custom), Some(0));
        assert_eq!(property_size(DEVICE_OBJECT, custom), Some(0));
        assert_eq!(property_size(STREAM_OBJECT, custom), Some(0));
        let icon = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_ICON,
            ..address
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, icon),
            Some(size_of::<*const c_void>() as u32)
        );
        let controls = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_OBJECT_PROPERTY_CONTROL_LIST,
            ..address
        };
        assert_eq!(property_size(DEVICE_OBJECT, controls), Some(0));
        let layout = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNEL_LAYOUT,
            ..address
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, layout),
            Some(size_of::<MonoAudioChannelLayout>() as u32)
        );
        let translate_box = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_PLUGIN_PROPERTY_TRANSLATE_UID_TO_BOX,
            ..address
        };
        assert_eq!(
            property_size(PLUGIN_OBJECT, translate_box),
            Some(size_of::<AudioObjectID>() as u32)
        );
    }

    #[test]
    fn device_scope_separates_input_from_output() {
        let input = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_STREAMS,
            mScope: K_AUDIO_OBJECT_PROPERTY_SCOPE_INPUT,
            mElement: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
        };
        let output = AudioObjectPropertyAddress {
            mScope: K_AUDIO_OBJECT_PROPERTY_SCOPE_OUTPUT,
            ..input
        };
        let global = AudioObjectPropertyAddress {
            mScope: K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
            ..input
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, input),
            Some(size_of::<AudioObjectID>() as u32)
        );
        assert_eq!(property_size(DEVICE_OBJECT, output), Some(0));
        assert_eq!(
            property_size(DEVICE_OBJECT, global),
            Some(size_of::<AudioObjectID>() as u32)
        );

        let mut stream_ids = [0_u32; 1];
        let mut out_size = 0_u32;
        assert_eq!(
            write_device_property(
                input,
                u32::try_from(size_of_val(&stream_ids)).unwrap(),
                &mut out_size,
                stream_ids.as_mut_ptr().cast(),
            ),
            NO_ERR
        );
        assert_eq!(out_size, size_of::<AudioObjectID>() as u32);
        assert_eq!(stream_ids[0], STREAM_OBJECT);
        out_size = 99;
        assert_eq!(
            write_device_property(output, 0, &mut out_size, stream_ids.as_mut_ptr().cast()),
            NO_ERR
        );
        assert_eq!(out_size, 0);

        let input_config = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION,
            ..input
        };
        let output_config = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_STREAM_CONFIGURATION,
            ..output
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, input_config),
            Some(size_of::<AudioBufferList>() as u32)
        );
        assert_eq!(
            property_size(DEVICE_OBJECT, output_config),
            Some(size_of::<u32>() as u32)
        );
        let mut zero_buffers = 99_u32;
        out_size = 0;
        assert_eq!(
            write_device_property(
                output_config,
                u32::try_from(size_of::<u32>()).unwrap(),
                &mut out_size,
                (&raw mut zero_buffers).cast(),
            ),
            NO_ERR
        );
        assert_eq!(zero_buffers, 0);

        let input_layout = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNEL_LAYOUT,
            ..input
        };
        let output_layout = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_PREFERRED_CHANNEL_LAYOUT,
            ..output
        };
        assert_eq!(
            property_size(DEVICE_OBJECT, input_layout),
            Some(size_of::<MonoAudioChannelLayout>() as u32)
        );
        assert_eq!(property_size(DEVICE_OBJECT, output_layout), None);
        let mut layout = mono_channel_layout();
        assert_eq!(
            write_device_property(
                output_layout,
                u32::try_from(size_of::<MonoAudioChannelLayout>()).unwrap(),
                &mut out_size,
                (&raw mut layout).cast(),
            ),
            K_AUDIO_HARDWARE_UNKNOWN_PROPERTY_ERROR
        );

        let can_default_in = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT,
            ..input
        };
        let can_default_out = AudioObjectPropertyAddress {
            mSelector: K_AUDIO_DEVICE_PROPERTY_CAN_BE_DEFAULT,
            ..output
        };
        let mut value = 99_u32;
        assert_eq!(
            write_device_property(
                can_default_in,
                u32::try_from(size_of::<u32>()).unwrap(),
                &mut out_size,
                (&raw mut value).cast(),
            ),
            NO_ERR
        );
        assert_eq!(value, 1);
        assert_eq!(
            write_device_property(
                can_default_out,
                u32::try_from(size_of::<u32>()).unwrap(),
                &mut out_size,
                (&raw mut value).cast(),
            ),
            NO_ERR
        );
        assert_eq!(value, 0);
    }

    #[test]
    fn undersized_cf_properties_fail_without_publishing_objects() {
        let mut out: CFStringRef = null();
        let mut out_size = 123_u32;
        assert_eq!(
            write_cf_string("Arcen", 0, &mut out_size, (&raw mut out).cast::<c_void>(),),
            K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR
        );
        assert!(out.is_null());
        assert_eq!(out_size, 123);
        let mut url: *const c_void = null();
        assert_eq!(
            write_cf_url(
                "/Library/Audio/Plug-Ins/HAL/ArcenMicrophone.driver",
                0,
                &mut out_size,
                (&raw mut url).cast::<c_void>(),
            ),
            K_AUDIO_HARDWARE_BAD_PROPERTY_SIZE_ERROR
        );
        assert!(url.is_null());
    }

    #[test]
    fn receiver_options_deliver_audit_trailer_for_parser() {
        let mut port = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and port points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &mut port,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);
        // SAFETY: inserts a send right for the receive right just allocated in this task.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach_task_self(),
                port,
                port,
                MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);

        let mut frame = FrameMessage {
            header: mach_msg_header_t {
                msgh_bits: MACH_MSGH_BITS(mach2::message::MACH_MSG_TYPE_COPY_SEND, 0),
                msgh_size: u32::try_from(size_of::<FrameMessage>()).unwrap(),
                msgh_remote_port: port,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: MACH_MSG_FRAME,
            },
            sequence: 33,
            generation: 44,
            token: 55,
            samples: [7; FRAME_SAMPLES],
        };
        // SAFETY: frame is a properly initialized fixed-size Mach message.
        let sent = unsafe {
            mach_msg(
                &mut frame.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                frame.header.msgh_size,
                0,
                MACH_PORT_NULL,
                100,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(sent, MACH_MSG_SUCCESS);

        let mut buffer =
            [0_u8; size_of::<FrameMessage>() + size_of::<mach_msg_audit_trailer_t>() + 8];
        // SAFETY: this is the driver's actual receive option mix; buffer is large enough for the audit trailer.
        let received = unsafe {
            mach_msg(
                buffer.as_mut_ptr().cast::<mach_msg_header_t>(),
                MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT_OPTIONS,
                0,
                u32::try_from(buffer.len()).unwrap(),
                port,
                100,
                MACH_PORT_NULL,
            )
        };
        assert_eq!(received, MACH_MSG_SUCCESS);
        let parsed = parse_received_frame(&buffer, buffer.len()).expect("audit trailer");
        assert_eq!(parsed.message.sequence, 33);
        assert_eq!(parsed.message.generation, 44);
        assert_eq!(parsed.message.token, 55);
        assert_eq!(parsed.message.samples[0], 7);
    }

    #[test]
    fn registration_send_does_not_leak_lookup_rights() {
        let mut service = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and service points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &mut service,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);
        let mut driver = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and driver points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &mut driver,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);

        for _ in 0..3 {
            // SAFETY: inserts one send right, modelling one bootstrap_look_up result.
            let inserted = unsafe {
                mach2::mach_port::mach_port_insert_right(
                    mach_task_self(),
                    service,
                    service,
                    MACH_MSG_TYPE_MAKE_SEND,
                )
            };
            assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
            assert_eq!(send_refs(service), 1);
            assert_eq!(send_register_message(service, driver), MACH_MSG_SUCCESS);
            assert_eq!(send_refs(service), 0);
        }

        // SAFETY: service and driver are receive rights allocated in this task.
        unsafe {
            let _ = mach2::mach_port::mach_port_destroy(mach_task_self(), service);
            let _ = mach2::mach_port::mach_port_destroy(mach_task_self(), driver);
        }
    }

    #[test]
    fn failed_registration_send_does_not_leak_lookup_or_local_rights() {
        let mut service = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and service points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &mut service,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);
        // SAFETY: inserts one persistent send right used to fill the receive queue.
        let inserted = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach_task_self(),
                service,
                service,
                MACH_MSG_TYPE_MAKE_SEND,
            )
        };
        assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
        fill_receive_queue(service);
        let service_refs_after_fill = send_refs(service);
        let mut driver = MACH_PORT_NULL;
        // SAFETY: the current task port is valid and driver points to writable storage.
        let allocated = unsafe {
            mach2::mach_port::mach_port_allocate(
                mach_task_self(),
                MACH_PORT_RIGHT_RECEIVE,
                &mut driver,
            )
        };
        assert_eq!(allocated, mach2::kern_return::KERN_SUCCESS);

        for _ in 0..3 {
            // SAFETY: models one bootstrap_look_up send right for this registration attempt.
            let inserted = unsafe {
                mach2::mach_port::mach_port_insert_right(
                    mach_task_self(),
                    service,
                    service,
                    MACH_MSG_TYPE_MAKE_SEND,
                )
            };
            assert_eq!(inserted, mach2::kern_return::KERN_SUCCESS);
            assert_eq!(send_refs(service), service_refs_after_fill + 1);
            assert_eq!(
                send_register_message(service, driver),
                mach2::message::MACH_SEND_TIMED_OUT
            );
            assert_eq!(send_refs(service), service_refs_after_fill);
            assert_eq!(send_refs(driver), 0);
        }

        // SAFETY: service and driver are receive rights allocated in this task.
        unsafe {
            let _ = mach2::mach_port::mach_port_destroy(mach_task_self(), service);
            let _ = mach2::mach_port::mach_port_destroy(mach_task_self(), driver);
        }
    }
}
