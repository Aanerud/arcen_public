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
//! The policy every Pier shares is here too: the Deck's user chooses desk
//! privacy (private by default), kept as far as the host can, unless the
//! administrator sets `privacy.desk = "required"` (always private, fail
//! closed) or `"off"` ([`DeskPrivacyPolicy`], [`DeckDeskPrivacy`]); it
//! applies to every desktop the desk's monitors show ([`DeskExposure`]);
//! one table decides what a shortfall does ([`DeskPrivacyState`]); and the
//! Deck is told the desk's state ([`DeskPrivacyStatus`]).

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

    /// Every refusal.
    pub const ALL: [Self; 8] = [
        Self::VirtualHidInput,
        Self::InputFilterUnavailable,
        Self::DisplaysNotDark,
        Self::DeskMonitorLit,
        Self::DeskMonitorUnknown,
        Self::DeskOutputsChanged,
        Self::InputBlockUnavailable,
        Self::DeskDetachFailed,
    ];

    /// The refusal [`Self::token`] names (a Pier's own processes pass it
    /// along); `None` for an unknown token.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|refusal| refusal.token() == token)
    }

    /// What the Deck is told (a close reason: at most 120 bytes), led by
    /// the shared `console_privacy_unavailable` code, then this cause. Only
    /// a Pier whose administrator requires desk privacy refuses
    /// ([`DeskPrivacyPlan::Required`]), so the reason says so, and names
    /// what to look for at the desk.
    #[must_use]
    pub fn deck_reason(self) -> String {
        format!(
            "{}: {}: {}",
            super::ConsoleRefusal::PrivacyUnavailable.token(),
            self.token(),
            match self.part() {
                DeskPart::Screens => {
                    "desk privacy is required; a screen or dummy plug cannot be darkened"
                }
                DeskPart::Input => "desk privacy is required; the desk's input cannot be blocked",
            }
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

/// What an output is to the attached session, as desk privacy must know it
/// before it acts on the desk (ADR 0025 §5, amended 2026-11-24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskOutputRole {
    /// The served topology is captured from it: the single served display,
    /// or, with Match My Layout, each Deck display's output (with dummy
    /// plugs on an NVIDIA GPU, each plug carries a stream). It is the
    /// session's own.
    Served,
    /// Any other output the desktop lights.
    Other,
}

/// What desk privacy does to one output ([`desk_output_measure`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskOutputMeasure {
    /// Never detached, disabled or powered down, nor changed in any way
    /// capture reads. Darkened only by what capture never reads (a zero
    /// gamma ramp or transfer table: Linux, macOS); where a host has no such
    /// means (Windows), a monitor behind it is the session's known shortfall
    /// ([`DeskPrivacyRefusal::DeskMonitorLit`]: `screens_visible` under best
    /// effort, a refusal when privacy is required).
    KeepServed,
    /// Darkened, or taken off the desktop (Windows detaches it).
    Hide,
}

/// What desk privacy may do to an output in `role`: it acts only on outputs
/// the session is not served from.
#[must_use]
pub const fn desk_output_measure(role: DeskOutputRole) -> DeskOutputMeasure {
    match role {
        DeskOutputRole::Served => DeskOutputMeasure::KeepServed,
        DeskOutputRole::Other => DeskOutputMeasure::Hide,
    }
}

/// The lit outputs desk privacy may take off the desktop: every one not in
/// `served`, in `lit`'s order. Empty when nothing but the session's own
/// outputs is lit, so nothing is detached.
#[must_use]
pub fn desk_outputs_to_hide<K: Ord + Clone>(
    lit: &[K],
    served: &std::collections::BTreeSet<K>,
) -> Vec<K> {
    lit.iter()
        .filter(|output| {
            let role = if served.contains(output) {
                DeskOutputRole::Served
            } else {
                DeskOutputRole::Other
            };
            desk_output_measure(role) == DeskOutputMeasure::Hide
        })
        .cloned()
        .collect()
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

/// One output the desktop lights, as a re-proof of a dark desk reads it:
/// `id` is the host's own name for it (on Windows, adapter and output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LitDeskOutput<K> {
    /// Which output.
    pub id: K,
    /// What is behind it.
    pub monitor: DeskMonitor,
}

/// What one re-proof of a dark desk read ([`recheck_dark_desk`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskReading<K> {
    /// The display topology or the devices behind it could not be read (a
    /// topology query or a device probe failed).
    Unreadable,
    /// Every output the desktop lights now (empty: none).
    Lit(Vec<LitDeskOutput<K>>),
}

/// Why a re-proof of a dark desk did not pass, and whether repeating it a
/// moment later may pass ([`DeskWatch`], [`DESK_PROOF_SETTLE`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskProofFailure {
    /// The refusal that ends the session if the failure stands.
    pub refusal: DeskPrivacyRefusal,
    /// The desk could not be read whole, but nothing it read shows the
    /// desktop anywhere it was not proved dark: a read failed, nothing was
    /// lit, an output proved without a monitor could not be described
    /// again, or only some of the proved outputs are lit (every output lit
    /// is one the passing proof lit). A display reconfiguration (a driver
    /// re-detecting its connectors, a mode set) passes through these.
    pub transient: bool,
}

impl DeskProofFailure {
    /// A failure that ends the session at once.
    #[must_use]
    pub const fn decided(refusal: DeskPrivacyRefusal) -> Self {
        Self {
            refusal,
            transient: false,
        }
    }

    /// A failure that may pass when the desk is read again.
    #[must_use]
    pub const fn transient(refusal: DeskPrivacyRefusal) -> Self {
        Self {
            refusal,
            transient: true,
        }
    }
}

/// Proves a dark desk again against the outputs its passing proof lit
/// (`proved`: every one of them was proved without a monitor).
///
/// Decided at once: a lit output with a monitor behind it
/// ([`DeskPrivacyRefusal::DeskMonitorLit`]); an output lit now that was
/// not lit then, whose monitor cannot be told (an output of a GPU or USB
/// display adapter the host cannot describe:
/// [`DeskPrivacyRefusal::DeskMonitorUnknown`]) or that has none
/// ([`DeskPrivacyRefusal::DeskOutputsChanged`]). Transient
/// ([`DeskProofFailure::transient`]), since fewer outputs than were proved
/// show the desktop nowhere new: an unreadable desk, nothing lit, a proved
/// output that cannot be described again
/// ([`DeskPrivacyRefusal::DeskMonitorUnknown`]), or only some of the
/// proved outputs lit ([`DeskPrivacyRefusal::DeskOutputsChanged`]).
///
/// # Errors
///
/// The failure, as above.
pub fn recheck_dark_desk<K: Ord>(
    proved: &std::collections::BTreeSet<K>,
    reading: &DeskReading<K>,
) -> Result<(), DeskProofFailure> {
    let unreadable = DeskProofFailure::transient(DeskPrivacyRefusal::DeskMonitorUnknown);
    let lit = match reading {
        DeskReading::Unreadable => return Err(unreadable),
        DeskReading::Lit(lit) if lit.is_empty() => return Err(unreadable),
        DeskReading::Lit(lit) => lit,
    };
    if lit
        .iter()
        .any(|output| output.monitor == DeskMonitor::Present)
    {
        return Err(DeskProofFailure::decided(
            DeskPrivacyRefusal::DeskMonitorLit,
        ));
    }
    let mut new = lit.iter().filter(|output| !proved.contains(&output.id));
    if let Some(first) = new.next() {
        let undescribed = first.monitor == DeskMonitor::Unknown
            || new.any(|output| output.monitor == DeskMonitor::Unknown);
        return Err(DeskProofFailure::decided(if undescribed {
            DeskPrivacyRefusal::DeskMonitorUnknown
        } else {
            DeskPrivacyRefusal::DeskOutputsChanged
        }));
    }
    if lit
        .iter()
        .any(|output| output.monitor == DeskMonitor::Unknown)
    {
        return Err(unreadable);
    }
    let now: std::collections::BTreeSet<&K> = lit.iter().map(|output| &output.id).collect();
    if now.len() != proved.len() {
        return Err(DeskProofFailure::transient(
            DeskPrivacyRefusal::DeskOutputsChanged,
        ));
    }
    Ok(())
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
/// read), or a proof being repeated while its refusal settles
/// ([`DESK_PROOF_SETTLE`]), may stay in flight before the session is
/// refused: a changed desk is not proved dark by a probe that never answers.
pub const DESK_PROOF_IN_FLIGHT_LIMIT: Duration = Duration::from_secs(3);

/// How long a [`DeskWatch`] repeats a proof whose failure is
/// [transient](DeskProofFailure::transient) before that failure ends the
/// session. A display reconfiguration (a driver re-detecting its
/// connectors, a mode set) passes through moments in which the desk cannot
/// be read whole; ending the session there ended it while the desk was
/// dark. Shorter than [`DESK_PROOF_IN_FLIGHT_LIMIT`], which
/// [`DeskWatch::verdict`] enforces meanwhile, so a desk that stays
/// unproved is refused at that limit at the latest.
pub const DESK_PROOF_SETTLE: Duration = Duration::from_secs(2);

/// The most time one [`DeskWatch`] (one attachment) spends repeating
/// transient failures, summed over every one: once it is spent, a
/// transient failure ends the session at once, so displays that keep
/// failing to read cannot keep a session going on repeats.
pub const DESK_SETTLE_BUDGET: Duration = Duration::from_secs(6);

/// The pause between two proofs while a refusal settles.
pub const DESK_PROOF_SETTLE_RETRY: Duration = Duration::from_millis(400);

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
    /// How long the topology read and the proof took (with every repeat).
    pub took: Duration,
    /// Proofs made: more than one when a transient failure was repeated
    /// ([`DESK_PROOF_SETTLE`]).
    pub attempts: u32,
    /// What is left of [`DESK_SETTLE_BUDGET`] after this proof.
    pub settle_left: Duration,
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
/// [transient](DeskProofFailure::transient) failure is first proved again
/// every [`DESK_PROOF_SETTLE_RETRY`] for up to [`DESK_PROOF_SETTLE`], while
/// the watch's [`DESK_SETTLE_BUDGET`] lasts (a lit monitor, an output lit
/// anew whose monitor cannot be told, or changed lit outputs never are); a
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
    /// A topology-triggered proof (or a repeat of any proof) running since
    /// then.
    proof_in_flight_since: Option<Instant>,
}

/// What [`DeskWatchShared::prove_settling`] reached: the last proof's
/// outcome, the topology read just before it and when.
struct SettledProof<T> {
    outcome: Result<(), DeskProofFailure>,
    topology: Option<T>,
    proved_at: Instant,
    attempts: u32,
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

/// What a [`DeskWatch`] does once it has stored a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskWatchOnRefusal {
    /// Stop watching: the refusal stands and ends the session
    /// ([`DeskPrivacyPlan::Required`]).
    End,
    /// Keep proving, at most once every [`DESK_FULL_PROOF_INTERVAL`], and
    /// clear the refusal (raising the alarm again) once a proof passes:
    /// a best-effort session reports the shortfall and its recovery
    /// ([`DeskPrivacyPlan::BestEffort`], [`DeskPrivacyState::watched`]).
    Keep,
}

impl DeskWatch {
    /// Starts watching a desk proved dark right after `topology` was read,
    /// ending at the first refusal ([`DeskWatchOnRefusal::End`]); see
    /// [`Self::start_with`].
    pub fn start<T, R, P, O, A>(
        read_every: Duration,
        topology: Option<T>,
        read: R,
        prove: P,
        report: O,
        alarm: A,
    ) -> Self
    where
        T: PartialEq + Send + 'static,
        R: FnMut() -> Option<T> + Send + 'static,
        P: FnMut() -> Result<(), DeskProofFailure> + Send + 'static,
        O: FnMut(&DeskProofReport) + Send + 'static,
        A: Fn() + Send + 'static,
    {
        Self::start_with(
            DeskWatchOnRefusal::End,
            read_every,
            topology,
            read,
            prove,
            report,
            alarm,
        )
    }

    /// Starts watching a desk proved dark right after `topology` was read.
    /// `report` is called on the watcher's thread after every whole proof
    /// (to log it); `alarm` once the watch has stored a refusal (to wake the
    /// session so it ends at once: the verdict is already a refusal then),
    /// and, under [`DeskWatchOnRefusal::Keep`], again once a later proof
    /// cleared it. A thread that cannot be started leaves a watch that
    /// refuses.
    pub fn start_with<T, R, P, O, A>(
        on_refusal: DeskWatchOnRefusal,
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
        P: FnMut() -> Result<(), DeskProofFailure> + Send + 'static,
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
                let mut settle_spent = Duration::ZERO;
                // Under `Keep`, when the last proof failed: the next proof
                // waits for the interval, and a pass clears the refusal.
                let mut failed_at: Option<Instant> = None;
                loop {
                    if worker.sleep(read_every) {
                        return;
                    }
                    let now = Instant::now();
                    if failed_at.is_some_and(|at| {
                        now.saturating_duration_since(at) < DESK_FULL_PROOF_INTERVAL
                    }) {
                        continue;
                    }
                    let topology = read();
                    let due = match failed_at {
                        Some(_) => DeskRecheck::FullProof(FullProofReason::Interval),
                        None => cadence.next(topology.as_ref(), now),
                    };
                    let (outcome, proof) = match due {
                        DeskRecheck::Topology => {
                            topology_reads = topology_reads.saturating_add(1);
                            (Ok(()), None)
                        }
                        DeskRecheck::FullProof(reason) => {
                            if reason != FullProofReason::Interval {
                                worker.state().proof_in_flight_since = Some(now);
                            }
                            let Some(settled) = worker.prove_settling(
                                now,
                                topology,
                                &mut settle_spent,
                                &mut read,
                                &mut prove,
                            ) else {
                                return;
                            };
                            let outcome = settled.outcome.map_err(|failure| failure.refusal);
                            if outcome.is_ok() {
                                cadence.proved(settled.topology, settled.proved_at);
                            }
                            let proof = DeskProofReport {
                                reason,
                                topology_reads,
                                took: now.elapsed(),
                                attempts: settled.attempts,
                                settle_left: DESK_SETTLE_BUDGET.saturating_sub(settle_spent),
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
                    let recovered = failed_at.is_some() && outcome.is_ok();
                    worker.store_check(outcome, recovered);
                    if let Some(proof) = proof.as_ref() {
                        report(proof);
                    }
                    if outcome.is_err() && on_refusal == DeskWatchOnRefusal::End {
                        exit.alarmed = true;
                        if !worker.stopped() {
                            (exit.alarm)();
                        }
                        return;
                    }
                    let changed = recovered || (outcome.is_err() && failed_at.is_none());
                    failed_at = outcome.is_err().then(Instant::now);
                    if changed && !worker.stopped() {
                        (exit.alarm)();
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
    /// Waits `period` (or until the watch is stopped); whether it was.
    fn sleep(&self, period: Duration) -> bool {
        drop(
            self.wake
                .wait_timeout_while(self.state(), period, |_| !self.stopped())
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        self.stopped()
    }

    /// Stores one completed check: its refusal, or, when `recovered`, the
    /// end of the refusal a failed proof left.
    fn store_check(&self, outcome: Result<(), DeskPrivacyRefusal>, recovered: bool) {
        let mut state = self.state();
        state.last_check = Instant::now();
        state.proof_in_flight_since = None;
        match outcome {
            Err(refusal) => state.refusal = Some(refusal),
            Ok(()) if recovered => state.refusal = None,
            Ok(()) => {}
        }
    }

    /// One whole proof begun at `since` right after `topology` was read,
    /// repeated (each after a fresh read) every [`DESK_PROOF_SETTLE_RETRY`]
    /// while its failure is [transient](DeskProofFailure::transient),
    /// [`DESK_PROOF_SETTLE`] has not run out and `spent` leaves room in
    /// [`DESK_SETTLE_BUDGET`]; the time repeated is added to `spent`.
    /// Meanwhile the desk counts as a proof in flight since `since`, so
    /// [`Self::verdict`] refuses it at [`DESK_PROOF_IN_FLIGHT_LIMIT`] whatever
    /// the watcher is doing. `None` when the watch was stopped meanwhile.
    fn prove_settling<T, R, P>(
        &self,
        since: Instant,
        topology: Option<T>,
        spent: &mut Duration,
        read: &mut R,
        prove: &mut P,
    ) -> Option<SettledProof<T>>
    where
        R: FnMut() -> Option<T>,
        P: FnMut() -> Result<(), DeskProofFailure>,
    {
        let mut settled = SettledProof {
            outcome: prove(),
            topology,
            proved_at: since,
            attempts: 1,
        };
        while let Err(failure) = settled.outcome {
            let next = since.elapsed() + DESK_PROOF_SETTLE_RETRY;
            if !failure.transient
                || next >= DESK_PROOF_SETTLE
                || spent.saturating_add(next) > DESK_SETTLE_BUDGET
            {
                break;
            }
            self.state().proof_in_flight_since.get_or_insert(since);
            drop(
                self.wake
                    .wait_timeout_while(self.state(), DESK_PROOF_SETTLE_RETRY, |_| !self.stopped())
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            );
            if self.stopped() {
                return None;
            }
            settled.proved_at = Instant::now();
            settled.topology = read();
            settled.outcome = prove();
            settled.attempts = settled.attempts.saturating_add(1);
        }
        if settled.attempts > 1 {
            *spent = spent.saturating_add(since.elapsed());
        }
        Some(settled)
    }

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
    /// Each Deck's user chooses (private unless they turn it off); a desk
    /// that cannot be made fully private is reported, never refused. What
    /// every installer ships.
    On,
    /// The desk's monitors and input stay live while a Deck is attached.
    Off,
    /// Always private, whatever the Deck asks; a desk that cannot be made
    /// private refuses the attachment, or ends it.
    Required,
}

/// Where the effective desk privacy decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacySource {
    /// `privacy.desk` in the Pier configuration.
    Config,
    /// Not configured: on.
    Default,
}

/// What the administrator's `privacy.desk` leaves to each attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacyMode {
    /// `"on"` or absent: the Deck's user chooses ([`DeckDeskPrivacy`]),
    /// private by default, kept as far as the host can
    /// ([`DeskPrivacyPlan::BestEffort`]).
    FollowDeck,
    /// `"required"`: always private, fail closed
    /// ([`DeskPrivacyPlan::Required`]).
    Required,
    /// `"off"`: never private.
    Off,
}

/// The effective desk privacy policy of one Pier (ADR 0025 §5, amended
/// 2026-11-18 and 2026-11-24).
///
/// Every Pier resolves it the same way and applies it to every session
/// whose picture is on the desk's own monitors
/// ([`DeskExposure::PhysicalDesk`]); a desktop no monitor at the desk shows
/// has nothing to keep private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskPrivacyPolicy {
    /// What `privacy.desk` leaves to each attachment.
    pub mode: DeskPrivacyMode,
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
                mode: match setting {
                    DeskPrivacySetting::On => DeskPrivacyMode::FollowDeck,
                    DeskPrivacySetting::Off => DeskPrivacyMode::Off,
                    DeskPrivacySetting::Required => DeskPrivacyMode::Required,
                },
                source: DeskPrivacySource::Config,
            },
            None => Self {
                mode: DeskPrivacyMode::FollowDeck,
                source: DeskPrivacySource::Default,
            },
        }
    }

    /// Whether any attachment may be kept private: the administrator did
    /// not turn desk privacy off.
    #[must_use]
    pub const fn enabled(self) -> bool {
        !matches!(self.mode, DeskPrivacyMode::Off)
    }

    /// Whether every attachment must be private, failing closed.
    #[must_use]
    pub const fn required(self) -> bool {
        matches!(self.mode, DeskPrivacyMode::Required)
    }

    /// The line every Pier logs at start, so the administrator sees the
    /// effective choice and where it came from (a warning when off).
    #[must_use]
    pub const fn startup_line(self) -> &'static str {
        match (self.mode, self.source) {
            (DeskPrivacyMode::FollowDeck, DeskPrivacySource::Default) => {
                "desk privacy: on (default): each Deck's user chooses, private unless they turn \
                 it off; while a private Deck is attached the desk's monitors are blanked and its \
                 input is blocked as far as this host can, and what it cannot do (a dummy plug, \
                 a monitor that cannot be darkened) is reported, not refused; privacy.desk = \
                 \"required\" refuses instead, \"off\" turns it off"
            }
            (DeskPrivacyMode::FollowDeck, DeskPrivacySource::Config) => {
                "desk privacy: on (privacy.desk): each Deck's user chooses, private unless they \
                 turn it off; while a private Deck is attached the desk's monitors are blanked \
                 and its input is blocked as far as this host can, and what it cannot do (a \
                 dummy plug, a monitor that cannot be darkened) is reported, not refused"
            }
            (DeskPrivacyMode::Required, _) => {
                "desk privacy: REQUIRED (privacy.desk = \"required\"): every Deck's session is \
                 private whatever the Deck asks; a desk that cannot be made private (a dummy \
                 plug, a monitor that cannot be darkened) refuses the session, or ends it"
            }
            (DeskPrivacyMode::Off, _) => {
                "desk privacy: OFF (privacy.desk = \"off\"): while a Deck is attached anyone at \
                 the desk sees the session and can use its keyboard and mouse"
            }
        }
    }

    /// What to do for one attachment whose desktop has `exposure`, for a
    /// Deck that asked `deck`.
    #[must_use]
    pub const fn plan(self, exposure: DeskExposure, deck: DeckDeskPrivacy) -> DeskPrivacyPlan {
        match (exposure, self.mode, deck) {
            (DeskExposure::NotShown, _, _) => DeskPrivacyPlan::NotShown,
            (DeskExposure::PhysicalDesk, DeskPrivacyMode::Off, _) => DeskPrivacyPlan::OperatorOff,
            (DeskExposure::PhysicalDesk, DeskPrivacyMode::Required, _) => DeskPrivacyPlan::Required,
            (DeskExposure::PhysicalDesk, DeskPrivacyMode::FollowDeck, DeckDeskPrivacy::Private) => {
                DeskPrivacyPlan::BestEffort
            }
            (DeskExposure::PhysicalDesk, DeskPrivacyMode::FollowDeck, DeckDeskPrivacy::Visible) => {
                DeskPrivacyPlan::DeckVisible
            }
        }
    }
}

/// What the Deck's user asked for the host's desk
/// (`AuthResponse.desk_privacy_request`). Absent (an older Deck) and any
/// token this build does not know are [`Self::Private`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeckDeskPrivacy {
    /// Keep the host's desk private while attached (the Deck's default).
    #[default]
    Private,
    /// The user turned desk privacy off: the desk may see the session.
    Visible,
}

impl DeckDeskPrivacy {
    /// Wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Visible => "visible",
        }
    }

    /// The Deck's request as a Pier reads it: only `visible` turns privacy
    /// off; absent and unknown tokens keep the desk private.
    #[must_use]
    pub fn from_request(token: Option<&str>) -> Self {
        match token {
            Some("visible") => Self::Visible,
            _ => Self::Private,
        }
    }

    /// The request for the Deck's setting "Keep the host's desk private".
    #[must_use]
    pub const fn from_setting(keep_private: bool) -> Self {
        if keep_private {
            Self::Private
        } else {
            Self::Visible
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
    /// The administrator requires privacy (`"required"`): blank the desk's
    /// monitors and block its input before the Deck sees anything; a desk
    /// that cannot be made private refuses the attachment, and one that
    /// stops being private ends it (`console_privacy_unavailable`).
    Required,
    /// The Deck's user asked for privacy: apply every measure the host can
    /// and report what it could not ([`DeskPrivacyState`]); never refuse or
    /// end the attachment for it.
    BestEffort,
    /// The Deck's user turned desk privacy off: the desk stays live.
    DeckVisible,
    /// The administrator turned desk privacy off: the desk stays live, and
    /// the Deck is told.
    OperatorOff,
    /// Nothing at the desk shows the desktop.
    NotShown,
}

impl DeskPrivacyPlan {
    /// Whether the Pier makes the desk private for this attachment.
    #[must_use]
    pub const fn protects(self) -> bool {
        matches!(self, Self::Required | Self::BestEffort)
    }

    /// Whether a desk that cannot be made private refuses (or ends) the
    /// attachment.
    #[must_use]
    pub const fn fails_closed(self) -> bool {
        matches!(self, Self::Required)
    }

    /// The status a Pier reports for an attachment served under this plan
    /// with every measure in place (a shortfall is
    /// [`DeskPrivacyState::status`]'s to report).
    #[must_use]
    pub const fn status(self) -> DeskPrivacyStatus {
        match self {
            Self::Required => DeskPrivacyStatus::Required,
            Self::BestEffort => DeskPrivacyStatus::Blanked,
            Self::DeckVisible => DeskPrivacyStatus::Visible,
            Self::OperatorOff => DeskPrivacyStatus::Off,
            Self::NotShown => DeskPrivacyStatus::NotShown,
        }
    }
}

/// Which part of the desk a [`DeskPrivacyRefusal`] left unprotected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPart {
    /// Its monitors: one shows, or may show, the session.
    Screens,
    /// Its keyboard and mouse: they reach the session.
    Input,
}

impl DeskPrivacyRefusal {
    /// Which part of the desk this leaves unprotected.
    #[must_use]
    pub const fn part(self) -> DeskPart {
        match self {
            Self::VirtualHidInput | Self::InputFilterUnavailable | Self::InputBlockUnavailable => {
                DeskPart::Input
            }
            Self::DisplaysNotDark
            | Self::DeskMonitorLit
            | Self::DeskMonitorUnknown
            | Self::DeskOutputsChanged
            | Self::DeskDetachFailed => DeskPart::Screens,
        }
    }
}

/// What could not be made private, part by part (the first cause of each).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeskShortfall {
    /// Why the screens are not all dark.
    pub screens: Option<DeskPrivacyRefusal>,
    /// Why the input is not blocked.
    pub input: Option<DeskPrivacyRefusal>,
}

impl DeskShortfall {
    /// Records `refusal` against its part (the first cause stays).
    pub fn record(&mut self, refusal: DeskPrivacyRefusal) {
        let slot = match refusal.part() {
            DeskPart::Screens => &mut self.screens,
            DeskPart::Input => &mut self.input,
        };
        slot.get_or_insert(refusal);
    }

    /// Nothing is missing.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.screens.is_none() && self.input.is_none()
    }

    /// Both shortfalls together.
    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self {
            screens: match self.screens {
                Some(refusal) => Some(refusal),
                None => other.screens,
            },
            input: match self.input {
                Some(refusal) => Some(refusal),
                None => other.input,
            },
        }
    }

    /// The status of a private desk with this shortfall.
    #[must_use]
    pub const fn status(self) -> DeskPrivacyStatus {
        match (self.screens, self.input) {
            (None, None) => DeskPrivacyStatus::Blanked,
            (Some(_), None) => DeskPrivacyStatus::ScreensVisible,
            (None, Some(_)) => DeskPrivacyStatus::InputLive,
            (Some(_), Some(_)) => DeskPrivacyStatus::NotPrivate,
        }
    }
}

/// The desk privacy of one attachment, from its plan to every shortfall
/// met while it is attached: the one table every Pier decides by.
///
/// | Plan | A measure fails at attach | A re-proof fails mid-session |
/// | --- | --- | --- |
/// | [`DeskPrivacyPlan::Required`] | refuse (`console_privacy_unavailable`) | end the session |
/// | [`DeskPrivacyPlan::BestEffort`] | keep the rest, warn, report it | keep the session, warn, report it; report the recovery |
/// | [`DeskPrivacyPlan::DeckVisible`], [`DeskPrivacyPlan::OperatorOff`], [`DeskPrivacyPlan::NotShown`] | nothing is applied | nothing is watched |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskPrivacyState {
    plan: DeskPrivacyPlan,
    attach: DeskShortfall,
    watch: DeskShortfall,
    told: Option<DeskPrivacyStatus>,
}

impl DeskPrivacyState {
    /// An attachment about to be served under `plan`, nothing yet missing.
    #[must_use]
    pub const fn new(plan: DeskPrivacyPlan) -> Self {
        Self {
            plan,
            attach: DeskShortfall {
                screens: None,
                input: None,
            },
            watch: DeskShortfall {
                screens: None,
                input: None,
            },
            told: None,
        }
    }

    /// The plan.
    #[must_use]
    pub const fn plan(&self) -> DeskPrivacyPlan {
        self.plan
    }

    /// One measure could not be applied while the desk was being made
    /// private. Under [`DeskPrivacyPlan::BestEffort`] it is recorded and the
    /// attachment goes on; a plan that does not protect ignores it.
    ///
    /// # Errors
    ///
    /// `refusal` under [`DeskPrivacyPlan::Required`]: the attachment is
    /// refused.
    pub fn attach_failed(&mut self, refusal: DeskPrivacyRefusal) -> Result<(), DeskPrivacyRefusal> {
        match self.plan {
            DeskPrivacyPlan::Required => Err(refusal),
            DeskPrivacyPlan::BestEffort => {
                self.attach.record(refusal);
                Ok(())
            }
            DeskPrivacyPlan::DeckVisible
            | DeskPrivacyPlan::OperatorOff
            | DeskPrivacyPlan::NotShown => Ok(()),
        }
    }

    /// One mid-session verdict on the desk (a [`DeskWatch`] verdict, a
    /// display watch): a failure is recorded until a verdict passes again.
    ///
    /// # Errors
    ///
    /// The refusal under [`DeskPrivacyPlan::Required`]: the session ends.
    pub fn watched(
        &mut self,
        verdict: Result<(), DeskPrivacyRefusal>,
    ) -> Result<(), DeskPrivacyRefusal> {
        match (self.plan, verdict) {
            (DeskPrivacyPlan::Required, Err(refusal)) => Err(refusal),
            (DeskPrivacyPlan::BestEffort, Err(refusal)) => {
                self.watch = DeskShortfall::default();
                self.watch.record(refusal);
                Ok(())
            }
            (_, Ok(())) => {
                self.watch = DeskShortfall::default();
                Ok(())
            }
            (_, Err(_)) => Ok(()),
        }
    }

    /// One verdict on one part of the desk, kept until that part's next
    /// verdict (a host that re-asserts its screens and its input apart,
    /// every second: the Linux console): a failure is recorded for `part`,
    /// a pass clears it.
    ///
    /// # Errors
    ///
    /// The refusal under [`DeskPrivacyPlan::Required`].
    pub fn watched_part(
        &mut self,
        part: DeskPart,
        verdict: Result<(), DeskPrivacyRefusal>,
    ) -> Result<(), DeskPrivacyRefusal> {
        let slot = match part {
            DeskPart::Screens => &mut self.watch.screens,
            DeskPart::Input => &mut self.watch.input,
        };
        match (self.plan, verdict) {
            (DeskPrivacyPlan::Required, Err(refusal)) => Err(refusal),
            (DeskPrivacyPlan::BestEffort, Err(refusal)) => {
                *slot = Some(refusal);
                Ok(())
            }
            (_, Ok(())) => {
                *slot = None;
                Ok(())
            }
            (_, Err(_)) => Ok(()),
        }
    }

    /// Everything missing now: what failed at attach and what the last
    /// mid-session verdicts found.
    #[must_use]
    pub fn shortfall(&self) -> DeskShortfall {
        self.attach.with(self.watch)
    }

    /// The desk's status now.
    #[must_use]
    pub fn status(&self) -> DeskPrivacyStatus {
        match self.plan {
            DeskPrivacyPlan::BestEffort => self.shortfall().status(),
            plan => plan.status(),
        }
    }

    /// The status a (re)attaching Deck's hello carries, which later updates
    /// are measured against.
    pub fn hello_status(&mut self) -> DeskPrivacyStatus {
        let status = self.status();
        self.told = Some(status);
        status
    }

    /// The status to tell the Deck, when it differs from the last one told
    /// (the first call always tells): the Pier sends it (the hello's
    /// `desk_privacy`, then `desk_privacy_update`) and logs it once.
    /// Marks it told at once; a Pier whose send can fail uses
    /// [`Self::pending`] and [`Self::mark_told`] instead.
    pub fn tell(&mut self) -> Option<DeskPrivacyStatus> {
        let status = self.pending()?;
        self.mark_told(status);
        Some(status)
    }

    /// The status the Deck has not been told yet, without marking it told:
    /// it stays pending until [`Self::mark_told`], so a send that failed (a
    /// full queue) is tried again.
    #[must_use]
    pub fn pending(&self) -> Option<DeskPrivacyStatus> {
        let status = self.status();
        (self.told != Some(status)).then_some(status)
    }

    /// Records that `status` reached the Deck.
    pub fn mark_told(&mut self, status: DeskPrivacyStatus) {
        self.told = Some(status);
    }
}

/// The desk's state as the Pier reports it to the Deck (the hello's
/// `desk_privacy`, and `desk_privacy_update` when it changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacyStatus {
    /// The desk's monitors are blank and its input blocked.
    Blanked,
    /// As [`Self::Blanked`], because the administrator requires it: the
    /// Deck's own choice was not asked.
    Required,
    /// The administrator turned desk privacy off: the desk shows the
    /// session.
    Off,
    /// The Deck's user turned desk privacy off: the desk shows the session.
    Visible,
    /// No monitor at the desk shows this desktop.
    NotShown,
    /// Private, but not every screen at the desk could be darkened (a dummy
    /// plug, a monitor that cannot be darkened): its input is blocked.
    ScreensVisible,
    /// Private, but the desk's keyboard and mouse could not be blocked: its
    /// screens are dark.
    InputLive,
    /// Neither the screens nor the input could be made private.
    NotPrivate,
}

/// What the Deck shows while the desk is private.
pub const DESK_BLANKED_NOTE: &str = "Host screens are blanked for privacy";

/// What the Deck shows when the host requires desk privacy.
pub const DESK_REQUIRED_NOTE: &str = "Host screens are blanked: this host requires desk privacy";

/// What the Deck shows when the administrator turned desk privacy off.
pub const DESK_VISIBLE_NOTE: &str =
    "Host screens are not blanked: desk privacy is turned off on this host";

/// What the Deck shows when its user turned desk privacy off.
pub const DESK_DECK_VISIBLE_NOTE: &str = "Host screens are visible: you turned desk privacy off.";

/// What the Deck shows when a screen at the host could not be darkened.
pub const DESK_SCREENS_VISIBLE_NOTE: &str = "Host screens could not be blanked (a dummy plug or a \
     monitor that cannot be darkened). People at the host may see this session.";

/// What the Deck shows when the host's input could not be blocked.
pub const DESK_INPUT_LIVE_NOTE: &str = "The host's keyboard and mouse could not be blocked. \
     People at the host may use this session.";

/// What the Deck shows when neither could be made private.
pub const DESK_NOT_PRIVATE_NOTE: &str = "Host screens could not be blanked and its keyboard and \
     mouse could not be blocked. People at the host may see and use this session.";

impl DeskPrivacyStatus {
    /// Every status, for exhaustive checks.
    pub const ALL: [Self; 8] = [
        Self::Blanked,
        Self::Required,
        Self::Off,
        Self::Visible,
        Self::NotShown,
        Self::ScreensVisible,
        Self::InputLive,
        Self::NotPrivate,
    ];

    /// Wire token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Blanked => "blanked",
            Self::Required => "required",
            Self::Off => "off",
            Self::Visible => "visible",
            Self::NotShown => "not_shown",
            Self::ScreensVisible => "screens_visible",
            Self::InputLive => "input_live",
            Self::NotPrivate => "not_private",
        }
    }

    /// Parses a wire token; `None` for a token this build does not know.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.token() == token)
    }

    /// The one line the Deck shows for this status, if any.
    #[must_use]
    pub const fn deck_note(self) -> Option<&'static str> {
        match self {
            Self::Blanked => Some(DESK_BLANKED_NOTE),
            Self::Required => Some(DESK_REQUIRED_NOTE),
            Self::Off => Some(DESK_VISIBLE_NOTE),
            Self::Visible => Some(DESK_DECK_VISIBLE_NOTE),
            Self::NotShown => None,
            Self::ScreensVisible => Some(DESK_SCREENS_VISIBLE_NOTE),
            Self::InputLive => Some(DESK_INPUT_LIVE_NOTE),
            Self::NotPrivate => Some(DESK_NOT_PRIVATE_NOTE),
        }
    }

    /// Privacy was asked for and is not whole: people at the host may see
    /// or use the session. The Deck shows it as a warning.
    #[must_use]
    pub const fn is_warning(self) -> bool {
        matches!(
            self,
            Self::ScreensVisible | Self::InputLive | Self::NotPrivate
        )
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
    fn desk_privacy_follows_the_deck_unless_the_administrator_requires_or_turns_it_off() {
        let default = DeskPrivacyPolicy::resolve(None);
        assert_eq!(default.mode, DeskPrivacyMode::FollowDeck);
        assert!(default.enabled() && !default.required());
        assert_eq!(default.source, DeskPrivacySource::Default);
        assert_eq!(DeskPrivacyPolicy::default(), default);
        assert!(default.startup_line().contains("on (default)"));
        assert!(default.startup_line().contains("not refused"));

        let on = DeskPrivacyPolicy::resolve(Some(DeskPrivacySetting::On));
        assert_eq!(on.mode, DeskPrivacyMode::FollowDeck);
        assert_eq!(on.source, DeskPrivacySource::Config);

        let off = DeskPrivacyPolicy::resolve(Some(DeskPrivacySetting::Off));
        assert!(!off.enabled() && !off.required());
        assert!(off.startup_line().contains("OFF"));

        let required = DeskPrivacyPolicy::resolve(Some(DeskPrivacySetting::Required));
        assert!(required.enabled() && required.required());
        assert!(required.startup_line().contains("REQUIRED"));
        assert!(required.startup_line().contains("dummy plug"));
    }

    #[test]
    fn the_deck_asks_private_unless_it_says_visible() {
        assert_eq!(
            DeckDeskPrivacy::from_request(None),
            DeckDeskPrivacy::Private
        );
        assert_eq!(
            DeckDeskPrivacy::from_request(Some("private")),
            DeckDeskPrivacy::Private
        );
        assert_eq!(
            DeckDeskPrivacy::from_request(Some("visible")),
            DeckDeskPrivacy::Visible
        );
        for unknown in ["", "VISIBLE", "off", "dimmed"] {
            assert_eq!(
                DeckDeskPrivacy::from_request(Some(unknown)),
                DeckDeskPrivacy::Private,
                "{unknown:?}"
            );
        }
        for deck in [DeckDeskPrivacy::Private, DeckDeskPrivacy::Visible] {
            assert_eq!(DeckDeskPrivacy::from_request(Some(deck.token())), deck);
        }
        assert_eq!(
            DeckDeskPrivacy::from_setting(true),
            DeckDeskPrivacy::Private
        );
        assert_eq!(
            DeckDeskPrivacy::from_setting(false),
            DeckDeskPrivacy::Visible
        );
        assert_eq!(DeckDeskPrivacy::default(), DeckDeskPrivacy::Private);
    }

    /// The whole decision table: `privacy.desk` x the Deck's choice x the
    /// desktop's exposure x what happens at attach and mid-session.
    #[test]
    fn every_choice_config_and_outcome_is_decided_by_one_table() {
        use DeskPrivacyPlan as Plan;
        use DeskPrivacyStatus as Status;
        let screens = DeskPrivacyRefusal::DeskMonitorLit;
        let input = DeskPrivacyRefusal::InputBlockUnavailable;
        let settings = [
            None,
            Some(DeskPrivacySetting::On),
            Some(DeskPrivacySetting::Required),
            Some(DeskPrivacySetting::Off),
        ];
        for setting in settings {
            let policy = DeskPrivacyPolicy::resolve(setting);
            for deck in [DeckDeskPrivacy::Private, DeckDeskPrivacy::Visible] {
                assert_eq!(
                    policy.plan(DeskExposure::NotShown, deck),
                    Plan::NotShown,
                    "{setting:?} {deck:?}"
                );
                let plan = policy.plan(DeskExposure::PhysicalDesk, deck);
                let expected = match (setting, deck) {
                    (Some(DeskPrivacySetting::Off), _) => Plan::OperatorOff,
                    (Some(DeskPrivacySetting::Required), _) => Plan::Required,
                    (_, DeckDeskPrivacy::Private) => Plan::BestEffort,
                    (_, DeckDeskPrivacy::Visible) => Plan::DeckVisible,
                };
                assert_eq!(plan, expected, "{setting:?} {deck:?}");
            }
        }

        // Everything in place: each plan's own status.
        for (plan, status) in [
            (Plan::Required, Status::Required),
            (Plan::BestEffort, Status::Blanked),
            (Plan::DeckVisible, Status::Visible),
            (Plan::OperatorOff, Status::Off),
            (Plan::NotShown, Status::NotShown),
        ] {
            assert_eq!(plan.status(), status);
            assert_eq!(DeskPrivacyState::new(plan).status(), status, "{plan:?}");
            assert_eq!(
                plan.protects(),
                matches!(plan, Plan::Required | Plan::BestEffort)
            );
            assert_eq!(plan.fails_closed(), plan == Plan::Required);
        }

        // At attach: required refuses; best effort keeps the session and
        // says which part is missing; the rest apply nothing.
        for refusal in [screens, input] {
            let mut required = DeskPrivacyState::new(Plan::Required);
            assert_eq!(required.attach_failed(refusal), Err(refusal));
            for plan in [Plan::DeckVisible, Plan::OperatorOff, Plan::NotShown] {
                let mut state = DeskPrivacyState::new(plan);
                assert_eq!(state.attach_failed(refusal), Ok(()));
                assert_eq!(state.status(), plan.status());
            }
        }
        let best = |failures: &[DeskPrivacyRefusal]| {
            let mut state = DeskPrivacyState::new(Plan::BestEffort);
            for refusal in failures {
                assert_eq!(state.attach_failed(*refusal), Ok(()));
            }
            state.status()
        };
        assert_eq!(best(&[]), Status::Blanked);
        assert_eq!(best(&[screens]), Status::ScreensVisible);
        assert_eq!(best(&[input]), Status::InputLive);
        assert_eq!(best(&[input, screens]), Status::NotPrivate);
        assert_eq!(
            best(&[
                DeskPrivacyRefusal::VirtualHidInput,
                DeskPrivacyRefusal::DisplaysNotDark
            ]),
            Status::NotPrivate
        );

        // Mid-session: required ends; best effort warns, then recovers.
        let mut required = DeskPrivacyState::new(Plan::Required);
        assert_eq!(required.watched(Ok(())), Ok(()));
        assert_eq!(required.watched(Err(screens)), Err(screens));
        let mut state = DeskPrivacyState::new(Plan::BestEffort);
        assert_eq!(state.hello_status(), Status::Blanked);
        assert_eq!(state.tell(), None, "the hello told it");
        assert_eq!(state.watched(Ok(())), Ok(()));
        assert_eq!(state.tell(), None, "unchanged is not told again");
        assert_eq!(state.watched(Err(screens)), Ok(()));
        assert_eq!(state.tell(), Some(Status::ScreensVisible));
        assert_eq!(state.watched(Err(screens)), Ok(()));
        assert_eq!(state.tell(), None, "a failure is told (and logged) once");
        assert_eq!(state.watched(Ok(())), Ok(()));
        assert_eq!(state.tell(), Some(Status::Blanked), "the recovery is told");
        // A shortfall from the attach stays when the watch recovers.
        let mut partial = DeskPrivacyState::new(Plan::BestEffort);
        assert_eq!(partial.attach_failed(input), Ok(()));
        assert_eq!(partial.watched(Err(screens)), Ok(()));
        assert_eq!(partial.status(), Status::NotPrivate);
        assert_eq!(partial.watched(Ok(())), Ok(()));
        assert_eq!(partial.status(), Status::InputLive);
        // Plans that protect nothing never end or change.
        for plan in [Plan::DeckVisible, Plan::OperatorOff, Plan::NotShown] {
            let mut state = DeskPrivacyState::new(plan);
            assert_eq!(state.watched(Err(screens)), Ok(()));
            assert_eq!(state.status(), plan.status());
        }
    }

    #[test]
    fn only_a_desktop_on_the_desk_is_protected_and_the_deck_is_told() {
        let on = DeskPrivacyPolicy::default();
        let private = DeckDeskPrivacy::Private;
        assert_eq!(
            on.plan(DeskExposure::PhysicalDesk, private),
            DeskPrivacyPlan::BestEffort
        );
        assert_eq!(
            DeskPrivacyPlan::BestEffort.status().deck_note(),
            Some("Host screens are blanked for privacy")
        );
        assert!(
            DeskPrivacyPlan::OperatorOff
                .status()
                .deck_note()
                .is_some_and(|note| note.contains("not blanked"))
        );
        assert!(
            DeskPrivacyPlan::DeckVisible
                .status()
                .deck_note()
                .is_some_and(|note| note.contains("you turned desk privacy off"))
        );
        assert!(
            DeskPrivacyPlan::Required
                .status()
                .deck_note()
                .is_some_and(|note| note.contains("requires"))
        );
        assert!(
            DeskPrivacyStatus::ScreensVisible
                .deck_note()
                .is_some_and(|note| note.contains("dummy plug"))
        );
        assert_eq!(DeskPrivacyPlan::NotShown.status().deck_note(), None);
        for status in DeskPrivacyStatus::ALL {
            assert_eq!(
                status.is_warning(),
                matches!(
                    status,
                    DeskPrivacyStatus::ScreensVisible
                        | DeskPrivacyStatus::InputLive
                        | DeskPrivacyStatus::NotPrivate
                ),
                "{status:?}"
            );
        }
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
        assert_eq!(DeskPrivacyRefusal::from_token("dark_enough"), None);
        for refusal in DeskPrivacyRefusal::ALL {
            assert_eq!(
                DeskPrivacyRefusal::from_token(refusal.token()),
                Some(refusal)
            );
            let reason = refusal.deck_reason();
            assert!(
                reason.starts_with("console_privacy_unavailable: "),
                "{reason}"
            );
            assert!(reason.contains("desk privacy is required"), "{reason}");
            assert_eq!(
                reason.contains("dummy plug"),
                refusal.part() == DeskPart::Screens,
                "{reason}"
            );
            assert!(reason.len() <= 120, "{reason}");
        }
    }

    #[test]
    fn desk_privacy_status_round_trips_and_ignores_unknown_tokens() {
        for status in DeskPrivacyStatus::ALL {
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
                Err(DeskProofFailure::decided(
                    DeskPrivacyRefusal::DeskMonitorLit,
                ))
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
        assert_eq!(
            proofs.load(Ordering::SeqCst),
            1,
            "a lit monitor is never repeated"
        );
        // The verdict is stored before the report is made: wait for it.
        assert!(wait_for(Duration::from_secs(5), || {
            !reports.lock().unwrap().is_empty()
        }));
        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].reason, FullProofReason::TopologyChanged);
        assert_eq!(reports[0].attempts, 1);
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

    fn lit(outputs: &[(u32, DeskMonitor)]) -> DeskReading<u32> {
        DeskReading::Lit(
            outputs
                .iter()
                .map(|&(id, monitor)| LitDeskOutput { id, monitor })
                .collect(),
        )
    }

    #[test]
    fn only_a_desk_that_could_not_be_read_whole_may_be_proved_again() {
        use DeskMonitor::{Absent, Present, Unknown};
        let proved: std::collections::BTreeSet<u32> = [1, 2].into_iter().collect();
        let transient = Err(DeskProofFailure::transient(
            DeskPrivacyRefusal::DeskMonitorUnknown,
        ));
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(1, Absent), (2, Absent)])),
            Ok(())
        );
        // A driver re-detecting its displays: unreadable, nothing lit, or
        // an output proved dark that cannot be described this moment.
        assert_eq!(
            recheck_dark_desk(&proved, &DeskReading::Unreadable),
            transient
        );
        assert_eq!(recheck_dark_desk(&proved, &lit(&[])), transient);
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(1, Absent), (2, Unknown)])),
            transient
        );
        // Decided at once: a monitor, an output lit anew that cannot be
        // described (an integrated GPU, a USB display), any other change.
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(1, Absent), (2, Present)])),
            Err(DeskProofFailure::decided(
                DeskPrivacyRefusal::DeskMonitorLit
            ))
        );
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(1, Absent), (2, Absent), (3, Unknown)])),
            Err(DeskProofFailure::decided(
                DeskPrivacyRefusal::DeskMonitorUnknown
            ))
        );
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(1, Unknown), (3, Unknown)])),
            Err(DeskProofFailure::decided(
                DeskPrivacyRefusal::DeskMonitorUnknown
            ))
        );
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(1, Absent), (3, Absent)])),
            Err(DeskProofFailure::decided(
                DeskPrivacyRefusal::DeskOutputsChanged
            ))
        );
        // Fewer outputs than were proved show the desktop nowhere new.
        let fewer = Err(DeskProofFailure::transient(
            DeskPrivacyRefusal::DeskOutputsChanged,
        ));
        assert_eq!(recheck_dark_desk(&proved, &lit(&[(1, Absent)])), fewer);
        assert_eq!(recheck_dark_desk(&proved, &lit(&[(1, Unknown)])), transient);
        // ... but a new output instead of them is decided.
        assert_eq!(
            recheck_dark_desk(&proved, &lit(&[(3, Absent)])),
            Err(DeskProofFailure::decided(
                DeskPrivacyRefusal::DeskOutputsChanged
            ))
        );
    }

    /// Runs a watch over a desk proved with `proved` lit that reads
    /// `readings` in turn (the last one again and again) after a topology
    /// change; returns its verdict once it ends or after `wait`, how many
    /// proofs it made, its first report and how long it ran.
    fn watch_readings(
        proved: &[u32],
        readings: Vec<DeskReading<u32>>,
        wait: Duration,
    ) -> (
        Result<(), DeskPrivacyRefusal>,
        u32,
        Option<DeskProofReport>,
        Duration,
    ) {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};
        let proved: std::collections::BTreeSet<u32> = proved.iter().copied().collect();
        let proofs = Arc::new(AtomicU32::new(0));
        let reports = Arc::new(Mutex::new(Vec::<DeskProofReport>::new()));
        let (proof_count, seen) = (Arc::clone(&proofs), Arc::clone(&reports));
        let started = Instant::now();
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            || Some(2),
            move || {
                let index = proof_count.fetch_add(1, Ordering::SeqCst) as usize;
                recheck_dark_desk(&proved, &readings[index.min(readings.len() - 1)])
            },
            move |report: &DeskProofReport| seen.lock().unwrap().push(*report),
            || {},
        );
        let ended = wait_for(wait, || watch.verdict(Instant::now()).is_err());
        let elapsed = started.elapsed();
        // An ending verdict is stored before its report is made.
        wait_for(Duration::from_secs(5), || {
            reports
                .lock()
                .unwrap()
                .last()
                .is_some_and(|report| report.outcome.is_err() == ended)
        });
        let verdict = watch.verdict(Instant::now());
        let first = reports.lock().unwrap().first().copied();
        (verdict, proofs.load(Ordering::SeqCst), first, elapsed)
    }

    #[test]
    fn a_proved_output_dropping_out_for_a_moment_settles() {
        use DeskMonitor::Absent;
        let (verdict, proofs, report, _) = watch_readings(
            &[1, 2],
            vec![lit(&[(1, Absent)]), lit(&[(1, Absent), (2, Absent)])],
            Duration::from_secs(2),
        );
        assert_eq!(verdict, Ok(()));
        assert_eq!(proofs, 2);
        let report = report.expect("reported");
        assert_eq!(report.outcome, Ok(()));
        assert_eq!(report.attempts, 2);
    }

    #[test]
    fn fewer_outputs_then_a_new_one_ends_the_session_at_once() {
        use DeskMonitor::Absent;
        let (verdict, proofs, report, _) = watch_readings(
            &[1, 2],
            vec![lit(&[(1, Absent)]), lit(&[(1, Absent), (3, Absent)])],
            Duration::from_secs(5),
        );
        assert_eq!(verdict, Err(DeskPrivacyRefusal::DeskOutputsChanged));
        assert_eq!(proofs, 2, "the new output is not repeated");
        assert_eq!(report.expect("reported").attempts, 2);
    }

    #[test]
    fn a_proved_output_that_stays_undescribed_ends_the_session_once_the_settle_runs_out() {
        use DeskMonitor::{Absent, Unknown};
        let (verdict, proofs, report, elapsed) = watch_readings(
            &[1, 2],
            vec![lit(&[(1, Absent), (2, Unknown)])],
            Duration::from_secs(10),
        );
        assert_eq!(verdict, Err(DeskPrivacyRefusal::DeskMonitorUnknown));
        assert!(proofs > 1, "repeated while it settled");
        let report = report.expect("reported");
        assert_eq!(report.outcome, Err(DeskPrivacyRefusal::DeskMonitorUnknown));
        assert!(
            report.took < DESK_PROOF_SETTLE + DESK_PROOF_SETTLE_RETRY,
            "{report:?}"
        );
        assert!(
            elapsed < DESK_PROOF_IN_FLIGHT_LIMIT + Duration::from_secs(1),
            "{elapsed:?}"
        );
    }

    #[test]
    fn a_desk_that_could_not_be_read_while_the_displays_settle_is_repeated_not_fatal() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};
        let topology = Arc::new(AtomicU32::new(1));
        let proofs = Arc::new(AtomicU32::new(0));
        let reports = Arc::new(Mutex::new(Vec::new()));
        let (read_topology, proof_count, seen) = (
            Arc::clone(&topology),
            Arc::clone(&proofs),
            Arc::clone(&reports),
        );
        let proved: std::collections::BTreeSet<u32> = [1].into_iter().collect();
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            move || Some(read_topology.load(Ordering::SeqCst)),
            move || {
                // A GPU control panel re-detects the displays: the desk
                // reads unreadable, then with nothing lit, then whole.
                let reading = match proof_count.fetch_add(1, Ordering::SeqCst) {
                    0 => DeskReading::Unreadable,
                    1 => lit(&[]),
                    _ => lit(&[(1, DeskMonitor::Absent)]),
                };
                recheck_dark_desk(&proved, &reading)
            },
            move |report: &DeskProofReport| seen.lock().unwrap().push(*report),
            || {},
        );
        topology.store(2, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(5), || {
            !reports.lock().unwrap().is_empty()
        }));
        assert_eq!(watch.verdict(Instant::now()), Ok(()));
        let report = reports.lock().unwrap()[0];
        assert_eq!(report.reason, FullProofReason::TopologyChanged);
        assert_eq!(report.attempts, 3);
        assert_eq!(report.outcome, Ok(()));
        assert!(report.took >= DESK_PROOF_SETTLE_RETRY * 2);
        assert!(report.settle_left < DESK_SETTLE_BUDGET);
        // The settled topology was recorded: nothing more to prove.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(proofs.load(Ordering::SeqCst), 3);
        assert_eq!(watch.verdict(Instant::now()), Ok(()));
    }

    #[test]
    fn an_output_lit_anew_that_cannot_be_described_ends_the_session_at_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let proofs = Arc::new(AtomicU32::new(0));
        let proof_count = Arc::clone(&proofs);
        let proved: std::collections::BTreeSet<u32> = [1].into_iter().collect();
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            || Some(2),
            move || {
                proof_count.fetch_add(1, Ordering::SeqCst);
                // A monitor on the integrated GPU (or a USB display) lit.
                recheck_dark_desk(
                    &proved,
                    &lit(&[(1, DeskMonitor::Absent), (7, DeskMonitor::Unknown)]),
                )
            },
            |_| {},
            || {},
        );
        let changed = Instant::now();
        assert!(wait_for(Duration::from_secs(5), || {
            watch.verdict(Instant::now()).is_err()
        }));
        assert!(
            changed.elapsed() < DESK_PROOF_SETTLE_RETRY,
            "{:?}",
            changed.elapsed()
        );
        assert_eq!(proofs.load(Ordering::SeqCst), 1, "never repeated");
        assert_eq!(
            watch.verdict(Instant::now()),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
    }

    #[test]
    fn a_desk_that_stays_unreadable_ends_the_session_once_the_settle_runs_out() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        let proofs = Arc::new(AtomicU32::new(0));
        let proof_count = Arc::clone(&proofs);
        let changed = Instant::now();
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(1_u32),
            || Some(2),
            move || {
                proof_count.fetch_add(1, Ordering::SeqCst);
                Err(DeskProofFailure::transient(
                    DeskPrivacyRefusal::DeskMonitorUnknown,
                ))
            },
            |_| {},
            || {},
        );
        assert!(wait_for(Duration::from_secs(10), || {
            watch.verdict(Instant::now()).is_err()
        }));
        let ended = changed.elapsed();
        assert_eq!(
            watch.verdict(Instant::now()),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
        assert!(
            proofs.load(Ordering::SeqCst) > 1,
            "repeated while it settled"
        );
        assert!(
            ended < DESK_PROOF_IN_FLIGHT_LIMIT + Duration::from_secs(1),
            "{ended:?}"
        );
        assert!(DESK_PROOF_SETTLE < DESK_PROOF_IN_FLIGHT_LIMIT);
    }

    #[test]
    fn repeated_unreadable_desks_spend_the_budget_and_then_end_the_session() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::{Arc, Mutex};
        let reads = Arc::new(AtomicU32::new(0));
        let proofs = Arc::new(AtomicU32::new(0));
        let reports = Arc::new(Mutex::new(Vec::<DeskProofReport>::new()));
        let (read_count, proof_count, seen) = (
            Arc::clone(&reads),
            Arc::clone(&proofs),
            Arc::clone(&reports),
        );
        // Every check reads a new topology, and every desk reading fails
        // once, then passes on the repeat: Unknown, Ok, Unknown, Ok, ...
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            Some(0_u32),
            move || Some(read_count.fetch_add(1, Ordering::SeqCst) + 1),
            move || {
                if proof_count.fetch_add(1, Ordering::SeqCst) % 2 == 0 {
                    Err(DeskProofFailure::transient(
                        DeskPrivacyRefusal::DeskMonitorUnknown,
                    ))
                } else {
                    Ok(())
                }
            },
            move |report: &DeskProofReport| seen.lock().unwrap().push(*report),
            || {},
        );
        assert!(wait_for(Duration::from_secs(30), || {
            watch.verdict(Instant::now()).is_err()
        }));
        assert_eq!(
            watch.verdict(Instant::now()),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
        );
        // The verdict is stored before the report is made: wait for it.
        assert!(wait_for(Duration::from_secs(5), || {
            reports
                .lock()
                .unwrap()
                .last()
                .is_some_and(|report| report.outcome.is_err())
        }));
        let reports = reports.lock().unwrap();
        let last = reports.last().expect("the ending proof is reported");
        assert_eq!(last.outcome, Err(DeskPrivacyRefusal::DeskMonitorUnknown));
        assert_eq!(last.attempts, 1, "not repeated once the budget is spent");
        let settled = reports
            .iter()
            .filter(|report| report.outcome.is_ok())
            .count();
        assert!(settled >= 2, "{settled} settled before the budget ran out");
        let spent: Duration = reports
            .iter()
            .filter(|report| report.attempts > 1)
            .map(|report| report.took)
            .sum();
        // Each repeat starts only while it fits; it may overrun by its own
        // proof time.
        assert!(
            spent <= DESK_SETTLE_BUDGET + DESK_PROOF_SETTLE_RETRY,
            "{spent:?}"
        );
        assert!(
            spent + DESK_PROOF_SETTLE_RETRY * 2 > DESK_SETTLE_BUDGET,
            "{spent:?}: ended only once the budget was spent"
        );
    }

    #[test]
    fn a_settling_interval_proof_is_held_to_the_in_flight_limit() {
        use std::sync::mpsc;
        let (proving_tx, proving_rx) = mpsc::channel();
        // Nothing was proved yet, so the first proof is an interval proof
        // (no topology change made it due); it fails and is repeated, slowly:
        // the verdict must not wait for the watcher to give up.
        let watch = DeskWatch::start(
            Duration::from_millis(5),
            None::<u32>,
            || Some(1),
            move || {
                let _ = proving_tx.send(Instant::now());
                std::thread::sleep(Duration::from_millis(900));
                Err(DeskProofFailure::transient(
                    DeskPrivacyRefusal::DeskMonitorUnknown,
                ))
            },
            |_| {},
            || {},
        );
        let first = proving_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let _second = proving_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            watch.verdict(first + DESK_PROOF_IN_FLIGHT_LIMIT),
            Err(DeskPrivacyRefusal::DeskMonitorUnknown)
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
            || {
                Err(DeskProofFailure::decided(
                    DeskPrivacyRefusal::DeskMonitorLit,
                ))
            },
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

    #[test]
    fn a_best_effort_watch_keeps_proving_after_a_refusal_and_reports_the_recovery() {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::{Arc, mpsc};
        let lit = Arc::new(AtomicBool::new(true));
        let proofs = Arc::new(AtomicU32::new(0));
        let (lit_now, proof_count) = (Arc::clone(&lit), Arc::clone(&proofs));
        let (alarm_tx, alarm_rx) = mpsc::channel();
        let watch = DeskWatch::start_with(
            DeskWatchOnRefusal::Keep,
            Duration::from_millis(5),
            Some(1_u32),
            || Some(2),
            move || {
                proof_count.fetch_add(1, Ordering::SeqCst);
                if lit_now.load(Ordering::SeqCst) {
                    Err(DeskProofFailure::decided(
                        DeskPrivacyRefusal::DeskMonitorLit,
                    ))
                } else {
                    Ok(())
                }
            },
            |_| {},
            move || {
                let _ = alarm_tx.send(());
            },
        );
        alarm_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            watch.verdict(Instant::now()),
            Err(DeskPrivacyRefusal::DeskMonitorLit)
        );
        // Still watching, but not proving faster than the interval.
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(proofs.load(Ordering::SeqCst), 1);
        lit.store(false, Ordering::SeqCst);
        alarm_rx
            .recv_timeout(DESK_FULL_PROOF_INTERVAL + Duration::from_secs(5))
            .unwrap();
        assert_eq!(watch.verdict(Instant::now()), Ok(()));
        assert_eq!(proofs.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn desk_privacy_never_takes_a_served_output_away() {
        use std::collections::BTreeSet;
        assert_eq!(
            desk_output_measure(DeskOutputRole::Served),
            DeskOutputMeasure::KeepServed
        );
        assert_eq!(
            desk_output_measure(DeskOutputRole::Other),
            DeskOutputMeasure::Hide
        );
        // Match My Layout on a GeForce with two dummy plugs: both plugs carry
        // a stream, so nothing is detached.
        let served: BTreeSet<&str> = ["DISPLAY1", "DISPLAY2"].into_iter().collect();
        assert!(desk_outputs_to_hide(&["DISPLAY1", "DISPLAY2"], &served).is_empty());
        // One served display: only the other lit outputs are hidden.
        let single: BTreeSet<&str> = ["DISPLAY2"].into_iter().collect();
        assert_eq!(
            desk_outputs_to_hide(&["DISPLAY1", "DISPLAY2", "DISPLAY3"], &single),
            vec!["DISPLAY1", "DISPLAY3"]
        );
        // A served output with a monitor behind it stays lit: the desk is
        // not dark, which best effort reports and required refuses.
        let lit_served = [DeskOutputFact {
            lit: true,
            monitor: DeskMonitor::Present,
        }];
        assert_eq!(
            desk_outputs_dark(&lit_served),
            Err(DeskPrivacyRefusal::DeskMonitorLit)
        );
        let mut best = DeskPrivacyState::new(DeskPrivacyPlan::BestEffort);
        assert_eq!(
            best.attach_failed(DeskPrivacyRefusal::DeskMonitorLit),
            Ok(())
        );
        assert_eq!(best.status(), DeskPrivacyStatus::ScreensVisible);
        let mut required = DeskPrivacyState::new(DeskPrivacyPlan::Required);
        assert_eq!(
            required.attach_failed(DeskPrivacyRefusal::DeskMonitorLit),
            Err(DeskPrivacyRefusal::DeskMonitorLit)
        );
    }

    #[test]
    fn a_status_the_deck_was_not_sent_stays_pending_until_it_is() {
        let mut state = DeskPrivacyState::new(DeskPrivacyPlan::BestEffort);
        assert_eq!(state.hello_status(), DeskPrivacyStatus::Blanked);
        assert_eq!(state.pending(), None);
        assert_eq!(
            state.watched(Err(DeskPrivacyRefusal::DeskMonitorLit)),
            Ok(())
        );
        // The send failed (a full queue): nothing is marked, so it is
        // still pending at the next try.
        assert_eq!(state.pending(), Some(DeskPrivacyStatus::ScreensVisible));
        assert_eq!(state.pending(), Some(DeskPrivacyStatus::ScreensVisible));
        state.mark_told(DeskPrivacyStatus::ScreensVisible);
        assert_eq!(state.pending(), None);
    }

    #[test]
    fn screens_and_input_are_watched_apart_and_each_recovers_alone() {
        let mut state = DeskPrivacyState::new(DeskPrivacyPlan::BestEffort);
        let screens = DeskPrivacyRefusal::DisplaysNotDark;
        let input = DeskPrivacyRefusal::InputBlockUnavailable;
        assert_eq!(state.watched_part(DeskPart::Screens, Err(screens)), Ok(()));
        assert_eq!(state.watched_part(DeskPart::Input, Err(input)), Ok(()));
        assert_eq!(state.status(), DeskPrivacyStatus::NotPrivate);
        assert_eq!(state.watched_part(DeskPart::Input, Ok(())), Ok(()));
        assert_eq!(state.status(), DeskPrivacyStatus::ScreensVisible);
        assert_eq!(state.watched_part(DeskPart::Screens, Ok(())), Ok(()));
        assert_eq!(state.status(), DeskPrivacyStatus::Blanked);
        let mut required = DeskPrivacyState::new(DeskPrivacyPlan::Required);
        assert_eq!(
            required.watched_part(DeskPart::Input, Err(input)),
            Err(input)
        );
    }
}
