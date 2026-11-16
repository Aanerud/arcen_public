// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `ArcenUsbComponent` as the service control manager sees it, with the
//! rights an ordinary user has (query status and configuration; start only
//! when the service's own ACL allows it, otherwise through an elevated
//! `sc.exe`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use arcen_usb_bridge::winusb_package::SERVICE_NAME;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DISABLED,
    ERROR_SERVICE_DOES_NOT_EXIST,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfigW,
    QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO, SERVICE_DISABLED,
    SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START,
    SERVICE_STATUS_PROCESS, StartServiceW,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::PCWSTR;

use super::{elevate, from_wide, wide};
use crate::{POLICY_BLOCK_CODES, ServiceFacts};

/// How long a start may take before the Deck stops waiting.
const START_WAIT: Duration = Duration::from_secs(15);

struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: opened by OpenSCManagerW/OpenServiceW and closed once.
        let _ = unsafe { CloseServiceHandle(self.0) };
    }
}

/// Opens the service with `access`; `Ok(None)` when it is not installed.
fn open(access: u32) -> Result<Option<ScHandle>, windows::core::Error> {
    // SAFETY: the local SCM database, connect right only.
    let manager =
        ScHandle(unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }?);
    let name = wide(SERVICE_NAME);
    // SAFETY: a valid manager handle and NUL-terminated name.
    match unsafe { OpenServiceW(manager.0, PCWSTR(name.as_ptr()), access) } {
        Ok(service) => Ok(Some(ScHandle(service))),
        Err(error) if error.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => Ok(None),
        Err(error) => Err(error),
    }
}

fn status(service: &ScHandle) -> Option<SERVICE_STATUS_PROCESS> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0_u32;
    // SAFETY: the buffer is exactly one SERVICE_STATUS_PROCESS.
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&raw mut status).cast::<u8>(),
            size_of::<SERVICE_STATUS_PROCESS>(),
        )
    };
    // SAFETY: a valid service handle with SERVICE_QUERY_STATUS.
    unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(bytes),
            &raw mut needed,
        )
    }
    .ok()?;
    Some(status)
}

/// The service's configuration: (start type disabled, binary path).
fn config(service: &ScHandle) -> Option<(bool, String)> {
    let mut needed = 0_u32;
    // SAFETY: a size query with no buffer.
    let _ = unsafe { QueryServiceConfigW(service.0, None, 0, &raw mut needed) };
    // u64 units keep QUERY_SERVICE_CONFIGW aligned.
    let mut buffer = vec![0_u64; usize::try_from(needed).ok()?.div_ceil(8).max(8)];
    let size = u32::try_from(buffer.len() * 8).ok()?;
    let config = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    // SAFETY: the buffer holds `size` bytes, enough for the configuration.
    unsafe { QueryServiceConfigW(service.0, Some(config), size, &raw mut needed) }.ok()?;
    // SAFETY: filled in by the call above; its strings point into `buffer`.
    let config = unsafe { &*config };
    // SAFETY: a NUL-terminated string inside `buffer`, or null.
    let binary = if config.lpBinaryPathName.is_null() {
        String::new()
    } else {
        unsafe { config.lpBinaryPathName.to_string() }.unwrap_or_default()
    };
    Some((config.dwStartType == SERVICE_DISABLED, binary))
}

/// What the Deck may know about the service, without any privilege.
pub(crate) fn service_facts() -> ServiceFacts {
    let Ok(Some(service)) = open(SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG) else {
        return ServiceFacts::default();
    };
    let status = status(&service);
    ServiceFacts {
        installed: true,
        running: status.is_some_and(|status| status.dwCurrentState == SERVICE_RUNNING),
        disabled: config(&service).is_some_and(|(disabled, _)| disabled),
        exit_code: status.map_or(0, |status| status.dwWin32ExitCode),
    }
}

/// The process ID of the running service, as the service control manager
/// recorded it when it started the process.
pub(crate) fn running_pid() -> Option<u32> {
    let service = open(SERVICE_QUERY_STATUS).ok()??;
    status(&service)
        .filter(|status| status.dwCurrentState == SERVICE_RUNNING && status.dwProcessId != 0)
        .map(|status| status.dwProcessId)
}

/// The directory the service's executable is installed in (only an
/// administrator can register or change it).
pub(crate) fn service_directory() -> Result<PathBuf, String> {
    let service = open(SERVICE_QUERY_CONFIG)
        .map_err(|error| format!("read the Arcen USB component's configuration: {error}"))?
        .ok_or("the Arcen USB component is not installed")?;
    let (_, binary) =
        config(&service).ok_or("the Arcen USB component's configuration is unreadable")?;
    crate::binary_from_command_line(&binary)
        .and_then(|path| path.parent().map(PathBuf::from))
        .ok_or_else(|| {
            format!("the Arcen USB component's command line names no executable: {binary:?}")
        })
}

/// The words for a start that failed with Win32 error `code`.
fn start_failure(code: u32) -> String {
    if code == ERROR_SERVICE_DISABLED.0 {
        "The Arcen USB component is disabled. An administrator can set it to Automatic in \
         Services."
            .to_owned()
    } else if POLICY_BLOCK_CODES.contains(&code) {
        "Windows policy (Smart App Control or App Control) blocked the Arcen USB component."
            .to_owned()
    } else {
        format!(
            "The Arcen USB component did not start: {}",
            windows::core::Error::from(windows::core::HRESULT::from_win32(code)).message()
        )
    }
}

/// Starts the service: directly when its ACL lets this user, otherwise
/// with one administrator approval through the in-box `sc.exe`.
pub(crate) fn start() -> Result<(), String> {
    match open(SERVICE_START | SERVICE_QUERY_STATUS) {
        Ok(Some(service)) => {
            // SAFETY: a valid handle opened with SERVICE_START.
            match unsafe { StartServiceW(service.0, None) } {
                Ok(()) => {}
                Err(error) if error.code() == ERROR_SERVICE_ALREADY_RUNNING.to_hresult() => {}
                Err(error) => return Err(start_failure(win32_code(&error))),
            }
        }
        Ok(None) => return Err("the Arcen USB component is not installed".to_owned()),
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            let sc = system_directory()?.join("sc.exe");
            let code = elevate::run_elevated(
                &sc,
                &format!("start {SERVICE_NAME}"),
                Duration::from_secs(60),
            )?;
            if code != 0 && code != ERROR_SERVICE_ALREADY_RUNNING.0 {
                return Err(start_failure(code));
            }
        }
        Err(error) => return Err(format!("open the Arcen USB component: {error}")),
    }
    let deadline = Instant::now() + START_WAIT;
    while Instant::now() < deadline {
        if running_pid().is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(start_failure(service_facts().exit_code))
}

fn win32_code(error: &windows::core::Error) -> u32 {
    // HRESULT_FROM_WIN32 keeps the Win32 code in the low 16 bits.
    u32::from_ne_bytes(error.code().0.to_ne_bytes()) & 0xFFFF
}

fn system_directory() -> Result<PathBuf, String> {
    let mut buffer = [0_u16; 260];
    // SAFETY: writes at most the buffer's length.
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) };
    if length == 0 || usize::try_from(length).unwrap_or(usize::MAX) >= buffer.len() {
        return Err("the Windows system directory is unknown".to_owned());
    }
    Ok(PathBuf::from(from_wide(&buffer)))
}
