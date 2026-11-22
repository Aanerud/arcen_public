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
}
