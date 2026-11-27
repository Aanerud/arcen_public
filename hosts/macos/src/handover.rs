// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

//! The macOS adapter of the shared change-of-account contract
//! (`arcen_session::session_handover`, ADR 0025 §13).
//!
//! The decisions are shared. This module only gathers what macOS says and
//! carries out what the shared ledger allows:
//!
//! - **Who holds the desktop.** The console user record the configuration
//!   daemon keeps (`State:/Users/ConsoleUser`, read with `scutil`): the
//!   account, its window-server session, its audit session and the
//!   session's unique id. With `kern.bootsessionuuid` that is the exact
//!   session a change of account may sign out, and its absence later is the
//!   proof that it ended.
//! - **Whether a remote client is attached.** Arcen's own attachments are
//!   the service's; the other routes this Pier knows are read from
//!   `netstat` ([`foreign_client_connected`]): Screen Sharing, Remote
//!   Management and VNC servers (TCP 5900-5909), the remote workstation
//!   protocol (TCP and UDP 4172) and the default relay ports of third-party
//!   remote-access products. Proved detached means a read of both found no
//!   connected client; a read that fails is unknown. A product on another
//!   port is not seen.
//! - **The graceful logout** (not offered yet, [`GRACEFUL_LOGOUT_ROUTE`]).
//!   Asked by the target account's own agent
//!   ([`agent_relay::ServiceMessage::SignOut`]): `kAEReallyLogOut` to that
//!   session's `loginwindow`, the system's ordinary logout without its
//!   confirmation dialog. Applications are asked to quit and may refuse;
//!   nothing is forced, no process is killed and the gui domain is never
//!   booted out.
//!
//! The network service runs as `_arcen` and never as root; it reads the
//! facts and keeps the one [`SwitchLedger`] of this desktop. It never signs
//! anybody out itself.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_session::direct_reconnect::{AccountPrincipal, MonotonicMillis};
use arcen_session::session_handover::consent::{ClientReturn, ConnectionId, SwitchLedger};
use arcen_session::session_handover::logout::{EntryPermit, SessionPresence};
use arcen_session::session_handover::{
    AttachmentEpoch, AuthenticatedPrincipal, BootId, ConsoleOccupant, DeskState, HandoverDecision,
    HandoverReason, HandoverRefusal, NativePrincipal, NativeSessionId, NativeSessionIdentity,
    RemoteAttachment, RemoteClient, SessionFacts,
};

/// Whether this Pier offers a change of account at all. Off: the graceful
/// logout is not proved on macOS 26, so another account is refused with
/// `PierCannotSignOut` (today's `console_other_user`). Measured on macOS
/// 26.5.2: without `com.apple.security.automation.apple-events` on the
/// helper tccd refuses its Apple events; with it, `loginwindow` received
/// `kAEReallyLogOut` and a new account's first-login session stayed
/// (`logout_vetoed` after two minutes); `osascript` got
/// `errAEEventNotHandled` (-1708) against a locked session. Turn it on
/// only with that entitlement and a lab proof of the whole route.
pub const GRACEFUL_LOGOUT_ROUTE: bool = false;

/// How often the end of a signed-out session is read back.
pub const EXIT_POLL: Duration = Duration::from_secs(1);

/// The window-server account the login window's session runs as.
const LOGIN_WINDOW_NAMES: [&str; 4] = ["root", "loginwindow", "_mbsetupuser", "unknown"];

/// Local ports a remote-access server listens on: Screen Sharing and
/// Remote Management, and any VNC server on display 0 to 9 (5900-5909); the
/// remote workstation protocol (4172).
const SERVER_PORTS: [std::ops::RangeInclusive<u16>; 2] = [5900..=5909, 4172..=4172];
/// Remote ports third-party remote-access products reach their relays on
/// by default (5938, 7070, 21115-21119): an established connection to one
/// is a client that may be attached.
const RELAY_PORTS: [std::ops::RangeInclusive<u16>; 3] = [5938..=5938, 7070..=7070, 21115..=21119];
const WORKSTATION_PROTOCOL_PORT: u16 = 4172;

fn in_ranges(port: Option<u16>, ranges: &[std::ops::RangeInclusive<u16>]) -> bool {
    port.is_some_and(|port| ranges.iter().any(|range| range.contains(&port)))
}

/// One graphical login session, as the console user record states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleRecord {
    /// The account's numeric id.
    pub uid: u32,
    /// The account's short name.
    pub name: String,
    /// The window server's session id (`kCGSSessionIDKey`).
    pub window_session: u32,
    /// The audit session every process of the login shares.
    pub audit_session: u32,
    /// The session's unique id (`CGSSessionUniqueSessionUUID`).
    pub unique: String,
    /// This session has the physical console.
    pub on_console: bool,
    /// The login finished (`kCGSessionLoginDoneKey`).
    pub login_done: bool,
}

impl ConsoleRecord {
    /// The login window's own session, not an account's desktop.
    #[must_use]
    pub fn is_login_window(&self) -> bool {
        !self.login_done || self.uid == 0 || LOGIN_WINDOW_NAMES.contains(&self.name.as_str())
    }
}

/// Parses `scutil`'s `show State:/Users/ConsoleUser`. `None` when any
/// session lacks a field the decision needs: a partial read is not a fact.
#[must_use]
pub fn parse_console_user(text: &str) -> Option<Vec<ConsoleRecord>> {
    let mut records = Vec::new();
    let mut in_sessions = false;
    let mut current: Option<std::collections::HashMap<String, String>> = None;
    let mut saw_sessions = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("SessionInfo : <array>") {
            in_sessions = true;
            saw_sessions = true;
            continue;
        }
        if !in_sessions {
            continue;
        }
        if line.ends_with("<dictionary> {") {
            current = Some(std::collections::HashMap::new());
            continue;
        }
        if line == "}" {
            match current.take() {
                Some(fields) => records.push(record_from(&fields)?),
                None => in_sessions = false,
            }
            continue;
        }
        if let (Some(fields), Some((key, value))) = (current.as_mut(), line.split_once(" : ")) {
            fields.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    (saw_sessions && current.is_none()).then_some(records)
}

fn record_from(fields: &std::collections::HashMap<String, String>) -> Option<ConsoleRecord> {
    let number = |key: &str| fields.get(key)?.parse::<u32>().ok();
    let flag = |key: &str| match fields.get(key).map(String::as_str) {
        Some("TRUE") => Some(true),
        Some("FALSE") => Some(false),
        _ => None,
    };
    let unique = fields.get("CGSSessionUniqueSessionUUID")?.clone();
    if unique.is_empty() {
        return None;
    }
    Some(ConsoleRecord {
        uid: number("kCGSSessionUserIDKey")?,
        name: fields.get("kCGSSessionUserNameKey")?.clone(),
        window_session: number("kCGSSessionIDKey")?,
        audit_session: number("kCGSSessionAuditIDKey")?,
        unique,
        on_console: flag("kCGSSessionOnConsoleKey")?,
        login_done: flag("kCGSessionLoginDoneKey").unwrap_or(false),
    })
}

/// Reads every graphical login session. `None` when it cannot be read.
#[must_use]
pub fn console_records() -> Option<Vec<ConsoleRecord>> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("/usr/sbin/scutil")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(b"show State:/Users/ConsoleUser\n")
        .ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_console_user(&String::from_utf8_lossy(&output.stdout))
}

/// Whether another product's client is connected, from `netstat -an` of
/// TCP (`tcp`) and UDP (`udp`): an established TCP connection to a
/// remote-access server port ([`SERVER_PORTS`]) or to a remote-access
/// relay port ([`RELAY_PORTS`]), or a UDP socket of the workstation
/// protocol connected to a peer. A listening socket is a product
/// installed, not a client attached. A product that carries its sessions
/// on another port (443, say) is not seen: that is this probe's limit.
#[must_use]
pub fn foreign_client_connected(tcp: &str, udp: &str) -> bool {
    let local_port = |address: &str| {
        address
            .rsplit_once('.')
            .and_then(|(_, port)| port.parse::<u16>().ok())
    };
    let tcp_attached = tcp.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        matches!(
            fields.as_slice(),
            [proto, _, _, local, foreign, "ESTABLISHED", ..]
                if proto.starts_with("tcp")
                    && (in_ranges(local_port(local), &SERVER_PORTS)
                        || in_ranges(local_port(foreign), &RELAY_PORTS))
        )
    });
    let udp_attached = udp.lines().any(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        matches!(
            fields.as_slice(),
            [proto, _, _, local, foreign, ..]
                if proto.starts_with("udp")
                    && local_port(local) == Some(WORKSTATION_PROTOCOL_PORT)
                    && *foreign != "*.*"
        )
    });
    tcp_attached || udp_attached
}

fn netstat(protocol: &str) -> Option<String> {
    let output = std::process::Command::new("/usr/sbin/netstat")
        .args(["-an", "-p", protocol])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Another product's client: `Some(true)` attached, `Some(false)` proved
/// absent on every route this Pier knows, `None` unreadable.
#[must_use]
pub fn foreign_attachment() -> Option<bool> {
    Some(foreign_client_connected(&netstat("tcp")?, &netstat("udp")?))
}

/// The attachment fact the shared decision takes.
#[must_use]
pub const fn remote_attachment(arcen_attached: bool, foreign: Option<bool>) -> RemoteAttachment {
    match (arcen_attached, foreign) {
        (true, _) => RemoteAttachment::Attached(RemoteClient::Arcen),
        (false, Some(true)) => RemoteAttachment::Attached(RemoteClient::Foreign),
        (false, Some(false)) => RemoteAttachment::ProvedDetached,
        (false, None) => RemoteAttachment::Unknown,
    }
}

/// An account's short name, as the directory resolves its uid.
#[must_use]
pub fn account_name(uid: u32) -> Option<String> {
    let mut record = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is live for the call and `buffer` is as long as
    // stated; `getpwuid_r` writes only into `record` and `buffer`.
    #[allow(unsafe_code)]
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            record.as_mut_ptr(),
            buffer.as_mut_ptr(),
            buffer.len(),
            &raw mut result,
        )
    };
    if status != 0 || result.is_null() {
        return None;
    }
    // SAFETY: a non-null result points at `record`, now initialised, whose
    // strings live in `buffer`.
    #[allow(unsafe_code)]
    let record = unsafe { record.assume_init_ref() };
    if record.pw_name.is_null() {
        return None;
    }
    // SAFETY: `pw_name` is a NUL-terminated string inside `buffer`.
    #[allow(unsafe_code)]
    let name = unsafe { std::ffi::CStr::from_ptr(record.pw_name) };
    let name = name.to_string_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

/// The native principal of `uid`, named as the directory names it, so the
/// requester and the session's owner are compared the same way.
#[must_use]
pub fn principal(uid: u32) -> Option<NativePrincipal> {
    let name = account_name(uid)?;
    NativePrincipal::from_account(AccountPrincipal {
        name: &name,
        id: u64::from(uid),
    })
    .ok()
}

/// The exact session `record` is: its account, window-server session and
/// unique id, this boot, and its audit session as the generation (a new
/// login gets a new one even where a window-server id is reused).
#[must_use]
pub fn session_identity(record: &ConsoleRecord, boot: &BootId) -> Option<NativeSessionIdentity> {
    Some(NativeSessionIdentity::new(
        principal(record.uid)?,
        NativeSessionId::new(format!("{}-{}", record.window_session, record.unique)).ok()?,
        boot.clone(),
        u64::from(record.audit_session),
    ))
}

/// This boot (`kern.bootsessionuuid`).
#[must_use]
pub fn boot_id() -> Option<BootId> {
    BootId::new(crate::lock_guard::boot_session()?).ok()
}

/// Who holds the desktop, from records read once: the login window's
/// session is nobody; a single account's session on the console is that
/// session; anything else is unknown.
#[must_use]
pub fn occupant_from(
    records: Option<&[ConsoleRecord]>,
    boot: Option<&BootId>,
    attachment: RemoteAttachment,
    epoch: AttachmentEpoch,
) -> (ConsoleOccupant, Option<ConsoleRecord>) {
    let (Some(records), Some(boot)) = (records, boot) else {
        return (ConsoleOccupant::Unknown, None);
    };
    let mut on_console = records.iter().filter(|record| record.on_console);
    let (Some(record), None) = (on_console.next(), on_console.next()) else {
        return (ConsoleOccupant::Unknown, None);
    };
    if record.is_login_window() {
        return (ConsoleOccupant::Nobody, None);
    }
    match session_identity(record, boot) {
        Some(identity) => (
            ConsoleOccupant::Session(SessionFacts {
                identity,
                attachment,
                epoch,
                desk: DeskState::Unknown,
            }),
            Some(record.clone()),
        ),
        None => (ConsoleOccupant::Unknown, None),
    }
}

/// Whether `target` still exists among `records`: by account, window-server
/// session, unique id and audit session, on this boot.
#[must_use]
pub fn presence_in(
    records: Option<&[ConsoleRecord]>,
    boot: Option<&BootId>,
    target: &NativeSessionIdentity,
) -> SessionPresence {
    let (Some(records), Some(boot)) = (records, boot) else {
        return SessionPresence::Unknown;
    };
    if boot != target.boot() {
        // Another boot: every session of the earlier one has ended.
        return SessionPresence::Gone;
    }
    let mut unresolved = false;
    let present = records.iter().any(|record| {
        let identity = session_identity(record, boot);
        unresolved |= identity.is_none();
        identity.is_some_and(|identity| identity == *target)
    });
    match (present, unresolved) {
        (true, _) => SessionPresence::Present,
        (false, false) => SessionPresence::Gone,
        (false, true) => SessionPresence::Unknown,
    }
}

/// The one change-of-account ledger of this Mac's desktop, with the
/// attachment epoch, kept by the network service.
#[derive(Debug)]
pub struct HandoverDesk {
    ledger: Mutex<SwitchLedger>,
    epoch: AtomicU64,
    attached: Mutex<std::collections::HashSet<u64>>,
    permit: Mutex<Option<EntryPermit>>,
    started: Instant,
}

impl Default for HandoverDesk {
    fn default() -> Self {
        Self::new()
    }
}

impl HandoverDesk {
    /// An idle desk: a restarted service forgets every offer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ledger: Mutex::new(SwitchLedger::new()),
            epoch: AtomicU64::new(0),
            attached: Mutex::new(std::collections::HashSet::new()),
            permit: Mutex::new(None),
            started: Instant::now(),
        }
    }

    /// The ledger's clock.
    #[must_use]
    pub fn now(&self) -> MonotonicMillis {
        MonotonicMillis::new(self.started.elapsed().as_millis())
    }

    /// The current attachment epoch.
    #[must_use]
    pub fn epoch(&self) -> AttachmentEpoch {
        AttachmentEpoch::new(self.epoch.load(Ordering::SeqCst))
    }

    /// Runs `body` on the ledger.
    pub fn with_ledger<T>(&self, body: impl FnOnce(&mut SwitchLedger) -> T) -> Option<T> {
        self.ledger.lock().ok().map(|mut ledger| body(&mut ledger))
    }

    /// A Deck was attached (`session`, its lanes granted): the epoch moves
    /// and an open offer is withdrawn.
    pub fn attached(&self, session: u64) -> Option<ClientReturn> {
        if let Ok(mut attached) = self.attached.lock() {
            attached.insert(session);
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.with_ledger(SwitchLedger::remote_client_returned)
    }

    /// A relayed Deck ended. Only an attached one moves the epoch.
    pub fn detached(&self, session: u64) {
        let was_attached = self
            .attached
            .lock()
            .is_ok_and(|mut attached| attached.remove(&session));
        if was_attached {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Whether an Arcen Deck other than `session` is attached. A poisoned
    /// set reads as attached: the safe answer.
    #[must_use]
    pub fn arcen_attached_besides(&self, session: u64) -> bool {
        self.attached.lock().map_or(true, |attached| {
            attached.iter().any(|other| *other != session)
        })
    }

    /// Reads who holds the desktop, as `session` sees it. Blocking.
    #[must_use]
    pub fn read_occupant(&self, session: u64) -> (ConsoleOccupant, Option<ConsoleRecord>) {
        let epoch = self.epoch();
        let arcen = self.arcen_attached_besides(session);
        let foreign = if arcen {
            Some(false)
        } else {
            foreign_attachment()
        };
        let records = console_records();
        occupant_from(
            records.as_deref(),
            boot_id().as_ref(),
            remote_attachment(arcen, foreign),
            epoch,
        )
    }

    /// Keeps the requester's entry permit until its sign-in is decided.
    pub fn hold_permit(&self, permit: EntryPermit) {
        if let Ok(mut held) = self.permit.lock() {
            *held = Some(permit);
        }
    }

    /// The requester on `session` reached the login window, or did not.
    pub fn login_finished(&self, session: u64, signed_in: bool) -> Option<HandoverReason> {
        let permit = self.permit.lock().ok().and_then(|mut held| {
            if held
                .as_ref()
                .is_some_and(|permit| permit.connection() == ConnectionId::new(session))
            {
                held.take()
            } else {
                None
            }
        })?;
        self.with_ledger(|ledger| ledger.login_finished(permit, signed_in))
            .flatten()
    }

    /// The relayed Deck on `session` went away.
    pub fn requester_left(&self, session: u64) -> Option<HandoverReason> {
        let left = self
            .with_ledger(|ledger| ledger.requester_left(ConnectionId::new(session)))
            .flatten();
        let _ = self.login_finished(session, false);
        left
    }

    /// Whether a Deck the agent of `agent_uid` (`None`: the login window's)
    /// just signed in may be attached now. The ledger decides
    /// (`SwitchLedger::decide`): the account's own session is joined
    /// whatever is attached, but never while it is being signed out, and
    /// nobody but the requester enters while a change of account holds the
    /// desktop. An idle ledger holds nothing, so the agent's own check
    /// (it serves only its own account) stands as before.
    ///
    /// # Errors
    ///
    /// Returns the shared refusal that keeps the Deck out.
    pub fn admit(&self, session: u64, agent_uid: Option<u32>) -> Result<(), HandoverRefusal> {
        let idle = self.with_ledger(|ledger| ledger.is_idle()).unwrap_or(false);
        if idle {
            return Ok(());
        }
        let connection = ConnectionId::new(session);
        let Some(uid) = agent_uid else {
            // The login window serves anybody: only a reserved entry for
            // another connection holds it.
            let held = self
                .with_ledger(|ledger| {
                    matches!(
                        ledger.hold(connection),
                        arcen_session::session_handover::consent::ConsoleHold::Entering(_)
                            | arcen_session::session_handover::consent::ConsoleHold::SigningOut(_)
                    )
                })
                .unwrap_or(true);
            return if held {
                Err(HandoverRefusal::SwitchInProgress)
            } else {
                Ok(())
            };
        };
        let requester = principal(uid)
            .map(AuthenticatedPrincipal::after_authentication)
            .ok_or(HandoverRefusal::OccupantUnknown)?;
        let (occupant, _) = self.read_occupant(session);
        let now = self.now();
        let decided = self
            .with_ledger(|ledger| {
                ledger.decide(
                    connection,
                    &requester,
                    &occupant,
                    arcen_session::session_handover::SwitchRoute::PierCannotSignOut,
                    now,
                )
            })
            .ok_or(HandoverRefusal::OccupantUnknown)?;
        match decided.decision {
            HandoverDecision::JoinOwnSession { .. }
            | HandoverDecision::JoinOwnSessionAfterIncumbent(_) => {
                // The owner is back: an offer against this session is
                // withdrawn now, before the attachment is set up.
                let _ = self.with_ledger(SwitchLedger::remote_client_returned);
                Ok(())
            }
            HandoverDecision::Refuse(refusal) => Err(refusal),
            HandoverDecision::SignInFresh | HandoverDecision::OfferSwitch(_) => {
                Err(HandoverRefusal::SwitchInProgress)
            }
        }
    }
}

/// What a parked agent answers when the service asks it for its session's
/// logout while this Pier offers no change of account.
pub const SIGN_OUT_REFUSED: i32 = -1;

/// Answers the service's request for this session's logout: refused
/// ([`SIGN_OUT_REFUSED`]) without asking anybody while
/// [`GRACEFUL_LOGOUT_ROUTE`] is off, the request's native status otherwise.
#[must_use]
pub fn sign_out_on_request() -> i32 {
    if GRACEFUL_LOGOUT_ROUTE {
        request_graceful_logout()
    } else {
        SIGN_OUT_REFUSED
    }
}

/// Asks this session's `loginwindow` for a graceful logout
/// (`kAEReallyLogOut`: the system's own logout, without its confirmation
/// dialog; applications may still refuse). Returns the native status: 0
/// means the request was delivered, not that the session ended.
#[must_use]
pub fn request_graceful_logout() -> i32 {
    // Never from a test: it would sign the person running it out.
    if cfg!(test) {
        return 0;
    }
    native::send_to_login_window(native::LOGOUT_EVENT)
}

/// Whether this agent has the logout route: the Pier offers it
/// ([`GRACEFUL_LOGOUT_ROUTE`]) and this account's `loginwindow` is found
/// and can be addressed. The Automation decision itself cannot be read beforehand:
/// `AEDeterminePermissionToAutomateTarget` does not return for
/// `loginwindow` (measured on macOS 26), so the request's own status is
/// the answer, and a refused one fails the change of account visibly.
#[must_use]
pub fn logout_route_present() -> bool {
    GRACEFUL_LOGOUT_ROUTE && native::login_window_addressable()
}

#[cfg(target_os = "macos")]
mod native {
    #![allow(unsafe_code)]

    use std::ffi::c_void;

    /// `kCoreEventClass`, `kAEReallyLogOut`.
    pub(super) const LOGOUT_EVENT: (u32, u32) =
        (u32::from_be_bytes(*b"aevt"), u32::from_be_bytes(*b"rlgo"));
    const TYPE_KERNEL_PROCESS_ID: u32 = u32::from_be_bytes(*b"kpid");
    /// `PROC_UID_ONLY` of `libproc.h`.
    const PROC_UID_ONLY: u32 = 4;
    const K_AUTO_GENERATE_RETURN_ID: i16 = -1;
    const K_ANY_TRANSACTION_ID: i32 = 0;
    const K_AE_NO_REPLY: i32 = 0x0000_0001;
    const K_AE_DEFAULT_TIMEOUT: std::ffi::c_long = -1;

    /// `AEDesc` as `AEDataModel.h` lays it out: two-byte packed (mac68k
    /// alignment), twelve bytes with the handle at offset 4. A naturally
    /// aligned copy puts the handle at offset 8, and the Apple Event Manager
    /// then reads half a pointer (measured: the agent died on its first
    /// request).
    #[repr(C, packed(2))]
    struct AEDesc {
        descriptor_type: u32,
        data_handle: *mut c_void,
    }

    const _: () = assert!(std::mem::size_of::<AEDesc>() == 12);

    impl AEDesc {
        const fn null() -> Self {
            Self {
                descriptor_type: u32::from_be_bytes(*b"null"),
                data_handle: std::ptr::null_mut(),
            }
        }
    }

    // SAFETY: declared to match the Apple Event Manager's published C
    // signatures (CoreServices, AE.framework): `AEDesc` is a type code and a
    // handle; `Size` is `long`; `AEReturnID` is `SInt16`; `AETransactionID`
    // and `AESendMode` are `SInt32`; `Boolean` is one byte.
    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn AECreateDesc(
            type_code: u32,
            data: *const c_void,
            size: isize,
            result: *mut AEDesc,
        ) -> i16;
        fn AECreateAppleEvent(
            event_class: u32,
            event_id: u32,
            target: *const AEDesc,
            return_id: i16,
            transaction_id: i32,
            result: *mut AEDesc,
        ) -> i16;
        fn AESendMessage(
            event: *const AEDesc,
            reply: *mut AEDesc,
            send_mode: i32,
            timeout_ticks: std::ffi::c_long,
        ) -> i32;
        fn AEDisposeDesc(desc: *mut AEDesc) -> i16;
    }

    /// The pid of this account's `loginwindow`: the one process of that
    /// name the account runs, in its own login session. The Apple Event
    /// daemon does not register `loginwindow` by bundle id (measured on
    /// macOS 26: `procNotFound`), so it is addressed by pid.
    fn login_window_pid() -> Option<i32> {
        // SAFETY: `getuid` has no preconditions.
        let uid = unsafe { libc::getuid() };
        let mut pids = vec![0_i32; 8192];
        let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>()).ok()?;
        // SAFETY: the buffer is `bytes` long; the call writes at most that.
        let written =
            unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), bytes) };
        let count = usize::try_from(written).ok()? / std::mem::size_of::<i32>();
        pids.truncate(count.min(pids.len()));
        let mut found = pids.into_iter().filter(|pid| *pid > 0).filter(|pid| {
            let mut name = [0_u8; 64];
            // SAFETY: the buffer is as long as stated.
            let length = unsafe { libc::proc_name(*pid, name.as_mut_ptr().cast(), 64) };
            usize::try_from(length)
                .ok()
                .and_then(|length| name.get(..length))
                .is_some_and(|name| name == b"loginwindow")
        });
        // Exactly one: two would be a state this does not guess about.
        match (found.next(), found.next()) {
            (Some(pid), None) => Some(pid),
            _ => None,
        }
    }

    /// The `loginwindow` of this account's own login session.
    fn login_window() -> Result<AEDesc, i32> {
        const PROC_NOT_FOUND: i32 = -600;
        let pid = login_window_pid().ok_or(PROC_NOT_FOUND)?;
        let mut target = AEDesc::null();
        // SAFETY: `pid` is live for the call, which copies it into the new
        // descriptor.
        let status = unsafe {
            AECreateDesc(
                TYPE_KERNEL_PROCESS_ID,
                (&raw const pid).cast(),
                isize::try_from(std::mem::size_of::<i32>()).unwrap_or(4),
                &raw mut target,
            )
        };
        if status == 0 {
            Ok(target)
        } else {
            Err(i32::from(status))
        }
    }

    pub(super) fn login_window_addressable() -> bool {
        match login_window() {
            Ok(mut target) => {
                // SAFETY: disposes the descriptor created above, once.
                unsafe { AEDisposeDesc(&raw mut target) };
                true
            }
            Err(_) => false,
        }
    }

    pub(super) fn send_to_login_window((class, id): (u32, u32)) -> i32 {
        let mut target = match login_window() {
            Ok(target) => target,
            Err(status) => return status,
        };
        let mut event = AEDesc::null();
        // SAFETY: a live target; the event is written to `event`.
        let created = unsafe {
            AECreateAppleEvent(
                class,
                id,
                &raw const target,
                K_AUTO_GENERATE_RETURN_ID,
                K_ANY_TRANSACTION_ID,
                &raw mut event,
            )
        };
        let status = if created == 0 {
            // SAFETY: a live event; no reply is asked for, so the reply
            // descriptor may be null.
            let sent = unsafe {
                AESendMessage(
                    &raw const event,
                    std::ptr::null_mut(),
                    K_AE_NO_REPLY,
                    K_AE_DEFAULT_TIMEOUT,
                )
            };
            // SAFETY: disposes the event created above, once.
            unsafe { AEDisposeDesc(&raw mut event) };
            sent
        } else {
            i32::from(created)
        };
        // SAFETY: disposes the target created above, once.
        unsafe { AEDisposeDesc(&raw mut target) };
        status
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    pub(super) const LOGOUT_EVENT: (u32, u32) = (0, 0);

    pub(super) fn login_window_addressable() -> bool {
        false
    }

    pub(super) fn send_to_login_window(_event: (u32, u32)) -> i32 {
        -1
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    const SIGNED_IN: &str = "<dictionary> {
  GID : 20
  Name : alice
  SessionInfo : <array> {
    0 : <dictionary> {
      CGSSessionUniqueSessionUUID : BFBA0E99-3841-4A36-922D-7D5167443DE6
      kCGSSessionAuditIDKey : 100017
      kCGSSessionGroupIDKey : 20
      kCGSSessionIDKey : 257
      kCGSSessionLoginwindowSafeLogin : FALSE
      kCGSSessionOnConsoleKey : TRUE
      kCGSSessionUserIDKey : 501
      kCGSSessionUserNameKey : alice
      kCGSessionLoginDoneKey : TRUE
      kSCSecuritySessionID : 100017
    }
  }
  UID : 501
}
";

    const LOGIN_WINDOW: &str = "<dictionary> {
  SessionInfo : <array> {
    0 : <dictionary> {
      CGSSessionUniqueSessionUUID : 9195E2F0-128F-401B-A6C6-BB4D2B4938D7
      kCGSSessionAuditIDKey : 101790
      kCGSSessionIDKey : 257
      kCGSSessionOnConsoleKey : TRUE
      kCGSSessionUserIDKey : 88
      kCGSSessionUserNameKey : unknown
      kCGSessionLoginDoneKey : FALSE
    }
  }
}
";

    #[test]
    fn the_console_user_record_names_the_exact_session() {
        let records = parse_console_user(SIGNED_IN).expect("parsed");
        assert_eq!(
            records,
            vec![ConsoleRecord {
                uid: 501,
                name: "alice".to_owned(),
                window_session: 257,
                audit_session: 100_017,
                unique: "BFBA0E99-3841-4A36-922D-7D5167443DE6".to_owned(),
                on_console: true,
                login_done: true,
            }]
        );
        assert!(!records[0].is_login_window());
        let login = parse_console_user(LOGIN_WINDOW).expect("parsed");
        assert!(login[0].is_login_window());
    }

    #[test]
    fn a_partial_or_missing_record_is_not_a_fact() {
        assert_eq!(parse_console_user(""), None);
        assert_eq!(parse_console_user("  No such key\n"), None);
        let without_unique = SIGNED_IN.replace(
            "CGSSessionUniqueSessionUUID : BFBA0E99-3841-4A36-922D-7D5167443DE6\n",
            "",
        );
        assert_eq!(parse_console_user(&without_unique), None);
        let truncated: String = SIGNED_IN.lines().take(8).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_console_user(&truncated), None);
    }

    #[test]
    fn the_login_window_on_the_console_is_nobody_and_an_unreadable_one_unknown() {
        let boot = BootId::new("4C0E5AA1-1111-2222-3333-444455556666").expect("boot");
        let login = parse_console_user(LOGIN_WINDOW).expect("parsed");
        let (occupant, record) = occupant_from(
            Some(&login),
            Some(&boot),
            RemoteAttachment::ProvedDetached,
            AttachmentEpoch::new(0),
        );
        assert_eq!(occupant, ConsoleOccupant::Nobody);
        assert!(record.is_none());
        let (occupant, _) = occupant_from(
            None,
            Some(&boot),
            RemoteAttachment::ProvedDetached,
            AttachmentEpoch::new(0),
        );
        assert_eq!(occupant, ConsoleOccupant::Unknown);
        let (occupant, _) = occupant_from(
            Some(&[]),
            Some(&boot),
            RemoteAttachment::ProvedDetached,
            AttachmentEpoch::new(0),
        );
        assert_eq!(occupant, ConsoleOccupant::Unknown);
    }

    #[test]
    fn a_signed_out_session_is_gone_only_when_its_exact_record_is() {
        // The current user exists on every Mac this test runs on.
        let uid = crate::service::current_uid();
        let name = account_name(uid).expect("this account resolves");
        let boot = BootId::new("4C0E5AA1-1111-2222-3333-444455556666").expect("boot");
        let mine = SIGNED_IN
            .replace(
                "kCGSSessionUserIDKey : 501",
                &format!("kCGSSessionUserIDKey : {uid}"),
            )
            .replace(
                "kCGSSessionUserNameKey : alice",
                &format!("kCGSSessionUserNameKey : {name}"),
            );
        let records = parse_console_user(&mine).expect("parsed");
        let target = session_identity(&records[0], &boot).expect("identity");
        assert_eq!(
            presence_in(Some(&records), Some(&boot), &target),
            SessionPresence::Present
        );
        // A new login of the same account, even with the same window-server
        // id, is another session.
        let relogin = mine
            .replace("100017", "100342")
            .replace("BFBA0E99", "0A1B2C3D");
        let records = parse_console_user(&relogin).expect("parsed");
        assert_eq!(
            presence_in(Some(&records), Some(&boot), &target),
            SessionPresence::Gone
        );
        let login = parse_console_user(LOGIN_WINDOW).expect("parsed");
        assert_eq!(
            presence_in(Some(&login), Some(&boot), &target),
            SessionPresence::Gone
        );
        assert_eq!(
            presence_in(None, Some(&boot), &target),
            SessionPresence::Unknown
        );
    }

    #[test]
    fn only_a_connected_client_of_a_known_product_is_attached() {
        let listening_tcp = "Proto Recv-Q Send-Q  Local Address  Foreign Address  (state)
tcp46      0      0  *.4172                 *.*                    LISTEN
tcp4       0      0  *.5900                 *.*                    LISTEN
tcp4       0    436  203.0.113.104.22     203.0.113.1.54798    ESTABLISHED
";
        let listening_udp = "udp4       0      0  *.4172                 *.*
udp4       0      0  *.18444                *.*
";
        assert!(!foreign_client_connected(listening_tcp, listening_udp));
        let screen_sharing =
            format!("{listening_tcp}tcp4 0 0 203.0.113.104.5900 203.0.113.7.50111 ESTABLISHED\n");
        assert!(foreign_client_connected(&screen_sharing, listening_udp));
        let workstation_tcp =
            format!("{listening_tcp}tcp6 0 0 fe80::1.4172 fe80::7.50111 ESTABLISHED\n");
        assert!(foreign_client_connected(&workstation_tcp, listening_udp));
        let workstation_udp =
            format!("{listening_udp}udp4 0 0 203.0.113.104.4172 203.0.113.7.50112\n");
        assert!(foreign_client_connected(listening_tcp, &workstation_udp));
        let relay =
            format!("{listening_tcp}tcp4 0 0 203.0.113.104.50200 198.51.100.9.5938 ESTABLISHED\n");
        assert!(foreign_client_connected(&relay, listening_udp));
        let vnc =
            format!("{listening_tcp}tcp4 0 0 203.0.113.104.5901 203.0.113.7.50114 ESTABLISHED\n");
        assert!(foreign_client_connected(&vnc, listening_udp));
        // Arcen's own port is not another product.
        let arcen = "udp4 0 0 203.0.113.104.18444 203.0.113.7.50113\n";
        assert!(!foreign_client_connected(listening_tcp, arcen));
    }

    #[test]
    fn attachment_is_proved_detached_only_with_every_route_read() {
        assert_eq!(
            remote_attachment(true, None),
            RemoteAttachment::Attached(RemoteClient::Arcen)
        );
        assert_eq!(
            remote_attachment(false, Some(true)),
            RemoteAttachment::Attached(RemoteClient::Foreign)
        );
        assert_eq!(
            remote_attachment(false, Some(false)),
            RemoteAttachment::ProvedDetached
        );
        assert_eq!(remote_attachment(false, None), RemoteAttachment::Unknown);
    }

    #[test]
    fn every_attach_and_detach_moves_the_epoch() {
        let desk = HandoverDesk::new();
        let first = desk.epoch();
        assert!(!desk.arcen_attached_besides(1));
        let _ = desk.attached(1);
        assert_ne!(desk.epoch(), first);
        assert!(!desk.arcen_attached_besides(1));
        assert!(desk.arcen_attached_besides(2));
        let attached = desk.epoch();
        desk.detached(1);
        assert_ne!(desk.epoch(), attached);
        // A Deck that never attached moves nothing.
        let detached = desk.epoch();
        desk.detached(9);
        assert_eq!(desk.epoch(), detached);
    }
}
