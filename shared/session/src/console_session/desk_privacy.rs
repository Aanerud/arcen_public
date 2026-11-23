// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The desk kept private while a Deck is attached to the user's own
//! session, on a host whose window server filters input event by event and
//! sets each display's transfer table (the macOS Pier; ADR 0025 §5, GAP-014
//! closed). The Linux Pier grabs evdev devices and zeroes `RandR` ramps
//! ([`super::disables_while_attached`]); the rules here are the same rules
//! for a host that can only see events and displays.
//!
//! - **Input:** an event filter sees who posted each event, not which
//!   device made it. Only the Pier's own process passes. Hardware events,
//!   the events of a virtual HID device (the window server cannot tell one
//!   from a desk keyboard), and other processes' synthetic events (an
//!   on-screen keyboard at the desk is the desk's input) are dropped. So a
//!   session whose Deck input travels through virtual HID devices cannot be
//!   kept private ([`DeckInputRoute`]).
//! - **Displays:** every display but the Pier's own virtual displays is
//!   given an all-zero transfer table, proved by reading it back
//!   ([`transfer_table_is_dark`]); capture reads the composited desktop
//!   before the table, so the Deck's picture is unchanged.
//! - **Hand-back:** the shared [`super::DETACH_STEPS`]: lock, the lock
//!   screen proved, outputs, sound, input. A session that would not lock
//!   stays private, retried, for at most [`super::UNLOCKED_GRACE`]
//!   ([`hand_back_unlocked`]).
//!
//! The policy every Pier shares is here too: desk privacy is on unless the
//! administrator sets `privacy.desk = "off"` ([`DeskPrivacyPolicy`]), it
//! applies to every desktop the desk's monitors show ([`DeskExposure`]),
//! and the Deck is told the desk's state ([`DeskPrivacyStatus`]).

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Who posted an input event, as an event filter can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPoster {
    /// The Pier's own process (its posted events).
    ThisPier,
    /// The window server's own input: a hardware device or a virtual HID
    /// device, which it cannot tell apart.
    HidSystem,
    /// Another process's synthetic events.
    OtherProcess,
}

impl EventPoster {
    /// Classifies an event by the process id the window server recorded for
    /// its poster (`0` for its own HID input).
    #[must_use]
    pub const fn from_pid(poster_pid: i64, pier_pid: i64) -> Self {
        if poster_pid == 0 {
            Self::HidSystem
        } else if poster_pid == pier_pid {
            Self::ThisPier
        } else {
            Self::OtherProcess
        }
    }
}

/// Whether an input event reaches the desktop while a Deck is attached and
/// the desk is private: only the Pier's own.
#[must_use]
pub const fn event_passes_while_attached(poster: EventPoster) -> bool {
    matches!(poster, EventPoster::ThisPier)
}

/// What the host knows about one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskDisplayFact {
    /// One of the Pier's own virtual displays (the Deck's displays).
    pub pier_virtual: bool,
}

/// Whether a display goes dark while a Deck is attached: every display but
/// the Pier's own virtual ones (a mirrored or wireless display shows the
/// desktop to someone too).
#[must_use]
pub const fn blanks_while_attached(display: DeskDisplayFact) -> bool {
    !display.pier_virtual
}

/// The largest transfer-table output that still counts as dark.
pub const DARK_TABLE_LIMIT: f32 = 1.0 / 1024.0;

/// Whether a transfer table read back from a display is dark: it has
/// entries, and every one is at most [`DARK_TABLE_LIMIT`].
#[must_use]
pub fn transfer_table_is_dark(entries: &[f32]) -> bool {
    !entries.is_empty()
        && entries
            .iter()
            .all(|entry| entry.is_finite() && *entry <= DARK_TABLE_LIMIT)
}

/// How the Deck's input reaches the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckInputRoute {
    /// Events the Pier posts itself.
    PostedEvents,
    /// Keys (or more) through the Pier's virtual HID devices.
    VirtualHidDevices,
}

/// Why the desk cannot be made private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacyRefusal {
    /// The Deck's input goes through virtual HID devices, which an event
    /// filter cannot tell from the desk's own.
    VirtualHidInput,
    /// The input filter could not be installed (the Pier's process lacks
    /// the Accessibility grant, or the window server refused it).
    InputFilterUnavailable,
    /// A display could not be darkened, or did not read back dark.
    DisplaysNotDark,
    /// A monitor at the desk shows the desktop and this host cannot darken
    /// it: the desk is private only when no lit output has a monitor behind
    /// it ([`desk_outputs_dark`]).
    DeskMonitorLit,
    /// Whether an output has a monitor at the desk cannot be told.
    DeskMonitorUnknown,
    /// The set of lit outputs changed after the desk was proved dark (a
    /// monitor plugged in, the desktop extended onto it).
    DeskOutputsChanged,
    /// The desk's keyboard and mouse could not be blocked.
    InputBlockUnavailable,
    /// The desk's monitors could not be detached from the desktop (a host
    /// that cannot darken a monitor serves the Deck on a display no monitor
    /// at the desk shows and detaches the rest).
    DeskDetachFailed,
}

impl DeskPrivacyRefusal {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::VirtualHidInput => "virtual_hid_input",
            Self::InputFilterUnavailable => "input_filter_unavailable",
            Self::DisplaysNotDark => "displays_not_dark",
            Self::DeskMonitorLit => "desk_monitor_lit",
            Self::DeskMonitorUnknown => "desk_monitor_unknown",
            Self::DeskOutputsChanged => "desk_outputs_changed",
            Self::InputBlockUnavailable => "input_block_unavailable",
            Self::DeskDetachFailed => "desk_detach_failed",
        }
    }

    /// What the Deck is told (a close reason: at most 120 bytes), led by
    /// the shared `console_privacy_unavailable` code, then this cause.
    #[must_use]
    pub fn deck_reason(self) -> String {
        format!(
            "{}: {}: the host's screens or input could not be made private",
            super::ConsoleRefusal::PrivacyUnavailable.token(),
            self.token()
        )
    }
}

/// Whether the desk can be kept private for a session whose Deck input
/// takes `route`.
///
/// # Errors
///
/// [`DeskPrivacyRefusal::VirtualHidInput`] for virtual HID input.
pub const fn admits_input_route(route: DeckInputRoute) -> Result<(), DeskPrivacyRefusal> {
    match route {
        DeckInputRoute::PostedEvents => Ok(()),
        DeckInputRoute::VirtualHidDevices => Err(DeskPrivacyRefusal::VirtualHidInput),
    }
}

/// How often a private desk's displays are proved dark again: a display
/// reconfiguration (a display plugged in, a mode change, wake) gives every
/// display its own table back, and the desk sees the session until the next
/// check.
pub const DISPLAY_CHECK: Duration = Duration::from_millis(100);

/// How often the console's holder is read while the desk is private.
pub const CONSOLE_CHECK: Duration = Duration::from_secs(1);

/// Consecutive console readings naming someone else before the desk is
/// handed to them: one failed reading is not a switch of account.
pub const CONSOLE_AWAY_READINGS: u32 = 3;

/// How long a desk display may stay undarkened before the Deck's session is
/// ended: privacy that cannot be kept is not kept by streaming on.
pub const NOT_DARK_LIMIT: Duration = Duration::from_secs(2);

/// What the watcher of a private desk does after an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchVerdict {
    /// The desk is private, or may still become so; keep watching.
    Hold,
    /// The console belongs to another account: give the desk to it and end
    /// the Deck's session.
    ConsoleLeft,
    /// A desk display could not be kept dark for [`NOT_DARK_LIMIT`]: end
    /// the Deck's session (it is locked before the desk gets anything back).
    PrivacyLost,
}

impl WatchVerdict {
    /// What the Deck whose attachment to `serving_uid`'s session this
    /// verdict ends is told, `holder` being who holds the console as it
    /// ends; `None` for [`Self::Hold`].
    ///
    /// A console that left the account is not a privacy failure: the desk
    /// was private until then, and its displays and input now belong to
    /// whoever holds it (ADR 0025 §4). Another account's session in front
    /// is [`super::ConsoleRefusal::OtherUser`], as the same Deck connecting
    /// now is told; the login window in front (the account's session gone
    /// to the background, a switch of user under way) is
    /// [`super::ConsoleRefusal::SessionChanged`]: signing in again reaches
    /// that login window. Only a display that stayed lit is
    /// [`super::ConsoleRefusal::PrivacyUnavailable`].
    #[must_use]
    pub const fn deck_refusal(
        self,
        serving_uid: u32,
        holder: crate::agent_relay::ConsoleHolder,
    ) -> Option<super::ConsoleRefusal> {
        use super::ConsoleRefusal;
        use crate::agent_relay::ConsoleHolder;
        match (self, holder) {
            (Self::Hold, _) => None,
            (Self::PrivacyLost, _) => Some(ConsoleRefusal::PrivacyUnavailable),
            (Self::ConsoleLeft, ConsoleHolder::User(uid)) if uid != serving_uid => {
                Some(ConsoleRefusal::OtherUser)
            }
            (Self::ConsoleLeft, _) => Some(ConsoleRefusal::SessionChanged),
        }
    }
}

/// What `SESSION_END` reports for a Deck session the desk watch ended.
///
/// The watch ends a session by dropping its stream mid-flight, so the
/// stream's own statistics are gone: `frames_sent` is the count the stream
/// kept where the session could still read it, never zero for a session
/// that streamed. A console that left the account (`console_left`) is not a
/// privacy failure (`privacy_lost`, also when the verdict was not kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchSessionEnd {
    /// `SESSION_END.reason_class`.
    pub reason_class: &'static str,
    /// `SESSION_END.frames_sent`.
    pub frames_sent: u64,
}

impl WatchSessionEnd {
    /// The report for a session ended on `verdict` after its stream sent
    /// `frames_sent` frames.
    #[must_use]
    pub const fn new(verdict: Option<WatchVerdict>, frames_sent: u64) -> Self {
        let reason_class = match verdict {
            Some(WatchVerdict::ConsoleLeft) => "console_left",
            Some(WatchVerdict::PrivacyLost | WatchVerdict::Hold) | None => "privacy_lost",
        };
        Self {
            reason_class,
            frames_sent,
        }
    }
}

/// The watcher's memory between observations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrivacyWatch {
    away_readings: u32,
    not_dark_since: Option<Instant>,
}

impl PrivacyWatch {
    /// One reading of the console: whether it still belongs to the session's
    /// account.
    pub fn console(&mut self, ours: bool) -> WatchVerdict {
        if ours {
            self.away_readings = 0;
            return WatchVerdict::Hold;
        }
        self.away_readings = self.away_readings.saturating_add(1);
        if self.away_readings >= CONSOLE_AWAY_READINGS {
            WatchVerdict::ConsoleLeft
        } else {
            WatchVerdict::Hold
        }
    }

    /// One proof of the displays at `now`: whether every desk display read
    /// back dark (a failed display list counts as not dark).
    pub fn displays(&mut self, dark: bool, now: Instant) -> WatchVerdict {
        if dark {
            self.not_dark_since = None;
            return WatchVerdict::Hold;
        }
        let since = *self.not_dark_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= NOT_DARK_LIMIT {
            WatchVerdict::PrivacyLost
        } else {
            WatchVerdict::Hold
        }
    }
}

/// How often a session that would not lock is asked to lock again while the
/// desk stays private.
pub const LOCK_RETRY: Duration = Duration::from_secs(5);

/// What happens to the desk when its session did not lock as the Deck left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockedHandBack {
    /// Keep it private and ask the session to lock again after
    /// [`LOCK_RETRY`].
    KeepPrivate,
    /// Give the desk back unlocked: the session has refused to lock for
    /// [`super::UNLOCKED_GRACE`] (its lock screen is off by policy), and
    /// its owner at the desk must get it back.
    GiveBack,
}

/// [`UnlockedHandBack`] for a session that has refused to lock for
/// `unlocked_for`.
#[must_use]
pub fn hand_back_unlocked(unlocked_for: Duration) -> UnlockedHandBack {
    if unlocked_for >= super::UNLOCKED_GRACE {
        UnlockedHandBack::GiveBack
    } else {
        UnlockedHandBack::KeepPrivate
    }
}

/// Whether a monitor at the desk is behind one lit output, as a host that
/// cannot darken a monitor tells it (the Windows Pier: no zero transfer
/// table reaches the panel, so the desk is dark only when nothing at it is
/// lit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskMonitor {
    /// No monitor: a virtual display of the Pier's own, a GPU head with
    /// nothing physically connected.
    Absent,
    /// A monitor is physically connected.
    Present,
    /// The host cannot tell.
    Unknown,
}

/// One output of the desktop, as [`desk_outputs_dark`] judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskOutputFact {
    /// The output shows part of the desktop.
    pub lit: bool,
    /// What is behind it.
    pub monitor: DeskMonitor,
}

/// Whether the desk sees nothing of the desktop on a host that cannot
/// darken a monitor: every lit output has no monitor behind it. An output
/// that is not lit shows nothing. A host with no lit output at all is not
/// proved dark (nothing is being served from it).
///
/// # Errors
///
/// [`DeskPrivacyRefusal::DeskMonitorLit`] when a lit output drives a
/// monitor, [`DeskPrivacyRefusal::DeskMonitorUnknown`] when one cannot be
/// told (or nothing is lit).
pub fn desk_outputs_dark(outputs: &[DeskOutputFact]) -> Result<(), DeskPrivacyRefusal> {
    let mut lit = outputs.iter().filter(|output| output.lit).peekable();
    if lit.peek().is_none() {
        return Err(DeskPrivacyRefusal::DeskMonitorUnknown);
    }
    let mut unknown = false;
    for output in lit {
        match output.monitor {
            DeskMonitor::Absent => {}
            DeskMonitor::Present => return Err(DeskPrivacyRefusal::DeskMonitorLit),
            DeskMonitor::Unknown => unknown = true,
        }
    }
    if unknown {
        Err(DeskPrivacyRefusal::DeskMonitorUnknown)
    } else {
        Ok(())
    }
}

/// How often a host that cannot darken a monitor reads its display topology
/// while the desk is kept dark: the operating system's own record of which
/// outputs are lit and which monitors are connected, read without probing a
/// device, off the media path.
pub const DESK_TOPOLOGY_READ: Duration = Duration::from_secs(1);

/// The longest a dark desk goes without the whole proof
/// ([`desk_outputs_dark`] over freshly probed devices) when its topology
/// reads unchanged: a monitor only that probe can see (the topology read
/// shows nothing) is found within this, plus the proof's own time. The
/// proof runs on [`DeskWatch`]'s thread, never on a session loop or the
/// media path, and at once on any topology change.
pub const DESK_FULL_PROOF_INTERVAL: Duration = Duration::from_secs(5);

/// How long a session may stream without a completed desk check before it
/// is refused: a watcher that stalled or died proves nothing. A few
/// [`DESK_TOPOLOGY_READ`]s, with room for one whole proof.
pub const DESK_CHECK_STALE: Duration = Duration::from_secs(4);

/// How long a proof the topology made due (it changed, or could not be
/// read) may stay in flight before the session is refused: a changed desk
/// is not proved dark by a probe that never answers.
pub const DESK_PROOF_IN_FLIGHT_LIMIT: Duration = Duration::from_secs(3);

/// The next check of a dark desk, as [`DeskRecheckCadence::next`] decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskRecheck {
    /// The topology reads as it did at the last passing proof, and that
    /// proof is recent: nothing more to probe.
    Topology,
    /// Probe again and prove the desk dark ([`desk_outputs_dark`]).
    FullProof(FullProofReason),
}

/// Why a whole proof is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullProofReason {
    /// The topology could not be read (fail closed: prove instead).
    TopologyUnreadable,
    /// The topology differs from the one the last proof saw (a monitor
    /// connected or removed, an output lit or detached).
    TopologyChanged,
    /// [`DESK_FULL_PROOF_INTERVAL`] has passed, or nothing was proved yet.
    Interval,
}

impl FullProofReason {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::TopologyUnreadable => "topology_unreadable",
            Self::TopologyChanged => "topology_changed",
            Self::Interval => "interval",
        }
    }
}

/// When a dark desk is proved again: a cheap topology read every
/// [`DESK_TOPOLOGY_READ`], the whole proof only when that read differs from
/// the one taken just before the last passing proof, cannot be made, or
/// [`DESK_FULL_PROOF_INTERVAL`] has passed. `T` is the platform's topology
/// reading, compared whole (no hash that could hide a change).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskRecheckCadence<T> {
    proved: Option<(T, Instant)>,
}

impl<T> Default for DeskRecheckCadence<T> {
    fn default() -> Self {
        Self { proved: None }
    }
}

impl<T: PartialEq> DeskRecheckCadence<T> {
    /// A cadence that has proved nothing yet (the first check is a proof).
    #[must_use]
    pub const fn new() -> Self {
        Self { proved: None }
    }

    /// Records a passing proof made at `at` right after `topology` was read
    /// (`None`: it could not be read, so the next check proves again).
    pub fn proved(&mut self, topology: Option<T>, at: Instant) {
        self.proved = topology.map(|topology| (topology, at));
    }

    /// The check due at `now` for the topology just read.
    #[must_use]
    pub fn next(&self, topology: Option<&T>, now: Instant) -> DeskRecheck {
        let Some(topology) = topology else {
            return DeskRecheck::FullProof(FullProofReason::TopologyUnreadable);
        };
        let Some((proved, at)) = &self.proved else {
            return DeskRecheck::FullProof(FullProofReason::Interval);
        };
        if proved != topology {
            return DeskRecheck::FullProof(FullProofReason::TopologyChanged);
        }
        if now.saturating_duration_since(*at) >= DESK_FULL_PROOF_INTERVAL {
            return DeskRecheck::FullProof(FullProofReason::Interval);
        }
        DeskRecheck::Topology
    }
}

/// Whether a session whose desk watcher last completed a check at
/// `last_check` must be refused at `now` ([`DESK_CHECK_STALE`]).
#[must_use]
pub fn desk_check_stale(last_check: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_check) >= DESK_CHECK_STALE
}

/// The name of the thread a [`DeskWatch`] proves the desk on.
pub const DESK_WATCH_THREAD: &str = "arcen-desk-privacy";

/// One whole proof a [`DeskWatch`] made, as its `report` callback sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskProofReport {
    /// Why the proof was due.
    pub reason: FullProofReason,
    /// Topology reads since the previous proof.
    pub topology_reads: u64,
    /// How long the topology read and the proof took.
    pub took: Duration,
    /// The proof's outcome (an `Err` ends the watch).
    pub outcome: Result<(), DeskPrivacyRefusal>,
}

/// Keeps a dark desk proved while a Deck is attached, on a thread of its
/// own ([`DESK_WATCH_THREAD`]), never on a session loop or the media path:
/// the platform's cheap topology read every `read_every`, its whole proof
/// when [`DeskRecheckCadence`] says so.
///
/// Fail closed: the first refusal is stored before anyone is told (the
/// `report` and `alarm` callbacks run after it), and ends the thread; a
/// thread that panics or ends for any other reason while the watch is held
/// stores [`DeskPrivacyRefusal::DeskMonitorUnknown`] and raises the alarm.
/// [`Self::verdict`] reads the result without waiting for a probe, and also
/// refuses a watcher that completed no check for [`DESK_CHECK_STALE`] or
/// whose topology-triggered proof has been in flight for
/// [`DESK_PROOF_IN_FLIGHT_LIMIT`]. Dropping the watch stops the thread (a
/// probe in progress finishes on its own; its result is discarded).
#[derive(Debug)]
pub struct DeskWatch {
    shared: std::sync::Arc<DeskWatchShared>,
}

#[derive(Debug)]
struct DeskWatchShared {
    stop: std::sync::atomic::AtomicBool,
    state: std::sync::Mutex<DeskWatchState>,
    wake: std::sync::Condvar,
}

impl DeskWatchShared {
    fn state(&self) -> std::sync::MutexGuard<'_, DeskWatchState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn stopped(&self) -> bool {
        self.stop.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[derive(Debug)]
struct DeskWatchState {
    last_check: Instant,
    refusal: Option<DeskPrivacyRefusal>,
    /// A topology-triggered proof running since then.
    proof_in_flight_since: Option<Instant>,
}

/// Ends a watcher thread fail-closed: unless the watch was stopped or the
/// thread already raised its alarm, dropping this (a return or a panic)
/// stores [`DeskPrivacyRefusal::DeskMonitorUnknown`] and raises the alarm.
struct WatcherExit<A: Fn()> {
    shared: std::sync::Arc<DeskWatchShared>,
    alarm: A,
    alarmed: bool,
}

impl<A: Fn()> Drop for WatcherExit<A> {
    fn drop(&mut self) {
        if self.alarmed || self.shared.stopped() {
            return;
        }
        {
            let mut state = self.shared.state();
            if state.refusal.is_none() {
                state.refusal = Some(DeskPrivacyRefusal::DeskMonitorUnknown);
            }
        }
        (self.alarm)();
    }
}

impl DeskWatch {
    /// Starts watching a desk proved dark right after `topology` was read.
    /// `report` is called on the watcher's thread after every whole proof
    /// (to log it); `alarm` once the watch has stored a refusal (to wake the
    /// session so it ends at once: the verdict is already a refusal then).
    /// A thread that cannot be started leaves a watch that refuses.
    pub fn start<T, R, P, O, A>(
        read_every: Duration,
        topology: Option<T>,
        mut read: R,
        mut prove: P,
        mut report: O,
        alarm: A,
    ) -> Self
    where
        T: PartialEq + Send + 'static,
        R: FnMut() -> Option<T> + Send + 'static,
        P: FnMut() -> Result<(), DeskPrivacyRefusal> + Send + 'static,
        O: FnMut(&DeskProofReport) + Send + 'static,
        A: Fn() + Send + 'static,
    {
        let started = Instant::now();
        let shared = std::sync::Arc::new(DeskWatchShared {
            stop: std::sync::atomic::AtomicBool::new(false),
            state: std::sync::Mutex::new(DeskWatchState {
                last_check: started,
                refusal: None,
                proof_in_flight_since: None,
            }),
            wake: std::sync::Condvar::new(),
        });
        let worker = std::sync::Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name(DESK_WATCH_THREAD.to_string())
            .spawn(move || {
                let mut exit = WatcherExit {
                    shared: std::sync::Arc::clone(&worker),
                    alarm,
                    alarmed: false,
                };
                let mut cadence = DeskRecheckCadence::new();
                cadence.proved(topology, started);
                let mut topology_reads = 0_u64;
                loop {
                    drop(
                        worker
                            .wake
                            .wait_timeout_while(worker.state(), read_every, |_| !worker.stopped())
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                    if worker.stopped() {
                        return;
                    }
                    let now = Instant::now();
                    let topology = read();
                    let (outcome, proof) = match cadence.next(topology.as_ref(), now) {
                        DeskRecheck::Topology => {
                            topology_reads = topology_reads.saturating_add(1);
                            (Ok(()), None)
                        }
                        DeskRecheck::FullProof(reason) => {
                            let triggered = reason != FullProofReason::Interval;
                            if triggered {
                                worker.state().proof_in_flight_since = Some(now);
                            }
                            let outcome = prove();
                            if outcome.is_ok() {
                                cadence.proved(topology, now);
                            }
                            let proof = DeskProofReport {
                                reason,
                                topology_reads,
                                took: now.elapsed(),
                                outcome,
                            };
                            topology_reads = 0;
                            (outcome, Some(proof))
                        }
                    };
                    if worker.stopped() {
                        return;
                    }
                    // Store first: whoever the report or the alarm wakes
                    // reads the verdict this check reached.
                    {
                        let mut state = worker.state();
                        state.last_check = Instant::now();
                        state.proof_in_flight_since = None;
                        if let Err(refusal) = outcome {
                            state.refusal = Some(refusal);
                        }
                    }
                    if let Some(proof) = proof.as_ref() {
                        report(proof);
                    }
                    if outcome.is_err() {
                        exit.alarmed = true;
                        if !worker.stopped() {
                            (exit.alarm)();
                        }
                        return;
                    }
                }
            });
        if spawned.is_err() {
            shared.state().refusal = Some(DeskPrivacyRefusal::DeskMonitorUnknown);
        }
        Self { shared }
    }

    /// The watch's verdict at `now`, never waiting for a probe.
    ///
    /// # Errors
    ///
    /// The stored refusal; or [`DeskPrivacyRefusal::DeskMonitorUnknown`]
    /// when a topology-triggered proof has been in flight for
    /// [`DESK_PROOF_IN_FLIGHT_LIMIT`], or the watcher completed no check
    /// for [`DESK_CHECK_STALE`].
    pub fn verdict(&self, now: Instant) -> Result<(), DeskPrivacyRefusal> {
        self.shared.verdict(now)
    }
}

impl DeskWatchShared {
    fn verdict(&self, now: Instant) -> Result<(), DeskPrivacyRefusal> {
        let state = self.state();
        if let Some(refusal) = state.refusal {
            return Err(refusal);
        }
        if state
            .proof_in_flight_since
            .is_some_and(|since| now.saturating_duration_since(since) >= DESK_PROOF_IN_FLIGHT_LIMIT)
        {
            return Err(DeskPrivacyRefusal::DeskMonitorUnknown);
        }
        if desk_check_stale(state.last_check, now) {
            return Err(DeskPrivacyRefusal::DeskMonitorUnknown);
        }
        Ok(())
    }
}

impl Drop for DeskWatch {
    fn drop(&mut self) {
        self.shared
            .stop
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared.wake.notify_all();
    }
}

/// The administrator's desk privacy choice (`privacy.desk` in the Pier
/// configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeskPrivacySetting {
    /// The desk is kept private while a Deck is attached (the default).
    On,
    /// The desk's monitors and input stay live while a Deck is attached.
    Off,
}

/// Where the effective desk privacy decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacySource {
    /// `privacy.desk` in the Pier configuration.
    Config,
    /// Not configured: on.
    Default,
}

/// The effective desk privacy policy of one Pier: on unless the
/// administrator turned it off (ADR 0025 §5, amended 2026-11-18).
///
/// Every Pier resolves it the same way and applies it to every session
/// whose picture is on the desk's own monitors
/// ([`DeskExposure::PhysicalDesk`]); a desktop no monitor at the desk shows
/// has nothing to keep private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskPrivacyPolicy {
    /// The desk is kept private while a Deck is attached.
    pub enabled: bool,
    /// Where the decision came from.
    pub source: DeskPrivacySource,
}

impl Default for DeskPrivacyPolicy {
    fn default() -> Self {
        Self::resolve(None)
    }
}

impl DeskPrivacyPolicy {
    /// Resolves `privacy.desk`: absent is on.
    #[must_use]
    pub const fn resolve(setting: Option<DeskPrivacySetting>) -> Self {
        match setting {
            Some(setting) => Self {
                enabled: matches!(setting, DeskPrivacySetting::On),
                source: DeskPrivacySource::Config,
            },
            None => Self {
                enabled: true,
                source: DeskPrivacySource::Default,
            },
        }
    }

    /// The line every Pier logs at start, so the administrator sees the
    /// effective choice and where it came from.
    #[must_use]
    pub const fn startup_line(self) -> &'static str {
        match (self.enabled, self.source) {
            (true, DeskPrivacySource::Default) => {
                "desk privacy: on (default): while a Deck is attached the desk's monitors are \
                 blanked and its input is blocked; privacy.desk = \"off\" turns it off"
            }
            (true, DeskPrivacySource::Config) => {
                "desk privacy: on (privacy.desk): while a Deck is attached the desk's monitors \
                 are blanked and its input is blocked"
            }
            (false, _) => {
                "desk privacy: OFF (privacy.desk = \"off\"): while a Deck is attached anyone at \
                 the desk sees the session and can use its keyboard and mouse"
            }
        }
    }

    /// What to do for one attachment whose desktop has `exposure`.
    #[must_use]
    pub const fn plan(self, exposure: DeskExposure) -> DeskPrivacyPlan {
        match (exposure, self.enabled) {
            (DeskExposure::NotShown, _) => DeskPrivacyPlan::NotShown,
            (DeskExposure::PhysicalDesk, true) => DeskPrivacyPlan::Protect,
            (DeskExposure::PhysicalDesk, false) => DeskPrivacyPlan::OperatorOff,
        }
    }
}

/// Whether an attachment's desktop is on the desk's own monitors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskExposure {
    /// The desktop is the one the desk's monitors show and the desk's
    /// keyboard and mouse drive: a joined console session (Linux, Windows),
    /// the console owner's Aqua session (macOS), another product's desktop
    /// on the console's X server.
    PhysicalDesk,
    /// No monitor at the desk shows it: an Arcen headless Wayland desktop,
    /// a desktop on Arcen's own virtual displays only.
    NotShown,
}

/// What a Pier does about the desk for one attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacyPlan {
    /// Blank the desk's monitors and block its input before the Deck sees
    /// anything; a desk that cannot be made private refuses the attachment
    /// (`console_privacy_unavailable`), never streams it unprotected.
    Protect,
    /// The administrator turned desk privacy off: the desk stays live, and
    /// the Deck is told.
    OperatorOff,
    /// Nothing at the desk shows the desktop.
    NotShown,
}

impl DeskPrivacyPlan {
    /// The status a Pier reports to the Deck for an attachment it serves
    /// under this plan (a [`Self::Protect`] attachment is served only once
    /// the desk is private).
    #[must_use]
    pub const fn status(self) -> DeskPrivacyStatus {
        match self {
            Self::Protect => DeskPrivacyStatus::Blanked,
            Self::OperatorOff => DeskPrivacyStatus::Off,
            Self::NotShown => DeskPrivacyStatus::NotShown,
        }
    }
}

/// The desk's state as the Pier reports it to the Deck (the hello's
/// `desk_privacy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacyStatus {
    /// The desk's monitors are blank and its input blocked.
    Blanked,
    /// The administrator turned desk privacy off: the desk shows the
    /// session.
    Off,
    /// No monitor at the desk shows this desktop.
    NotShown,
}

/// What the Deck shows while the desk is private.
pub const DESK_BLANKED_NOTE: &str = "Host screens are blanked for privacy";

/// What the Deck shows when the administrator turned desk privacy off.
pub const DESK_VISIBLE_NOTE: &str =
    "Host screens are not blanked: desk privacy is turned off on this host";

impl DeskPrivacyStatus {
    /// Wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Blanked => "blanked",
            Self::Off => "off",
            Self::NotShown => "not_shown",
        }
    }

    /// Parses a wire token; `None` for a token this build does not know.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "blanked" => Some(Self::Blanked),
            "off" => Some(Self::Off),
            "not_shown" => Some(Self::NotShown),
            _ => None,
        }
    }

    /// The one line the Deck shows for this status, if any.
    #[must_use]
    pub const fn deck_note(self) -> Option<&'static str> {
        match self {
            Self::Blanked => Some(DESK_BLANKED_NOTE),
            Self::Off => Some(DESK_VISIBLE_NOTE),
            Self::NotShown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_the_watch_ends_reports_its_class_and_the_frames_it_sent() {
        assert_eq!(
            WatchSessionEnd::new(Some(WatchVerdict::ConsoleLeft), 42),
            WatchSessionEnd {
                reason_class: "console_left",
                frames_sent: 42
            }
        );
        for verdict in [Some(WatchVerdict::PrivacyLost), None] {
            assert_eq!(
                WatchSessionEnd::new(verdict, 7),
                WatchSessionEnd {
                    reason_class: "privacy_lost",
                    frames_sent: 7
                }
            );
        }
    }

    #[test]
    fn desk_privacy_is_on_unless_the_administrator_turns_it_off() {
        let default = DeskPrivacyPolicy::resolve(None);
        assert!(default.enabled);
        assert_eq!(default.source, DeskPrivacySource::Default);
        assert_eq!(DeskPrivacyPolicy::default(), default);
        assert!(default.startup_line().contains("on (default)"));

        let off = DeskPrivacyPolicy::resolve(Some(DeskPrivacySetting::Off));
        assert!(!off.enabled);
        assert_eq!(off.source, DeskPrivacySource::Config);
        assert!(off.startup_line().contains("OFF"));

        let on = DeskPrivacyPolicy::resolve(Some(DeskPrivacySetting::On));
        assert!(on.enabled);
        assert_eq!(on.source, DeskPrivacySource::Config);
    }

    #[test]
    fn only_a_desktop_on_the_desk_is_protected_and_the_deck_is_told() {
        let on = DeskPrivacyPolicy::default();
        let off = DeskPrivacyPolicy::resolve(Some(DeskPrivacySetting::Off));
        assert_eq!(
            on.plan(DeskExposure::PhysicalDesk),
            DeskPrivacyPlan::Protect
        );
        assert_eq!(
            off.plan(DeskExposure::PhysicalDesk),
            DeskPrivacyPlan::OperatorOff
        );
        assert_eq!(on.plan(DeskExposure::NotShown), DeskPrivacyPlan::NotShown);
        assert_eq!(off.plan(DeskExposure::NotShown), DeskPrivacyPlan::NotShown);

        assert_eq!(
            DeskPrivacyPlan::Protect.status().deck_note(),
            Some("Host screens are blanked for privacy")
        );
        assert!(
            DeskPrivacyPlan::OperatorOff
                .status()
                .deck_note()
                .is_some_and(|note| note.contains("not blanked"))
        );
        assert_eq!(DeskPrivacyPlan::NotShown.status().deck_note(), None);
    }

    #[test]
    fn a_desk_is_dark_only_when_no_lit_output_has_a_monitor() {
        let output = |lit, monitor| DeskOutputFact { lit, monitor };
        assert_eq!(
            desk_outputs_dark(&[
                output(true, DeskMonitor::Absent),
                output(false, DeskMonitor::Present),
            ]),
            Ok(())
        );
        assert_eq!(
            desk_outputs_dark(&[
                output(true, DeskMonitor::Absent),
                output(true, DeskMonitor::Unknown),
                output(true, DeskMonitor::Present),
            ]),
            Err(DeskPrivacyRefusal::DeskMonitorLit)
        );
        assert_eq!(
            desk_outputs_dark(&[output(true, DeskMonitor::Unknown)]),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
        assert_eq!(
            desk_outputs_dark(&[output(false, DeskMonitor::Absent)]),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
        assert_eq!(
            desk_outputs_dark(&[]),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
        for refusal in [
            DeskPrivacyRefusal::DeskMonitorUnknown,
            DeskPrivacyRefusal::DeskOutputsChanged,
            DeskPrivacyRefusal::InputBlockUnavailable,
            DeskPrivacyRefusal::DeskDetachFailed,
        ] {
            let reason = refusal.deck_reason();
            assert!(
                reason.starts_with("console_privacy_unavailable: "),
                "{reason}"
            );
            assert!(reason.len() <= 120, "{reason}");
        }
    }

    #[test]
    fn desk_privacy_status_round_trips_and_ignores_unknown_tokens() {
        for status in [
            DeskPrivacyStatus::Blanked,
            DeskPrivacyStatus::Off,
            DeskPrivacyStatus::NotShown,
        ] {
            assert_eq!(DeskPrivacyStatus::from_token(status.token()), Some(status));
        }
        assert_eq!(DeskPrivacyStatus::from_token("dimmed"), None);
    }

    #[test]
    fn only_the_pier_s_own_events_reach_a_private_desktop() {
        let pier = 4242;
        assert!(event_passes_while_attached(EventPoster::from_pid(
            pier, pier
        )));
        // The desk's keyboard, a virtual HID keyboard, an on-screen
        // keyboard at the desk: all dropped.
        assert!(!event_passes_while_attached(EventPoster::from_pid(0, pier)));
        assert!(!event_passes_while_attached(EventPoster::from_pid(
            517, pier
        )));
        assert_eq!(EventPoster::from_pid(0, pier), EventPoster::HidSystem);
        assert_eq!(EventPoster::from_pid(517, pier), EventPoster::OtherProcess);
    }

    #[test]
    fn every_display_but_the_pier_s_own_goes_dark() {
        assert!(blanks_while_attached(DeskDisplayFact {
            pier_virtual: false
        }));
        assert!(!blanks_while_attached(DeskDisplayFact {
            pier_virtual: true
        }));
    }

    #[test]
    fn a_table_is_dark_only_when_every_entry_is() {
        assert!(transfer_table_is_dark(&[0.0; 256]));
        assert!(transfer_table_is_dark(&[0.0, DARK_TABLE_LIMIT]));
        assert!(!transfer_table_is_dark(&[]));
        assert!(!transfer_table_is_dark(&[0.0, 0.0, 0.01]));
        assert!(!transfer_table_is_dark(&[0.0, f32::NAN]));
        let identity: Vec<f32> = (0..256_u16).map(|i| f32::from(i) / 255.0).collect();
        assert!(!transfer_table_is_dark(&identity));
    }

    #[test]
    fn virtual_hid_input_cannot_be_kept_private() {
        assert_eq!(admits_input_route(DeckInputRoute::PostedEvents), Ok(()));
        assert_eq!(
            admits_input_route(DeckInputRoute::VirtualHidDevices),
            Err(DeskPrivacyRefusal::VirtualHidInput)
        );
    }

    #[test]
    fn the_console_leaves_only_after_consecutive_readings_name_someone_else() {
        let mut watch = PrivacyWatch::default();
        assert_eq!(watch.console(false), WatchVerdict::Hold);
        assert_eq!(watch.console(false), WatchVerdict::Hold);
        assert_eq!(
            watch.console(true),
            WatchVerdict::Hold,
            "a reading of ours resets"
        );
        for _ in 1..CONSOLE_AWAY_READINGS {
            assert_eq!(watch.console(false), WatchVerdict::Hold);
        }
        assert_eq!(watch.console(false), WatchVerdict::ConsoleLeft);
    }

    #[test]
    fn a_console_that_left_the_account_is_not_a_privacy_failure() {
        use super::super::ConsoleRefusal;
        use crate::agent_relay::ConsoleHolder;
        let serving = 501;
        // Another account signed in (a user switch completed): the Deck is
        // told what it is told connecting now.
        assert_eq!(
            WatchVerdict::ConsoleLeft.deck_refusal(serving, ConsoleHolder::User(502)),
            Some(ConsoleRefusal::OtherUser)
        );
        // The login window in front: the account's session went to the
        // background; signing in again reaches that login window.
        assert_eq!(
            WatchVerdict::ConsoleLeft.deck_refusal(serving, ConsoleHolder::LoginWindow),
            Some(ConsoleRefusal::SessionChanged)
        );
        assert_eq!(
            WatchVerdict::ConsoleLeft.deck_refusal(serving, ConsoleHolder::User(serving)),
            Some(ConsoleRefusal::SessionChanged)
        );
        // Only a desk that could not be kept dark is a privacy failure.
        for holder in [ConsoleHolder::User(serving), ConsoleHolder::User(502)] {
            assert_eq!(
                WatchVerdict::PrivacyLost.deck_refusal(serving, holder),
                Some(ConsoleRefusal::PrivacyUnavailable)
            );
        }
        assert_eq!(
            WatchVerdict::Hold.deck_refusal(serving, ConsoleHolder::LoginWindow),
            None
        );
    }

    #[test]
    fn a_display_that_stays_lit_ends_the_session_after_the_limit() {
        let start = Instant::now();
        let mut watch = PrivacyWatch::default();
        assert_eq!(watch.displays(false, start), WatchVerdict::Hold);
        assert_eq!(
            watch.displays(false, start + NOT_DARK_LIMIT / 2),
            WatchVerdict::Hold
        );
        assert_eq!(
            watch.displays(true, start + NOT_DARK_LIMIT / 2),
            WatchVerdict::Hold,
            "dark again resets"
        );
        assert_eq!(
            watch.displays(false, start + NOT_DARK_LIMIT),
            WatchVerdict::Hold
        );
        assert_eq!(
            watch.displays(false, start + NOT_DARK_LIMIT * 2),
            WatchVerdict::PrivacyLost
        );
    }

    #[test]
    fn a_session_that_will_not_lock_stays_private_for_the_grace_only() {
        assert_eq!(
            hand_back_unlocked(Duration::ZERO),
            UnlockedHandBack::KeepPrivate
        );
        assert_eq!(
            hand_back_unlocked(super::super::UNLOCKED_GRACE.saturating_sub(Duration::from_secs(1))),
            UnlockedHandBack::KeepPrivate
        );
        assert_eq!(
            hand_back_unlocked(super::super::UNLOCKED_GRACE),
            UnlockedHandBack::GiveBack
        );
    }

    #[test]
    fn a_dark_desk_is_proved_first_then_only_its_topology_is_read() {
        let start = Instant::now();
        let mut cadence = DeskRecheckCadence::<u32>::new();
        assert_eq!(
            cadence.next(Some(&1), start),
            DeskRecheck::FullProof(FullProofReason::Interval)
        );
        cadence.proved(Some(1), start);
        for millis in (500..5_000).step_by(500) {
            assert_eq!(
                cadence.next(Some(&1), start + Duration::from_millis(millis)),
                DeskRecheck::Topology,
                "{millis} ms"
            );
        }
        assert_eq!(DESK_FULL_PROOF_INTERVAL, Duration::from_secs(5));
        assert_eq!(
            cadence.next(Some(&1), start + DESK_FULL_PROOF_INTERVAL),
            DeskRecheck::FullProof(FullProofReason::Interval)
        );
    }

    #[test]
    fn a_topology_change_or_an_unreadable_topology_proves_the_desk_at_once() {
        let start = Instant::now();
        let mut cadence = DeskRecheckCadence::<Vec<u32>>::new();
        cadence.proved(Some(vec![1, 2]), start);
        let soon = start + DESK_TOPOLOGY_READ;
        assert_eq!(
            cadence.next(Some(&vec![1, 2, 3]), soon),
            DeskRecheck::FullProof(FullProofReason::TopologyChanged)
        );
        assert_eq!(
            cadence.next(None, soon),
            DeskRecheck::FullProof(FullProofReason::TopologyUnreadable)
        );
        cadence.proved(None, soon);
        assert_eq!(
            cadence.next(Some(&vec![1, 2]), soon),
            DeskRecheck::FullProof(FullProofReason::Interval),
            "a proof whose topology was unreadable leaves nothing to compare with"
        );
    }

    #[test]
    fn a_watcher_that_stops_checking_refuses_the_session() {
        let start = Instant::now();
        assert!(!desk_check_stale(
            start,
            start + DESK_CHECK_STALE.saturating_sub(Duration::from_millis(1))
        ));
        assert!(desk_check_stale(start, start + DESK_CHECK_STALE));
        assert!(DESK_CHECK_STALE > DESK_TOPOLOGY_READ * 2);
        assert!(DESK_CHECK_STALE > DESK_PROOF_IN_FLIGHT_LIMIT);
    }

    fn wait_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        done()
    }

    #[test]
    fn the_desk_is_proved_on_its_own_thread_and_a_verdict_never_waits_for_the_probe() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};
        let threads = Arc::new(Mutex::new(Vec::<String>::new()));
        let proofs = Arc::new(AtomicU32::new(0));
        let (read_threads, proof_threads) = (Arc::clone(&threads), Arc::clone(&threads));
        let (reads, proof_count) = (Arc::new(AtomicU32::new(0)), Arc::clone(&proofs));
        let here = |threads: &Mutex<Vec<String>>| {
            threads
                .lock()
                .unwrap()
                .push(std::thread::current().name().unwrap_or("").to_string());
        };
        let watch = DeskWatch::start(
            Duration::from_millis(10),
            Some(1_u32),
            move || {
                here(&read_threads);
                // The topology changes on every read, so every check proves.
                Some(reads.fetch_add(1, Ordering::SeqCst) + 2)
            },
            move || {
                here(&proof_threads);
                proof_count.fetch_add(1, Ordering::SeqCst);
                // As slow as the probe that froze the picture.
                std::thread::sleep(Duration::from_millis(400));
                Ok(())
            },
            |_| {},
            || {},
        );
        assert!(wait_for(Duration::from_secs(5), || {
            proofs.load(Ordering::SeqCst) >= 1
        }));
        // A proof is in progress; the verdict must not wait for it.
        for _ in 0..20 {
            let asked = Instant::now();
            assert_eq!(watch.verdict(Instant::now()), Ok(()));
            assert!(asked.elapsed() < Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(watch);
        let threads = threads.lock().unwrap();
        assert!(!threads.is_empty());
        assert!(
            threads.iter().all(|name| name == DESK_WATCH_THREAD),
            "{threads:?}"
        );
    }

    #[test]
    fn an_unchanged_topology_is_only_read_and_a_change_is_proved_at_once() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};
        let topology = Arc::new(AtomicU32::new(7));
        let proofs = Arc::new(AtomicU32::new(0));
        let reads = Arc::new(AtomicU32::new(0));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (read_topology, read_count, proof_count, seen) = (
            Arc::clone(&topology),
            Arc::clone(&reads),
            Arc::clone(&proofs),
            Arc::clone(&reports),
        );
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(7_u32),
            move || {
                read_count.fetch_add(1, Ordering::SeqCst);
                Some(read_topology.load(Ordering::SeqCst))
            },
            move || {
                proof_count.fetch_add(1, Ordering::SeqCst);
                Err(DeskPrivacyRefusal::DeskMonitorLit)
            },
            move |report: &DeskProofReport| seen.lock().unwrap().push(*report),
            || {},
        );
        assert!(wait_for(Duration::from_secs(5), || {
            reads.load(Ordering::SeqCst) >= 20
        }));
        assert_eq!(
            proofs.load(Ordering::SeqCst),
            0,
            "no proof while nothing changed"
        );
        assert_eq!(watch.verdict(Instant::now()), Ok(()));
        // A monitor connected: the topology changes, the proof runs at once
        // and finds it lit.
        let changed = Instant::now();
        topology.store(8, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(5), || {
            watch.verdict(Instant::now()).is_err()
        }));
        assert!(changed.elapsed() < Duration::from_secs(1));
        assert_eq!(proofs.load(Ordering::SeqCst), 1);
        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].reason, FullProofReason::TopologyChanged);
        assert_eq!(reports[0].outcome, Err(DeskPrivacyRefusal::DeskMonitorLit));
        assert_eq!(
            watch.verdict(Instant::now() + DESK_CHECK_STALE),
            Err(DeskPrivacyRefusal::DeskMonitorLit)
        );
    }

    #[test]
    fn a_watch_whose_reads_hang_refuses_once_its_last_check_is_stale() {
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            || {
                std::thread::sleep(Duration::from_secs(60));
                Some(1)
            },
            || Ok(()),
            |_| {},
            || {},
        );
        let now = Instant::now();
        assert_eq!(watch.verdict(now), Ok(()));
        assert_eq!(
            watch.verdict(now + DESK_CHECK_STALE),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
    }

    #[test]
    fn a_panicking_proof_refuses_and_raises_the_alarm() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let alarmed = Arc::new(AtomicBool::new(false));
        let raised = Arc::clone(&alarmed);
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            || Some(2),
            || panic!("probe failed"),
            |_| {},
            move || raised.store(true, Ordering::SeqCst),
        );
        assert!(wait_for(Duration::from_secs(5), || alarmed.load(Ordering::SeqCst)));
        assert_eq!(
            watch.verdict(Instant::now()),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
    }

    #[test]
    fn a_change_whose_proof_never_answers_refuses_after_the_in_flight_limit() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let proving = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&proving);
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            || Some(2),
            move || {
                started.store(true, Ordering::SeqCst);
                std::thread::sleep(Duration::from_secs(60));
                Ok(())
            },
            |_| {},
            || {},
        );
        assert!(wait_for(Duration::from_secs(5), || proving.load(Ordering::SeqCst)));
        let now = Instant::now();
        assert_eq!(watch.verdict(now), Ok(()));
        assert!(DESK_PROOF_IN_FLIGHT_LIMIT < DESK_CHECK_STALE);
        assert_eq!(
            watch.verdict(now + DESK_PROOF_IN_FLIGHT_LIMIT),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown),
            "refused by the in-flight limit, before the watch is stale"
        );
    }

    #[test]
    fn the_verdict_is_already_a_refusal_when_the_alarm_is_raised() {
        use std::sync::mpsc;
        use std::sync::{Arc, Mutex};
        let watch_slot: Arc<Mutex<Option<std::sync::Weak<DeskWatchShared>>>> =
            Arc::new(Mutex::new(None));
        let seen_by_alarm = Arc::new(Mutex::new(None));
        let (slot, seen) = (Arc::clone(&watch_slot), Arc::clone(&seen_by_alarm));
        let (alarm_tx, alarm_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            move || {
                // Hold the first reading until the test has published the
                // watch, so the alarm can read its verdict.
                let _ = go_rx.recv_timeout(Duration::from_secs(5));
                Some(2)
            },
            || Err(DeskPrivacyRefusal::DeskMonitorLit),
            |_| {},
            move || {
                let shared = slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade);
                let verdict = shared.map(|shared| shared.verdict(Instant::now()));
                *seen.lock().unwrap() = verdict;
                let _ = alarm_tx.send(());
            },
        );
        *watch_slot.lock().unwrap() = Some(std::sync::Arc::downgrade(&watch.shared));
        go_tx.send(()).unwrap();
        alarm_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            *seen_by_alarm.lock().unwrap(),
            Some(Err(DeskPrivacyRefusal::DeskMonitorLit))
        );
        assert_eq!(
            watch.verdict(Instant::now()),
            Err(DeskPrivacyRefusal::DeskMonitorLit)
        );
    }
}
