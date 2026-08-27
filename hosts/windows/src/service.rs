//! Windows Service Control Manager host and transactional log handles.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tokio::sync::Notify;
use tracing_subscriber::fmt::MakeWriter;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows::Win32::System::Console::{
    GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SC_MANAGER_CONNECT, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

pub const SERVICE_NAME: &str = "ArcenPier";
pub const SERVICE_CONTROL_TEMPORARY_DEBUG: u32 = 200;
pub const SERVICE_CONTROL_RELOAD_CONFIGURED: u32 = 201;
pub const SERVICE_CONTROL_RELOAD_TLS: u32 = 202;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstalledServiceState {
    Stopped,
    StartPending,
    Running,
    StopPending,
    Other,
    NotInstalled,
    Unavailable,
}

pub(crate) fn query_installed_service_state() -> InstalledServiceState {
    const HRESULT_SERVICE_DOES_NOT_EXIST: i32 = 0x8007_0424_u32 as i32;
    let manager = match unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) } {
        Ok(manager) => manager,
        Err(_) => return InstalledServiceState::Unavailable,
    };
    let service_name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let service =
        match unsafe { OpenServiceW(manager, PCWSTR(service_name.as_ptr()), SERVICE_QUERY_STATUS) }
        {
            Ok(service) => service,
            Err(error) => {
                let _ = unsafe { CloseServiceHandle(manager) };
                return if error.code().0 == HRESULT_SERVICE_DOES_NOT_EXIST {
                    InstalledServiceState::NotInstalled
                } else {
                    InstalledServiceState::Unavailable
                };
            }
        };
    let mut status = SERVICE_STATUS::default();
    let result = unsafe { QueryServiceStatus(service, &mut status) };
    let _ = unsafe { CloseServiceHandle(service) };
    let _ = unsafe { CloseServiceHandle(manager) };
    if result.is_err() {
        return InstalledServiceState::Unavailable;
    }
    match status.dwCurrentState {
        SERVICE_STOPPED => InstalledServiceState::Stopped,
        SERVICE_START_PENDING => InstalledServiceState::StartPending,
        SERVICE_RUNNING => InstalledServiceState::Running,
        SERVICE_STOP_PENDING => InstalledServiceState::StopPending,
        _ => InstalledServiceState::Other,
    }
}

static STOP_SIGNAL: OnceLock<Arc<StopSignal>> = OnceLock::new();
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
static CHECKPOINT: AtomicU32 = AtomicU32::new(1);
static SERVICE_LOG: OnceLock<WindowsLogFile> = OnceLock::new();
static CONTROL_REQUEST: AtomicU8 = AtomicU8::new(0);
static CONTROL_NOTIFY: OnceLock<Notify> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceControlRequest {
    TemporaryDebug,
    ReloadConfigured,
    ReloadTls,
}

impl ServiceControlRequest {
    const fn encoded(self) -> u8 {
        match self {
            Self::TemporaryDebug => 1,
            Self::ReloadConfigured => 2,
            Self::ReloadTls => 4,
        }
    }

    const fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::TemporaryDebug),
            2 => Some(Self::ReloadConfigured),
            4 => Some(Self::ReloadTls),
            _ => None,
        }
    }
}

fn take_control_request() -> Option<ServiceControlRequest> {
    loop {
        let pending = CONTROL_REQUEST.load(Ordering::Acquire);
        if pending == 0 {
            return None;
        }
        let bit = 1u8 << pending.trailing_zeros();
        if CONTROL_REQUEST
            .compare_exchange_weak(pending, pending & !bit, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return ServiceControlRequest::decode(bit);
        }
    }
}

struct StopSignal {
    requested: AtomicBool,
    notify: Notify,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn request(&self) {
        self.requested.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            if self.requested.load(Ordering::Acquire) {
                return;
            }
            let notified = self.notify.notified();
            if self.requested.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// Lock-backed file used by tracing and the Windows standard handles.
#[derive(Clone)]
pub(crate) struct WindowsLogFile {
    path: Arc<PathBuf>,
    file: Arc<Mutex<File>>,
    create_on_reopen: bool,
}

impl WindowsLogFile {
    pub(crate) fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        Self::open_inner(path, true)
    }

    pub(crate) fn open_existing(path: PathBuf) -> Result<Self, String> {
        Self::open_inner(path, false)
    }

    fn open_inner(path: PathBuf, create_on_reopen: bool) -> Result<Self, String> {
        let file = open_shared_append_mode(&path, create_on_reopen)?;
        bind_standard_handles(&SystemStdHandles, file.as_raw_handle() as isize)?;
        Ok(Self {
            path: Arc::new(path),
            file: Arc::new(Mutex::new(file)),
            create_on_reopen,
        })
    }

    pub(crate) fn writer(&self) -> WindowsLogWriter {
        WindowsLogWriter {
            file: Arc::clone(&self.file),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub(crate) fn reopen(&self) -> Result<(), String> {
        let replacement = open_shared_append_mode(self.path(), self.create_on_reopen)?;
        let replacement_handle = replacement.as_raw_handle() as isize;
        let mut file = lock_file(&self.file)?;
        bind_standard_handles(&SystemStdHandles, replacement_handle)?;
        *file = replacement;
        Ok(())
    }

    pub(crate) fn flush(&self) -> Result<(), String> {
        lock_file(&self.file)?
            .flush()
            .map_err(|error| format!("flush {}: {error}", self.path.display()))
    }
}

/// `MakeWriter` adapter serialized with log reopening.
#[derive(Clone)]
pub(crate) struct WindowsLogWriter {
    file: Arc<Mutex<File>>,
}

impl<'writer> MakeWriter<'writer> for WindowsLogWriter {
    type Writer = WindowsLogGuard<'writer>;

    fn make_writer(&'writer self) -> Self::Writer {
        let guard = match self.file.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        WindowsLogGuard { guard }
    }
}

impl Write for WindowsLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.file.lock() {
            Ok(mut file) => file.write(buffer),
            Err(poisoned) => poisoned.into_inner().write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.lock() {
            Ok(mut file) => file.flush(),
            Err(poisoned) => poisoned.into_inner().flush(),
        }
    }
}

pub(crate) struct WindowsLogGuard<'writer> {
    guard: MutexGuard<'writer, File>,
}

impl Write for WindowsLogGuard<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.guard.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.guard.flush()
    }
}

pub(crate) fn open_shared_append(path: &Path) -> Result<File, String> {
    open_shared_append_mode(path, true)
}

fn open_shared_append_mode(path: &Path, create: bool) -> Result<File, String> {
    OpenOptions::new()
        .create(create)
        .append(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))
}

fn lock_file(file: &Mutex<File>) -> Result<MutexGuard<'_, File>, String> {
    file.lock()
        .map_err(|_| "managed log writer lock is poisoned".to_string())
}

trait StdHandleAdapter {
    fn stdout(&self) -> Result<isize, String>;
    fn stderr(&self) -> Result<isize, String>;
    fn set_stdout(&self, handle: isize) -> Result<(), String>;
    fn set_stderr(&self, handle: isize) -> Result<(), String>;
}

struct SystemStdHandles;

impl StdHandleAdapter for SystemStdHandles {
    fn stdout(&self) -> Result<isize, String> {
        // SAFETY: this reads the process standard-handle table without taking ownership.
        unsafe { GetStdHandle(STD_OUTPUT_HANDLE) }
            .map(|handle| handle.0 as isize)
            .map_err(|error| format!("read service stdout handle: {error}"))
    }

    fn stderr(&self) -> Result<isize, String> {
        // SAFETY: this reads the process standard-handle table without taking ownership.
        unsafe { GetStdHandle(STD_ERROR_HANDLE) }
            .map(|handle| handle.0 as isize)
            .map_err(|error| format!("read service stderr handle: {error}"))
    }

    fn set_stdout(&self, handle: isize) -> Result<(), String> {
        // SAFETY: the caller keeps the supplied file alive until another handle is installed.
        unsafe {
            SetStdHandle(
                STD_OUTPUT_HANDLE,
                windows::Win32::Foundation::HANDLE(handle as *mut _),
            )
        }
        .map_err(|error| format!("redirect service stdout: {error}"))
    }

    fn set_stderr(&self, handle: isize) -> Result<(), String> {
        // SAFETY: the caller keeps the supplied file alive until another handle is installed.
        unsafe {
            SetStdHandle(
                STD_ERROR_HANDLE,
                windows::Win32::Foundation::HANDLE(handle as *mut _),
            )
        }
        .map_err(|error| format!("redirect service stderr: {error}"))
    }
}

fn bind_standard_handles(
    adapter: &impl StdHandleAdapter,
    replacement: isize,
) -> Result<(), String> {
    // In a service context services.exe may leave INVALID_HANDLE_VALUE rather
    // than NULL for the inherited standard handles.  Treat that as "no handle"
    // (0/null) so the rollback path is a no-op rather than an error.
    let stdout = adapter.stdout().unwrap_or(0);
    adapter.set_stdout(replacement)?;
    if let Err(error) = adapter.set_stderr(replacement) {
        return match adapter.set_stdout(stdout) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!("{error}; stdout rollback also failed: {rollback}")),
        };
    }
    Ok(())
}

pub fn run_dispatcher() -> Result<(), String> {
    let mut service_name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(service_name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::null(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: the table is NUL-terminated and remains live until the dispatcher
    // returns after ServiceMain exits.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }
        .map_err(|error| format!("StartServiceCtrlDispatcherW: {error}"))
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let result = std::panic::catch_unwind(service_main_impl);
    if result.is_err() {
        report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 2, 0);
    }
}

fn service_main_impl() {
    let name: Vec<u16> = SERVICE_NAME
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: the service name is NUL-terminated and the callback has the exact
    // handler ABI required by the SCM.
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control_handler), None)
    } {
        Ok(handle) => handle,
        Err(_) => return,
    };
    STATUS_HANDLE.store(handle.0 as isize, Ordering::Release);
    let stop = Arc::new(StopSignal::new());
    if STOP_SIGNAL.set(Arc::clone(&stop)).is_err() {
        report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 3, 0);
        return;
    }
    report_status(SERVICE_START_PENDING, NO_ERROR.0, 0, 10_000);

    let args = match timezone_recovery_before_init(
        || {
            // Recovery is warning-only. run_host repeats reconciliation to retain
            // controller state after logging and runtime initialization.
            let _ = crate::timezone::reconcile_service_entry();
        },
        crate::parse_service_args,
    ) {
        Ok(args) => args,
        Err(_) => {
            report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 4, 0);
            return;
        }
    };
    let log_file = match init_service_log() {
        Ok(log_file) => log_file,
        Err(error) => {
            report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 7, 0);
            eprintln!("service log setup failed: {error}");
            return;
        }
    };
    let log_controller = match crate::logging::init(
        args.profile,
        crate::logging::COMPONENT_BROKER,
        Some(log_file),
        true,
    ) {
        Ok(controller) => controller,
        Err(error) => {
            report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 8, 0);
            eprintln!("service tracing setup failed: {error}");
            return;
        }
    };
    // The process-local Windows Event Log source is registered once tracing
    // is available; registration failure never blocks service startup.
    let emitter = crate::eventlog::LifecycleEmitter::init_process_local(log_controller.handle());
    let started_at = std::time::Instant::now();
    let runtime = match crate::build_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(target: crate::logging::NET, %error, "service runtime setup failed");
            emit_service_failed(&emitter, "runtime_init", "runtime_construction_failed");
            if let Err(error) = log_controller.flush_log() {
                eprintln!("service observability flush failed: {error}");
            }
            report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 5, 0);
            return;
        }
    };

    let started_emitter = emitter.clone();
    let result = runtime.block_on(crate::run_host(
        args,
        log_controller.clone(),
        stop.wait(),
        || {
            report_status(SERVICE_RUNNING, NO_ERROR.0, 0, 0);
            emit_service_start(&started_emitter);
        },
        emitter.clone(),
    ));
    match result {
        Ok(()) => {
            emit_service_stop(&emitter, started_at.elapsed());
            if let Err(error) = log_controller.flush_log() {
                eprintln!("service observability flush failed: {error}");
            }
            report_status(SERVICE_STOPPED, NO_ERROR.0, 0, 0);
        }
        Err(error) => {
            tracing::error!(target: crate::logging::NET, %error, "service host failed");
            let (stage, reason_class) = if error.starts_with("tls configuration failed:") {
                ("tls_config", "tls_configuration_failed")
            } else {
                ("host_loop", "host_loop_failed")
            };
            emit_service_failed(&emitter, stage, reason_class);
            if let Err(error) = log_controller.flush_log() {
                eprintln!("service observability flush failed: {error}");
            }
            report_status(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR.0, 6, 0);
        }
    }
}

fn timezone_recovery_before_init<T>(recover: impl FnOnce(), initialize: impl FnOnce() -> T) -> T {
    recover();
    initialize()
}

const SERVICE_COMPONENT: &str = "pier_broker";

/// Emits `SERVICE_START` (1000) only after the SCM has observed
/// `SERVICE_RUNNING`.
fn emit_service_start(emitter: &crate::eventlog::LifecycleEmitter) {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "component",
        arcen_telemetry::FieldValue::String(SERVICE_COMPONENT.to_string()),
    );
    let _ = fields.insert(
        "version",
        arcen_telemetry::FieldValue::String(env!("CARGO_PKG_VERSION").to_string()),
    );
    let _ = fields.insert(
        "pid",
        arcen_telemetry::FieldValue::Integer(i64::from(std::process::id())),
    );
    crate::emit_lifecycle_event(
        emitter,
        arcen_telemetry::LifecycleEventKind::ServiceStart,
        crate::eventlog::random_correlation_id(),
        fields,
    );
}

/// Emits `SERVICE_STOP` (1001) after a clean stop/shutdown and host drain.
fn emit_service_stop(emitter: &crate::eventlog::LifecycleEmitter, uptime: std::time::Duration) {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "component",
        arcen_telemetry::FieldValue::String(SERVICE_COMPONENT.to_string()),
    );
    let _ = fields.insert(
        "reason_class",
        arcen_telemetry::FieldValue::String("clean_shutdown".to_string()),
    );
    let uptime_ms = i64::try_from(uptime.as_millis()).unwrap_or(i64::MAX);
    let _ = fields.insert("uptime_ms", arcen_telemetry::FieldValue::Integer(uptime_ms));
    crate::emit_lifecycle_event(
        emitter,
        arcen_telemetry::LifecycleEventKind::ServiceStop,
        crate::eventlog::random_correlation_id(),
        fields,
    );
}

/// Emits `SERVICE_FAILED` (1002) for runtime construction or host-loop
/// failure, once tracing is initialized. Pre-logging parse/setup failures
/// remain SCM status-only, matching the existing `service_main_impl` control
/// flow above.
fn emit_service_failed(
    emitter: &crate::eventlog::LifecycleEmitter,
    stage: &'static str,
    reason_class: &'static str,
) {
    let mut fields = arcen_telemetry::StructuredFields::default();
    let _ = fields.insert(
        "component",
        arcen_telemetry::FieldValue::String(SERVICE_COMPONENT.to_string()),
    );
    let _ = fields.insert(
        "stage",
        arcen_telemetry::FieldValue::String(stage.to_string()),
    );
    let _ = fields.insert(
        "reason_class",
        arcen_telemetry::FieldValue::String(reason_class.to_string()),
    );
    crate::emit_lifecycle_event(
        emitter,
        arcen_telemetry::LifecycleEventKind::ServiceFailed,
        crate::eventlog::random_correlation_id(),
        fields,
    );
}

fn init_service_log() -> Result<WindowsLogFile, String> {
    let log = WindowsLogFile::open(crate::paths::broker_log_path())?;
    SERVICE_LOG
        .set(log.clone())
        .map_err(|_| "service log was already initialized".to_string())?;
    Ok(log)
}

pub(crate) fn service_log() -> Option<&'static WindowsLogFile> {
    SERVICE_LOG.get()
}

pub(crate) async fn next_control_request() -> ServiceControlRequest {
    let notify = CONTROL_NOTIFY.get_or_init(Notify::new);
    loop {
        if let Some(request) = take_control_request() {
            return request;
        }
        let notified = notify.notified();
        if let Some(request) = take_control_request() {
            return request;
        }
        notified.await;
    }
}

const fn decode_service_control(control: u32) -> Option<ServiceControlRequest> {
    match control {
        SERVICE_CONTROL_TEMPORARY_DEBUG => Some(ServiceControlRequest::TemporaryDebug),
        SERVICE_CONTROL_RELOAD_CONFIGURED => Some(ServiceControlRequest::ReloadConfigured),
        SERVICE_CONTROL_RELOAD_TLS => Some(ServiceControlRequest::ReloadTls),
        _ => None,
    }
}

unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    if control == SERVICE_CONTROL_STOP || control == SERVICE_CONTROL_SHUTDOWN {
        report_status(SERVICE_STOP_PENDING, NO_ERROR.0, 0, 30_000);
        if let Some(stop) = STOP_SIGNAL.get() {
            stop.request();
        }
    } else if let Some(request) = decode_service_control(control) {
        CONTROL_REQUEST.fetch_or(request.encoded(), Ordering::AcqRel);
        CONTROL_NOTIFY.get_or_init(Notify::new).notify_one();
    }
    NO_ERROR.0
}

fn report_status(
    current_state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE,
    win32_exit_code: u32,
    service_exit_code: u32,
    wait_hint: u32,
) {
    let raw = STATUS_HANDLE.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let pending = current_state == SERVICE_START_PENDING || current_state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: current_state,
        dwControlsAccepted: if current_state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: win32_exit_code,
        dwServiceSpecificExitCode: service_exit_code,
        dwCheckPoint: if pending {
            CHECKPOINT.fetch_add(1, Ordering::Relaxed)
        } else {
            0
        },
        dwWaitHint: wait_hint,
    };
    // SAFETY: the status handle was returned for this service and `status`
    // remains live for the duration of the call.
    let _ = unsafe {
        SetServiceStatus(
            SERVICE_STATUS_HANDLE(raw as *mut core::ffi::c_void),
            &status,
        )
    };
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct FakeStdHandles {
        stdout: isize,
        stderr: isize,
        fail_stderr: bool,
        calls: Mutex<Vec<(&'static str, isize)>>,
    }

    impl StdHandleAdapter for FakeStdHandles {
        fn stdout(&self) -> Result<isize, String> {
            Ok(self.stdout)
        }

        fn stderr(&self) -> Result<isize, String> {
            Ok(self.stderr)
        }

        fn set_stdout(&self, handle: isize) -> Result<(), String> {
            self.calls.lock().expect("calls").push(("stdout", handle));
            Ok(())
        }

        fn set_stderr(&self, handle: isize) -> Result<(), String> {
            self.calls.lock().expect("calls").push(("stderr", handle));
            if self.fail_stderr {
                Err("forced stderr failure".to_string())
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn standard_handle_binding_rolls_stdout_back_when_stderr_fails() {
        let adapter = FakeStdHandles {
            stdout: 10,
            stderr: 20,
            fail_stderr: true,
            calls: Mutex::new(Vec::new()),
        };
        assert!(bind_standard_handles(&adapter, 30).is_err());
        assert_eq!(
            *adapter.calls.lock().expect("calls"),
            [("stdout", 30), ("stderr", 30), ("stdout", 10)]
        );
    }

    #[test]
    fn custom_controls_decode_without_doing_io() {
        assert_eq!(
            decode_service_control(SERVICE_CONTROL_TEMPORARY_DEBUG),
            Some(ServiceControlRequest::TemporaryDebug)
        );
        assert_eq!(
            decode_service_control(SERVICE_CONTROL_RELOAD_CONFIGURED),
            Some(ServiceControlRequest::ReloadConfigured)
        );
        assert_eq!(
            decode_service_control(SERVICE_CONTROL_RELOAD_TLS),
            Some(ServiceControlRequest::ReloadTls)
        );
        assert_eq!(decode_service_control(199), None);
    }

    #[test]
    fn pending_custom_controls_are_not_overwritten() {
        CONTROL_REQUEST.store(0, Ordering::Release);
        CONTROL_REQUEST.fetch_or(ServiceControlRequest::ReloadTls.encoded(), Ordering::AcqRel);
        CONTROL_REQUEST.fetch_or(
            ServiceControlRequest::TemporaryDebug.encoded(),
            Ordering::AcqRel,
        );
        assert_eq!(
            take_control_request(),
            Some(ServiceControlRequest::TemporaryDebug)
        );
        assert_eq!(
            take_control_request(),
            Some(ServiceControlRequest::ReloadTls)
        );
        assert_eq!(take_control_request(), None);
    }

    #[test]
    fn timezone_recovery_hook_precedes_fallible_service_initialization() {
        let calls = Mutex::new(Vec::new());
        let result: Result<(), &'static str> = timezone_recovery_before_init(
            || calls.lock().expect("calls").push("recovery"),
            || {
                calls.lock().expect("calls").push("fallible_init");
                Err("simulated config failure")
            },
        );
        assert_eq!(result, Err("simulated config failure"));
        assert_eq!(*calls.lock().expect("calls"), ["recovery", "fallible_init"]);
    }
}
