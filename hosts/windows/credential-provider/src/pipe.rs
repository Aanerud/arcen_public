//! The Credential Provider side of the SYSTEM-only broker control pipe.
//!
//! When LogonUI `Advise`s the provider, [`CredentialPipe::start`] launches a
//! background thread that connects to the broker's named pipe, verifies the
//! server is running as **SYSTEM** (fail-closed), publishes a fresh per-Advise
//! ephemeral public key + challenge via `Ready`, and blocks for exactly one
//! sealed credential push. On a valid push it decrypts the credential (single
//! use, see [`arcen_cp_ipc::cp_session`]), stores it in the shared
//! [`crate::fields::CredentialFields`] as a one-shot autologon, and — outside
//! any lock — signals `ICredentialProviderEvents::CredentialsChanged` through a
//! thread-agile reference so LogonUI re-queries and auto-submits the tile once.
//!
//! `UnAdvise` (or provider teardown) sets a stop flag and closes the connection
//! handle to unblock the thread, then joins it: no deadlock, no leak. All I/O and
//! decryption logic that can be reasoned about without Windows lives in the pure
//! [`arcen_cp_ipc`] crate and is unit-tested there; this module is the thin,
//! Windows-only transport + COM notification glue.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{AgileReference, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, FreeLibrary, HANDLE, HMODULE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser, WinLocalSystemSid, PSID,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{
    FreeLibraryAndExitThread, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
};
use windows::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::ICredentialProviderEvents;

use arcen_cp_ipc::cp_session::{provider_serve, CredentialSession, ProviderIdentity};
use arcen_cp_ipc::transport::StreamFrames;
use arcen_cp_ipc::{image_basename_matches, UsageScenario, CP_PIPE_NAME};

use crate::credential::SharedFields;
use crate::provider::ProviderUsage;
use crate::secret::SecretWide;

const FILE_SHARE_NONE: FILE_SHARE_MODE = FILE_SHARE_MODE(0);
const GENERIC_READ_WRITE: u32 = 0x8000_0000 | 0x4000_0000;

/// The broker image the pipe server is expected to run as. Verified best-effort;
/// the SYSTEM token check is the mandatory gate (see [`verify_server`]).
const EXPECTED_BROKER_IMAGE: &str = "arcen-pier.exe";

/// Single-use decryption window per connection. The recipient is also consumed
/// on first use, so this only bounds how long one published `Ready` is valid.
const SESSION_EXPIRY_MS: u64 = 5 * 60 * 1000;

/// A pushed plaintext credential should be consumed by LogonUI immediately.
/// Profile creation may take minutes, but the password must not remain armed
/// for that entire wait.
const ARMED_CREDENTIAL_TTL: Duration = Duration::from_secs(30);

/// Backoff between (re)connect attempts while the provider stays advised.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// How long `CreateFile`/`WaitNamedPipe` waits for a pipe instance to be free.
const CONNECT_WAIT_MS: u32 = 2_000;

/// Owns the background pipe worker for one Advise lifecycle.
pub struct CredentialPipe {
    worker: Option<Worker>,
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle_slot: SharedHandle,
    join: JoinHandle<()>,
}

/// A shared, idempotently-closable pipe handle. The raw value crosses the thread
/// boundary (`HANDLE` is not `Send`); whichever of the worker or `UnAdvise`
/// closes it first wins, and the other becomes a no-op.
type SharedHandle = Arc<Mutex<Option<isize>>>;

impl CredentialPipe {
    pub fn new() -> Self {
        Self { worker: None }
    }

    /// Start (or restart) the worker for a freshly-advised provider.
    pub fn start(
        &mut self,
        fields: SharedFields,
        events: &ICredentialProviderEvents,
        context: usize,
        usage: ProviderUsage,
    ) {
        self.stop_worker();
        let agile = match AgileReference::new(events) {
            Ok(agile) => agile,
            Err(error) => {
                crate::log::debug(&format!("could not marshal CP events: {error}"));
                return;
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let handle_slot: SharedHandle = Arc::new(Mutex::new(None));
        let scenario = match usage {
            ProviderUsage::Logon => UsageScenario::Logon,
            ProviderUsage::UnlockWorkstation => UsageScenario::UnlockWorkstation,
        };
        let params = WorkerParams {
            fields,
            agile,
            context,
            scenario,
            clsid: crate::registration::CLSID_STRING.to_string(),
            stop: Arc::clone(&stop),
            handle_slot: Arc::clone(&handle_slot),
        };
        match std::thread::Builder::new()
            .name("arcen-cp-client".to_string())
            .spawn(move || worker_main(params))
        {
            Ok(join) => {
                self.worker = Some(Worker {
                    stop,
                    handle_slot,
                    join,
                });
            }
            Err(error) => {
                crate::log::debug(&format!("could not spawn CP pipe worker: {error}"));
            }
        }
    }

    /// Stop the worker: signal, unblock the blocked read by closing the handle,
    /// and join. Idempotent and safe to call from `UnAdvise` and `Drop`.
    pub fn stop(&mut self) {
        self.stop_worker();
    }

    fn stop_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.stop.store(true, Ordering::Release);
            close_shared_handle(&worker.handle_slot);
            let _ = worker.join.join();
        }
    }
}

impl Default for CredentialPipe {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CredentialPipe {
    fn drop(&mut self) {
        self.stop_worker();
    }
}

struct WorkerParams {
    fields: SharedFields,
    agile: AgileReference<ICredentialProviderEvents>,
    context: usize,
    scenario: UsageScenario,
    clsid: String,
    stop: Arc<AtomicBool>,
    handle_slot: SharedHandle,
}

fn worker_main(params: WorkerParams) {
    // Initialize COM so the agile reference can be resolved on this thread. Track
    // whether we owe a matching CoUninitialize.
    // SAFETY: no COM object is passed; multithreaded apartment is requested.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let com_owned = com.is_ok();

    while !params.stop.load(Ordering::Acquire) {
        match connect_and_serve(&params) {
            Ok(true) => {
                // A credential was armed; loop to serve any retry within this
                // Advise lifecycle (each push uses a fresh single-use session).
            }
            Ok(false) => {}
            Err(detail) => {
                crate::log::debug(&format!("CP pipe attempt ended: {detail}"));
            }
        }
        close_shared_handle(&params.handle_slot);
        if params.stop.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(RECONNECT_BACKOFF);
    }

    if com_owned {
        // SAFETY: balanced with the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
}

/// Connect, verify the server, serve exactly one push, and (on success) arm the
/// autologon and notify LogonUI. Returns `Ok(true)` if a credential was armed.
fn connect_and_serve(params: &WorkerParams) -> Result<bool, String> {
    let conn = connect_verified(&params.handle_slot)?;
    let mut frames = StreamFrames::new(conn);
    let mut session = CredentialSession::generate(now_ms().saturating_add(SESSION_EXPIRY_MS))
        .map_err(|error| format!("generate CP session: {error}"))?;
    // SAFETY: GetCurrentProcessId has no failure mode and no arguments.
    let pid = unsafe { GetCurrentProcessId() };
    let identity = ProviderIdentity {
        clsid: params.clsid.clone(),
        usage: params.scenario,
        pid,
    };

    match provider_serve(
        &mut frames,
        &mut session,
        &identity,
        &now_ms,
        |request_id, payload| arm_and_notify(params, request_id, &payload),
    ) {
        Ok(()) => Ok(true),
        Err(error) => Err(error.to_string()),
    }
}

/// Store the decrypted credential as a one-shot autologon, then signal
/// `CredentialsChanged` outside the fields lock.
fn arm_and_notify(
    params: &WorkerParams,
    request_id: u64,
    payload: &arcen_cp_ipc::CredentialPayload,
) -> Result<(), String> {
    let expires_at_ms = now_ms().saturating_add(ARMED_CREDENTIAL_TTL.as_millis() as u64);

    // A newly accepted push supersedes every earlier pending credential even if
    // native arming setup later fails.
    match params.fields.lock() {
        Ok(mut fields) => fields.clear_autologon(),
        Err(poisoned) => poisoned.into_inner().clear_autologon(),
    }

    let username = payload.username().to_string();
    let password = SecretWide::from_text(payload.password());
    let expiry_fields = Arc::clone(&params.fields);
    let module = match pin_current_module() {
        Ok(module) => module,
        Err(error) => {
            return Err(format!("pin credential-provider module: {error}"));
        }
    };
    let module_raw = module.0 as isize;
    let expiry = std::thread::Builder::new()
        .name("arcen-cp-expiry".to_string())
        .spawn(move || {
            std::thread::sleep(ARMED_CREDENTIAL_TTL);
            let expired = match expiry_fields.lock() {
                Ok(mut fields) => fields.clear_autologon_request(request_id),
                Err(poisoned) => poisoned.into_inner().clear_autologon_request(request_id),
            };
            if expired {
                crate::log::debug("expired an unconsumed broker credential");
            }
            drop(expiry_fields);
            // SAFETY: `module` is the extra reference acquired specifically for
            // this worker. The API releases it and terminates the current thread
            // atomically, so no DLL instruction executes after the final release.
            unsafe { FreeLibraryAndExitThread(HMODULE(module_raw as *mut _), 0) };
        });
    if let Err(error) = expiry {
        // SAFETY: the worker did not start, so this thread still owns the pin.
        let _ = unsafe { FreeLibrary(module) };
        return Err(format!("start credential-expiry worker: {error}"));
    }
    {
        // Hold the lock only long enough to atomically swap in the credential.
        match params.fields.lock() {
            Ok(mut fields) => fields.arm_autologon(username, password, request_id, expires_at_ms),
            Err(poisoned) => {
                poisoned
                    .into_inner()
                    .arm_autologon(username, password, request_id, expires_at_ms)
            }
        }
    }
    // Notify LogonUI *after* releasing the lock so its re-query cannot re-enter
    // us while we hold the fields mutex.
    let notify_result = match params.agile.resolve() {
        Ok(events) => {
            // SAFETY: `events` is a valid apartment-local proxy for the LogonUI
            // callback; the advise context is passed back verbatim.
            unsafe { events.CredentialsChanged(params.context) }
                .map_err(|error| format!("CredentialsChanged: {error}"))
        }
        Err(error) => Err(format!("resolve CP events: {error}")),
    };
    if let Err(error) = notify_result {
        match params.fields.lock() {
            Ok(mut fields) => {
                fields.clear_autologon_request(request_id);
            }
            Err(poisoned) => {
                poisoned.into_inner().clear_autologon_request(request_id);
            }
        }
        return Err(error);
    }
    crate::log::debug(&format!("armed broker credential request={request_id}"));
    Ok(())
}

fn pin_current_module() -> Result<HMODULE, String> {
    let mut module = HMODULE::default();
    let address = arm_and_notify as *const () as *const u16;
    // SAFETY: FROM_ADDRESS interprets the pointer as an address within this DLL,
    // not as a string. Omitting UNCHANGED_REFCOUNT acquires one module reference.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            PCWSTR(address),
            &mut module,
        )
    }
    .map_err(|error| format!("GetModuleHandleExW(FROM_ADDRESS): {error}"))?;
    if module.is_invalid() {
        Err("GetModuleHandleExW returned an invalid module".to_string())
    } else {
        Ok(module)
    }
}

/// Open the broker pipe and confirm the server is SYSTEM. Fails closed.
fn connect_verified(handle_slot: &SharedHandle) -> Result<PipeConn, String> {
    let name = wide(CP_PIPE_NAME);
    // Best-effort wait for a free instance; ignore its result and let CreateFile
    // report the real outcome.
    // SAFETY: `name` is a NUL-terminated wide string.
    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), CONNECT_WAIT_MS) };

    // SAFETY: `name` is NUL-terminated; no security attributes/template handle.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ_WRITE,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            // SECURITY_IDENTIFICATION caps what the pipe server may do with
            // this connection's token: it can learn who we are and check our
            // group membership, but it cannot impersonate us.
            //
            // This client runs inside LogonUI.exe as SYSTEM. Without the flag,
            // Windows offers the connection at SecurityImpersonation, so
            // whichever process owns the pipe name can call
            // ImpersonateNamedPipeClient and act as SYSTEM — the classic
            // named-pipe impersonation escalation. The name is only guaranteed
            // to be ours while the broker holds it, and the DLL stays
            // registered across service stops, crashes and upgrades, so the
            // window is real.
            //
            // verify_server below does not close this on its own: it runs after
            // CreateFileW returns, and by then the impersonation context
            // already exists. A verdict reached afterwards cannot take back a
            // token that has already been handed over. This flag is what
            // prevents it being handed over at all, and costs nothing, because
            // we never want the broker to impersonate us.
            FILE_FLAGS_AND_ATTRIBUTES(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0),
            None,
        )
    }
    .map_err(|error| format!("open broker pipe: {error}"))?;
    if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
        return Err("broker pipe handle is invalid".to_string());
    }

    if let Err(error) = verify_server(handle) {
        // SAFETY: the just-opened handle is owned here and closed exactly once.
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(error);
    }

    *handle_slot.lock().expect("cp handle slot") = Some(handle.0 as isize);
    Ok(PipeConn {
        slot: Arc::clone(handle_slot),
    })
}

/// Confirm the pipe server process is running as SYSTEM (mandatory) and, if the
/// image path is resolvable, that it is the configured broker (best-effort).
fn verify_server(pipe: HANDLE) -> Result<(), String> {
    let mut server_pid = 0u32;
    // SAFETY: pipe is a live client handle; server_pid is a valid out-param.
    unsafe { GetNamedPipeServerProcessId(pipe, &mut server_pid) }
        .map_err(|error| format!("GetNamedPipeServerProcessId: {error}"))?;

    // SAFETY: pid came from the kernel; limited-information access suffices.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, server_pid) }
        .map_err(|error| format!("OpenProcess(server pid {server_pid}): {error}"))?;
    let process = OwnedHandle(process);

    if !process_is_system(process.0)? {
        return Err("pipe server is not running as SYSTEM".to_string());
    }
    // Image check is advisory: a renamed broker binary must not lock out a
    // SYSTEM-verified server, but a mismatch is logged.
    match query_image_name(process.0) {
        Ok(image) if !image_basename_matches(&image, EXPECTED_BROKER_IMAGE) => {
            crate::log::debug(&format!(
                "pipe server image {image:?} differs from expected broker; proceeding on SYSTEM check"
            ));
        }
        Ok(_) => {}
        Err(error) => crate::log::debug(&format!("could not read server image: {error}")),
    }
    Ok(())
}

fn process_is_system(process: HANDLE) -> Result<bool, String> {
    let mut token = HANDLE::default();
    // SAFETY: process is a live handle; token is a valid out-param.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .map_err(|error| format!("OpenProcessToken: {error}"))?;
    let token = OwnedHandle(token);

    let mut bytes = 0u32;
    // SAFETY: sizing call with no output buffer.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes) };
    if bytes < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err("GetTokenInformation(TokenUser) sizing failed".to_string());
    }
    let mut storage = vec![0u8; bytes as usize];
    // SAFETY: storage has at least the sized byte count and holds a TOKEN_USER.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )
    }
    .map_err(|error| format!("GetTokenInformation(TokenUser): {error}"))?;
    // SAFETY: storage holds a TOKEN_USER whose Sid points within it.
    let user_sid = unsafe { (*(storage.as_ptr().cast::<TOKEN_USER>())).User.Sid };

    let mut system = vec![0u8; 128];
    let mut system_len = system.len() as u32;
    // SAFETY: system buffer/len are valid out-params for the well-known SID.
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            PSID(system.as_mut_ptr().cast()),
            &mut system_len,
        )
    }
    .map_err(|error| format!("CreateWellKnownSid(SYSTEM): {error}"))?;

    // SAFETY: both SIDs are valid and live for the comparison.
    Ok(unsafe { EqualSid(user_sid, PSID(system.as_ptr() as *mut _)) }.is_ok())
}

fn query_image_name(process: HANDLE) -> Result<String, String> {
    let mut buffer = vec![0u16; 1024];
    let mut size = buffer.len() as u32;
    // SAFETY: process is a live handle; buffer/size are valid out-params.
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    }
    .map_err(|error| format!("QueryFullProcessImageNameW: {error}"))?;
    Ok(String::from_utf16_lossy(&buffer[..size as usize]))
}

/// A blocking, closable byte stream over the broker pipe handle.
struct PipeConn {
    slot: SharedHandle,
}

impl PipeConn {
    fn handle(&self) -> std::io::Result<HANDLE> {
        self.slot
            .lock()
            .expect("cp handle slot")
            .map(|value| HANDLE(value as *mut _))
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe closed"))
    }
}

impl Read for PipeConn {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let handle = self.handle()?;
        let mut read = 0u32;
        // SAFETY: handle is a live pipe; buffer/read are valid for the call.
        unsafe { ReadFile(handle, Some(buffer), Some(&mut read), None) }.map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string())
        })?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "pipe closed",
            ));
        }
        Ok(read as usize)
    }
}

impl Write for PipeConn {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let handle = self.handle()?;
        let mut written = 0u32;
        // SAFETY: handle is a live pipe; buffer/written are valid for the call.
        unsafe { WriteFile(handle, Some(buffer), Some(&mut written), None) }.map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, error.to_string())
        })?;
        Ok(written as usize)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for PipeConn {
    fn drop(&mut self) {
        close_shared_handle(&self.slot);
    }
}

/// A plain owned kernel HANDLE closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: this guard uniquely owns the handle.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Take and close the shared pipe handle if it is still open. Idempotent.
fn close_shared_handle(slot: &SharedHandle) {
    let value = slot.lock().expect("cp handle slot").take();
    if let Some(value) = value {
        // SAFETY: exactly one caller takes the Some(value); the handle is closed once.
        unsafe {
            let _ = CloseHandle(HANDLE(value as *mut _));
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
