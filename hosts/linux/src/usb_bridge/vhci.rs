use arcen_usb_bridge::{TransferDirection, TransferKind, UsbSpeed, MAX_TRANSFER_BYTES};
use nix::errno::Errno;
use std::fmt::{Display, Formatter};
use std::fs::{File, OpenOptions};
use std::mem::{size_of, MaybeUninit};
use std::os::fd::AsRawFd;
use std::time::Duration;

const DEVICE_PATH: &str = "/dev/usb-vhci";
const IOCTL_MAGIC: u8 = 138;
const WORK_PORT_STATE: u8 = 0;
const WORK_PROCESS_URB: u8 = 1;
const WORK_CANCEL_URB: u8 = 2;
const URB_INTERRUPT: u8 = 1;
const URB_CONTROL: u8 = 2;
const PORT_CONNECTION: u16 = 0x0001;
const PORT_ENABLE: u16 = 0x0002;
const PORT_SUSPEND: u16 = 0x0004;
const PORT_RESET: u16 = 0x0010;
const PORT_POWER: u16 = 0x0100;
const PORT_LOW_SPEED: u16 = 0x0200;
const PORT_HIGH_SPEED: u16 = 0x0400;
const PORT_CHANGE_CONNECTION: u16 = 0x0001;
const PORT_CHANGE_SUSPEND: u16 = 0x0004;
const PORT_CHANGE_RESET: u16 = 0x0010;
const PORT_FLAG_RESUMING: u8 = 0x01;

#[repr(C)]
#[derive(Clone, Copy)]
struct VhciRegister {
    id: i32,
    usb_busnum: i32,
    bus_id: [u8; 20],
    port_count: u8,
    _padding: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VhciPortState {
    status: u16,
    change: u16,
    index: u8,
    flags: u8,
    _reserved: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VhciSetup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VhciRawUrb {
    setup: VhciSetup,
    buffer_length: i32,
    interval: i32,
    packet_count: i32,
    flags: u16,
    address: u8,
    endpoint: u8,
    transfer_type: u8,
    _padding: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
union VhciWorkPayload {
    urb: VhciRawUrb,
    port: VhciPortState,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct VhciWork {
    handle: u64,
    payload: VhciWorkPayload,
    timeout: i16,
    work_type: u8,
    _padding: u8,
}

#[repr(C)]
struct VhciUrbData {
    handle: u64,
    buffer: *mut nix::libc::c_void,
    iso_packets: *mut nix::libc::c_void,
    buffer_length: i32,
    packet_count: i32,
}

#[repr(C)]
struct VhciGiveback {
    handle: u64,
    buffer: *mut nix::libc::c_void,
    iso_packets: *mut nix::libc::c_void,
    status: i32,
    buffer_actual: i32,
    packet_count: i32,
    error_count: i32,
}

const _: () = assert!(size_of::<VhciRegister>() == 32);
const _: () = assert!(size_of::<VhciPortState>() == 8);
const _: () = assert!(size_of::<VhciSetup>() == 8);
const _: () = assert!(size_of::<VhciRawUrb>() == 28);
const _: () = assert!(size_of::<VhciWork>() == 40);
const _: () = assert!(size_of::<VhciUrbData>() == 32);
const _: () = assert!(size_of::<VhciGiveback>() == 40);

nix::ioctl_readwrite!(register_controller, IOCTL_MAGIC, 0, VhciRegister);
nix::ioctl_write_ptr!(set_port_state, IOCTL_MAGIC, 1, VhciPortState);
nix::ioctl_readwrite!(fetch_work, IOCTL_MAGIC, 2, VhciWork);
nix::ioctl_write_ptr!(giveback_urb, IOCTL_MAGIC, 3, VhciGiveback);
nix::ioctl_write_ptr!(fetch_urb_data, IOCTL_MAGIC, 4, VhciUrbData);

#[derive(Debug)]
pub enum VhciError {
    Open(std::io::Error),
    Ioctl(Errno),
    TimedOut,
    InvalidRegistration,
    InvalidWorkType(u8),
    InvalidUrbType(u8),
    InvalidLength(i32),
}

impl Display for VhciError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open(error) => write!(formatter, "open {DEVICE_PATH}: {error}"),
            Self::Ioctl(error) => write!(formatter, "usb-vhci ioctl: {error}"),
            Self::TimedOut => formatter.write_str("usb-vhci fetch timed out"),
            Self::InvalidRegistration => {
                formatter.write_str("usb-vhci returned invalid registration")
            }
            Self::InvalidWorkType(kind) => write!(formatter, "unknown usb-vhci work type {kind}"),
            Self::InvalidUrbType(kind) => write!(formatter, "unsupported usb-vhci URB type {kind}"),
            Self::InvalidLength(length) => {
                write!(formatter, "invalid usb-vhci transfer length {length}")
            }
        }
    }
}

impl std::error::Error for VhciError {}

pub struct VhciController {
    file: File,
    bus_number: i32,
    speed: Option<UsbSpeed>,
}

impl VhciController {
    pub fn open_and_register(port_count: u8) -> Result<Self, VhciError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(DEVICE_PATH)
            .map_err(VhciError::Open)?;
        let mut registration = VhciRegister {
            id: -1,
            usb_busnum: -1,
            bus_id: [0; 20],
            port_count,
            _padding: [0; 3],
        };
        // SAFETY: `registration` is an initialized `#[repr(C)]` value with
        // compile-time-checked ABI size, remains exclusively borrowed for the
        // synchronous ioctl, and `file` owns a live usb-vhci control FD.
        unsafe { register_controller(file.as_raw_fd(), &mut registration) }
            .map_err(VhciError::Ioctl)?;
        if registration.id < 0 || registration.usb_busnum <= 0 {
            return Err(VhciError::InvalidRegistration);
        }
        Ok(Self {
            file,
            bus_number: registration.usb_busnum,
            speed: None,
        })
    }

    #[must_use]
    pub const fn bus_number(&self) -> i32 {
        self.bus_number
    }

    pub fn connect(&mut self, index: u8, speed: UsbSpeed) -> Result<(), VhciError> {
        let status = Self::port_status(speed, false);
        self.apply_port(PortState {
            status,
            change: PORT_CHANGE_CONNECTION,
            index,
            flags: 0,
        })?;
        self.speed = Some(speed);
        Ok(())
    }

    pub fn disconnect(&mut self, index: u8) -> Result<(), VhciError> {
        let result = self.apply_port(PortState {
            status: PORT_POWER,
            change: PORT_CHANGE_CONNECTION,
            index,
            flags: 0,
        });
        if result.is_ok() {
            self.speed = None;
        }
        result
    }

    pub fn complete_reset(&mut self, index: u8) -> Result<(), VhciError> {
        let speed = self.speed.ok_or(VhciError::InvalidRegistration)?;
        self.apply_port(PortState {
            status: Self::port_status(speed, true),
            change: PORT_CHANGE_RESET,
            index,
            flags: 0,
        })
    }

    pub fn complete_resume(&mut self, index: u8) -> Result<(), VhciError> {
        let speed = self.speed.ok_or(VhciError::InvalidRegistration)?;
        self.apply_port(PortState {
            status: Self::port_status(speed, true),
            change: PORT_CHANGE_SUSPEND,
            index,
            flags: 0,
        })
    }

    const fn port_status(speed: UsbSpeed, enabled: bool) -> u16 {
        let speed_flag = match speed {
            UsbSpeed::Low => PORT_LOW_SPEED,
            UsbSpeed::Full => 0,
            UsbSpeed::High => PORT_HIGH_SPEED,
        };
        PORT_CONNECTION | PORT_POWER | speed_flag | if enabled { PORT_ENABLE } else { 0 }
    }

    pub fn fetch_work(&mut self, timeout: Duration) -> Result<FetchedWork, VhciError> {
        let timeout_ms = timeout.as_millis().min(1_000);
        let timeout = i16::try_from(timeout_ms).unwrap_or(1_000);
        // SAFETY: every integer and union bit pattern in `VhciWork` is valid;
        // zero initialization avoids exposing uninitialized padding to the
        // kernel. The ioctl synchronously initializes the active payload
        // selected by `work_type` before this function reads it.
        let mut work = unsafe { MaybeUninit::<VhciWork>::zeroed().assume_init() };
        work.timeout = timeout;
        // SAFETY: `work` has the exact checked ABI layout, is exclusively
        // borrowed for the synchronous ioctl, and the FD remains open.
        match unsafe { fetch_work(self.file.as_raw_fd(), &mut work) } {
            Ok(_) => {}
            Err(Errno::ETIMEDOUT) => return Err(VhciError::TimedOut),
            Err(error) => return Err(VhciError::Ioctl(error)),
        }
        match work.work_type {
            WORK_PORT_STATE => {
                // SAFETY: the kernel selected `WORK_PORT_STATE`, so it wrote
                // the `port` union member before returning.
                let port = unsafe { work.payload.port };
                Ok(FetchedWork::Port(port.into()))
            }
            WORK_PROCESS_URB => {
                // SAFETY: the kernel selected `WORK_PROCESS_URB`, so it wrote
                // the `urb` union member before returning.
                let urb = unsafe { work.payload.urb };
                Ok(FetchedWork::Urb(VhciUrb {
                    handle: work.handle,
                    setup: urb.setup,
                    buffer_length: checked_length(urb.buffer_length)?,
                    interval: urb.interval,
                    endpoint: urb.endpoint,
                    transfer_type: urb.transfer_type,
                }))
            }
            WORK_CANCEL_URB => Ok(FetchedWork::Cancel {
                handle: work.handle,
            }),
            other => Err(VhciError::InvalidWorkType(other)),
        }
    }

    pub fn fetch_out_data(&mut self, handle: u64, length: usize) -> Result<Vec<u8>, VhciError> {
        if length > MAX_TRANSFER_BYTES {
            return Err(VhciError::InvalidLength(
                i32::try_from(length).unwrap_or(i32::MAX),
            ));
        }
        let mut bytes = vec![0_u8; length];
        let request = VhciUrbData {
            handle,
            buffer: bytes.as_mut_ptr().cast(),
            iso_packets: std::ptr::null_mut(),
            buffer_length: i32::try_from(length).map_err(|_| VhciError::InvalidLength(i32::MAX))?,
            packet_count: 0,
        };
        // SAFETY: `bytes` owns `length` writable bytes and cannot move or drop
        // during the synchronous ioctl; iso pointers/count are both zero.
        unsafe { fetch_urb_data(self.file.as_raw_fd(), &request) }.map_err(VhciError::Ioctl)?;
        Ok(bytes)
    }

    pub fn giveback(&mut self, handle: u64, status: i32, data: &[u8]) -> Result<(), VhciError> {
        if data.len() > MAX_TRANSFER_BYTES {
            return Err(VhciError::InvalidLength(
                i32::try_from(data.len()).unwrap_or(i32::MAX),
            ));
        }
        let request = VhciGiveback {
            handle,
            buffer: if data.is_empty() {
                std::ptr::null_mut()
            } else {
                data.as_ptr().cast_mut().cast()
            },
            iso_packets: std::ptr::null_mut(),
            status,
            buffer_actual: i32::try_from(data.len())
                .map_err(|_| VhciError::InvalidLength(i32::MAX))?,
            packet_count: 0,
            error_count: 0,
        };
        // SAFETY: the optional pointer references `data` for the full
        // synchronous ioctl; the kernel only copies from it for IN giveback.
        // Iso pointers/count are both zero.
        match unsafe { giveback_urb(self.file.as_raw_fd(), &request) } {
            Ok(_) | Err(Errno::ECANCELED) => Ok(()),
            Err(error) => Err(VhciError::Ioctl(error)),
        }
    }

    fn apply_port(&mut self, state: PortState) -> Result<(), VhciError> {
        let raw = VhciPortState {
            status: state.status,
            change: state.change,
            index: state.index,
            flags: state.flags,
            _reserved: [0; 2],
        };
        // SAFETY: `raw` is an initialized, layout-checked value exclusively
        // borrowed for the synchronous ioctl; the control FD remains open.
        unsafe { set_port_state(self.file.as_raw_fd(), &raw) }.map_err(VhciError::Ioctl)?;
        Ok(())
    }
}

fn checked_length(length: i32) -> Result<usize, VhciError> {
    let length = usize::try_from(length).map_err(|_| VhciError::InvalidLength(length))?;
    if length > MAX_TRANSFER_BYTES {
        return Err(VhciError::InvalidLength(
            i32::try_from(length).unwrap_or(i32::MAX),
        ));
    }
    Ok(length)
}

#[derive(Debug, Clone, Copy)]
pub struct PortState {
    status: u16,
    change: u16,
    pub index: u8,
    flags: u8,
}

impl PortState {
    #[must_use]
    pub const fn reset_requested(self) -> bool {
        self.status & PORT_RESET != 0
    }

    #[must_use]
    pub const fn resume_requested(self) -> bool {
        self.status & PORT_SUSPEND != 0 || self.flags & PORT_FLAG_RESUMING != 0
    }
}

impl From<VhciPortState> for PortState {
    fn from(value: VhciPortState) -> Self {
        Self {
            status: value.status,
            change: value.change,
            index: value.index,
            flags: value.flags,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VhciUrb {
    pub handle: u64,
    pub setup: VhciSetup,
    pub buffer_length: usize,
    interval: i32,
    endpoint: u8,
    transfer_type: u8,
}

impl VhciUrb {
    pub fn transfer_kind(self) -> Result<TransferKind, VhciError> {
        match self.transfer_type {
            URB_CONTROL => Ok(TransferKind::Control),
            URB_INTERRUPT => Ok(TransferKind::Interrupt),
            other => Err(VhciError::InvalidUrbType(other)),
        }
    }

    #[must_use]
    pub const fn direction(self) -> TransferDirection {
        if self.endpoint & 0x80 == 0 {
            TransferDirection::Out
        } else {
            TransferDirection::In
        }
    }

    #[must_use]
    pub const fn endpoint(self) -> u8 {
        self.endpoint
    }

    /// Conservative lab cadence for interrupt reports.
    ///
    /// The legacy VHCI ABI exposes the kernel URB interval directly, whose
    /// high-speed encoding is not a simple millisecond value. The lab fixture
    /// therefore enforces an independent 125 Hz ceiling rather than letting a
    /// completed URB be resubmitted in a CPU-bound loop.
    #[must_use]
    pub const fn interrupt_cadence(self) -> Duration {
        let _ = self.interval;
        Duration::from_millis(8)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FetchedWork {
    Port(PortState),
    Urb(VhciUrb),
    Cancel { handle: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_abi_sizes_match_public_header() {
        assert_eq!(size_of::<VhciRegister>(), 32);
        assert_eq!(size_of::<VhciPortState>(), 8);
        assert_eq!(size_of::<VhciRawUrb>(), 28);
        assert_eq!(size_of::<VhciWork>(), 40);
        assert_eq!(size_of::<VhciUrbData>(), 32);
        assert_eq!(size_of::<VhciGiveback>(), 40);
    }

    #[test]
    fn transfer_direction_comes_from_endpoint_bit() {
        let mut urb = VhciUrb {
            handle: 1,
            setup: VhciSetup::default(),
            buffer_length: 0,
            interval: 1,
            endpoint: 0x81,
            transfer_type: URB_INTERRUPT,
        };
        assert_eq!(urb.direction(), TransferDirection::In);
        urb.endpoint = 0x01;
        assert_eq!(urb.direction(), TransferDirection::Out);
    }
}
