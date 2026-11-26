// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Display recovery outside a Deck session: at Pier start and when the
//! console changes hands.
//!
//! The display recovery journal is replayed by the session's own lease, by
//! its out-of-process watchdog, or by the next session's display acquire.
//! Some exits leave it behind anyway — a sign-out ends the agent and its
//! watchdog together, power is lost, the watchdog itself fails — and until
//! the next Deck connected the console kept the session's topology: the
//! physical monitor dark, a virtual display primary, every window on it.
//!
//! The service runs in session 0, where `SetDisplayConfig` is refused, so it
//! starts `arcen-pier restore-display --console` as LocalSystem in the active
//! console session; that process attaches to the console's input desktop,
//! replays the journal and moves stranded windows when the desktop is the
//! user's. When the shared policy says a session owns the record
//! (`display_lease_action`), nothing runs. A failed attempt (a locked
//! console refuses display changes) leaves the journal for the next console
//! change.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use arcen_outputs::display_lease::{display_lease_action, DisplayLeaseAction, DisplayLeaseEvent};

use crate::logging::DISPLAY;

/// Long enough for the crash watchdog of a killed Pier to finish its own
/// restore first; replaying the same journal twice is harmless but noisy.
const STARTUP_DELAY: Duration = Duration::from_secs(15);
/// Lets a sign-in or unlock settle before display changes are attempted.
const CONSOLE_CHANGE_DELAY: Duration = Duration::from_secs(5);

/// Keeps a console recovery and a session's agent apart: the recovery holds
/// it for the whole life of its child, a session waits for it before it
/// starts an agent. Both act on the one display journal and the window
/// record beside it; nothing else orders them across processes.
fn gate() -> Arc<tokio::sync::RwLock<()>> {
    static GATE: OnceLock<Arc<tokio::sync::RwLock<()>>> = OnceLock::new();
    Arc::clone(GATE.get_or_init(|| Arc::new(tokio::sync::RwLock::new(()))))
}

/// Waits until no console recovery is running. Called before a session
/// starts its agent.
pub(crate) async fn wait_until_idle() {
    let gate = gate();
    let _idle = gate.read().await;
}

pub(crate) struct ConsoleDisplayRecovery {
    journal: PathBuf,
    due: Option<(Instant, DisplayLeaseEvent)>,
    running: Option<tokio::task::JoinHandle<()>>,
}

impl ConsoleDisplayRecovery {
    pub(crate) fn new(journal: PathBuf) -> Self {
        Self {
            journal,
            due: None,
            running: None,
        }
    }

    /// Schedules a check for a pending journal after `event`.
    pub(crate) fn request(&mut self, event: DisplayLeaseEvent) {
        let delay = match event {
            DisplayLeaseEvent::PierStarted => STARTUP_DELAY,
            _ => CONSOLE_CHANGE_DELAY,
        };
        let due = Instant::now() + delay;
        self.due = Some(match self.due {
            Some((earlier, _)) if earlier > due => (due, event),
            Some(existing) => existing,
            None => (due, event),
        });
    }

    /// Runs a due check. `session_live` says whether any session task is
    /// still running; it owns its journal then.
    pub(crate) fn tick(&mut self, session_live: bool) {
        let Some((due, event)) = self.due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| !running.is_finished())
        {
            return;
        }
        if display_lease_action(event, session_live) != DisplayLeaseAction::RecoverPending {
            self.due = None;
            return;
        }
        if !self.journal.exists() {
            self.due = None;
            return;
        }
        // A session between its start and its agent holds the gate briefly;
        // try again on the next tick.
        let Ok(exclusive) = gate().try_write_owned() else {
            return;
        };
        self.due = None;
        tracing::warn!(
            target: DISPLAY,
            journal = %self.journal.display(),
            ?event,
            "a display recovery journal outlived its session; restoring in the console session"
        );
        self.running = Some(tokio::task::spawn_blocking(move || {
            let _exclusive = exclusive;
            match platform::run_in_console() {
                Ok(code) if code == 0 => tracing::info!(
                    target: DISPLAY,
                    "console display recovery finished"
                ),
                Ok(code) => tracing::warn!(
                    target: DISPLAY,
                    exit_code = code,
                    "console display recovery did not restore; the journal waits for the next console change"
                ),
                Err(error) => tracing::warn!(
                    target: DISPLAY,
                    %error,
                    "console display recovery could not start"
                ),
            }
        }));
    }
}

/// Attaches the calling thread to the console's input desktop, for the
/// `restore-display --console` process. Returns the desktop's name
/// (`Default` once a user is signed in, `Winlogon` at the sign-in screen).
pub(crate) fn attach_input_desktop() -> Result<String, String> {
    platform::attach_input_desktop()
}

/// What [`restore_behind_lock`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BehindLock {
    /// No journal was left: the session agent or its watchdog restored.
    NothingLeft,
    /// The console recovery replayed the journal behind the lock screen.
    Restored,
    /// It ran and did not restore (exit code); the journal waits for the
    /// next console change.
    NotRestored(u32),
    /// It could not run.
    Failed(String),
}

/// Gives the desk its displays back behind a lock screen that is already
/// up (ADR 0025 §4, the lock-first hand-back): a locked session's own
/// processes may not change its displays, so a journal the session agent
/// left is replayed now by `restore-display --console`, LocalSystem on the
/// console's input desktop (the secure desktop while the session is
/// locked), rather than at the next console change.
pub(crate) async fn restore_behind_lock(journal: PathBuf) -> BehindLock {
    if !journal.exists() {
        return BehindLock::NothingLeft;
    }
    let exclusive = gate().write_owned().await;
    tokio::task::spawn_blocking(move || {
        let _exclusive = exclusive;
        if !journal.exists() {
            return BehindLock::NothingLeft;
        }
        match platform::run_in_console() {
            Ok(0) => BehindLock::Restored,
            Ok(code) => BehindLock::NotRestored(code),
            Err(error) => BehindLock::Failed(error),
        }
    })
    .await
    .unwrap_or_else(|_| BehindLock::Failed("console display recovery task failed".to_string()))
}

#[cfg(windows)]
mod platform {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId,
        TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_SESSIONID, TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY,
        TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    use windows::Win32::System::StationsAndDesktops::{
        GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
        DESKTOP_CONTROL_FLAGS, UOI_NAME,
    };
    use windows::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
        TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
    };

    const NO_CONSOLE_SESSION: u32 = u32::MAX;
    const RECOVERY_TIMEOUT_MS: u32 = 120_000;

    fn own(handle: HANDLE) -> OwnedHandle {
        // SAFETY: callers pass a newly created, uniquely owned handle.
        unsafe { OwnedHandle::from_raw_handle(handle.0) }
    }

    fn raw(handle: &OwnedHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle())
    }

    fn quote(argument: &str) -> String {
        if argument.contains([' ', '\t', '"']) {
            format!("\"{}\"", argument.replace('"', "\\\""))
        } else {
            argument.to_string()
        }
    }

    pub(super) fn run_in_console() -> Result<u32, String> {
        // SAFETY: no arguments, no failure side effects.
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == NO_CONSOLE_SESSION {
            return Err("Windows has no active console session".to_string());
        }
        let mut own_token = HANDLE::default();
        // SAFETY: the pseudo-handle names this process; the out-parameter is valid.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_DUPLICATE
                    | TOKEN_QUERY
                    | TOKEN_ASSIGN_PRIMARY
                    | TOKEN_ADJUST_DEFAULT
                    | TOKEN_ADJUST_SESSIONID,
                &mut own_token,
            )
        }
        .map_err(|error| format!("OpenProcessToken: {error}"))?;
        let own_token = own(own_token);
        let mut primary = HANDLE::default();
        // SAFETY: the source token is live; the output is a valid out-parameter.
        unsafe {
            DuplicateTokenEx(
                raw(&own_token),
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            )
        }
        .map_err(|error| format!("DuplicateTokenEx: {error}"))?;
        let primary = own(primary);
        // SAFETY: a LocalSystem service holds SeTcbPrivilege, which moving a
        // primary token to another session requires; the value is a u32.
        unsafe {
            SetTokenInformation(
                raw(&primary),
                TokenSessionId,
                (&raw const session).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        }
        .map_err(|error| format!("SetTokenInformation(TokenSessionId={session}): {error}"))?;

        let executable = std::env::current_exe()
            .map_err(|error| format!("resolve the Pier executable: {error}"))?;
        let executable = executable
            .to_str()
            .ok_or_else(|| "the Pier executable path is not valid UTF-8".to_string())?;
        let journal = crate::recovery::default_path();
        let journal = journal
            .to_str()
            .ok_or_else(|| "the display journal path is not valid UTF-8".to_string())?;
        let command = format!(
            "{} restore-display --console --journal {}",
            quote(executable),
            quote(journal)
        );
        let executable_wide: Vec<u16> = executable.encode_utf16().chain([0]).collect();
        let mut command_wide: Vec<u16> = command.encode_utf16().chain([0]).collect();
        let mut desktop: Vec<u16> = r"winsta0\default".encode_utf16().chain([0]).collect();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: every buffer is NUL-terminated and outlives the call; no
        // handle is inherited.
        unsafe {
            CreateProcessAsUserW(
                raw(&primary),
                PCWSTR(executable_wide.as_ptr()),
                PWSTR(command_wide.as_mut_ptr()),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup,
                &mut process,
            )
        }
        .map_err(|error| format!("CreateProcessAsUserW(console display recovery): {error}"))?;
        let child = own(process.hProcess);
        drop(own(process.hThread));
        tracing::info!(
            target: crate::logging::DISPLAY,
            windows_session_id = session,
            pid = process.dwProcessId,
            "console display recovery started"
        );
        // SAFETY: the process handle is live.
        if unsafe { WaitForSingleObject(raw(&child), RECOVERY_TIMEOUT_MS) } != WAIT_OBJECT_0 {
            // SAFETY: the handle is live; a recovery that hangs must not linger.
            unsafe {
                let _ = TerminateProcess(raw(&child), 1);
            }
            return Err("console display recovery timed out".to_string());
        }
        let mut code = 0u32;
        // SAFETY: the handle is live and the out-parameter is valid.
        unsafe { GetExitCodeProcess(raw(&child), &mut code) }
            .map_err(|error| format!("GetExitCodeProcess: {error}"))?;
        Ok(code)
    }

    pub(super) fn attach_input_desktop() -> Result<String, String> {
        // SAFETY: plain desktop calls on this thread, before it owns windows.
        let desktop = unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
            )
        }
        .map_err(|error| format!("OpenInputDesktop: {error}"))?;
        // SAFETY: `desktop` is a live desktop handle; it stays open for the
        // process lifetime, as a thread's desktop must.
        unsafe { SetThreadDesktop(desktop) }
            .map_err(|error| format!("SetThreadDesktop: {error}"))?;
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        // SAFETY: the buffer and its byte length match.
        unsafe {
            GetUserObjectInformationW(
                HANDLE(desktop.0),
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                std::mem::size_of_val(&name) as u32,
                Some(&mut needed),
            )
        }
        .map_err(|error| format!("GetUserObjectInformationW(UOI_NAME): {error}"))?;
        let length = name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(name.len());
        Ok(String::from_utf16_lossy(&name[..length]))
    }
}

#[cfg(not(windows))]
mod platform {
    pub(super) fn run_in_console() -> Result<u32, String> {
        Err("console display recovery is only available on Windows".to_string())
    }

    pub(super) fn attach_input_desktop() -> Result<String, String> {
        Err("input desktops are only available on Windows".to_string())
    }
}
