// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Running under the service control manager as `ArcenUsbComponent`.
//!
//! The service accepts stop and session-change controls; a session change
//! (console connect or disconnect, remote connect, logoff, lock…) goes to
//! the lease policy, which may revoke the served Deck's tablet.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows::Win32::System::RemoteDesktop::WTSSESSION_NOTIFICATION;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
    SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW,
    SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{PCWSTR, PWSTR};

use super::runtime::{self, Config};
use super::wide;

/// The service name the installer registers.
pub(crate) const SERVICE_NAME: &str = arcen_usb_bridge::winusb_package::SERVICE_NAME;

static STOP: AtomicBool = AtomicBool::new(false);
static STATUS: OnceLock<usize> = OnceLock::new();

fn report(state: SERVICE_STATUS_CURRENT_STATE, exit: u32) {
    let Some(&handle) = STATUS.get() else { return };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SESSIONCHANGE
        } else {
            0
        },
        dwWin32ExitCode: exit,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 5000,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW.
    let _ = unsafe { SetServiceStatus(SERVICE_STATUS_HANDLE(handle as *mut _), &raw const status) };
}

unsafe extern "system" fn control(
    control: u32,
    event: u32,
    data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_SESSIONCHANGE => {
            if !data.is_null() {
                // SAFETY: for SERVICE_CONTROL_SESSIONCHANGE the SCM passes a
                // WTSSESSION_NOTIFICATION that lives for this call.
                let session = unsafe { (*data.cast::<WTSSESSION_NOTIFICATION>()).dwSessionId };
                runtime::session_changed(event, session);
            }
            NO_ERROR.0
        }
        SERVICE_CONTROL_STOP => {
            report(SERVICE_STOP_PENDING, 0);
            STOP.store(true, Ordering::SeqCst);
            runtime::wake();
            NO_ERROR.0
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR.0,
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = wide(SERVICE_NAME);
    // SAFETY: registers our handler for this service.
    let Ok(handle) =
        (unsafe { RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control), None) })
    else {
        return;
    };
    let _ = STATUS.set(handle.0 as usize);
    report(SERVICE_START_PENDING, 0);
    let Ok(root) = super::secure_component_root() else {
        report(SERVICE_STOPPED, 1);
        return;
    };
    report(SERVICE_RUNNING, 0);
    runtime::run(&Config::installed(root), &STOP);
    report(SERVICE_STOPPED, 0);
}

/// Hands the process to the service control manager; returns `false` when
/// the process was not started by it.
pub(crate) fn dispatch() -> bool {
    let mut name = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: a NULL-terminated table whose strings outlive the call.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.is_ok()
}
