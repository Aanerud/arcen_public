//! Active-console Winlogon activation through the documented service SAS API.

use windows::core::{PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, BOOL, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;

const NO_ACTIVE_CONSOLE_SESSION: u32 = u32::MAX;

struct Module(HMODULE);

impl Drop for Module {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: this guard owns the reference returned by LoadLibraryExW.
            let _ = unsafe { FreeLibrary(self.0) };
        }
    }
}

/// Return the physical console session that must host the first interactive login.
pub fn active_console_session() -> Result<u32, String> {
    // SAFETY: WTSGetActiveConsoleSessionId has no arguments or failure side effects.
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == NO_ACTIVE_CONSOLE_SESSION {
        Err("Windows has no active physical console session".to_string())
    } else {
        Ok(session_id)
    }
}

/// Put the physical console into its credential-collection flow.
///
/// This process must be running as a real LocalSystem Windows service and the
/// software-SAS policy must permit services.
pub fn activate_console() -> Result<u32, String> {
    let session_id = active_console_session()?;
    send_sas()?;
    tracing::info!(
        target: crate::logging::CPPIPE,
        windows_session_id = session_id,
        "SendSAS issued from the Windows service"
    );
    Ok(session_id)
}

fn send_sas() -> Result<(), String> {
    let name: Vec<u16> = "sas.dll\0".encode_utf16().collect();
    // SAFETY: the name is NUL-terminated; a null file handle and System32-only
    // search flag prevent loading a sibling DLL from the install directory.
    let module = unsafe {
        LoadLibraryExW(
            PCWSTR(name.as_ptr()),
            HANDLE::default(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map(Module)
    .map_err(|error| format!("load System32 sas.dll: {error}"))?;

    // SAFETY: module is live and the export name is NUL-terminated.
    let entry = unsafe { GetProcAddress(module.0, PCSTR(c"SendSAS".as_ptr().cast())) }
        .ok_or_else(|| "sas.dll does not export SendSAS".to_string())?;
    // SAFETY: the documented sas.dll export has this exact system ABI.
    let send: unsafe extern "system" fn(BOOL) = unsafe { core::mem::transmute(entry) };
    // SAFETY: the caller is the LocalSystem SCM service, so AsUser is FALSE.
    unsafe { send(BOOL(0)) };
    Ok(())
}
