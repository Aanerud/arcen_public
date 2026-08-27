//! Session-bound microphone-v1 ingress and the user/kernel feeder contract.
//!
//! The native device transport is intentionally behind [`MicrophoneDevice`]:
//! protocol parsing and jitter behavior remain safe Rust, while Windows handle
//! and IOCTL ownership stays in the platform implementation.

use arcen_media::audio::{
    MicrophoneDecodeError, MicrophoneDecoder, MicrophoneFrameOutput, MicrophoneIngestOutcome,
    MicrophoneStats, MicrophoneStatsTracker, ResolvedMicrophoneStream, MICROPHONE_V1_FRAME_SAMPLES,
};
use arcen_protocol::decode_microphone_frame;
use arcen_telemetry::CorrelationId;
use zeroize::Zeroize;
#[cfg(windows)]
use zeroize::ZeroizeOnDrop;

#[cfg(test)]
pub const DRIVER_RING_FRAMES: usize = 10;
const DEVICE_PATH: &str = r"\\.\ArcenMicrophone";
const BIND_IOCTL: u32 = 0x8000_A000;
const FEED_IOCTL: u32 = 0x8000_A004;
const STOP_IOCTL: u32 = 0x8000_A008;
const BIND_VERSION: u32 = 1;
const MAX_BINARY_SID_BYTES: usize = 68;
const FEEDER_MAILBOX_FRAMES: usize = 2;
#[cfg(windows)]
const DRIVER_IO_CANCEL_AFTER: std::time::Duration = std::time::Duration::from_millis(100);
const DEVICE_LIFECYCLE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(250);
const WORKER_OBSERVE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrophoneSessionBinding {
    pub wts_session_id: u32,
    pub user_sid: String,
    pub generation: u32,
}

impl MicrophoneSessionBinding {
    pub fn new(
        wts_session_id: u32,
        user_sid: String,
        generation: u32,
    ) -> Result<Self, MicrophoneDeviceError> {
        if wts_session_id == 0 || user_sid.trim().is_empty() || generation == 0 {
            return Err(MicrophoneDeviceError::InvalidBinding);
        }
        Ok(Self {
            wts_session_id,
            user_sid,
            generation,
        })
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

    fn clear(&mut self, binding: &MicrophoneSessionBinding);
}

#[must_use]
/// Reports whether the installed Arcen control endpoint can be opened.
///
/// This is an availability probe only. Arcen never changes Windows recording
/// defaults; the user or recording application selects `Arcen Microphone`.
pub fn backend_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(DEVICE_PATH)
        .is_ok()
}

pub async fn backend_available_if_enabled(
    operator_enabled: bool,
) -> Result<bool, MicrophoneDeviceError> {
    probe_if_enabled(operator_enabled, backend_available).await
}

async fn probe_if_enabled(
    operator_enabled: bool,
    probe: impl FnOnce() -> bool + Send + 'static,
) -> Result<bool, MicrophoneDeviceError> {
    if !operator_enabled {
        return Ok(false);
    }
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let join = std::thread::Builder::new()
        .name("arcen-microphone-probe".to_string())
        .spawn(move || {
            let _ = completion_tx.send(Ok(probe()));
        })
        .map_err(|_| MicrophoneDeviceError::WorkerFailed)?;
    let fatal_cleanup = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    DeviceWorker::new(completion_rx, join, fatal_cleanup)
        .finish_until(std::time::Instant::now() + DEVICE_LIFECYCLE_DEADLINE)
        .await
}

struct DeviceWorker<T> {
    completion: tokio::sync::oneshot::Receiver<Result<T, MicrophoneDeviceError>>,
    join: Option<std::thread::JoinHandle<()>>,
    fatal_cleanup: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<T> DeviceWorker<T> {
    fn new(
        completion: tokio::sync::oneshot::Receiver<Result<T, MicrophoneDeviceError>>,
        join: std::thread::JoinHandle<()>,
        fatal_cleanup: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            completion,
            join: Some(join),
            fatal_cleanup,
        }
    }

    async fn finish_until(
        mut self,
        deadline: std::time::Instant,
    ) -> Result<T, MicrophoneDeviceError> {
        let result = loop {
            if self
                .fatal_cleanup
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.join.take();
                return Err(MicrophoneDeviceError::FatalCleanup);
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                self.fatal_cleanup
                    .store(true, std::sync::atomic::Ordering::Release);
                self.join.take();
                return Err(MicrophoneDeviceError::FatalCleanup);
            }
            let observed = WORKER_OBSERVE_INTERVAL.min(deadline.saturating_duration_since(now));
            match tokio::time::timeout(observed, &mut self.completion).await {
                Ok(Ok(result)) => break result,
                Ok(Err(_)) => break Err(MicrophoneDeviceError::WorkerFailed),
                Err(_) => {}
            }
        };
        let Some(join) = self.join.take() else {
            return Err(MicrophoneDeviceError::WorkerFailed);
        };
        while !join.is_finished() {
            if std::time::Instant::now() >= deadline {
                self.fatal_cleanup
                    .store(true, std::sync::atomic::Ordering::Release);
                return Err(MicrophoneDeviceError::FatalCleanup);
            }
            tokio::time::sleep(WORKER_OBSERVE_INTERVAL).await;
        }
        join.join()
            .map_err(|_| MicrophoneDeviceError::WorkerFailed)?;
        result
    }
}

#[cfg(windows)]
#[repr(C)]
#[derive(Zeroize, ZeroizeOnDrop)]
struct DriverBindRequest {
    version: u32,
    wts_session_id: u32,
    generation: u32,
    sid_length: u32,
    sid: [u8; MAX_BINARY_SID_BYTES],
}

#[repr(C)]
#[derive(Zeroize)]
struct DriverFeedRequest {
    version: u32,
    generation: u32,
    frame_bytes: u32,
    reserved: u32,
    frame: [u8; MICROPHONE_V1_FRAME_SAMPLES * 2],
}

#[cfg(windows)]
#[repr(C)]
struct DriverStopRequest {
    version: u32,
    generation: u32,
}

#[cfg(windows)]
struct DriverHandle(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for DriverHandle {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the handle returned by CreateFileW.
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
impl DriverHandle {
    fn open() -> Result<Self, MicrophoneDeviceError> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING,
        };

        let path = DEVICE_PATH
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        // SAFETY: `path` is NUL-terminated and remains alive for the call. The
        // returned handle is uniquely owned by DriverHandle.
        let handle = unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(map_windows_error)?;
        Ok(Self(handle))
    }

    fn ioctl<T>(
        &self,
        code: u32,
        request: &mut T,
        stop: Option<&std::sync::atomic::AtomicBool>,
        lifecycle_deadline: &std::sync::Mutex<Option<std::time::Instant>>,
        operation_deadline: std::time::Instant,
        fatal_cleanup: &std::sync::atomic::AtomicBool,
    ) -> Result<(), MicrophoneDeviceError> {
        use windows::Win32::Foundation::{ERROR_IO_PENDING, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
        use windows::Win32::System::IO::{
            CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
        };

        let event = {
            // SAFETY: unnamed manual-reset event with no security descriptor.
            unsafe { CreateEventW(None, true, false, None) }
                .map_err(|_| MicrophoneDeviceError::DeviceUnavailable)?
        };
        let event = DriverHandle(event);
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event.0;
        let mut returned = 0;
        // SAFETY: the request and OVERLAPPED storage stay alive until completion
        // or cancellation is observed. The handle was opened for overlapped I/O.
        let submitted = unsafe {
            DeviceIoControl(
                self.0,
                code,
                Some((request as *mut T).cast()),
                u32::try_from(std::mem::size_of::<T>()).expect("driver request size fits u32"),
                None,
                0,
                Some(&mut returned),
                Some(&mut overlapped),
            )
        };
        if let Err(error) = submitted {
            if error.code() != windows::core::HRESULT::from_win32(ERROR_IO_PENDING.0) {
                return Err(map_windows_error(error));
            }
            let mut cancel_and_drain = || {
                // SAFETY: this targets only this still-live OVERLAPPED.
                let _ = unsafe { CancelIoEx(self.0, Some(&overlapped)) };
                loop {
                    // SAFETY: the event remains owned by this call. A signaled
                    // event proves the driver has released all operation pointers.
                    let wait = unsafe { WaitForSingleObject(event.0, 10) };
                    if wait == WAIT_OBJECT_0 {
                        // SAFETY: the event proves completion, so this never waits.
                        let _ = unsafe {
                            GetOverlappedResult(self.0, &overlapped, &mut returned, false)
                        };
                        return;
                    }
                    let absolute_deadline = lifecycle_deadline
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .map_or(operation_deadline, |deadline| {
                            deadline.min(operation_deadline)
                        });
                    if std::time::Instant::now() >= absolute_deadline {
                        fatal_cleanup.store(true, std::sync::atomic::Ordering::Release);
                        // The owner exits the process after observing `FatalCleanup`.
                        // Remaining in this frame keeps all live I/O storage valid.
                        loop {
                            std::thread::park();
                        }
                    }
                    if wait != WAIT_TIMEOUT {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            };
            let started = std::time::Instant::now();
            let cancel_at = started + DRIVER_IO_CANCEL_AFTER;
            loop {
                let stopping =
                    stop.is_some_and(|stop| stop.load(std::sync::atomic::Ordering::Acquire));
                let absolute_deadline = lifecycle_deadline
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .map_or(operation_deadline, |deadline| {
                        deadline.min(operation_deadline)
                    });
                let now = std::time::Instant::now();
                if stopping || now >= cancel_at || now >= absolute_deadline {
                    // SAFETY: the operation and all of its storage remain owned
                    // by this worker until the driver reports final completion.
                    cancel_and_drain();
                    return if stopping {
                        Ok(())
                    } else {
                        Err(MicrophoneDeviceError::Timeout)
                    };
                }
                let remaining = cancel_at
                    .min(absolute_deadline)
                    .saturating_duration_since(now);
                let wait_ms = remaining.as_millis().min(10);
                // SAFETY: event and OVERLAPPED remain live for this entire wait.
                let wait = unsafe {
                    WaitForSingleObject(
                        event.0,
                        u32::try_from(wait_ms.max(1)).expect("driver poll interval fits u32"),
                    )
                };
                if wait == WAIT_OBJECT_0 {
                    break;
                }
                if wait != WAIT_TIMEOUT {
                    // SAFETY: cancellation plus the final wait keeps every
                    // operation pointer valid until the driver completes it.
                    cancel_and_drain();
                    return Err(MicrophoneDeviceError::DeviceUnavailable);
                }
            }
            // SAFETY: the event is signaled, so the OVERLAPPED operation has
            // completed and its result may be queried without waiting.
            unsafe { GetOverlappedResult(self.0, &overlapped, &mut returned, false) }
                .map_err(map_windows_error)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
fn map_windows_error(error: windows::core::Error) -> MicrophoneDeviceError {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_DEVICE_NOT_CONNECTED, ERROR_NO_SUCH_DEVICE,
    };

    if error.code() == windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0) {
        MicrophoneDeviceError::AccessDenied
    } else if error.code() == windows::core::HRESULT::from_win32(ERROR_DEVICE_NOT_CONNECTED.0)
        || error.code() == windows::core::HRESULT::from_win32(ERROR_NO_SUCH_DEVICE.0)
    {
        MicrophoneDeviceError::DeviceRemoved
    } else {
        MicrophoneDeviceError::DeviceUnavailable
    }
}

struct FeederFrame {
    binding: MicrophoneSessionBinding,
    request: DriverFeedRequest,
}

impl Drop for FeederFrame {
    fn drop(&mut self) {
        self.request.zeroize();
    }
}

#[cfg(windows)]
pub struct NativeMicrophoneDevice {
    binding: MicrophoneSessionBinding,
    frames: Option<std::sync::mpsc::SyncSender<FeederFrame>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    error: std::sync::Arc<std::sync::Mutex<Option<MicrophoneDeviceError>>>,
    lifecycle_deadline: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    fatal_cleanup: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<DeviceWorker<()>>,
}

#[cfg(windows)]
impl NativeMicrophoneDevice {
    pub async fn open(binding: MicrophoneSessionBinding) -> Result<Self, MicrophoneDeviceError> {
        let open_deadline = std::time::Instant::now() + DEVICE_LIFECYCLE_DEADLINE;
        let sid = parse_string_sid(&binding.user_sid)?;
        let mut request = DriverBindRequest {
            version: BIND_VERSION,
            wts_session_id: binding.wts_session_id,
            generation: binding.generation,
            sid_length: u32::try_from(sid.len()).expect("SID length fits u32"),
            sid: [0; MAX_BINARY_SID_BYTES],
        };
        request.sid[..sid.len()].copy_from_slice(&sid);
        let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel(FEEDER_MAILBOX_FRAMES);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let lifecycle_deadline = std::sync::Arc::new(std::sync::Mutex::new(Some(open_deadline)));
        let fatal_cleanup = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stop = std::sync::Arc::clone(&stop);
        let worker_error = std::sync::Arc::clone(&error);
        let worker_deadline = std::sync::Arc::clone(&lifecycle_deadline);
        let worker_fatal_cleanup = std::sync::Arc::clone(&fatal_cleanup);
        let worker_binding = binding.clone();
        let worker = std::thread::Builder::new()
            .name("arcen-microphone-feeder".to_string())
            .spawn(move || {
                run_native_feeder(
                    worker_binding,
                    request,
                    frame_rx,
                    worker_stop,
                    worker_error,
                    worker_deadline,
                    worker_fatal_cleanup,
                    open_deadline,
                    started_tx,
                    completion_tx,
                );
            })
            .map_err(|_| MicrophoneDeviceError::DeviceUnavailable)?;
        let mut device = Self {
            binding,
            frames: Some(frame_tx),
            stop,
            error,
            lifecycle_deadline,
            fatal_cleanup: std::sync::Arc::clone(&fatal_cleanup),
            worker: Some(DeviceWorker::new(completion_rx, worker, fatal_cleanup)),
        };
        let remaining = open_deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, started_rx).await {
            Ok(Ok(Ok(()))) => {
                *device
                    .lifecycle_deadline
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                Ok(device)
            }
            Ok(Ok(Err(error))) => match device.shutdown_until(open_deadline).await {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(cleanup_error),
            },
            Ok(Err(_)) => match device.shutdown_until(open_deadline).await {
                Ok(()) => Err(MicrophoneDeviceError::WorkerFailed),
                Err(cleanup_error) => Err(cleanup_error),
            },
            Err(_) => match device.shutdown_until(open_deadline).await {
                Ok(()) => Err(MicrophoneDeviceError::Timeout),
                Err(cleanup_error) => Err(cleanup_error),
            },
        }
    }

    fn request_stop_until(&mut self, deadline: std::time::Instant) {
        let mut lifecycle_deadline = self
            .lifecycle_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *lifecycle_deadline =
            Some(lifecycle_deadline.map_or(deadline, |current| current.min(deadline)));
        drop(lifecycle_deadline);
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        self.frames.take();
    }

    fn request_stop(&mut self) -> std::time::Instant {
        let deadline = std::time::Instant::now() + DEVICE_LIFECYCLE_DEADLINE;
        self.request_stop_until(deadline);
        let installed = *self
            .lifecycle_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        installed.expect("stop request installs a lifecycle deadline")
    }

    async fn shutdown_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<(), MicrophoneDeviceError> {
        self.request_stop_until(deadline);
        if let Some(worker) = self.worker.take() {
            worker.finish_until(deadline).await
        } else {
            if self
                .fatal_cleanup
                .load(std::sync::atomic::Ordering::Acquire)
            {
                Err(MicrophoneDeviceError::FatalCleanup)
            } else {
                Ok(())
            }
        }
    }

    pub async fn shutdown_wait(&mut self) -> Result<(), MicrophoneDeviceError> {
        let deadline = self.request_stop();
        self.shutdown_until(deadline).await
    }
}

#[cfg(windows)]
impl MicrophoneDevice for NativeMicrophoneDevice {
    fn write_frame(
        &mut self,
        binding: &MicrophoneSessionBinding,
        frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> Result<(), MicrophoneDeviceError> {
        if binding != &self.binding {
            return Err(
                if binding.wts_session_id == self.binding.wts_session_id
                    && binding.user_sid == self.binding.user_sid
                {
                    MicrophoneDeviceError::StaleGeneration
                } else {
                    MicrophoneDeviceError::AccessDenied
                },
            );
        }
        if self
            .fatal_cleanup
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(MicrophoneDeviceError::FatalCleanup);
        }
        if let Some(error) = *self.error.lock().expect("feeder error lock poisoned") {
            return Err(error);
        }
        let mut request = DriverFeedRequest {
            version: BIND_VERSION,
            generation: binding.generation,
            frame_bytes: u32::try_from(MICROPHONE_V1_FRAME_SAMPLES * 2)
                .expect("fixed frame size fits u32"),
            reserved: 0,
            frame: [0; MICROPHONE_V1_FRAME_SAMPLES * 2],
        };
        for (target, sample) in request.frame.chunks_exact_mut(2).zip(frame) {
            target.copy_from_slice(&sample.to_le_bytes());
        }
        let command = FeederFrame {
            binding: binding.clone(),
            request,
        };
        self.frames
            .as_ref()
            .ok_or(MicrophoneDeviceError::DeviceUnavailable)?
            .try_send(command)
            .map_err(|error| match error {
                std::sync::mpsc::TrySendError::Full(_) => self
                    .error
                    .lock()
                    .expect("feeder error lock poisoned")
                    .unwrap_or(MicrophoneDeviceError::Backpressure),
                std::sync::mpsc::TrySendError::Disconnected(_) => {
                    MicrophoneDeviceError::DeviceUnavailable
                }
            })
    }

    fn clear(&mut self, binding: &MicrophoneSessionBinding) {
        if binding == &self.binding {
            self.request_stop();
        }
    }
}

#[cfg(windows)]
impl Drop for NativeMicrophoneDevice {
    fn drop(&mut self) {
        self.request_stop();
        self.worker.take();
    }
}

#[cfg(not(windows))]
pub struct NativeMicrophoneDevice;

#[cfg(not(windows))]
impl NativeMicrophoneDevice {
    pub async fn open(_binding: MicrophoneSessionBinding) -> Result<Self, MicrophoneDeviceError> {
        Err(MicrophoneDeviceError::DeviceUnavailable)
    }

    pub async fn shutdown_wait(&mut self) -> Result<(), MicrophoneDeviceError> {
        Err(MicrophoneDeviceError::DeviceUnavailable)
    }
}

#[cfg(not(windows))]
impl MicrophoneDevice for NativeMicrophoneDevice {
    fn write_frame(
        &mut self,
        _binding: &MicrophoneSessionBinding,
        _frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
    ) -> Result<(), MicrophoneDeviceError> {
        Err(MicrophoneDeviceError::DeviceUnavailable)
    }

    fn clear(&mut self, _binding: &MicrophoneSessionBinding) {}
}

#[cfg(windows)]
fn run_native_feeder(
    binding: MicrophoneSessionBinding,
    mut bind_request: DriverBindRequest,
    frames: std::sync::mpsc::Receiver<FeederFrame>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    error: std::sync::Arc<std::sync::Mutex<Option<MicrophoneDeviceError>>>,
    lifecycle_deadline: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    fatal_cleanup: std::sync::Arc<std::sync::atomic::AtomicBool>,
    open_deadline: std::time::Instant,
    started: tokio::sync::oneshot::Sender<Result<(), MicrophoneDeviceError>>,
    completion: tokio::sync::oneshot::Sender<Result<(), MicrophoneDeviceError>>,
) {
    let result = match DriverHandle::open().and_then(|handle| {
        handle.ioctl(
            BIND_IOCTL,
            &mut bind_request,
            Some(&stop),
            &lifecycle_deadline,
            open_deadline,
            &fatal_cleanup,
        )?;
        Ok(handle)
    }) {
        Ok(handle) => {
            *lifecycle_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            let _ = started.send(Ok(()));
            let feed_result = process_feeder_frames(&binding, &frames, &stop, |request| {
                handle.ioctl(
                    FEED_IOCTL,
                    request,
                    Some(&stop),
                    &lifecycle_deadline,
                    std::time::Instant::now() + DEVICE_LIFECYCLE_DEADLINE,
                    &fatal_cleanup,
                )
            });
            let mut stop_request = DriverStopRequest {
                version: BIND_VERSION,
                generation: binding.generation,
            };
            let stop_deadline = lifecycle_deadline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .unwrap_or_else(|| std::time::Instant::now() + DEVICE_LIFECYCLE_DEADLINE);
            complete_feeder_cleanup(feed_result, &error, || {
                handle.ioctl(
                    STOP_IOCTL,
                    &mut stop_request,
                    None,
                    &lifecycle_deadline,
                    stop_deadline,
                    &fatal_cleanup,
                )
            })
        }
        Err(start_error) => {
            let _ = started.send(Err(start_error));
            Err(start_error)
        }
    };
    if let Err(feeder_error) = result {
        let mut published = error.lock().expect("feeder error lock poisoned");
        if published.is_none() {
            *published = Some(feeder_error);
        }
    }
    let _ = completion.send(result);
}

fn complete_feeder_cleanup(
    feed_result: Result<(), MicrophoneDeviceError>,
    error: &std::sync::Mutex<Option<MicrophoneDeviceError>>,
    stop: impl FnOnce() -> Result<(), MicrophoneDeviceError>,
) -> Result<(), MicrophoneDeviceError> {
    if let Err(feed_error) = feed_result {
        *error.lock().expect("feeder error lock poisoned") = Some(feed_error);
    }
    let stop_result = stop();
    match feed_result {
        Err(feed_error) => Err(feed_error),
        Ok(()) => stop_result,
    }
}

fn process_feeder_frames(
    binding: &MicrophoneSessionBinding,
    frames: &std::sync::mpsc::Receiver<FeederFrame>,
    stop: &std::sync::atomic::AtomicBool,
    mut feed: impl FnMut(&mut DriverFeedRequest) -> Result<(), MicrophoneDeviceError>,
) -> Result<(), MicrophoneDeviceError> {
    use std::sync::atomic::Ordering;

    while !stop.load(Ordering::Acquire) {
        match frames.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(mut frame) => {
                if stop.load(Ordering::Acquire) {
                    frame.request.zeroize();
                    break;
                }
                if frame.binding != *binding {
                    frame.request.zeroize();
                    return Err(MicrophoneDeviceError::StaleGeneration);
                }
                feed(&mut frame.request)?;
                frame.request.zeroize();
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(mut frame) = frames.try_recv() {
        frame.request.zeroize();
    }
    Ok(())
}

fn parse_string_sid(value: &str) -> Result<Vec<u8>, MicrophoneDeviceError> {
    let mut parts = value.split('-');
    if parts.next() != Some("S") {
        return Err(MicrophoneDeviceError::InvalidBinding);
    }
    let revision = parts
        .next()
        .and_then(|part| part.parse::<u8>().ok())
        .filter(|revision| *revision == 1)
        .ok_or(MicrophoneDeviceError::InvalidBinding)?;
    let authority = parts
        .next()
        .and_then(|part| part.parse::<u64>().ok())
        .filter(|authority| *authority <= 0x0000_FFFF_FFFF_FFFF)
        .ok_or(MicrophoneDeviceError::InvalidBinding)?;
    let subauthorities = parts
        .map(|part| {
            part.parse::<u32>()
                .map_err(|_| MicrophoneDeviceError::InvalidBinding)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if subauthorities.is_empty() || subauthorities.len() > 15 {
        return Err(MicrophoneDeviceError::InvalidBinding);
    }
    let length = 8 + subauthorities.len() * 4;
    if length > MAX_BINARY_SID_BYTES {
        return Err(MicrophoneDeviceError::InvalidBinding);
    }
    let mut sid = Vec::with_capacity(length);
    sid.push(revision);
    sid.push(u8::try_from(subauthorities.len()).expect("bounded subauthority count"));
    sid.extend_from_slice(&authority.to_be_bytes()[2..]);
    for subauthority in subauthorities {
        sid.extend_from_slice(&subauthority.to_le_bytes());
    }
    Ok(sid)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneIngressError {
    MalformedFrame,
    Decode(MicrophoneDecodeError),
    Device(MicrophoneDeviceError),
}

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
        Self::try_new(binding, stream, device).map_err(|(error, _device)| error)
    }

    pub(crate) fn try_new(
        binding: MicrophoneSessionBinding,
        stream: ResolvedMicrophoneStream,
        device: D,
    ) -> Result<Self, (MicrophoneIngressError, D)> {
        if stream.generation != binding.generation {
            return Err((
                MicrophoneIngressError::Device(MicrophoneDeviceError::StaleGeneration),
                device,
            ));
        }
        let decoder = match MicrophoneDecoder::new(stream).map_err(MicrophoneIngressError::Decode) {
            Ok(decoder) => decoder,
            Err(error) => return Err((error, device)),
        };
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
        let (header, payload) = match decode_microphone_frame(bytes) {
            Ok(frame) => frame,
            Err(_) => {
                self.stats.record_decoder_error();
                return Err(MicrophoneIngressError::MalformedFrame);
            }
        };
        match self.decoder.ingest(header, payload) {
            Ok(outcome) => {
                self.stats.record_ingest(outcome, bytes.len());
                Ok(outcome)
            }
            Err(error) => {
                self.stats.record_decoder_error();
                Err(MicrophoneIngressError::Decode(error))
            }
        }
    }

    pub fn playout_tick(&mut self) -> Result<MicrophoneFrameOutput, MicrophoneIngressError> {
        let output = match self.decoder.pop_into(&mut self.output) {
            Ok(output) => output,
            Err(error) => {
                self.stats.record_decoder_error();
                return Err(MicrophoneIngressError::Decode(error));
            }
        };
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

    pub fn shutdown(&mut self) {
        self.clear_once();
    }

    pub fn generation(&self) -> u32 {
        self.binding.generation
    }

    pub fn take_interval_stats(&mut self) -> MicrophoneStats {
        self.stats.take_interval()
    }

    #[must_use]
    pub fn jitter_depth(&self) -> usize {
        self.decoder.queued_frames()
    }

    fn clear_once(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.decoder.clear();
        self.output.zeroize();
        self.device.clear(&self.binding);
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
            MicrophoneDeviceError::Backpressure
            | MicrophoneDeviceError::InvalidBinding
            | MicrophoneDeviceError::AccessDenied
            | MicrophoneDeviceError::StaleGeneration
            | MicrophoneDeviceError::WorkerFailed
            | MicrophoneDeviceError::FatalCleanup => {}
        }
    }
}

impl<D: MicrophoneDevice> Drop for MicrophoneIngress<D> {
    fn drop(&mut self) {
        self.clear_once();
    }
}

impl MicrophoneIngress<NativeMicrophoneDevice> {
    pub async fn shutdown_wait(
        &mut self,
        stop_reason: &'static str,
    ) -> Result<(), MicrophoneIngressError> {
        let jitter_depth = self.decoder.queued_frames();
        self.clear_once();
        let result = self
            .device
            .shutdown_wait()
            .await
            .map_err(MicrophoneIngressError::Device);
        if let Err(MicrophoneIngressError::Device(error)) = result {
            self.record_device_error(error);
        }
        let stats = self.stats.total();
        let duration_ms = self
            .started_at
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let session_log_id = self
            .session_log_id
            .as_ref()
            .map_or("unavailable", CorrelationId::as_str);
        if result.is_ok() {
            tracing::info!(
                target: crate::logging::AUDIO,
                event = "mic_windows_feeder_stopped",
                sid = session_log_id,
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
                "Windows microphone feeder stopped"
            );
        } else {
            let event = match result {
                Err(MicrophoneIngressError::Device(MicrophoneDeviceError::Timeout)) => {
                    "mic_windows_feeder_timeout"
                }
                Err(MicrophoneIngressError::Device(MicrophoneDeviceError::DeviceRemoved)) => {
                    "mic_windows_device_removed"
                }
                _ => "mic_windows_feeder_failure",
            };
            let cleanup_verified = !matches!(
                result,
                Err(MicrophoneIngressError::Device(error)) if error.is_fatal_cleanup()
            );
            tracing::warn!(
                target: crate::logging::AUDIO,
                event,
                sid = session_log_id,
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
                cleanup_verified,
                stop_reason,
                reason = ?result,
                "Windows microphone feeder stopped after failure"
            );
        }
        match result {
            Err(MicrophoneIngressError::Device(error)) if !error.is_fatal_cleanup() => Ok(()),
            result => result,
        }
    }
}

/// Fixed-storage model of the authenticated driver ring.
///
/// The WDF feeder must enforce the same binding before mapping or writing the
/// physical ring. This model is also used by host tests without a WDK.
#[cfg(test)]
pub struct SessionAudioRing {
    binding: MicrophoneSessionBinding,
    frames: [[i16; MICROPHONE_V1_FRAME_SAMPLES]; DRIVER_RING_FRAMES],
    read: usize,
    len: usize,
}

#[cfg(test)]
impl SessionAudioRing {
    pub fn new(binding: MicrophoneSessionBinding) -> Self {
        Self {
            binding,
            frames: [[0; MICROPHONE_V1_FRAME_SAMPLES]; DRIVER_RING_FRAMES],
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
        self.read = (self.read + 1) % DRIVER_RING_FRAMES;
        self.len -= 1;
    }

    pub fn queued_frames(&self) -> usize {
        self.len
    }

    fn authorize(&self, candidate: &MicrophoneSessionBinding) -> Result<(), MicrophoneDeviceError> {
        if candidate.wts_session_id != self.binding.wts_session_id
            || candidate.user_sid != self.binding.user_sid
        {
            return Err(MicrophoneDeviceError::AccessDenied);
        }
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
        if self.len == DRIVER_RING_FRAMES {
            self.frames[self.read].zeroize();
            self.read = (self.read + 1) % DRIVER_RING_FRAMES;
            self.len -= 1;
        }
        let write = (self.read + self.len) % DRIVER_RING_FRAMES;
        self.frames[write].copy_from_slice(frame);
        self.len += 1;
        Ok(())
    }

    fn clear(&mut self, binding: &MicrophoneSessionBinding) {
        if self.authorize(binding).is_err() {
            return;
        }
        self.frames.iter_mut().for_each(Zeroize::zeroize);
        self.read = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn binding(generation: u32) -> MicrophoneSessionBinding {
        MicrophoneSessionBinding::new(12, "S-1-5-21-1000".to_string(), generation).unwrap()
    }

    #[test]
    fn binding_rejects_cross_session_and_stale_writers() {
        let owner = binding(7);
        let mut ring = SessionAudioRing::new(owner.clone());
        let frame = [5; MICROPHONE_V1_FRAME_SAMPLES];
        let cross_session =
            MicrophoneSessionBinding::new(13, owner.user_sid.clone(), owner.generation).unwrap();
        assert_eq!(
            ring.write_frame(&cross_session, &frame),
            Err(MicrophoneDeviceError::AccessDenied)
        );
        assert_eq!(
            ring.write_frame(&binding(8), &frame),
            Err(MicrophoneDeviceError::StaleGeneration)
        );
        assert_eq!(ring.queued_frames(), 0);
    }

    #[test]
    fn string_sid_parser_produces_windows_binary_layout() {
        let sid = parse_string_sid("S-1-5-21-1000").unwrap();
        assert_eq!(sid, vec![1, 2, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 232, 3, 0, 0]);
        assert_eq!(
            parse_string_sid("S-2-5-21"),
            Err(MicrophoneDeviceError::InvalidBinding)
        );
        assert_eq!(
            parse_string_sid("S-1-5"),
            Err(MicrophoneDeviceError::InvalidBinding)
        );
    }

    #[test]
    fn ring_wrap_is_bounded_and_drops_oldest() {
        let owner = binding(7);
        let mut ring = SessionAudioRing::new(owner.clone());
        for value in 1..=DRIVER_RING_FRAMES + 2 {
            ring.write_frame(
                &owner,
                &[i16::try_from(value).unwrap(); MICROPHONE_V1_FRAME_SAMPLES],
            )
            .unwrap();
        }
        assert_eq!(ring.queued_frames(), DRIVER_RING_FRAMES);
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
        ring.clear(&owner);
        output.fill(9);
        ring.read_frame(&mut output);
        assert!(output.iter().all(|sample| *sample == 0));
    }

    struct CountingDevice(Arc<AtomicUsize>);

    impl MicrophoneDevice for CountingDevice {
        fn write_frame(
            &mut self,
            _binding: &MicrophoneSessionBinding,
            _frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
        ) -> Result<(), MicrophoneDeviceError> {
            Ok(())
        }

        fn clear(&mut self, _binding: &MicrophoneSessionBinding) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct FailingDevice;

    impl MicrophoneDevice for FailingDevice {
        fn write_frame(
            &mut self,
            _binding: &MicrophoneSessionBinding,
            _frame: &[i16; MICROPHONE_V1_FRAME_SAMPLES],
        ) -> Result<(), MicrophoneDeviceError> {
            Err(MicrophoneDeviceError::Backpressure)
        }

        fn clear(&mut self, _binding: &MicrophoneSessionBinding) {}
    }

    fn pcm_stream(generation: u32) -> ResolvedMicrophoneStream {
        ResolvedMicrophoneStream {
            codec: Some(arcen_protocol::AudioCodec::Pcm),
            bitrate: arcen_media::audio::AudioBitrateTier::Off,
            generation,
            reason: arcen_protocol::messages::MicrophoneStreamReason::Enabled,
        }
    }

    fn opus_frame(sequence: u32, generation: u32, payload: &[u8]) -> Vec<u8> {
        let header = arcen_protocol::encode_microphone_header(arcen_protocol::MicrophoneHeader {
            codec: arcen_protocol::AudioCodec::Opus,
            sequence,
            timestamp_ms: (sequence - 1) * 20,
            generation,
        })
        .unwrap();
        [header.as_slice(), payload].concat()
    }

    fn opus_ingress() -> MicrophoneIngress<CountingDevice> {
        let owner = binding(7);
        MicrophoneIngress::new(
            owner,
            ResolvedMicrophoneStream {
                codec: Some(arcen_protocol::AudioCodec::Opus),
                bitrate: arcen_media::audio::AudioBitrateTier::Kbps64,
                generation: 7,
                reason: arcen_protocol::messages::MicrophoneStreamReason::Enabled,
            },
            CountingDevice(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap()
    }

    #[test]
    fn ingress_clears_device_exactly_once() {
        let owner = binding(7);
        let clears = Arc::new(AtomicUsize::new(0));
        let stream = pcm_stream(owner.generation);
        {
            let mut ingress =
                MicrophoneIngress::new(owner, stream, CountingDevice(Arc::clone(&clears))).unwrap();
            ingress.shutdown();
            ingress.shutdown();
        }
        assert_eq!(clears.load(Ordering::Acquire), 1);
    }

    #[test]
    fn ingress_recovers_from_bounded_feeder_backpressure() {
        let owner = binding(7);
        let mut ingress =
            MicrophoneIngress::new(owner.clone(), pcm_stream(owner.generation), FailingDevice)
                .unwrap();
        for sequence in 1..=3 {
            let header =
                arcen_protocol::encode_microphone_header(arcen_protocol::MicrophoneHeader {
                    codec: arcen_protocol::AudioCodec::Pcm,
                    sequence,
                    timestamp_ms: (sequence - 1) * 20,
                    generation: owner.generation,
                })
                .unwrap();
            let mut frame = Vec::with_capacity(header.len() + MICROPHONE_V1_FRAME_SAMPLES * 2);
            frame.extend_from_slice(&header);
            for _ in 0..MICROPHONE_V1_FRAME_SAMPLES {
                frame.extend_from_slice(&42i16.to_le_bytes());
            }

            ingress.ingest(&frame).unwrap();
        }
        assert_eq!(ingress.playout_tick(), Ok(MicrophoneFrameOutput::Audio));
        assert!(ingress.output.iter().all(|sample| *sample == 0));
        assert_eq!(ingress.stats.total().transport_backpressure_drops, 1);
    }

    #[test]
    fn terminal_device_error_is_counted_once_before_final_snapshot() {
        let owner = binding(7);
        let mut ingress =
            MicrophoneIngress::new(owner.clone(), pcm_stream(owner.generation), FailingDevice)
                .unwrap();
        ingress.record_device_error(MicrophoneDeviceError::Timeout);
        ingress.record_device_error(MicrophoneDeviceError::Timeout);
        let stats = ingress.stats.total();
        assert_eq!(stats.backend_timeouts, 1);
        assert_eq!(stats.transport_backpressure_drops, 0);
    }

    #[test]
    fn feed_error_is_published_before_slow_stop_cleanup() {
        let error = Arc::new(std::sync::Mutex::new(None));
        let (stop_started, stop_started_rx) = std::sync::mpsc::channel();
        let (release_stop, release_stop_rx) = std::sync::mpsc::channel();
        let worker_error = Arc::clone(&error);
        let worker = std::thread::spawn(move || {
            complete_feeder_cleanup(
                Err(MicrophoneDeviceError::Timeout),
                worker_error.as_ref(),
                || {
                    stop_started.send(()).unwrap();
                    release_stop_rx.recv().unwrap();
                    Err(MicrophoneDeviceError::DeviceRemoved)
                },
            )
        });
        stop_started_rx.recv().unwrap();
        assert_eq!(
            *error.lock().expect("feeder error lock poisoned"),
            Some(MicrophoneDeviceError::Timeout)
        );
        release_stop.send(()).unwrap();
        let result = worker.join().unwrap();
        assert_eq!(result, Err(MicrophoneDeviceError::Timeout));
        assert_eq!(
            *error.lock().expect("feeder error lock poisoned"),
            Some(MicrophoneDeviceError::Timeout)
        );
    }

    #[test]
    fn shared_gate_rejects_malformed_stale_and_retries_failed_sequence() {
        let mut encoder = arcen_media::audio::OpusEncoder::new_for_spec(
            arcen_media::audio::AudioFrameSpec::MICROPHONE_V1,
            arcen_media::audio::AudioBitrateTier::Kbps64,
        )
        .unwrap();
        let mut packet = [0; arcen_media::audio::MAX_OPUS_PACKET_BYTES];
        let encoded = encoder
            .encode(&[0; MICROPHONE_V1_FRAME_SAMPLES], &mut packet)
            .unwrap();

        let mut ingress = opus_ingress();
        assert_eq!(
            ingress.ingest(&opus_frame(1, 7, &packet[..encoded])),
            Ok(MicrophoneIngestOutcome::Accepted)
        );
        assert_eq!(
            ingress.ingest(&opus_frame(1, 7, &[0xff])),
            Ok(MicrophoneIngestOutcome::DroppedDuplicate)
        );
        assert_eq!(
            ingress.ingest(&opus_frame(2, 8, &[0xff])),
            Ok(MicrophoneIngestOutcome::DroppedWrongGeneration)
        );

        let mut retry = opus_ingress();
        assert_eq!(
            retry.ingest(&opus_frame(1, 7, &[0xff])),
            Err(MicrophoneIngressError::Decode(
                MicrophoneDecodeError::CodecFailure
            ))
        );
        assert_eq!(
            retry.ingest(&opus_frame(1, 7, &packet[..encoded])),
            Ok(MicrophoneIngestOutcome::Accepted)
        );
    }

    fn feeder_frame(owner: &MicrophoneSessionBinding, value: u8) -> FeederFrame {
        FeederFrame {
            binding: owner.clone(),
            request: DriverFeedRequest {
                version: BIND_VERSION,
                generation: owner.generation,
                frame_bytes: u32::try_from(MICROPHONE_V1_FRAME_SAMPLES * 2).unwrap(),
                reserved: 0,
                frame: [value; MICROPHONE_V1_FRAME_SAMPLES * 2],
            },
        }
    }

    #[test]
    fn feeder_mailbox_is_bounded() {
        let owner = binding(7);
        let (tx, _rx) = std::sync::mpsc::sync_channel(FEEDER_MAILBOX_FRAMES);
        tx.try_send(feeder_frame(&owner, 1)).unwrap();
        tx.try_send(feeder_frame(&owner, 2)).unwrap();
        assert!(matches!(
            tx.try_send(feeder_frame(&owner, 3)),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
    }

    #[test]
    fn feeder_stop_preempts_queued_audio() {
        let owner = binding(7);
        let (tx, rx) = std::sync::mpsc::sync_channel(FEEDER_MAILBOX_FRAMES);
        tx.try_send(feeder_frame(&owner, 9)).unwrap();
        drop(tx);
        let stop = std::sync::atomic::AtomicBool::new(true);
        let mut writes = 0;
        process_feeder_frames(&owner, &rx, &stop, |_| {
            writes += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(writes, 0);
    }

    #[test]
    fn feeder_propagates_deadline_and_stale_generation_errors() {
        let owner = binding(7);
        let (tx, rx) = std::sync::mpsc::sync_channel(FEEDER_MAILBOX_FRAMES);
        tx.try_send(feeder_frame(&owner, 1)).unwrap();
        drop(tx);
        let stop = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(
            process_feeder_frames(&owner, &rx, &stop, |_| {
                Err(MicrophoneDeviceError::Timeout)
            }),
            Err(MicrophoneDeviceError::Timeout)
        );

        let (tx, rx) = std::sync::mpsc::sync_channel(FEEDER_MAILBOX_FRAMES);
        tx.try_send(feeder_frame(&binding(8), 1)).unwrap();
        drop(tx);
        assert_eq!(
            process_feeder_frames(&owner, &rx, &stop, |_| Ok(())),
            Err(MicrophoneDeviceError::StaleGeneration)
        );
    }

    #[tokio::test]
    async fn policy_off_and_non_cancelling_worker_is_process_fatal() {
        let probes = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&probes);
        assert!(!probe_if_enabled(false, move || {
            observed.fetch_add(1, Ordering::AcqRel);
            true
        })
        .await
        .unwrap());
        assert_eq!(probes.load(Ordering::Acquire), 0);

        let started = std::time::Instant::now();
        assert_eq!(
            probe_if_enabled(true, move || {
                loop {
                    std::thread::park();
                }
            })
            .await,
            Err(MicrophoneDeviceError::FatalCleanup)
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
        assert!(MicrophoneDeviceError::FatalCleanup.is_fatal_cleanup());
        assert!(MicrophoneDeviceError::WorkerFailed.is_fatal_cleanup());
        assert!(!MicrophoneDeviceError::Timeout.is_fatal_cleanup());
        assert!(!MicrophoneDeviceError::DeviceUnavailable.is_fatal_cleanup());
        assert!(!MicrophoneDeviceError::DeviceRemoved.is_fatal_cleanup());
    }
}
