// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The named pipe the Deck reaches the service on, and who may use it.
//!
//! - The pipe is created first (`FILE_FLAG_FIRST_PIPE_INSTANCE`), one
//!   instance, local clients only (`PIPE_REJECT_REMOTE_CLIENTS`), with a DACL
//!   that grants SYSTEM everything and interactive users read and write.
//! - A connected client is accepted only when, on a **held** process handle
//!   (never a bare PID): it runs in the active console session, its
//!   process token is that session's user, and its image is an allowed
//!   Deck executable. The PID is checked again after the handle is open, so
//!   a process that exited and had its PID reused cannot pass.
//! - The accept records the console session and the console generation
//!   (read before verification) that the lease is granted under; the lease
//!   ends when either moves (`arcen_usb_bridge::component_lease`).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeClientSessionId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use super::wide;

/// The pipe's name (shared with the Deck's client).
pub(crate) const PIPE_NAME: &str = arcen_usb_bridge::winusb_package::PIPE_NAME;
/// SYSTEM and the creating owner: all. Interactive users: read data,
/// write data, read attributes and control, synchronize — explicitly not
/// `GENERIC_WRITE`, which includes `FILE_CREATE_PIPE_INSTANCE` and would let
/// any user add a pipe instance of their own. A network logon (SSH, a
/// service) is not an interactive user and cannot open the pipe at all.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)(A;;0x12018b;;;IU)";
/// Lab builds with `--lab-any-session` only: administrators may open it too,
/// so a lab probe can connect over SSH.
#[cfg(feature = "lab-emulate")]
const LAB_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;OW)(A;;0x12018b;;;IU)(A;;0x12018b;;;BA)";
// A Deck opens the pipe with exactly 0x0012_018b (the rights the DACL grants
// users), never GENERIC_WRITE; see examples/pipe_probe.rs.
const BUFFER: u32 = 64 * 1024;

/// Who may connect.
#[derive(Debug, Clone)]
pub(crate) struct ClientPolicy {
    /// Deck executables (full paths, compared case-insensitively).
    pub(crate) allowed_images: Vec<PathBuf>,
    /// Lab builds only: accept a client outside the console session (an
    /// SSH session on a lab machine nobody is signed in to). The image
    /// check still applies.
    #[cfg(feature = "lab-emulate")]
    pub(crate) any_session: bool,
}

/// One accepted, verified client connection.
pub(crate) struct Connection {
    pipe: Arc<PipeHandle>,
    /// Held for the session: the verified client cannot be replaced by
    /// another process reusing its PID while this is open.
    client_process: OwnedHandle,
    pub(crate) client_image: PathBuf,
    pub(crate) client_pid: u32,
    /// The active console session at verification (the client's own,
    /// except in the lab's any-session mode).
    pub(crate) console_session: u32,
    /// The console generation read before verification began.
    pub(crate) generation: u64,
}

struct OwnedHandle(HANDLE);
// SAFETY: a kernel handle, usable from any thread.
unsafe impl Send for OwnedHandle {}
// SAFETY: as above.
unsafe impl Sync for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: owned and closed once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The pipe instance, shared by the reader, the writer and `abort`.
pub(crate) struct PipeHandle(OwnedHandle);

impl PipeHandle {
    /// Makes our blocked read or write return without disconnecting the
    /// client: what was already written stays readable for it until the
    /// handle is closed (`DisconnectNamedPipe` would discard it).
    pub(crate) fn cancel(&self) {
        // SAFETY: cancels I/O on our own pipe handle.
        let _ = unsafe { CancelIoEx(self.0.0, None) };
    }

    /// Makes any blocked read or write return and disconnects the client.
    pub(crate) fn abort(&self) {
        // SAFETY: cancels I/O on our own pipe handle, then disconnects it.
        unsafe {
            let _ = CancelIoEx(self.0.0, None);
            let _ = DisconnectNamedPipe(self.0.0);
        }
    }
}

impl Connection {
    /// A reader over the pipe.
    pub(crate) fn reader(&self) -> PipeIo {
        PipeIo(Arc::clone(&self.pipe))
    }

    /// A writer over the pipe.
    pub(crate) fn writer(&self) -> PipeIo {
        PipeIo(Arc::clone(&self.pipe))
    }

    /// The handle `abort` acts on.
    pub(crate) fn handle(&self) -> Arc<PipeHandle> {
        Arc::clone(&self.pipe)
    }

    /// Whether the verified client process has exited (the connection
    /// holds its handle, so the answer is about that very process).
    pub(crate) fn client_exited(&self) -> bool {
        // SAFETY: a zero-timeout wait on the held process handle.
        let state = unsafe { WaitForSingleObject(self.client_process.0, 0) };
        state == WAIT_OBJECT_0
    }
}

/// Blocking byte I/O on the pipe.
pub(crate) struct PipeIo(Arc<PipeHandle>);

/// One overlapped operation on the pipe, waited for. The pipe is opened
/// for overlapped I/O so that the reader and the writer threads never
/// serialise on the handle (synchronous I/O on one file object does), and
/// `abort` (`CancelIoEx`) ends either.
fn overlapped_io(
    pipe: HANDLE,
    start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> std::io::Result<usize> {
    // SAFETY: a manual-reset, unsignalled event, closed below.
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let event = OwnedHandle(event);
    let mut overlapped = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    let started = start(&raw mut overlapped);
    if let Err(error) = started {
        if error.code() != ERROR_IO_PENDING.to_hresult() {
            return Err(std::io::Error::other(error.to_string()));
        }
    }
    let mut transferred = 0_u32;
    // SAFETY: waits for our own operation to finish (or be cancelled).
    unsafe { GetOverlappedResult(pipe, &raw const overlapped, &raw mut transferred, true) }
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(transferred as usize)
}

impl Read for PipeIo {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let pipe = self.0.0.0;
        // SAFETY: `buffer` outlives the operation, which `overlapped_io`
        // waits for before returning.
        let outcome = overlapped_io(pipe, |overlapped| unsafe {
            ReadFile(pipe, Some(buffer), None, Some(overlapped))
        });
        // A disconnected or cancelled pipe is the end of the stream.
        Ok(outcome.unwrap_or(0))
    }
}

impl Write for PipeIo {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let pipe = self.0.0.0;
        // SAFETY: as for `read`.
        overlapped_io(pipe, |overlapped| unsafe {
            WriteFile(pipe, Some(buffer), None, Some(overlapped))
        })
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The pipe instance currently waiting or serving, so a service stop can
/// end it.
static ACTIVE: std::sync::Mutex<Option<Arc<PipeHandle>>> = std::sync::Mutex::new(None);

fn active() -> std::sync::MutexGuard<'static, Option<Arc<PipeHandle>>> {
    ACTIVE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cancels whatever the active pipe instance is doing (waiting for a Deck
/// or serving one).
pub(crate) fn abort_active() {
    if let Some(pipe) = active().as_ref() {
        pipe.abort();
    }
}

/// Drops the finished session's pipe instance.
pub(crate) fn forget_active() {
    active().take();
}

/// The next listening instance, created while the current one is still
/// connected, so the pipe's name is never free for another process to take.
static SPARE: std::sync::Mutex<Option<Arc<PipeHandle>>> = std::sync::Mutex::new(None);

/// Creates one pipe instance: the very first with
/// `FILE_FLAG_FIRST_PIPE_INSTANCE` (fails if anyone else owns the name), the
/// following ones beside an instance the service still holds. Two instances
/// at most; a user cannot add one (the DACL grants no create-instance right).
#[cfg_attr(not(feature = "lab-emulate"), allow(unused_variables))]
fn create_instance(policy: &ClientPolicy, first: bool) -> Result<Arc<PipeHandle>, String> {
    let name = wide(PIPE_NAME);
    #[cfg(feature = "lab-emulate")]
    let sddl = wide(if policy.any_session {
        LAB_PIPE_SDDL
    } else {
        PIPE_SDDL
    });
    #[cfg(not(feature = "lab-emulate"))]
    let sddl = wide(PIPE_SDDL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: parses a constant SDDL string into a LocalAlloc'd descriptor.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .map_err(|error| format!("pipe security descriptor: {error}"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(0),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    // SAFETY: creates the single, local-only pipe instance with that DACL.
    let pipe = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            if first {
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED
            } else {
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED
            },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            2,
            BUFFER,
            BUFFER,
            0,
            Some(&raw const attributes),
        )
    };
    // SAFETY: frees the descriptor allocated above.
    unsafe { LocalFree(HLOCAL(descriptor.0)) };
    if pipe == INVALID_HANDLE_VALUE || pipe.is_invalid() {
        return Err(format!(
            "create {PIPE_NAME}: {} (another process may own the name)",
            windows::core::Error::from_win32()
        ));
    }
    Ok(Arc::new(PipeHandle(OwnedHandle(pipe))))
}

/// Waits for one client on the listening instance, keeps the name held by a
/// fresh spare, then verifies the client. A refused client is disconnected
/// and the refusal returned.
pub(crate) fn accept(
    policy: &ClientPolicy,
    stop: &std::sync::atomic::AtomicBool,
    console_generation: impl FnOnce() -> u64,
) -> Result<Connection, String> {
    let pipe = SPARE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let pipe = match pipe {
        Some(pipe) => pipe,
        None => create_instance(policy, true)?,
    };
    // The successor exists before this instance can fail or close, so the
    // name is never free for another process to take.
    match create_instance(policy, false) {
        Ok(next) => {
            *SPARE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(next);
        }
        Err(error) => {
            *SPARE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pipe);
            return Err(format!("keep the pipe name: {error}"));
        }
    }
    {
        // Published before the stop flag is read: a stop either sees this
        // instance and cancels it, or is seen here.
        let mut slot = active();
        *slot = Some(Arc::clone(&pipe));
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            slot.take();
            return Err("stopping".to_owned());
        }
    }
    let handle = pipe.0.0;
    // SAFETY: waits for a client on our pipe. ERROR_PIPE_CONNECTED (a client
    // that connected before the call) is also success.
    let connected = overlapped_io(handle, |overlapped| unsafe {
        ConnectNamedPipe(handle, Some(overlapped))
    });
    if let Err(error) = connected {
        if !error.to_string().contains("0x80070217") {
            return Err(format!("wait for the Deck: {error}"));
        }
    }
    let generation = console_generation();
    match verify(&pipe, policy) {
        Ok((client, pid, image, console_session)) => Ok(Connection {
            pipe,
            client_process: client,
            client_image: image,
            client_pid: pid,
            console_session,
            generation,
        }),
        Err(refusal) => {
            pipe.abort();
            Err(refusal)
        }
    }
}

fn verify(
    pipe: &PipeHandle,
    policy: &ClientPolicy,
) -> Result<(OwnedHandle, u32, PathBuf, u32), String> {
    let handle = pipe.0.0;
    let mut pid = 0_u32;
    // SAFETY: reads the client PID the kernel recorded for this pipe.
    unsafe { GetNamedPipeClientProcessId(handle, &raw mut pid) }
        .map_err(|error| format!("client process: {error}"))?;
    // SAFETY: opens the client with query rights only, and holds it.
    let process = OwnedHandle(
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map_err(|error| format!("open client process {pid}: {error}"))?,
    );

    let mut session = 0_u32;
    // SAFETY: reads the client's session id for this pipe.
    unsafe { GetNamedPipeClientSessionId(handle, &raw mut session) }
        .map_err(|error| format!("client session: {error}"))?;
    #[cfg(feature = "lab-emulate")]
    let lab_any_session = policy.any_session;
    #[cfg(not(feature = "lab-emulate"))]
    let lab_any_session = false;
    // SAFETY: no arguments.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    if lab_any_session {
        // Lab only: the console-user check below needs a console user.
    } else if session != console {
        return Err(format!(
            "refusing a client in session {session}: only the console session's Deck may use the tablet"
        ));
    }

    let client_user = process_user(process.0)?;
    if !lab_any_session {
        same_user_as_console(console, &client_user)?;
    }

    let image = image_path(process.0)?;
    if !policy
        .allowed_images
        .iter()
        .any(|allowed| same_path(allowed, &image))
    {
        return Err(format!(
            "refusing {}: not an allowed Arcen Deck executable",
            image.display()
        ));
    }

    // The PID is still the connected client's, and that process is alive:
    // the handle we verified is the client's own.
    let mut again = 0_u32;
    // SAFETY: as above.
    unsafe { GetNamedPipeClientProcessId(handle, &raw mut again) }
        .map_err(|error| format!("client process: {error}"))?;
    // SAFETY: a zero-timeout wait on the held process handle.
    let alive = unsafe { WaitForSingleObject(process.0, 0) } == WAIT_TIMEOUT;
    if again != pid || !alive {
        return Err("the client changed while it was verified".to_owned());
    }
    Ok((process, pid, image, console))
}

fn same_user_as_console(console: u32, client_user: &str) -> Result<(), String> {
    let mut console_token = HANDLE::default();
    // SAFETY: LocalSystem may query the console user's token.
    unsafe { WTSQueryUserToken(console, &raw mut console_token) }
        .map_err(|error| format!("console user: {error}"))?;
    let console_token = OwnedHandle(console_token);
    if token_user(console_token.0)? == client_user {
        Ok(())
    } else {
        Err("refusing a client that is not the console user".to_owned())
    }
}

/// The user SID of the client process's token, read through the held
/// process handle (impersonating a byte pipe needs the client to write
/// first, and the component speaks first on this boundary).
fn process_user(process: HANDLE) -> Result<String, String> {
    let mut token = HANDLE::default();
    // SAFETY: opens the held client process's token for query only.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) }
        .map_err(|error| format!("client token: {error}"))?;
    let token = OwnedHandle(token);
    token_user(token.0)
}

/// The user SID of a token, as its string form (`S-1-5-21-…`).
fn token_user(token: HANDLE) -> Result<String, String> {
    let mut buffer = vec![0_u64; 64];
    let mut needed = 0_u32;
    // SAFETY: reads TokenUser into a 512-byte aligned buffer.
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            512,
            &raw mut needed,
        )
    }
    .map_err(|error| format!("token user: {error}"))?;
    // SAFETY: the buffer now holds a TOKEN_USER whose SID lies within it.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = PWSTR::null();
    // SAFETY: converts a valid SID; the string is LocalAlloc'd.
    unsafe { ConvertSidToStringSidW(sid, &raw mut text) }
        .map_err(|error| format!("token user SID: {error}"))?;
    // SAFETY: a NUL-terminated string from the call above.
    let value = unsafe { text.to_string() }.unwrap_or_default();
    // SAFETY: frees the string allocated above.
    unsafe { LocalFree(HLOCAL(text.0.cast())) };
    if value.is_empty() {
        return Err("token user SID is empty".to_owned());
    }
    Ok(value)
}

fn image_path(process: HANDLE) -> Result<PathBuf, String> {
    let mut buffer = [0_u16; 1024];
    let mut size = 1024_u32;
    // SAFETY: reads the image path into `buffer`.
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &raw mut size,
        )
    }
    .map_err(|error| format!("client image: {error}"))?;
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..size as usize],
    )))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}
