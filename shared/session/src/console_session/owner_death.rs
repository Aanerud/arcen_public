// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A Pier process that dies while a Deck is attached to the user's session
//! (ADR 0025 §4: disconnect locks, never ends).
//!
//! The process serving the Deck locks the session when the Deck leaves
//! ([`super::DETACH_STEPS`]). If that process dies, the operating system
//! gives the desk its displays and input back at once, and the session is
//! unlocked. Something that outlives the process must lock it, and it must
//! do so before anything else is given back. Two mechanisms do this, and a
//! Pier uses whichever its operating system allows:
//!
//! - **A lock guard.** When a Deck is attached, the owner starts a small
//!   process outside its own process group. The guard reads a pipe from the
//!   owner and reports [`GUARD_ARMED`] once it is ready. Once the owner has
//!   handed the session back itself, it sends [`GUARD_RELEASE`]. Anything
//!   else means the owner is gone: the pipe closing without a release (which
//!   happens however the owner dies, `SIGKILL` included), a partial line, or
//!   noise. The guard then locks the session ([`guard_end`]).
//! - **An attachment record.** When a Deck is attached, the owner writes a
//!   record naming the operating system's login session, the boot it
//!   belongs to (a per-boot identifier, never a boot time: a clock step
//!   moves the boot time) and the account, and removes it after the hand-back. A Pier
//!   process that starts in the same login session and boot and finds such a
//!   record locks the session before it serves anything
//!   ([`startup_recovery`]). A record from another login session or boot is
//!   stale, because the session it names has ended, and is discarded. A
//!   record that cannot be read is treated as this session's, so the check
//!   fails closed.
//!
//!   A service that runs outside its users' sessions decides with
//!   [`service_startup_recovery`] instead: it compares the record with every
//!   login session alive now, and locks only the one it names exactly. A
//!   record it cannot read names no session, so it locks none: locking
//!   whoever holds the console now could lock an account the record never
//!   named.
//!
//! The Linux Pier's service runs outside the session and keeps a root
//! journal of the console it made private (`recover_console`). The macOS
//! Pier's per-user agent uses both mechanisms in its own session. The
//! Windows Pier's broker uses both from session 0: its guard is a `LocalSystem`
//! process outside the broker, and the record names the WTS session and its
//! logon, the boot, and the account's SID.

use super::Departure;

/// What a lock guard writes to its owner once it watches the pipe: the
/// owner knows the session is covered from then on.
pub const GUARD_ARMED: &[u8] = b"arcen-lock-guard armed\n";

/// What the owner writes to its lock guard after handing the session back
/// itself (locked, or deliberately not: the console went to another
/// account, or the lock was refused for the whole grace).
pub const GUARD_RELEASE: &[u8] = b"arcen-lock-guard release\n";

/// The most a guard reads from its owner: a release, and one byte more to
/// tell a release from a release followed by noise.
pub const GUARD_READ_LIMIT: usize = GUARD_RELEASE.len() + 1;

/// How a lock guard's watch over its owner ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardEnd {
    /// The owner handed the session back itself; the guard does nothing.
    Released,
    /// The owner is gone without handing the session back: the guard
    /// locks it.
    OwnerGone,
}

impl GuardEnd {
    /// Why the Deck's attachment ended, when the guard must act on it.
    #[must_use]
    pub const fn departure(self) -> Option<Departure> {
        match self {
            Self::Released => None,
            Self::OwnerGone => Some(Departure::AgentFailed),
        }
    }
}

/// Reads what a lock guard received from its owner before the pipe closed
/// (at most [`GUARD_READ_LIMIT`] bytes): only an exact [`GUARD_RELEASE`]
/// releases it.
#[must_use]
pub fn guard_end(received: &[u8]) -> GuardEnd {
    if received == GUARD_RELEASE {
        GuardEnd::Released
    } else {
        GuardEnd::OwnerGone
    }
}

/// The first line of an attachment record.
const RECORD_HEADER: &str = "arcen-attachment 1";
/// The longest token (login session, boot) a record holds.
const MAX_TOKEN: usize = 64;
/// The longest Windows SID a record holds: `S-1-` and fifteen 32-bit
/// sub-authorities fit in 184 bytes.
const MAX_SID: usize = 184;

/// The account a recorded login session belongs to, as its platform names
/// accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordAccount {
    /// A POSIX user id (Linux, macOS).
    Uid(u32),
    /// A Windows security identifier in its string form (`S-1-5-21-…`).
    Sid(String),
}

impl RecordAccount {
    /// A SID account, or `None` unless `sid` is `S-` followed by at least
    /// two dash-separated decimal components, at most 184 bytes.
    #[must_use]
    pub fn sid(sid: &str) -> Option<Self> {
        sid_valid(sid).then(|| Self::Sid(sid.to_owned()))
    }

    fn to_line(&self) -> String {
        match self {
            Self::Uid(uid) => format!("uid {uid}"),
            Self::Sid(sid) => format!("sid {sid}"),
        }
    }

    fn parse_line(line: &str) -> Option<Self> {
        if let Some(uid) = line.strip_prefix("uid ") {
            return u32::try_from(parse_decimal(uid)?).ok().map(Self::Uid);
        }
        Self::sid(line.strip_prefix("sid ")?)
    }
}

/// A Deck attached to a login session, as recorded for the next Pier
/// process that starts in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    /// The operating system's login session, as the platform names it (a
    /// security session id, a logind session id, a WTS session and logon
    /// id): `[A-Za-z0-9._:-]`, at most 64 bytes.
    pub login_session: String,
    /// The boot the session belongs to, as a per-boot identifier (a boot
    /// session UUID, a boot id; never a boot time, which moves when the
    /// clock is stepped): a login session id may be reused after a restart.
    /// The same characters and length as `login_session`.
    pub boot: String,
    /// The account the session belongs to.
    pub account: RecordAccount,
}

impl AttachmentRecord {
    /// A record of a POSIX account's session, or `None` when the
    /// login-session or boot token is not a short identifier.
    #[must_use]
    pub fn new(login_session: &str, boot: &str, uid: u32) -> Option<Self> {
        Self::for_account(login_session, boot, RecordAccount::Uid(uid))
    }

    /// A record of `account`'s session, or `None` when the login-session or
    /// boot token is not a short identifier or the SID is malformed.
    #[must_use]
    pub fn for_account(login_session: &str, boot: &str, account: RecordAccount) -> Option<Self> {
        let account_valid = match &account {
            RecordAccount::Uid(_) => true,
            RecordAccount::Sid(sid) => sid_valid(sid),
        };
        (token_valid(login_session) && token_valid(boot) && account_valid).then(|| Self {
            login_session: login_session.to_owned(),
            boot: boot.to_owned(),
            account,
        })
    }

    /// The record as stored.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!(
            "{RECORD_HEADER}\nlogin_session {}\nboot {}\n{}\n",
            self.login_session,
            self.boot,
            self.account.to_line()
        )
    }

    /// Parses a stored record, strictly: the header, the three fields in
    /// order, nothing else.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.strip_suffix('\n')?.split('\n');
        if lines.next()? != RECORD_HEADER {
            return None;
        }
        let login_session = lines.next()?.strip_prefix("login_session ")?;
        let boot = lines.next()?.strip_prefix("boot ")?;
        let account = RecordAccount::parse_line(lines.next()?)?;
        if lines.next().is_some() {
            return None;
        }
        Self::for_account(login_session, boot, account)
    }
}

fn sid_valid(sid: &str) -> bool {
    let Some(components) = sid.strip_prefix("S-") else {
        return false;
    };
    sid.len() <= MAX_SID
        && components.split('-').count() >= 2
        && components
            .split('-')
            .all(|part| (1..=10).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit()))
}

fn token_valid(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn parse_decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// What a Pier process found where its attachment record lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredRecord {
    /// Nothing there: the read failed with `NotFound`, and only that.
    Absent,
    /// Something there that could not be read (permissions, a symlink loop,
    /// an I/O error, text that is not UTF-8). It may be this session's
    /// record, so it is not trusted to be absent: it locks.
    Unreadable,
    /// The stored text, at most [`StoredRecord::READ_LIMIT`] bytes of it.
    Text(String),
}

impl StoredRecord {
    /// How much of a record is read; longer than any record, so a larger
    /// file fails to parse (and locks) rather than being trusted.
    pub const READ_LIMIT: u64 = 512;

    /// Reads the record at `path`, at most [`Self::READ_LIMIT`] bytes.
    #[must_use]
    pub fn read(path: &std::path::Path) -> Self {
        use std::io::Read as _;
        Self::from_read(std::fs::File::open(path).and_then(|file| {
            let mut text = String::new();
            file.take(Self::READ_LIMIT)
                .read_to_string(&mut text)
                .map(|_| text)
        }))
    }

    /// Classifies a read of the record: only `NotFound` is "no record".
    #[must_use]
    pub fn from_read(read: std::io::Result<String>) -> Self {
        match read {
            Ok(text) => Self::Text(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::Absent,
            Err(_) => Self::Unreadable,
        }
    }
}

/// What a Pier process does with the attachment record it finds at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupRecovery {
    /// No record: nothing was attached.
    Nothing,
    /// A Deck was attached to this login session when the process before
    /// this one ended, or the record cannot be trusted: lock the session
    /// before serving anything, and remove the record only once it is
    /// locked ([`Self::removes_record`]).
    LockFirst,
    /// The record is of an earlier boot or another account: remove it.
    Stale,
    /// The record names another login session of this boot and account
    /// (the process runs outside it, such as over SSH, or the account's
    /// earlier login session): neither lock nor remove; it is that
    /// session's agent's, or the next boot's, to decide.
    LeaveAlone,
}

impl StartupRecovery {
    /// Why the Deck's attachment ended, when the session must be locked.
    #[must_use]
    pub const fn departure(self) -> Option<Departure> {
        match self {
            Self::LockFirst => Some(Departure::AgentFailed),
            Self::Nothing | Self::Stale | Self::LeaveAlone => None,
        }
    }

    /// Whether the record is removed after this recovery, `locked` being
    /// how the lock of [`Self::LockFirst`] ended. A session left unlocked
    /// keeps its record, so the next start tries again.
    #[must_use]
    pub const fn removes_record(self, locked: Option<OwnerGoneOutcome>) -> bool {
        match self {
            Self::Stale => true,
            Self::LockFirst => match locked {
                Some(outcome) => outcome.clears_record(),
                None => false,
            },
            Self::Nothing | Self::LeaveAlone => false,
        }
    }
}

/// Decides what a Pier process starting in login session `current` does
/// with the stored record `stored` (`None`: no record). A record that
/// cannot be parsed locks: it may be this session's, and a lock is only
/// ever a password away. A record of another login session is stale only
/// when it is of another boot or account; one of this boot and account is
/// left alone, since only the agent in that session may judge it.
#[must_use]
pub fn startup_recovery(stored: Option<&str>, current: &AttachmentRecord) -> StartupRecovery {
    let Some(text) = stored else {
        return StartupRecovery::Nothing;
    };
    match AttachmentRecord::parse(text) {
        None => StartupRecovery::LockFirst,
        Some(record) if record == *current => StartupRecovery::LockFirst,
        Some(record) if record.boot != current.boot || record.account != current.account => {
            StartupRecovery::Stale
        }
        Some(_) => StartupRecovery::LeaveAlone,
    }
}

/// [`startup_recovery`] for what a Pier process found
/// ([`StoredRecord`]), when its own login session may not be readable
/// (`current` is `None`). A record that cannot be read, or one a process
/// that cannot name its session finds, locks: it may be this session's.
#[must_use]
pub fn startup_recovery_in(
    stored: &StoredRecord,
    current: Option<&AttachmentRecord>,
) -> StartupRecovery {
    match (stored, current) {
        (StoredRecord::Absent, _) => StartupRecovery::Nothing,
        (StoredRecord::Unreadable, _) | (StoredRecord::Text(_), None) => StartupRecovery::LockFirst,
        (StoredRecord::Text(text), Some(current)) => startup_recovery(Some(text), current),
    }
}

/// What [`recover_record`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordRecovery {
    /// What the record asked for.
    pub recovery: StartupRecovery,
    /// How the lock ended, when the record asked for one.
    pub locked: Option<OwnerGoneOutcome>,
    /// Whether the record was removed.
    pub removed: bool,
}

/// What a Pier process starting in `current` does with the record at
/// `path`: [`startup_recovery_in`] on what is there, `lock` when that is
/// [`StartupRecovery::LockFirst`], and the record removed only when
/// [`StartupRecovery::removes_record`] says so. Fails closed: a record that
/// cannot be read locks, a session left unlocked keeps its record, and
/// another login session's record is left alone.
pub fn recover_record(
    path: &std::path::Path,
    current: Option<&AttachmentRecord>,
    lock: impl FnOnce() -> OwnerGoneOutcome,
) -> RecordRecovery {
    let recovery = startup_recovery_in(&StoredRecord::read(path), current);
    let locked = (recovery == StartupRecovery::LockFirst).then(lock);
    let removed = recovery.removes_record(locked) && std::fs::remove_file(path).is_ok();
    RecordRecovery {
        recovery,
        locked,
        removed,
    }
}

/// What a service that runs outside its users' sessions does with the
/// attachment record it finds at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceStartupRecovery {
    /// No record: nothing was attached.
    Nothing,
    /// The login session the record names is alive, in this boot, with the
    /// same account: lock it before giving the desk anything back, then
    /// remove the record.
    LockFirst(AttachmentRecord),
    /// No login session alive now is the one the record names (it ended, or
    /// its id now belongs to another logon, account or boot): remove it
    /// without locking anyone.
    Stale,
    /// The record cannot be read, so it names no session: lock nobody, and
    /// keep it aside for the operator.
    Unreadable,
}

impl ServiceStartupRecovery {
    /// Why the Deck's attachment ended, when a session must be locked.
    #[must_use]
    pub const fn departure(&self) -> Option<Departure> {
        match self {
            Self::LockFirst(_) => Some(Departure::AgentFailed),
            Self::Nothing | Self::Stale | Self::Unreadable => None,
        }
    }
}

/// Decides what a service starting outside its users' sessions does with
/// the stored record `stored` (`None`: no record), given every login session
/// `alive` now, each described as a record would describe it (this boot, its
/// logon, its account). Only an exact match is locked: a login session id
/// reused by another logon, account or boot is not the recorded one.
#[must_use]
pub fn service_startup_recovery(
    stored: Option<&str>,
    alive: &[AttachmentRecord],
) -> ServiceStartupRecovery {
    let Some(text) = stored else {
        return ServiceStartupRecovery::Nothing;
    };
    let Some(record) = AttachmentRecord::parse(text) else {
        return ServiceStartupRecovery::Unreadable;
    };
    if alive.contains(&record) {
        ServiceStartupRecovery::LockFirst(record)
    } else {
        ServiceStartupRecovery::Stale
    }
}

/// What a process that outlives the owner (a lock guard, or the next Pier
/// process finding a record) can do to the session and the desk.
pub trait OwnerGoneHost {
    /// Whether the console is this session's account's, read once now.
    fn console_is_ours(&mut self) -> bool;
    /// Makes the desk private again, as far as this process can (on macOS,
    /// darkens the desk's displays). Called before every lock attempt.
    fn keep_private(&mut self);
    /// Asks the session to lock and waits, bounded, until it reports
    /// itself locked.
    fn lock(&mut self) -> bool;
    /// Gives back what [`Self::keep_private`] took.
    fn give_back(&mut self);
    /// Waits, keeping the desk private meanwhile when it is.
    fn wait(&mut self, duration: std::time::Duration);
    /// How long since the owner was found gone.
    fn elapsed(&self) -> std::time::Duration;
}

/// How [`lock_after_owner`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerGoneOutcome {
    /// The session is locked; the desk got it back on the lock screen.
    Locked,
    /// The console is another account's (or the login window's): the
    /// session is not on screen, and the desk was left to whoever holds it.
    NotOurs,
    /// The session would not lock for [`super::UNLOCKED_GRACE`]; the desk
    /// got it back unlocked, as after a hand-back whose lock was refused.
    GaveBackUnlocked,
}

impl OwnerGoneOutcome {
    /// Whether the attachment record may go: only once the session is
    /// locked. A session left unlocked (refused, or behind another
    /// account's console) keeps it for the next start to try again.
    #[must_use]
    pub const fn clears_record(self) -> bool {
        matches!(self, Self::Locked)
    }
}

/// The hand-back a lock guard (or the next Pier process) does for an owner
/// that died: [`super::DETACH_STEPS`] from the start. The desk is kept
/// private when `private` (it was while the Deck was attached), then the
/// session is locked, then the desk gets back what was taken. A lock that
/// fails is asked again every
/// [`LOCK_RETRY`](super::desk_privacy::LOCK_RETRY) for at most
/// [`super::UNLOCKED_GRACE`]. A console read as another account's on
/// [`CONSOLE_AWAY_READINGS`](super::desk_privacy::CONSOLE_AWAY_READINGS)
/// consecutive readings is left alone: one failed reading is not a switch of
/// account, and nothing of another account's desk is darkened.
pub fn lock_after_owner(host: &mut impl OwnerGoneHost, private: bool) -> OwnerGoneOutcome {
    use super::desk_privacy::{
        CONSOLE_AWAY_READINGS, CONSOLE_CHECK, LOCK_RETRY, UnlockedHandBack, hand_back_unlocked,
    };
    let mut kept_private = false;
    let mut away = 0;
    loop {
        let ours = host.console_is_ours();
        if ours {
            away = 0;
            if private {
                host.keep_private();
                kept_private = true;
            }
            if host.lock() {
                if kept_private {
                    host.give_back();
                }
                return OwnerGoneOutcome::Locked;
            }
        } else {
            if kept_private {
                host.give_back();
                kept_private = false;
            }
            away += 1;
            if away >= CONSOLE_AWAY_READINGS {
                return OwnerGoneOutcome::NotOurs;
            }
        }
        if hand_back_unlocked(host.elapsed()) == UnlockedHandBack::GiveBack {
            if kept_private {
                host.give_back();
            }
            return OwnerGoneOutcome::GaveBackUnlocked;
        }
        host.wait(if ours { LOCK_RETRY } else { CONSOLE_CHECK });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Records what the hand-back did, in order.
    struct FakeHost {
        console: Vec<bool>,
        locks: Vec<bool>,
        elapsed: Duration,
        log: Vec<&'static str>,
    }

    impl FakeHost {
        fn new(console: &[bool], locks: &[bool]) -> Self {
            Self {
                console: console.iter().rev().copied().collect(),
                locks: locks.iter().rev().copied().collect(),
                elapsed: Duration::ZERO,
                log: Vec::new(),
            }
        }
    }

    impl OwnerGoneHost for FakeHost {
        fn console_is_ours(&mut self) -> bool {
            self.console.pop().unwrap_or(true)
        }
        fn keep_private(&mut self) {
            self.log.push("private");
        }
        fn lock(&mut self) -> bool {
            self.log.push("lock");
            self.locks.pop().unwrap_or(false)
        }
        fn give_back(&mut self) {
            self.log.push("give_back");
        }
        fn wait(&mut self, duration: Duration) {
            self.log.push("wait");
            self.elapsed += duration;
        }
        fn elapsed(&self) -> Duration {
            self.elapsed
        }
    }

    #[test]
    fn an_owner_that_died_has_its_session_locked_before_the_desk_gets_anything_back() {
        let mut host = FakeHost::new(&[true], &[true]);
        assert_eq!(lock_after_owner(&mut host, true), OwnerGoneOutcome::Locked);
        assert_eq!(host.log, ["private", "lock", "give_back"]);

        let mut host = FakeHost::new(&[true], &[true]);
        assert_eq!(lock_after_owner(&mut host, false), OwnerGoneOutcome::Locked);
        assert_eq!(host.log, ["lock"]);
    }

    #[test]
    fn a_refused_lock_is_retried_with_the_desk_private_and_given_back_after_the_grace() {
        let mut host = FakeHost::new(&[], &[false, true]);
        assert_eq!(lock_after_owner(&mut host, true), OwnerGoneOutcome::Locked);
        assert_eq!(
            host.log,
            ["private", "lock", "wait", "private", "lock", "give_back"]
        );

        let mut host = FakeHost::new(&[], &[]);
        assert_eq!(
            lock_after_owner(&mut host, true),
            OwnerGoneOutcome::GaveBackUnlocked
        );
        assert!(host.elapsed >= super::super::UNLOCKED_GRACE);
        assert_eq!(host.log.last(), Some(&"give_back"));
        assert!(
            host.log
                .iter()
                .rposition(|step| *step == "lock")
                .is_some_and(|last_lock| !host.log[..last_lock].contains(&"give_back"))
        );
    }

    #[test]
    fn a_console_another_account_holds_is_left_alone_after_consecutive_readings() {
        let mut host = FakeHost::new(&[false, false, false], &[]);
        assert_eq!(lock_after_owner(&mut host, true), OwnerGoneOutcome::NotOurs);
        assert_eq!(host.log, ["wait", "wait"]);

        // One failed reading is not a switch of account.
        let mut host = FakeHost::new(&[false, true], &[true]);
        assert_eq!(lock_after_owner(&mut host, true), OwnerGoneOutcome::Locked);
        assert_eq!(host.log, ["wait", "private", "lock", "give_back"]);

        // A desk darkened for this account is given back when another
        // account turns out to hold the console.
        let mut host = FakeHost::new(&[true, false, false, false], &[false]);
        assert_eq!(lock_after_owner(&mut host, true), OwnerGoneOutcome::NotOurs);
        assert_eq!(
            host.log,
            ["private", "lock", "wait", "give_back", "wait", "wait"]
        );
    }

    const BOOT: &str = "6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10";

    fn current() -> AttachmentRecord {
        AttachmentRecord::new("100008", BOOT, 501).expect("record")
    }

    #[test]
    fn only_an_exact_release_releases_the_guard() {
        assert_eq!(guard_end(GUARD_RELEASE), GuardEnd::Released);
        assert_eq!(guard_end(b""), GuardEnd::OwnerGone);
        assert_eq!(
            guard_end(&GUARD_RELEASE[..GUARD_RELEASE.len() - 1]),
            GuardEnd::OwnerGone
        );
        let mut trailing = GUARD_RELEASE.to_vec();
        trailing.push(b'x');
        assert_eq!(trailing.len(), GUARD_READ_LIMIT);
        assert_eq!(guard_end(&trailing), GuardEnd::OwnerGone);
        assert_eq!(guard_end(GUARD_ARMED), GuardEnd::OwnerGone);
        assert_eq!(GuardEnd::Released.departure(), None);
        let departure = GuardEnd::OwnerGone.departure().expect("departure");
        assert_eq!(departure, Departure::AgentFailed);
        assert!(departure.locks_session());
    }

    #[test]
    fn a_record_round_trips_and_is_parsed_strictly() {
        let record = current();
        assert_eq!(AttachmentRecord::parse(&record.to_text()), Some(record));
        for bad in [
            "",
            "arcen-attachment 1\nlogin_session 100008\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nuid 501",
            "arcen-attachment 2\nlogin_session 100008\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nuid 501\n",
            "arcen-attachment 1\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nlogin_session 100008\nuid 501\n",
            "arcen-attachment 1\nlogin_session 100008\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nuid 501\nextra\n",
            "arcen-attachment 1\nlogin_session 1 2\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nuid 501\n",
            "arcen-attachment 1\nlogin_session 100008\nboot a/b\nuid 501\n",
            "arcen-attachment 1\nlogin_session 100008\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nuid 4294967296\n",
        ] {
            assert_eq!(AttachmentRecord::parse(bad), None, "{bad:?}");
        }
        assert!(AttachmentRecord::new("", BOOT, 1).is_none());
        assert!(AttachmentRecord::new("1", "", 1).is_none());
        assert!(AttachmentRecord::new(&"a".repeat(65), BOOT, 1).is_none());
        assert!(AttachmentRecord::new("1", "boot id", 1).is_none());
        assert!(AttachmentRecord::new("c2:logon-0x1f.a_b", BOOT, 1).is_some());
    }

    #[test]
    fn a_process_starting_in_the_recorded_session_locks_first() {
        let record = current();
        assert_eq!(startup_recovery(None, &record), StartupRecovery::Nothing);
        assert_eq!(
            startup_recovery(Some(&record.to_text()), &record),
            StartupRecovery::LockFirst
        );
        assert_eq!(
            StartupRecovery::LockFirst.departure(),
            Some(Departure::AgentFailed)
        );
        // An unreadable record may be this session's: it locks.
        assert_eq!(
            startup_recovery(Some("garbage"), &record),
            StartupRecovery::LockFirst
        );
    }

    #[test]
    fn only_a_record_that_is_not_there_counts_as_no_record() {
        use std::io::{Error, ErrorKind};
        assert_eq!(
            StoredRecord::from_read(Err(Error::from(ErrorKind::NotFound))),
            StoredRecord::Absent
        );
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::IsADirectory,
            ErrorKind::Other,
        ] {
            let stored = StoredRecord::from_read(Err(Error::from(kind)));
            assert_eq!(stored, StoredRecord::Unreadable, "{kind:?}");
            assert_eq!(
                startup_recovery_in(&stored, Some(&current())),
                StartupRecovery::LockFirst
            );
        }
        let elsewhere = AttachmentRecord::new("100009", BOOT, 501).expect("record");
        assert_eq!(
            startup_recovery_in(&StoredRecord::Unreadable, Some(&elsewhere)),
            StartupRecovery::LockFirst
        );
        let text = current().to_text();
        assert_eq!(
            StoredRecord::from_read(Ok(text.clone())),
            StoredRecord::Text(text)
        );
    }

    #[test]
    fn a_process_that_cannot_name_its_session_locks_on_any_record() {
        let record = current();
        assert_eq!(
            startup_recovery_in(&StoredRecord::Absent, None),
            StartupRecovery::Nothing
        );
        for stored in [
            StoredRecord::Text(record.to_text()),
            StoredRecord::Text("garbage".to_owned()),
            StoredRecord::Unreadable,
        ] {
            assert_eq!(
                startup_recovery_in(&stored, None),
                StartupRecovery::LockFirst
            );
        }
        assert_eq!(
            startup_recovery_in(&StoredRecord::Text(record.to_text()), Some(&record)),
            StartupRecovery::LockFirst
        );
    }

    #[test]
    fn a_record_goes_only_once_its_session_is_locked() {
        assert!(StartupRecovery::LockFirst.removes_record(Some(OwnerGoneOutcome::Locked)));
        for left_unlocked in [
            Some(OwnerGoneOutcome::GaveBackUnlocked),
            Some(OwnerGoneOutcome::NotOurs),
            None,
        ] {
            assert!(!StartupRecovery::LockFirst.removes_record(left_unlocked));
        }
        assert!(StartupRecovery::Stale.removes_record(None));
        assert!(!StartupRecovery::LeaveAlone.removes_record(None));
        assert!(!StartupRecovery::Nothing.removes_record(None));
        assert!(OwnerGoneOutcome::Locked.clears_record());
        assert!(!OwnerGoneOutcome::GaveBackUnlocked.clears_record());
    }

    #[test]
    fn a_record_of_another_boot_or_account_is_stale() {
        let record = current();
        for other in [
            AttachmentRecord::new(
                &record.login_session,
                "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0",
                501,
            ),
            AttachmentRecord::new("100009", "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0", 501),
            AttachmentRecord::new(&record.login_session, &record.boot, 502),
        ] {
            let other = other.expect("record");
            assert_eq!(
                startup_recovery(Some(&other.to_text()), &record),
                StartupRecovery::Stale
            );
        }
        assert_eq!(StartupRecovery::Stale.departure(), None);
    }

    #[test]
    fn a_record_of_another_session_of_this_boot_and_account_is_left_alone() {
        // A process outside the recorded session (over SSH, say) neither
        // locks that session nor removes its record.
        let record = current();
        let outside = AttachmentRecord::new("100009", &record.boot, 501).expect("record");
        let recovery = startup_recovery(Some(&record.to_text()), &outside);
        assert_eq!(recovery, StartupRecovery::LeaveAlone);
        assert_eq!(recovery.departure(), None);
        assert!(!recovery.removes_record(None));
    }

    /// A directory beside the test binary, emptied first; removed by the
    /// caller.
    fn scratch(name: &str) -> std::path::PathBuf {
        let binary = std::env::current_exe().expect("test binary");
        let directory = binary
            .parent()
            .expect("binary directory")
            .join(format!("arcen-owner-death-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch created");
        directory
    }

    /// [`recover_record`] with a lock that ends as `outcome`.
    fn recover_with(
        path: &std::path::Path,
        current: &AttachmentRecord,
        outcome: OwnerGoneOutcome,
    ) -> RecordRecovery {
        recover_record(path, Some(current), || outcome)
    }

    #[test]
    fn a_record_that_cannot_be_read_locks_and_is_not_taken_for_absent() {
        let directory = scratch("unreadable");
        let current = current();
        // A directory opens, and reading it fails.
        let folder = directory.join("folder");
        std::fs::create_dir(&folder).expect("folder");
        // Not UTF-8.
        let binary = directory.join("binary");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).expect("binary");
        for path in [&folder, &binary] {
            assert_eq!(StoredRecord::read(path), StoredRecord::Unreadable);
            assert_eq!(
                recover_with(path, &current, OwnerGoneOutcome::GaveBackUnlocked),
                RecordRecovery {
                    recovery: StartupRecovery::LockFirst,
                    locked: Some(OwnerGoneOutcome::GaveBackUnlocked),
                    removed: false,
                },
                "{}",
                path.display()
            );
        }
        let absent = directory.join("absent");
        assert_eq!(StoredRecord::read(&absent), StoredRecord::Absent);
        assert_eq!(
            recover_record(&absent, Some(&current), || panic!("nothing to lock")),
            RecordRecovery {
                recovery: StartupRecovery::Nothing,
                locked: None,
                removed: false,
            }
        );
        std::fs::remove_dir_all(&directory).expect("scratch removed");
    }

    #[test]
    fn a_record_stays_until_its_session_is_locked() {
        let directory = scratch("unlocked");
        let path = directory.join("deck-attachment");
        let current = current();
        std::fs::write(&path, current.to_text()).expect("record written");
        for unlocked in [
            OwnerGoneOutcome::GaveBackUnlocked,
            OwnerGoneOutcome::NotOurs,
        ] {
            let done = recover_with(&path, &current, unlocked);
            assert_eq!(done.recovery, StartupRecovery::LockFirst);
            assert_eq!(done.locked, Some(unlocked));
            assert!(!done.removed);
            assert_eq!(
                StoredRecord::read(&path),
                StoredRecord::Text(current.to_text()),
                "{unlocked:?}"
            );
        }
        let done = recover_with(&path, &current, OwnerGoneOutcome::Locked);
        assert!(done.removed);
        assert_eq!(StoredRecord::read(&path), StoredRecord::Absent);
        std::fs::remove_dir_all(&directory).expect("scratch removed");
    }

    #[test]
    fn only_a_new_boot_or_account_removes_another_sessions_record() {
        let directory = scratch("stale");
        let path = directory.join("deck-attachment");
        let recorded = current();
        std::fs::write(&path, recorded.to_text()).expect("record written");
        // A process outside the recorded session, such as one started over
        // SSH, or the account's next login session in this boot.
        let outside = AttachmentRecord::new("100250", BOOT, 501).expect("record");
        assert_eq!(
            recover_record(&path, Some(&outside), || panic!("not its session to lock")),
            RecordRecovery {
                recovery: StartupRecovery::LeaveAlone,
                locked: None,
                removed: false,
            }
        );
        assert_eq!(
            StoredRecord::read(&path),
            StoredRecord::Text(recorded.to_text())
        );
        let next_boot =
            AttachmentRecord::new("100008", "0F1E2D3C-4B5A-6978-8796-A5B4C3D2E1F0", 501)
                .expect("record");
        assert_eq!(
            recover_record(&path, Some(&next_boot), || panic!(
                "stale records lock nothing"
            )),
            RecordRecovery {
                recovery: StartupRecovery::Stale,
                locked: None,
                removed: true,
            }
        );
        assert_eq!(StoredRecord::read(&path), StoredRecord::Absent);
        std::fs::remove_dir_all(&directory).expect("scratch removed");
    }

    const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    fn windows_session(login_session: &str, boot: &str, sid: &str) -> AttachmentRecord {
        AttachmentRecord::for_account(login_session, boot, RecordAccount::sid(sid).expect("sid"))
            .expect("record")
    }

    #[test]
    fn a_windows_record_names_the_account_by_sid_and_parses_strictly() {
        let record = windows_session("wts2-logon-00000000000a1b2c", BOOT, SID);
        let text = record.to_text();
        assert!(text.ends_with(&format!("\nsid {SID}\n")), "{text}");
        assert_eq!(AttachmentRecord::parse(&text), Some(record.clone()));
        // A POSIX record still reads as before.
        assert_eq!(
            AttachmentRecord::parse(
                "arcen-attachment 1\nlogin_session 100008\nboot 6D1F4A52-0C2B-4E43-9E0A-5B8A1C3D2E10\nuid 501\n"
            ),
            Some(current())
        );
        for bad in [
            "S-1",
            "S-",
            "s-1-5-18",
            "S-1-5-x",
            "S-1--5",
            "S-1-5-21-12345678901",
            "1-5-21",
            "S-1-5 18",
        ] {
            assert_eq!(RecordAccount::sid(bad), None, "{bad:?}");
            let text = record.to_text().replace(SID, bad);
            assert_eq!(AttachmentRecord::parse(&text), None, "{bad:?}");
        }
        let longest = format!("S-1{}", "-4294967295".repeat(17));
        assert!(longest.len() > MAX_SID);
        assert_eq!(RecordAccount::sid(&longest), None);
        assert!(RecordAccount::sid("S-1-5-18").is_some());
        assert!(
            AttachmentRecord::for_account("1", BOOT, RecordAccount::Sid("bogus".to_owned()))
                .is_none()
        );
    }

    #[test]
    fn a_service_locks_only_the_live_session_a_record_names_exactly() {
        let record = windows_session("wts2-logon-00000000000a1b2c", BOOT, SID);
        let text = record.to_text();
        let another_user =
            windows_session("wts3-logon-00000000000b0001", BOOT, "S-1-5-21-1-2-3-1002");
        assert_eq!(
            service_startup_recovery(None, std::slice::from_ref(&record)),
            ServiceStartupRecovery::Nothing
        );
        let recovery =
            service_startup_recovery(Some(&text), &[another_user.clone(), record.clone()]);
        assert_eq!(recovery, ServiceStartupRecovery::LockFirst(record.clone()));
        assert_eq!(recovery.departure(), Some(Departure::AgentFailed));
        assert!(recovery.departure().is_some_and(Departure::locks_session));

        // The session ended, or its id came back for another logon of the
        // same account, another account, or another boot: nobody is locked.
        for alive in [
            vec![],
            vec![another_user.clone()],
            vec![windows_session("wts2-logon-00000000000a1b2d", BOOT, SID)],
            vec![windows_session(
                "wts2-logon-00000000000a1b2c",
                BOOT,
                "S-1-5-21-1004336348-1177238915-682003330-1002",
            )],
            vec![windows_session(
                "wts2-logon-00000000000a1b2c",
                "0F1E2D3C4B5A69788796A5B4C3D2E1F0",
                SID,
            )],
        ] {
            let recovery = service_startup_recovery(Some(&text), &alive);
            assert_eq!(recovery, ServiceStartupRecovery::Stale, "{alive:?}");
            assert_eq!(recovery.departure(), None);
        }

        // A record that cannot be read names nobody: locking whoever holds
        // the console could lock an account it never named.
        for unreadable in ["", "garbage", &text[..text.len() - 1]] {
            let recovery =
                service_startup_recovery(Some(unreadable), &[record.clone(), another_user.clone()]);
            assert_eq!(
                recovery,
                ServiceStartupRecovery::Unreadable,
                "{unreadable:?}"
            );
            assert_eq!(recovery.departure(), None);
        }
    }
}
