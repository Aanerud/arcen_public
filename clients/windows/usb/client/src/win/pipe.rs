// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's end of `\\.\pipe\arcen-usb-component`.
//!
//! - Opened with exactly the rights the service's DACL grants interactive
//!   users (`0x0012_018B`: read and write data, attributes, `READ_CONTROL`,
//!   `SYNCHRONIZE`), never `GENERIC_WRITE`.
//! - Opened with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`: the
//!   server may learn who the Deck is, but can never act as the user.
//! - Accepted only when the process serving it
//!   (`GetNamedPipeServerProcessId`) is the process the service control
//!   manager started as `ArcenUsbComponent` and reports running. While that
//!   process lives no other process can hold its PID, the service keeps a
//!   pipe instance at all times, and users cannot create instances, so an
//!   impostor can neither hold the name nor pass this check.

use arcen_deck_core::runtime::usb_component::ComponentClient;
use arcen_usb_bridge::winusb_package::PIPE_NAME;
use tokio::net::windows::named_pipe::NamedPipeClient;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::core::PCWSTR;

use super::{OwnedHandle, scm, wide};
use crate::{PEER, PLATFORM};

/// The rights the pipe's DACL grants interactive users.
const PIPE_ACCESS: u32 = 0x0012_018B;
/// How long a busy pipe is waited for once (a previous session ending).
const BUSY_WAIT_MS: u32 = 2_000;

/// Opens the pipe, proves the service serves it, and completes the
/// component handshake (which captures the tablet).
pub(crate) async fn connect() -> Result<ComponentClient<NamedPipeClient>, String> {
    let handle = open()?;
    verify_server(handle.0)?;
    let raw = handle.0.0;
    std::mem::forget(handle);
    // SAFETY: `raw` is an overlapped pipe handle we own; tokio takes
    // ownership (and closes it on error).
    let pipe = unsafe { NamedPipeClient::from_raw_handle(raw) }
        .map_err(|error| format!("attach the {PEER} pipe: {error}"))?;
    ComponentClient::handshake(pipe, PEER).await
}

fn open() -> Result<OwnedHandle, String> {
    let name = wide(PIPE_NAME);
    for attempt in 0..2 {
        // SAFETY: opens an existing pipe by NUL-terminated name.
        let opened = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                HANDLE::default(),
            )
        };
        match opened {
            Ok(handle) => return Ok(OwnedHandle(handle)),
            Err(error) if error.code() == ERROR_PIPE_BUSY.to_hresult() && attempt == 0 => {
                // SAFETY: waits for an instance of the named pipe.
                let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), BUSY_WAIT_MS) };
            }
            Err(error) if error.code() == ERROR_PIPE_BUSY.to_hresult() => {
                return Err(format!("the {PEER} is serving another Deck session"));
            }
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => {
                return Err(format!(
                    "the {PEER} is not answering. {}",
                    crate::component_state().guidance(PLATFORM)
                ));
            }
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                return Err(format!(
                    "Windows refused the {PEER} pipe: only a user signed in at this PC can use \
                     Native tablet"
                ));
            }
            Err(error) => return Err(format!("open the {PEER} pipe: {error}")),
        }
    }
    Err(format!("the {PEER} is serving another Deck session"))
}

fn verify_server(pipe: HANDLE) -> Result<(), String> {
    let mut server = 0_u32;
    // SAFETY: a connected pipe handle we own.
    unsafe { GetNamedPipeServerProcessId(pipe, &raw mut server) }
        .map_err(|error| format!("refusing the {PEER} pipe: its server is unknown: {error}"))?;
    let service = scm::running_pid();
    let lab = cfg!(feature = "lab");
    crate::check_pipe_server(server, service, lab)?;
    if lab && service != Some(server) {
        tracing::warn!(
            target: arcen_deck_core::log_target::USB,
            server,
            service = ?service,
            "lab build: accepting a component pipe the installed service does not serve"
        );
    }
    Ok(())
}
