// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session locked when the broker serving a Deck dies (ADR 0025 §4,
//! GAP-047). The decisions are shared
//! ([`arcen_session::console_session::owner_death`]); this is the adapter.
//!
//! The broker locks the session when the Deck leaves (`hand_back_session`).
//! If the broker itself dies (killed, crashed, an installer stopping it),
//! closing its last handle to the agent's job ends the agent, the agent's
//! display watchdog gives the desk its monitors back, and nothing locked the
//! session: it stayed unlocked on the console until the service started
//! again, and longer, because a restarted broker locked nothing either.
//! Two mechanisms now cover this, and both act only on the exact session
//! the Deck was attached to (its WTS session id, its logon and its account's
//! SID; a session id Windows gave to a later logon is another session):
//!
//! - **The session guard.** For each attachment the broker starts
//!   `session-guard`: the same binary, as LocalSystem in session 0, outside
//!   the agent's job, reading a pipe only the broker can write. The pipe
//!   closes however the broker dies. A pipe that closes without
//!   [`GUARD_RELEASE`] means the broker did not hand the session back, and
//!   the guard does: it waits, as the live hand-back does, for the display
//!   watchdog to give the desk its monitors back (a locked session's
//!   processes may not change its displays), then locks the session
//!   (`windows_session::lock_session`: `LockWorkStation` as the user,
//!   disconnected from the console if it will not lock). This needs no
//!   restarted service.
//! - **The attachment record** (`recovery\deck-attachment.record`, which
//!   only the service can write): it names the session, its logon, this
//!   boot (a random identifier kept in a volatile registry key, gone at the
//!   next boot) and the SID. A broker that starts and finds a record whose
//!   session is still alive locks it before it gives the desk its speakers
//!   or anything else back. This covers a guard that was stopped with the
//!   broker or never started.
//!
//! Not covered: between the broker's death and the lock (the display
//! watchdog's restore, then the lock, a few seconds) the session is on the
//! desk unlocked, as in every hand-back on Windows; the desk's speakers
//! stay silent until the service runs again.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arcen_session::console_session::owner_death::{
    guard_end, lock_after_owner, service_startup_recovery, AttachmentRecord, GuardEnd,
    OwnerGoneHost, OwnerGoneOutcome, RecordAccount, ServiceStartupRecovery, GUARD_ARMED,
    GUARD_READ_LIMIT,
};

use crate::logging::SESSION;
use crate::windows_session::{LockOutcome, SessionOwner};

/// The broker's subcommand that runs a session guard.
pub(crate) const SUBCOMMAND: &str = "session-guard";
/// How long a session guard is given to report itself armed.
const ARMED_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a released guard is given to exit before it is stopped.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a starting broker waits for a display watchdog still restoring
/// the desk before it locks.
const STARTUP_DISPLAY_WAIT: Duration = Duration::from_secs(5);
/// The largest record read; anything longer is unreadable.
const MAX_RECORD_BYTES: u64 = 1024;

/// Where the attachment record lives: beside the service's other recovery
/// journals, which the agent and the user cannot write.
pub(crate) fn record_path() -> PathBuf {
    crate::paths::recovery_dir().join("deck-attachment.record")
}

/// How the record names a WTS session and its logon.
fn login_session_token(session_id: u32, logon_id: u64) -> String {
    format!("wts{session_id}-logon-{logon_id:016x}")
}

/// The WTS session id a record's login session names.
fn recorded_session_id(login_session: &str) -> Option<u32> {
    let (session_id, logon) = login_session.strip_prefix("wts")?.split_once("-logon-")?;
    (logon.len() == 16 && u64::from_str_radix(logon, 16).is_ok())
        .then(|| session_id.parse().ok())
        .flatten()
}

/// `session_id` and its owner as a record describes them, in boot `boot`.
fn record_for(session_id: u32, owner: &SessionOwner, boot: &str) -> Option<AttachmentRecord> {
    AttachmentRecord::for_account(
        &login_session_token(session_id, owner.logon_id),
        boot,
        RecordAccount::sid(&owner.sid)?,
    )
}

fn read_record(path: &Path) -> Result<Option<String>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    // Bytes that are not text are an unreadable record, not a missing one.
    Ok(Some(String::from_utf8(bytes).unwrap_or_default()))
}

/// Removes the record when it still names `session` (another attachment's
/// record is left alone).
fn remove_record_naming(path: &Path, session: &AttachmentRecord) {
    let names_it = read_record(path).ok().flatten().is_some_and(|text| {
        AttachmentRecord::parse(&text).is_some_and(|stored| {
            stored.login_session == session.login_session && stored.account == session.account
        })
    });
    if names_it {
        if let Err(error) = crate::recovery::remove_file(path, "deck attachment record") {
            tracing::warn!(target: SESSION, %error, "the attachment record could not be removed");
        }
    }
}

/// Waits, bounded, until the display watchdog has given the desk its
/// monitors back: Windows refuses that once the session is locked.
fn wait_for_display_restore(wait: Duration) {
    let journal = crate::recovery::default_path();
    let deadline = Instant::now() + wait;
    while journal.exists() {
        if Instant::now() >= deadline {
            tracing::warn!(
                target: SESSION,
                journal = %journal.display(),
                "the display journal is still there; the session is locked and its displays are restored at the next unlock"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The guard over one Deck's attachment, held by the broker from the
/// attachment to the end of its own hand-back.
///
/// Released, the guard exits without acting and the record goes. Dropped
/// without a release, it closes the pipe, so the guard locks: always while
/// a panic unwinds, and otherwise when [`Self::set_lock_on_drop`] says an
/// unfinished attachment must leave the session locked (once the agent
/// runs, or when the sign-in unlocked the session). A refusal before that
/// leaves the session as the desk had it and releases the guard.
pub(crate) struct OwnerGuard {
    record: Option<(PathBuf, AttachmentRecord)>,
    guard: Option<platform::GuardProcess>,
    lock_on_drop: bool,
}

impl OwnerGuard {
    /// Records the attachment of `session_id`, which must belong to `sid`,
    /// and starts its guard. Failures are logged and leave whichever half
    /// did start: the record alone still has a restarted broker lock the
    /// session. Blocks for up to [`ARMED_TIMEOUT`].
    pub(crate) fn arm(session_id: u32, sid: &str, lock_on_drop: bool) -> Self {
        let mut armed = Self {
            record: None,
            guard: None,
            lock_on_drop,
        };
        let owner = match crate::windows_session::session_owner(session_id) {
            Ok(Some(owner)) if owner.sid.eq_ignore_ascii_case(sid) => owner,
            Ok(_) => {
                tracing::error!(
                    target: SESSION,
                    windows_session_id = session_id,
                    "the attached session is not the Deck's account's; nothing guards it if the broker dies"
                );
                return armed;
            }
            Err(error) => {
                tracing::error!(
                    target: SESSION,
                    windows_session_id = session_id,
                    %error,
                    "the attached session's logon could not be read; nothing guards it if the broker dies"
                );
                return armed;
            }
        };
        match platform::boot_session()
            .and_then(|boot| {
                record_for(session_id, &owner, &boot)
                    .ok_or_else(|| "the session cannot be described as a record".to_string())
            })
            .and_then(|record| {
                let path = record_path();
                crate::recovery::write_atomic_bytes(
                    &path,
                    record.to_text().as_bytes(),
                    MAX_RECORD_BYTES,
                    "deck attachment record",
                )
                .map(|()| (path, record))
            }) {
            Ok(record) => armed.record = Some(record),
            Err(error) => tracing::error!(
                target: SESSION,
                windows_session_id = session_id,
                %error,
                "the Deck's attachment could not be recorded for a restarted broker"
            ),
        }
        match platform::start_guard(session_id, &owner) {
            Ok(guard) => {
                tracing::info!(
                    target: SESSION,
                    windows_session_id = session_id,
                    guard_pid = guard.pid,
                    "the session locks if this broker dies while the Deck is attached"
                );
                armed.guard = Some(guard);
            }
            Err(error) => tracing::error!(
                target: SESSION,
                windows_session_id = session_id,
                %error,
                "the session guard did not start; a restarted broker locks the session instead"
            ),
        }
        armed
    }

    /// Whether dropping the guard without [`Self::release`] leaves the
    /// session to be locked.
    pub(crate) fn set_lock_on_drop(&mut self, lock_on_drop: bool) {
        self.lock_on_drop = lock_on_drop;
    }

    /// The broker handed the session back itself: the guard exits without
    /// acting and the record goes.
    pub(crate) fn release(mut self) {
        self.lock_on_drop = false;
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        if self.lock_on_drop || std::thread::panicking() {
            // The pipe closes without a release and the guard locks; the
            // record stays for a restarted broker.
            if let Some(guard) = self.guard.take() {
                guard.abandon();
            }
            return;
        }
        if let Some(guard) = self.guard.take() {
            guard.release();
        }
        if let Some((path, record)) = self.record.take() {
            remove_record_naming(&path, &record);
        }
    }
}

/// What the session guard and a restarted broker do to a WTS session.
struct WtsOwnerGoneHost {
    session_id: u32,
    owner: SessionOwner,
    elsewhere: bool,
    started: Instant,
}

impl WtsOwnerGoneHost {
    fn new(session_id: u32, owner: SessionOwner) -> Self {
        Self {
            session_id,
            owner,
            elsewhere: false,
            started: Instant::now(),
        }
    }
}

impl OwnerGoneHost for WtsOwnerGoneHost {
    /// Whether the session is still the attached logon of the account. A
    /// reading that fails is not proof of another logon: the lock checks
    /// again.
    fn console_is_ours(&mut self) -> bool {
        if self.elsewhere {
            return false;
        }
        match crate::windows_session::session_owner(self.session_id) {
            Ok(Some(owner)) => owner == self.owner,
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(target: SESSION, %error, "the guarded session could not be read");
                true
            }
        }
    }

    // The desk's monitors are the display watchdog's (`wait_for_display_restore`).
    fn keep_private(&mut self) {}

    fn lock(&mut self) -> bool {
        match crate::windows_session::lock_session(self.session_id, &self.owner.sid) {
            Ok(LockOutcome::Elsewhere) => {
                self.elsewhere = true;
                false
            }
            Ok(outcome) => {
                tracing::info!(
                    target: SESSION,
                    windows_session_id = self.session_id,
                    outcome = outcome.token(),
                    "the session is secured, not ended"
                );
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: SESSION,
                    windows_session_id = self.session_id,
                    %error,
                    "the session could not be locked"
                );
                false
            }
        }
    }

    fn give_back(&mut self) {}

    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

fn outcome_token(outcome: OwnerGoneOutcome) -> &'static str {
    match outcome {
        OwnerGoneOutcome::Locked => "locked",
        OwnerGoneOutcome::NotOurs => "not_ours",
        OwnerGoneOutcome::GaveBackUnlocked => "gave_back_unlocked",
    }
}

/// Locks the session a broker died attached to: the displays first, as the
/// live hand-back, then the lock. Removes the record unless the session
/// could not be secured.
fn lock_left_session(
    session_id: u32,
    owner: SessionOwner,
    record: &AttachmentRecord,
    display_wait: Duration,
) {
    wait_for_display_restore(display_wait);
    let outcome = lock_after_owner(&mut WtsOwnerGoneHost::new(session_id, owner), false);
    if outcome == OwnerGoneOutcome::GaveBackUnlocked {
        tracing::error!(
            target: SESSION,
            windows_session_id = session_id,
            departure = "agent_failed",
            outcome = outcome_token(outcome),
            "the session left by a dead broker would not lock; its record is kept"
        );
        return;
    }
    tracing::warn!(
        target: SESSION,
        windows_session_id = session_id,
        departure = "agent_failed",
        outcome = outcome_token(outcome),
        "the session left by a dead broker is handed back"
    );
    remove_record_naming(&record_path(), record);
}

/// The arguments of `session-guard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardArguments {
    session_id: u32,
    owner: SessionOwner,
}

/// Parses `--session-id N --sid SID --logon HEX`.
pub(crate) fn parse_arguments(
    arguments: &mut impl Iterator<Item = String>,
) -> Result<GuardArguments, String> {
    let mut session_id = None;
    let mut sid = None;
    let mut logon_id = None;
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))
        };
        match argument.as_str() {
            "--session-id" => {
                session_id = Some(
                    value()?
                        .parse::<u32>()
                        .map_err(|_| "invalid --session-id".to_string())?,
                );
            }
            "--sid" => {
                let text = value()?;
                RecordAccount::sid(&text).ok_or_else(|| "invalid --sid".to_string())?;
                sid = Some(text);
            }
            "--logon" => {
                logon_id = Some(
                    u64::from_str_radix(&value()?, 16)
                        .map_err(|_| "invalid --logon".to_string())?,
                );
            }
            other => return Err(format!("unknown {SUBCOMMAND} argument: {other}")),
        }
    }
    Ok(GuardArguments {
        session_id: session_id.ok_or_else(|| format!("{SUBCOMMAND} requires --session-id"))?,
        owner: SessionOwner {
            sid: sid.ok_or_else(|| format!("{SUBCOMMAND} requires --sid"))?,
            logon_id: logon_id.ok_or_else(|| format!("{SUBCOMMAND} requires --logon"))?,
        },
    })
}

/// Runs `session-guard`: reports itself armed on standard output, waits for
/// the broker's pipe on standard input to close, and locks the session
/// unless the broker released it first.
pub(crate) fn run(arguments: GuardArguments) -> Result<(), String> {
    let GuardArguments { session_id, owner } = arguments;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(GUARD_ARMED)
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("report the guard armed: {error}"))?;
    drop(stdout);
    let mut received = Vec::with_capacity(GUARD_READ_LIMIT);
    let _ = std::io::stdin()
        .lock()
        .take(u64::try_from(GUARD_READ_LIMIT).unwrap_or(u64::MAX))
        .read_to_end(&mut received);
    if guard_end(&received) == GuardEnd::Released {
        return Ok(());
    }
    tracing::warn!(
        target: SESSION,
        windows_session_id = session_id,
        "the broker serving a Deck ended without handing the session back; locking it"
    );
    let record = AttachmentRecord::for_account(
        &login_session_token(session_id, owner.logon_id),
        "guard",
        RecordAccount::Sid(owner.sid.clone()),
    )
    .ok_or_else(|| "the guarded session cannot be described as a record".to_string())?;
    lock_left_session(
        session_id,
        owner,
        &record,
        crate::session::DISPLAY_RESTORE_WAIT,
    );
    Ok(())
}

/// What a broker starting does about a Deck the broker before it was
/// serving: when the record names a session still alive (its logon, its
/// account, this boot), locks it first; then removes the record. Called
/// before the desk's speakers and displays are given back.
pub(crate) fn recover_at_start() -> ServiceStartupRecovery {
    let path = record_path();
    let stored = match read_record(&path) {
        Ok(Some(stored)) => stored,
        Ok(None) => return ServiceStartupRecovery::Nothing,
        Err(error) => {
            tracing::error!(target: SESSION, %error, "the attachment record could not be read; it is kept");
            return ServiceStartupRecovery::Nothing;
        }
    };
    // Only the session the record names is read: another session that
    // cannot be read now (one signing out) must not stop this one's lock.
    let named = AttachmentRecord::parse(&stored).and_then(|record| {
        recorded_session_id(&record.login_session).map(|session_id| (record, session_id))
    });
    let alive = match named {
        None => Ok(Vec::new()),
        Some((_, session_id)) => platform::boot_session().and_then(|boot| {
            crate::windows_session::session_owner(session_id).map(|owner| {
                owner
                    .and_then(|owner| {
                        record_for(session_id, &owner, &boot).map(|live| (session_id, owner, live))
                    })
                    .into_iter()
                    .collect::<Vec<_>>()
            })
        }),
    };
    let alive = match alive {
        Ok(alive) => alive,
        Err(error) => {
            tracing::error!(
                target: SESSION,
                %error,
                "the recorded session could not be read; the attachment record is kept and nothing is locked"
            );
            return ServiceStartupRecovery::Nothing;
        }
    };
    let records: Vec<AttachmentRecord> =
        alive.iter().map(|(_, _, record)| record.clone()).collect();
    let recovery = service_startup_recovery(Some(&stored), &records);
    match &recovery {
        ServiceStartupRecovery::Nothing => {}
        ServiceStartupRecovery::Stale => {
            tracing::info!(
                target: SESSION,
                "the session a Deck was attached to has ended; its record is removed"
            );
            if let Err(error) = crate::recovery::remove_file(&path, "deck attachment record") {
                tracing::warn!(target: SESSION, %error, "the stale attachment record could not be removed");
            }
        }
        ServiceStartupRecovery::Unreadable => {
            let aside = path.with_extension("record.unreadable");
            let moved = std::fs::rename(&path, &aside);
            tracing::error!(
                target: SESSION,
                kept = %aside.display(),
                moved = moved.is_ok(),
                "the attachment record cannot be read and names no session; nothing is locked"
            );
        }
        ServiceStartupRecovery::LockFirst(record) => {
            if let Some((session_id, owner, _)) =
                alive.into_iter().find(|(_, _, live)| live == record)
            {
                tracing::warn!(
                    target: SESSION,
                    windows_session_id = session_id,
                    departure = "agent_failed",
                    "the broker before this one ended while a Deck was attached; locking the session first"
                );
                // A watchdog still restoring the displays gets a moment; one
                // that is gone left the journal to the console recovery,
                // which restores it at the next unlock.
                lock_left_session(session_id, owner, record, STARTUP_DISPLAY_WAIT);
            }
        }
    }
    recovery
}

#[cfg(windows)]
mod platform {
    use std::io::{Read as _, Write as _};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle as _, OwnedHandle};

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        DuplicateHandle, SetHandleInformation, DUPLICATE_SAME_ACCESS, HANDLE, HANDLE_FLAG_INHERIT,
        WAIT_TIMEOUT,
    };
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_LOCAL_MACHINE, KEY_CREATE_SUB_KEY, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_BINARY,
        REG_OPTION_VOLATILE, REG_VALUE_TYPE,
    };
    use windows::Win32::System::Threading::{
        CreateProcessW, GetCurrentProcess, TerminateProcess, WaitForSingleObject,
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, EXTENDED_STARTUPINFO_PRESENT,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    };

    use arcen_session::console_session::owner_death::{GUARD_ARMED, GUARD_RELEASE};

    use super::{ARMED_TIMEOUT, RELEASE_TIMEOUT, SUBCOMMAND};
    use crate::windows_session::SessionOwner;

    fn own(handle: HANDLE) -> OwnedHandle {
        // SAFETY: callers pass a newly created, uniquely owned handle.
        unsafe { OwnedHandle::from_raw_handle(handle.0) }
    }

    fn raw(handle: &OwnedHandle) -> HANDLE {
        HANDLE(handle.as_raw_handle())
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain([0]).collect()
    }

    /// The service's key, under which the boot identifier lives in a
    /// volatile subkey: Windows keeps it in memory only, so it is gone at
    /// the next boot, and it goes with the service when it is deleted.
    const SERVICE_KEY: &str = r"SYSTEM\CurrentControlSet\Services\ArcenPier";
    const BOOT_SUBKEY: &str = "ArcenBootSession";
    const BOOT_VALUE: &str = "Id";

    struct Key(HKEY);

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the key was opened by this wrapper and is closed once.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }

    /// This boot's identifier: 128 random bits in a volatile registry value,
    /// written by the first broker of the boot. Never a boot time, which a
    /// clock step moves.
    pub(super) fn boot_session() -> Result<String, String> {
        static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        if let Some(boot) = BOOT.get() {
            return Ok(boot.clone());
        }
        let boot = read_or_create_boot_session()?;
        Ok(BOOT.get_or_init(|| boot).clone())
    }

    fn read_or_create_boot_session() -> Result<String, String> {
        let service = wide(SERVICE_KEY);
        let mut parent = HKEY::default();
        // SAFETY: the subkey is NUL-terminated and outlives the call; the
        // out-parameter is valid.
        unsafe {
            RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(service.as_ptr()),
                0,
                KEY_CREATE_SUB_KEY,
                &mut parent,
            )
        }
        .ok()
        .map_err(|error| format!("open the service key {SERVICE_KEY}: {error}"))?;
        let parent = Key(parent);
        let subkey = wide(BOOT_SUBKEY);
        let mut key = HKEY::default();
        // SAFETY: as above; REG_OPTION_VOLATILE keeps the key in memory only.
        unsafe {
            RegCreateKeyExW(
                parent.0,
                PCWSTR(subkey.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .map_err(|error| format!("create the volatile boot key: {error}"))?;
        let key = Key(key);
        let value = wide(BOOT_VALUE);
        let mut stored = [0u8; 16];
        let mut size = stored.len() as u32;
        let mut kind = REG_VALUE_TYPE::default();
        // SAFETY: the buffer and its size describe live storage.
        let read = unsafe {
            RegQueryValueExW(
                key.0,
                PCWSTR(value.as_ptr()),
                None,
                Some(&mut kind),
                Some(stored.as_mut_ptr()),
                Some(&mut size),
            )
        };
        if read.is_ok() && kind == REG_BINARY && size as usize == stored.len() {
            return Ok(hex(&stored));
        }
        let mut fresh = [0u8; 16];
        getrandom::getrandom(&mut fresh)
            .map_err(|error| format!("draw a boot identifier: {error}"))?;
        // SAFETY: the key is open for writing and the data slice is live.
        unsafe { RegSetValueExW(key.0, PCWSTR(value.as_ptr()), 0, REG_BINARY, Some(&fresh)) }
            .ok()
            .map_err(|error| format!("write the boot identifier: {error}"))?;
        Ok(hex(&fresh))
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A running session guard: its process and the broker's end of its
    /// pipe, which nothing else holds.
    pub(super) struct GuardProcess {
        process: OwnedHandle,
        release: std::fs::File,
        pub(super) pid: u32,
    }

    impl GuardProcess {
        /// Tells the guard the session was handed back and waits, off the
        /// caller's thread, for it to exit; stops one that does not.
        pub(super) fn release(self) {
            let Self {
                process,
                mut release,
                ..
            } = self;
            let _ = release
                .write_all(GUARD_RELEASE)
                .and_then(|()| release.flush());
            drop(release);
            let _ = std::thread::Builder::new()
                .name("arcen-session-guard-reap".to_owned())
                .spawn(move || {
                    // SAFETY: the process handle is live and owned here.
                    let wait = unsafe {
                        WaitForSingleObject(raw(&process), RELEASE_TIMEOUT.as_millis() as u32)
                    };
                    if wait == WAIT_TIMEOUT {
                        // SAFETY: as above; a released guard must not linger.
                        unsafe {
                            let _ = TerminateProcess(raw(&process), 1);
                        }
                    }
                });
        }

        /// Closes the pipe without a release: the guard locks the session.
        pub(super) fn abandon(self) {}
    }

    fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        // SAFETY: both outputs are valid; no security attributes, so neither
        // end is inheritable until marked so.
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }
            .map_err(|error| format!("CreatePipe(session guard): {error}"))?;
        Ok((own(read), own(write)))
    }

    fn make_inheritable(handle: &OwnedHandle) -> Result<(), String> {
        // SAFETY: the handle is live and owned by the caller.
        unsafe { SetHandleInformation(raw(handle), HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .map_err(|error| format!("SetHandleInformation(session guard): {error}"))
    }

    /// An inheritable copy of this process's standard error (the service's
    /// log), so the guard's own lines land beside the broker's.
    fn inheritable_stderr() -> Option<OwnedHandle> {
        // SAFETY: reads the process standard-handle table without ownership.
        let stderr = unsafe { GetStdHandle(STD_ERROR_HANDLE) }.ok()?;
        if stderr.is_invalid() || stderr.0.is_null() {
            return None;
        }
        // SAFETY: the pseudo-handle names this process.
        let process = unsafe { GetCurrentProcess() };
        let mut copy = HANDLE::default();
        // SAFETY: source and target are this process; the output is valid.
        unsafe {
            DuplicateHandle(
                process,
                stderr,
                process,
                &mut copy,
                0,
                true,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .ok()?;
        Some(own(copy))
    }

    /// Starts `session-guard` as this process's account (LocalSystem) in
    /// session 0, outside any job, inheriting only its two pipe ends and
    /// standard error, and waits until it reports itself armed.
    pub(super) fn start_guard(
        session_id: u32,
        owner: &SessionOwner,
    ) -> Result<GuardProcess, String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("resolve the Pier executable: {error}"))?;
        let executable = executable
            .to_str()
            .ok_or_else(|| "the Pier executable path is not valid UTF-8".to_string())?
            .to_owned();
        let (release_read, release_write) = pipe()?;
        let (armed_read, armed_write) = pipe()?;
        make_inheritable(&release_read)?;
        make_inheritable(&armed_write)?;
        let stderr = inheritable_stderr();
        let mut inherited = vec![raw(&release_read), raw(&armed_write)];
        if let Some(stderr) = &stderr {
            inherited.push(raw(stderr));
        }
        let attributes = crate::windows_session::inherited_handle_list(inherited)?;
        let command_line = format!(
            "{} {SUBCOMMAND} --session-id {session_id} --sid {} --logon {:016x}",
            crate::windows_session::quote_windows_argument(&executable),
            crate::windows_session::quote_windows_argument(&owner.sid),
            owner.logon_id,
        );
        let executable_wide = wide(&executable);
        let startup = STARTUPINFOEXW {
            StartupInfo: STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: raw(&release_read),
                hStdOutput: raw(&armed_write),
                hStdError: stderr.as_ref().map_or(HANDLE::default(), raw),
                ..Default::default()
            },
            lpAttributeList: attributes.raw,
        };
        let create = |flags| {
            let mut command_wide = wide(&command_line);
            let mut process_info = PROCESS_INFORMATION::default();
            // SAFETY: every buffer is NUL-terminated and outlives the call;
            // the attribute list restricts inheritance to the listed handles.
            unsafe {
                CreateProcessW(
                    PCWSTR(executable_wide.as_ptr()),
                    PWSTR(command_wide.as_mut_ptr()),
                    None,
                    None,
                    true,
                    flags,
                    None,
                    PCWSTR::null(),
                    &startup.StartupInfo,
                    &mut process_info,
                )
            }?;
            Ok::<_, windows::core::Error>(process_info)
        };
        let base = EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW;
        let process_info = match create(base | CREATE_BREAKAWAY_FROM_JOB) {
            Ok(process_info) => process_info,
            Err(error) if error.code().0 as u32 == 0x8007_0005 => {
                create(base).map_err(|error| format!("start the session guard: {error}"))?
            }
            Err(error) => return Err(format!("start the session guard: {error}")),
        };
        let process = own(process_info.hProcess);
        drop(own(process_info.hThread));
        drop(attributes);
        // Only the guard holds these ends now: its exit closes the armed
        // pipe, and only the broker's end keeps the release pipe open.
        drop(release_read);
        drop(armed_write);
        drop(stderr);
        // SAFETY: the read end is owned and moved into the File.
        let mut armed = unsafe { std::fs::File::from_raw_handle(armed_read.into_raw_handle()) };
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("arcen-session-guard-armed".to_owned())
            .spawn(move || {
                let mut line = vec![0u8; GUARD_ARMED.len()];
                let _ = sender.send(armed.read_exact(&mut line).map(|()| line));
            })
            .map_err(|error| format!("watch the session guard: {error}"))?;
        let armed = receiver
            .recv_timeout(ARMED_TIMEOUT)
            .map_err(|_| "the session guard did not report itself armed in time".to_string())
            .and_then(|read| read.map_err(|error| format!("read the session guard: {error}")));
        match armed {
            Ok(line) if line == GUARD_ARMED => Ok(GuardProcess {
                process,
                // SAFETY: the write end is owned and moved into the File.
                release: unsafe { std::fs::File::from_raw_handle(release_write.into_raw_handle()) },
                pid: process_info.dwProcessId,
            }),
            other => {
                // SAFETY: the process handle is live; a guard that did not
                // arm must not act later.
                unsafe {
                    let _ = TerminateProcess(raw(&process), 1);
                    let _ = WaitForSingleObject(raw(&process), 1_000);
                }
                Err(match other {
                    Ok(_) => "the session guard answered something other than armed".to_string(),
                    Err(error) => error,
                })
            }
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use crate::windows_session::SessionOwner;

    pub(super) struct GuardProcess {
        pub(super) pid: u32,
    }

    impl GuardProcess {
        pub(super) fn release(self) {}
        pub(super) fn abandon(self) {}
    }

    pub(super) fn boot_session() -> Result<String, String> {
        Err("the boot identifier is only available on Windows".to_string())
    }

    pub(super) fn start_guard(
        _session_id: u32,
        _owner: &SessionOwner,
    ) -> Result<GuardProcess, String> {
        Err("the session guard is only available on Windows".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";
    const BOOT: &str = "00112233445566778899aabbccddeeff";

    fn owner(logon_id: u64) -> SessionOwner {
        SessionOwner {
            sid: SID.to_owned(),
            logon_id,
        }
    }

    #[test]
    fn a_record_names_the_wts_session_its_logon_the_boot_and_the_sid() {
        let record = record_for(2, &owner(0x000a_1b2c), BOOT).expect("record");
        assert_eq!(record.login_session, "wts2-logon-00000000000a1b2c");
        assert_eq!(record.boot, BOOT);
        assert_eq!(record.account, RecordAccount::Sid(SID.to_owned()));
        assert_eq!(AttachmentRecord::parse(&record.to_text()), Some(record));
        assert_eq!(
            login_session_token(u32::MAX, u64::MAX),
            "wts4294967295-logon-ffffffffffffffff"
        );
        assert!(record_for(u32::MAX, &owner(u64::MAX), BOOT).is_some());
        let bad = SessionOwner {
            sid: "not-a-sid".to_owned(),
            logon_id: 1,
        };
        assert!(record_for(2, &bad, BOOT).is_none());
    }

    #[test]
    fn a_starting_broker_reads_only_the_session_the_record_names() {
        let record = record_for(7, &owner(0x10), BOOT).expect("record");
        assert_eq!(recorded_session_id(&record.login_session), Some(7));
        assert_eq!(
            recorded_session_id(&login_session_token(u32::MAX, u64::MAX)),
            Some(u32::MAX)
        );
        for bad in [
            "7",
            "wts7",
            "wts-logon-0000000000000010",
            "wtsx-logon-0000000000000010",
            "wts7-logon-10",
            "wts7-logon-000000000000001g",
            "wts4294967296-logon-0000000000000010",
            "100008",
        ] {
            assert_eq!(recorded_session_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_later_logon_in_the_same_session_id_is_not_the_recorded_session() {
        let recorded = record_for(2, &owner(0x10), BOOT).expect("record");
        let alive = [
            record_for(2, &owner(0x11), BOOT).expect("record"),
            record_for(3, &owner(0x10), BOOT).expect("record"),
        ];
        assert_eq!(
            service_startup_recovery(Some(&recorded.to_text()), &alive),
            ServiceStartupRecovery::Stale
        );
    }

    #[test]
    fn the_guard_takes_exactly_its_session_and_logon() {
        let parsed = parse_arguments(
            &mut [
                "--session-id",
                "2",
                "--sid",
                SID,
                "--logon",
                "00000000000a1b2c",
            ]
            .map(String::from)
            .into_iter(),
        )
        .expect("arguments");
        assert_eq!(
            parsed,
            GuardArguments {
                session_id: 2,
                owner: owner(0x000a_1b2c),
            }
        );
        for bad in [
            vec!["--session-id", "2", "--sid", SID],
            vec!["--session-id", "x", "--sid", SID, "--logon", "1"],
            vec!["--session-id", "2", "--sid", "bogus", "--logon", "1"],
            vec!["--session-id", "2", "--sid", SID, "--logon", "zz"],
            vec!["--session-id", "2", "--sid", SID, "--logon", "1", "--x"],
            vec!["--session-id"],
        ] {
            assert!(
                parse_arguments(&mut bad.iter().map(|s| (*s).to_owned())).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_record_is_read_strictly_and_removed_only_when_it_names_the_session() {
        let directory =
            std::env::temp_dir().join(format!("arcen-owner-guard-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("directory");
        let path = directory.join("deck-attachment.record");
        assert_eq!(read_record(&path), Ok(None));
        let ours = record_for(2, &owner(0x10), BOOT).expect("record");
        let theirs = record_for(3, &owner(0x20), BOOT).expect("record");
        std::fs::write(&path, theirs.to_text()).expect("write");
        remove_record_naming(&path, &ours);
        assert!(path.exists(), "another attachment's record is left alone");
        std::fs::write(&path, ours.to_text()).expect("write");
        // The guard describes the session without the boot; the session and
        // account decide.
        let guard_view =
            AttachmentRecord::for_account(&ours.login_session, "guard", ours.account.clone())
                .expect("record");
        remove_record_naming(&path, &guard_view);
        assert!(!path.exists());
        std::fs::write(&path, vec![b'x'; 4096]).expect("write");
        let oversized = read_record(&path).expect("read").expect("record");
        assert_eq!(oversized.len(), MAX_RECORD_BYTES as usize);
        assert_eq!(AttachmentRecord::parse(&oversized), None);
        std::fs::write(&path, [0xff, 0xfe]).expect("write");
        assert_eq!(read_record(&path), Ok(Some(String::new())));
        let _ = std::fs::remove_dir_all(&directory);
    }
}
