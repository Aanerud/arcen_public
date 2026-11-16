// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Opening a document or page, and running one program with administrator
//! approval (the "runas" verb: Windows shows its own consent prompt; the
//! Deck itself never holds an elevated token).

use std::path::Path;
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_CANCELLED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};
use windows::core::PCWSTR;

use super::{OwnedHandle, wide};

/// The shell wants COM on the calling thread; balanced on drop.
struct ComScope(bool);

impl ComScope {
    fn enter() -> Self {
        // SAFETY: initialises COM for this thread; a thread already in
        // another apartment keeps it (RPC_E_CHANGED_MODE, not balanced).
        let result =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        Self(result.is_ok())
    }
}

impl Drop for ComScope {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

fn execute(info: &mut SHELLEXECUTEINFOW) -> Result<(), String> {
    let _com = ComScope::enter();
    // SAFETY: `info` is fully initialised and its strings outlive the call.
    unsafe { ShellExecuteExW(info) }.map_err(|error| {
        if error.code() == ERROR_CANCELLED.to_hresult() {
            "Administrator approval was cancelled.".to_owned()
        } else {
            error.message()
        }
    })
}

/// Opens a file or URL with its registered handler, without waiting.
pub(crate) fn open(target: &str) -> Result<(), String> {
    let verb = wide("open");
    let file = wide(target);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        fMask: SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    execute(&mut info).map_err(|error| format!("open {target}: {error}"))
}

/// Runs `program` with administrator approval and waits for its exit code.
pub(crate) fn run_elevated(
    program: &Path,
    parameters: &str,
    timeout: Duration,
) -> Result<u32, String> {
    let verb = wide("runas");
    let file = wide(&program.to_string_lossy());
    let parameters_wide = wide(parameters);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: u32::try_from(size_of::<SHELLEXECUTEINFOW>()).unwrap_or(0),
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters_wide.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    execute(&mut info).map_err(|error| format!("{}: {error}", program.display()))?;
    if info.hProcess.is_invalid() {
        return Err(format!("{} did not start", program.display()));
    }
    let process = OwnedHandle(info.hProcess);
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: a process handle we own.
    match unsafe { WaitForSingleObject(process.0, millis) } {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            return Err(format!(
                "{} did not finish within {} s",
                program.display(),
                timeout.as_secs()
            ));
        }
        other => {
            return Err(format!(
                "waiting for {} failed ({other:?})",
                program.display()
            ));
        }
    }
    let mut code = 0_u32;
    // SAFETY: the process has exited; reads its exit code.
    unsafe { GetExitCodeProcess(process.0, &raw mut code) }
        .map_err(|error| format!("{}: {error}", program.display()))?;
    Ok(code)
}
