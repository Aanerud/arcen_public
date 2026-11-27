// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The session locked when the agent serving a Deck dies (ADR 0025 §4,
//! GAP-048 closed). The decisions are shared
//! ([`arcen_session::console_session::owner_death`]); this is the adapter.
//!
//! When a process exits, the window server gives the desk its displays and
//! input back: the agent's zero transfer tables and its event tap go with
//! it, and the session is left unlocked. launchd restarts the agent within
//! a fraction of a second, but the restarted agent knew nothing of the Deck.
//! Two mechanisms now cover this:
//!
//! - **The lock guard.** For each Deck attached to a signed-in user's
//!   session the agent starts `lock-guard`: the same binary, in its own
//!   process group. When the agent dies, launchd sends `SIGTERM` to the
//!   agent's process group, and a process outside that group is not
//!   signalled (measured on macOS 26 with a `KeepAlive` agent killed by
//!   `SIGKILL`: the child in the agent's group got `SIGTERM`; the child in
//!   its own group got nothing and saw its pipe close at once). The guard
//!   reads a pipe from the agent. A pipe that closes without
//!   [`GUARD_RELEASE`](arcen_session::console_session::owner_death::GUARD_RELEASE)
//!   means the agent died, however it died. The guard then darkens the
//!   desk's displays again (when the desk was private), locks the session,
//!   proves the lock from the window server, and gives the displays back
//!   (shared `lock_after_owner`).
//! - **The attachment record**
//!   (`~/Library/Application Support/Arcen/Pier/deck-attachment`): it names
//!   the security session (`SessionGetInfo`, the audit session every
//!   process of the login session shares), the boot
//!   (`kern.bootsessionuuid`) and the account,
//!   and the agent removes it after its own hand-back. An agent that starts
//!   and finds its own session's record locks the session before it serves
//!   anything. This covers a guard that was killed together with the agent
//!   (an installer stopping every Arcen process) or never started. It fails
//!   closed (shared `StoredRecord`, `startup_recovery_in`): only a record
//!   that is not there is no record, one that cannot be read locks, the
//!   record goes only once the session is locked, and one naming another
//!   login session of this boot and account (an agent started over SSH)
//!   is left alone; only a new boot or account makes it stale.
//!
//! Not covered, between the agent's death and the lock screen (the time it
//! takes to start the lock, well under a second): the desk's input reaches
//! the session, because the input filter's Accessibility grant belongs to
//! the agent, and with desk privacy on the displays show the session until
//! the guard darkens them again.

use std::io::{Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use arcen_session::console_session::owner_death::{
    AttachmentRecord, GUARD_ARMED, GUARD_READ_LIMIT, GUARD_RELEASE, GuardEnd, OwnerGoneHost,
    OwnerGoneOutcome, StartupRecovery, guard_end, lock_after_owner, recover_record,
};

/// The agent's subcommand that runs a lock guard.
pub const SUBCOMMAND: &str = "lock-guard";
/// How long a lock guard is given to report itself armed.
const ARMED_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a released guard is given to exit before it is stopped.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// Where this account's attachment record lives.
#[must_use]
pub fn record_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Arcen/Pier/deck-attachment"))
}

/// This process's login session as an attachment record, or `None` when
/// the session or the boot cannot be read.
#[must_use]
pub fn this_session(uid: u32) -> Option<AttachmentRecord> {
    let session = crate::screen_lock::login_session_id()?;
    AttachmentRecord::new(&session.to_string(), &boot_session()?, uid)
}

/// This boot's identifier (`kern.bootsessionuuid`). Not the boot time:
/// `kern.boottime` moves when the clock is stepped (a time sync after
/// waking), and a record must not read as another boot's for that.
pub(crate) fn boot_session() -> Option<String> {
    // A handful of C entry points, each with its SAFETY note below.
    #[allow(unsafe_code)]
    {
        let mut buffer = [0_u8; 64];
        let mut size = buffer.len();
        // SAFETY: a NUL-terminated name, a buffer and its size, which the
        // kernel writes at most `size` bytes into (and updates); no new
        // value.
        let status = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if status != 0 {
            return None;
        }
        let text = buffer.get(..size.min(buffer.len()))?;
        let text = text.split(|byte| *byte == 0).next()?;
        let text = std::str::from_utf8(text).ok()?;
        (!text.is_empty()).then(|| text.to_owned())
    }
}

/// Writes the record at `path`, readable only by the account, replacing
/// any.
fn write_record(path: PathBuf, record: &AttachmentRecord) -> Result<PathBuf, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let directory = path.parent().ok_or("no record directory")?;
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let staging = path.with_extension("new");
    let _ = std::fs::remove_file(&staging);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .map_err(|error| error.to_string())?;
    file.write_all(record.to_text().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| error.to_string())?;
    std::fs::rename(&staging, &path).map_err(|error| error.to_string())?;
    Ok(path)
}

/// The guard over one Deck's attachment, held by the agent from the
/// admission to the end of its own hand-back. Dropped by a panic, it closes
/// the pipe without a release, so the guard locks; dropped otherwise (a
/// session refused before anything was shown) it is released.
#[derive(Debug)]
pub struct LockGuard {
    child: Option<Child>,
    pipe: Option<ChildStdin>,
    record: Option<PathBuf>,
}

impl LockGuard {
    /// Records the attachment and starts the guard. Failures are logged and
    /// leave whichever half did start: the record alone still has the next
    /// agent lock the session.
    #[must_use]
    pub fn arm(uid: u32, private: bool) -> Self {
        let record = if let Some(session) = this_session(uid) {
            match record_path()
                .ok_or_else(|| "no home directory".to_owned())
                .and_then(|path| write_record(path, &session))
            {
                Ok(path) => Some(path),
                Err(error) => {
                    tracing::error!(
                        target: arcen_telemetry::names::target::SESSION,
                        %error,
                        "the Deck's attachment could not be recorded for a restarted agent"
                    );
                    None
                }
            }
        } else {
            tracing::error!(
                target: arcen_telemetry::names::target::SESSION,
                "this agent's login session could not be read; the attachment is not recorded"
            );
            None
        };
        let (child, pipe) = match start_guard(uid, private) {
            Ok((child, pipe)) => {
                tracing::info!(
                    target: arcen_telemetry::names::target::SESSION,
                    guard_pid = child.id(),
                    "the session locks if this agent dies while the Deck is attached"
                );
                (Some(child), Some(pipe))
            }
            Err(error) => {
                tracing::error!(
                    target: arcen_telemetry::names::target::SESSION,
                    %error,
                    "the lock guard did not start; a restarted agent locks the session instead"
                );
                (None, None)
            }
        };
        Self {
            child,
            pipe,
            record,
        }
    }

    /// Whether a guard process watches this attachment.
    #[must_use]
    pub fn is_watching(&self) -> bool {
        self.pipe.is_some()
    }

    /// The agent handed the session back itself: the guard exits without
    /// acting and the record goes.
    pub fn release(mut self) {
        self.release_now();
    }

    fn release_now(&mut self) {
        if let Some(mut pipe) = self.pipe.take() {
            let _ = pipe.write_all(GUARD_RELEASE).and_then(|()| pipe.flush());
        }
        if let Some(path) = self.record.take() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(child) = self.child.take() {
            reap(child);
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The pipe closes without a release and the guard locks; the
            // record stays for a restarted agent.
            self.pipe.take();
            self.record.take();
            return;
        }
        self.release_now();
    }
}

/// Waits, off the caller's thread, for a released guard to exit, and stops
/// one that does not.
fn reap(mut child: Child) {
    let _ = std::thread::Builder::new()
        .name("arcen-lock-guard-reap".to_owned())
        .spawn(move || {
            let deadline = Instant::now() + RELEASE_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => return,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
}

/// Starts `lock-guard` in its own process group and waits until it reports
/// itself armed.
fn start_guard(uid: u32, private: bool) -> Result<(Child, ChildStdin), String> {
    let program = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(program);
    command
        .arg(SUBCOMMAND)
        .arg("--uid")
        .arg(uid.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        // Outside the agent's process group, which launchd signals when
        // the agent exits.
        .process_group(0);
    if private {
        command.arg("--private");
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let (Some(pipe), Some(mut armed)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("the guard's pipes were not created".to_owned());
    };
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::Builder::new()
        .name("arcen-lock-guard-armed".to_owned())
        .spawn(move || {
            let mut line = vec![0_u8; GUARD_ARMED.len()];
            let _ = sender.send(armed.read_exact(&mut line).map(|()| line));
        })
        .map_err(|error| error.to_string());
    let armed = reader.and_then(|_| {
        receiver
            .recv_timeout(ARMED_TIMEOUT)
            .map_err(|_| "the guard did not report itself armed in time".to_owned())
            .and_then(|read| read.map_err(|error| error.to_string()))
    });
    match armed {
        Ok(line) if line == GUARD_ARMED => Ok((child, pipe)),
        Ok(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err("the guard answered something other than armed".to_owned())
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// What the lock guard and a restarted agent do to this Mac's session.
struct MacOwnerGoneHost {
    uid: u32,
    dark: Option<crate::desk_privacy::DarkDesk>,
    kept_private: bool,
    started: Instant,
}

impl MacOwnerGoneHost {
    fn new(uid: u32, private: bool) -> Self {
        Self {
            uid,
            dark: private.then(crate::desk_privacy::DarkDesk::default),
            kept_private: false,
            started: Instant::now(),
        }
    }
}

impl OwnerGoneHost for MacOwnerGoneHost {
    fn console_is_ours(&mut self) -> bool {
        crate::console::console_is_ours(self.uid)
    }

    fn keep_private(&mut self) {
        if let Some(dark) = &self.dark {
            self.kept_private = true;
            if !dark.darken() {
                eprintln!("lock-guard: a desk display could not be darkened");
            }
        }
    }

    fn lock(&mut self) -> bool {
        match crate::screen_lock::lock_and_confirm(crate::screen_lock::LOCK_CONFIRM_TIMEOUT) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("lock-guard: the session did not lock: {error}");
                false
            }
        }
    }

    fn give_back(&mut self) {
        self.kept_private = false;
        if let Some(dark) = &mut self.dark {
            dark.give_back();
        }
    }

    fn wait(&mut self, duration: Duration) {
        let step = arcen_session::console_session::desk_privacy::DISPLAY_CHECK;
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            std::thread::sleep(step.min(deadline.saturating_duration_since(Instant::now())));
            if self.kept_private {
                if let Some(dark) = &self.dark {
                    let _ = dark.darken();
                }
            }
        }
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

/// What an operator reads beside an outcome that did not lock: whether the
/// desk is safe, and what to do when it is not.
const fn outcome_explanation(outcome: OwnerGoneOutcome) -> Option<&'static str> {
    match outcome {
        OwnerGoneOutcome::Locked => None,
        // The console left this account for consecutive readings while
        // this session itself said it was no longer on screen: it signed
        // out or was switched away, and either way nobody at the desk sees
        // it without signing in.
        OwnerGoneOutcome::NotOurs => Some(
            "the session left the console (signed out, or another account switched in); \
             nothing of it is on screen to lock",
        ),
        OwnerGoneOutcome::GaveBackUnlocked => Some(
            "ERROR: the session would not lock after its agent ended: this Mac's desk is \
             unlocked and signed in. Lock it at the desk (Control-Command-Q) and check that \
             its lock screen is not turned off by policy",
        ),
    }
}

/// Runs `lock-guard --uid UID [--private]`: reports itself armed, waits for
/// the agent's pipe to close, and locks the session unless the agent
/// released it first.
#[must_use]
pub fn run(arguments: &[String]) -> ExitCode {
    let mut uid = None;
    let mut private = false;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--uid" => uid = arguments.next().and_then(|value| value.parse::<u32>().ok()),
            "--private" => private = true,
            other => {
                eprintln!("lock-guard: unknown argument: {other}");
                return ExitCode::FAILURE;
            }
        }
    }
    // A guard only ever guards its own account's session.
    let Some(uid) = uid.filter(|uid| *uid == crate::service::current_uid() && *uid != 0) else {
        eprintln!("lock-guard: --uid must name this process's own account");
        return ExitCode::FAILURE;
    };
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(GUARD_ARMED)
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    drop(stdout);
    let mut received = Vec::with_capacity(GUARD_READ_LIMIT);
    let _ = std::io::stdin()
        .lock()
        .take(u64::try_from(GUARD_READ_LIMIT).unwrap_or(u64::MAX))
        .read_to_end(&mut received);
    if guard_end(&received) == GuardEnd::Released {
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "lock-guard: the agent serving a Deck ended without handing the session back; locking it"
    );
    let outcome = lock_after_owner(&mut MacOwnerGoneHost::new(uid, private), private);
    eprintln!(
        "lock-guard: departure=agent_failed outcome={}",
        outcome_token(outcome)
    );
    if let Some(explanation) = outcome_explanation(outcome) {
        eprintln!("lock-guard: {explanation}");
    }
    if outcome.clears_record() {
        if let Some(path) = record_path() {
            let _ = std::fs::remove_file(path);
        }
    }
    ExitCode::SUCCESS
}

/// What an agent starting in a user's session does about a Deck the agent
/// before it was serving (shared `recover_record`, fail-closed): locks the
/// session first when this session's record says one was attached, or the
/// record cannot be read (the desk darkened meanwhile when `private`); the
/// record goes once the session is locked, or when it is of another boot or
/// account. Returns what it did.
#[must_use]
pub fn recover_at_start(uid: u32, private: bool) -> StartupRecovery {
    let Some(path) = record_path() else {
        return StartupRecovery::Nothing;
    };
    let done = recover_record(&path, this_session(uid).as_ref(), || {
        tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            departure = "agent_failed",
            "the agent before this one ended while a Deck was attached; locking the session first"
        );
        lock_after_owner(&mut MacOwnerGoneHost::new(uid, private), private)
    });
    match (done.recovery, done.locked) {
        (StartupRecovery::LeaveAlone, _) => tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            "the Deck attachment record names another login session; it is left for that session"
        ),
        (_, Some(outcome)) if !done.removed => tracing::error!(
            target: arcen_telemetry::names::target::SESSION,
            outcome = outcome_token(outcome),
            "the session is not locked; the Deck attachment record stays so the next start locks it"
        ),
        (_, Some(outcome)) => tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            outcome = outcome_token(outcome),
            "the session left by the agent before this one is handed back"
        ),
        _ => {}
    }
    done.recovery
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_record_lives_under_the_accounts_own_home() {
        let path = record_path().expect("HOME is set for tests");
        assert!(path.ends_with("Library/Application Support/Arcen/Pier/deck-attachment"));
        assert!(path.starts_with(std::env::var_os("HOME").expect("HOME")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_boot_has_a_stable_identifier() {
        let boot = boot_session().expect("kern.bootsessionuuid");
        assert_eq!(boot_session().as_deref(), Some(boot.as_str()));
        assert!(AttachmentRecord::new("1", &boot, 1).is_some(), "{boot}");
    }

    /// A directory beside the test binary, removed by the caller.
    fn scratch(name: &str) -> PathBuf {
        let binary = std::env::current_exe().expect("test binary");
        let directory = binary
            .parent()
            .expect("binary directory")
            .join(format!("arcen-lock-guard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch created");
        directory
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_agent_records_its_session_and_the_agent_after_it_locks_first() {
        use arcen_session::console_session::owner_death::{StoredRecord, startup_recovery_in};
        use std::os::unix::fs::PermissionsExt as _;
        let uid = crate::service::current_uid();
        // Read through `kCGSSessionIDKey`, which the session dictionary does
        // not carry, this was `None` in every process and no record was
        // ever written.
        let session = this_session(uid).expect("this process's login session");
        let directory = scratch("record");
        let path = directory.join("deck-attachment");
        assert_eq!(
            startup_recovery_in(&StoredRecord::read(&path), Some(&session)),
            StartupRecovery::Nothing
        );

        let written = write_record(path.clone(), &session).expect("record written");
        assert_eq!(written, path);
        let mode = std::fs::metadata(&path)
            .expect("record")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        // The agent launchd starts next runs in the same login session.
        let restarted = this_session(uid).expect("the same login session");
        assert_eq!(restarted, session);
        assert_eq!(
            startup_recovery_in(&StoredRecord::read(&path), Some(&restarted)),
            StartupRecovery::LockFirst
        );
        std::fs::remove_dir_all(&directory).expect("scratch removed");
    }

    #[test]
    fn a_record_behind_a_symlink_loop_locks_and_stays() {
        use arcen_session::console_session::owner_death::StoredRecord;
        let directory = scratch("looped");
        let looped = directory.join("deck-attachment");
        // Opening it fails with ELOOP, not NotFound.
        std::os::unix::fs::symlink(&looped, &looped).expect("symlink");
        assert_eq!(StoredRecord::read(&looped), StoredRecord::Unreadable);
        let current = AttachmentRecord::new("100017", "6F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0", 501)
            .expect("record");
        let done = recover_record(&looped, Some(&current), || {
            OwnerGoneOutcome::GaveBackUnlocked
        });
        assert_eq!(done.recovery, StartupRecovery::LockFirst);
        assert!(!done.removed);
        assert!(std::fs::symlink_metadata(&looped).is_ok());
        std::fs::remove_dir_all(&directory).expect("scratch removed");
    }

    #[test]
    fn a_guard_runs_only_for_its_own_account() {
        let other = crate::service::current_uid().wrapping_add(1).to_string();
        assert_eq!(run(&["--uid".to_owned(), other]), ExitCode::FAILURE);
        assert_eq!(run(&["--bogus".to_owned()]), ExitCode::FAILURE);
    }
}
