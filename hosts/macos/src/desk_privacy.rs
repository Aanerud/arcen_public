// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The desk kept private while a Deck is attached to a signed-in user's
//! session (ADR 0025 §5, GAP-014): every display but the Pier's own virtual
//! ones dark, and the desk's input dropped. The rules are the shared
//! [`arcen_session::console_session::desk_privacy`]; this is the adapter.
//!
//! - **Displays:** `CGSetDisplayTransferByTable` with an all-zero table on
//!   every online display that is not one of the Pier's virtual displays
//!   (vendor `0x3456`, products `0x1234`/`0x1235`), proved by reading it back
//!   with `CGGetDisplayTransferByTable`, re-asserted every 100 ms (a display
//!   reconfiguration, wake, a colour daemon or Night Shift gives a display its
//!   own table back, so the desk may glimpse up to 100 ms) and given back
//!   with `CGDisplayRestoreColorSyncSettings`. A display that stays lit for
//!   two seconds, or a display list that cannot be read, ends the Deck's
//!   session (locked first) when the administrator requires desk privacy;
//!   otherwise the session goes on and the Deck is told
//!   (`desk_privacy_update`), and told again once every display is dark. `ScreenCaptureKit` reads the composited desktop
//!   before the table (prove it with `probe-desk-privacy --capture`).
//! - **Input:** an active `CGEventTap` at the HID tap point drops every
//!   keyboard, pointer, scroll, tablet, gesture and system-key event the
//!   window server did not record as posted by this process
//!   (`kCGEventSourceUnixProcessID`). It needs the Accessibility grant
//!   keyboard input already needs. Where it cannot run (no grant, or a Deck
//!   whose input goes through virtual HID devices, which it would drop too),
//!   a best-effort desk keeps its displays dark and reports its input live.
//! - **Hand-back:** the session is locked first; a panic locks it before the
//!   displays come back. When the agent dies, the window server restores the
//!   tables and removes the tap at once. The agent's lock guard then darkens
//!   the displays again and locks the session (`lock_guard`, GAP-048).
//! - **Not covered:** keystrokes into a field holding Secure Event Input (a
//!   password field) reach it without passing any event tap (logged when it
//!   happens); Siri, Voice Control, Switch Control and head-pointer input,
//!   and dictation, act through accessibility and input methods rather than
//!   HID events; a display whose picture is itself a screen capture
//!   (`DisplayLink`, Sidecar, `AirPlay`) may show the session although its
//!   table reads back dark.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arcen_session::console_session::desk_privacy::{
    self, DeckInputRoute, DeskDisplayFact, DeskPrivacyRefusal, DeskPrivacyState, DeskPrivacyStatus,
    EventPoster,
};

/// The desk's privacy as it changes while held, for the stream to tell the
/// Deck (`desk_privacy_update`).
pub type DeskPrivacyUpdates = tokio::sync::mpsc::UnboundedReceiver<DeskPrivacyStatus>;

/// The vendor id every virtual display this Pier creates carries.
pub const PIER_VIRTUAL_VENDOR: u32 = 0x3456;
/// The product ids of this Pier's SDR and HDR virtual displays.
pub const PIER_VIRTUAL_PRODUCTS: [u32; 2] = [0x1234, 0x1235];

/// How long the input filter may take to start.
const FILTER_START: Duration = Duration::from_secs(2);
/// How long a session that panics is given to show its lock screen before
/// the desk gets its displays back.
const PANIC_LOCK: Duration = Duration::from_secs(3);

/// Whether a display is one of this Pier's own virtual displays.
#[must_use]
pub fn is_pier_virtual_display(vendor: u32, product: u32) -> bool {
    vendor == PIER_VIRTUAL_VENDOR && PIER_VIRTUAL_PRODUCTS.contains(&product)
}

/// What the desk's privacy did while it was held, for the log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrivacyCounts {
    /// Displays darkened (at most once each).
    pub displays: usize,
    /// Desk input events dropped.
    pub events_dropped: u64,
    /// Times the window server disabled the filter and it was re-enabled.
    pub filter_reenabled: u64,
}

/// The desk, private. Dropping it gives the displays back, then the input
/// (locking the session first when the drop is a panic's).
pub struct DeskPrivacy {
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    /// Set before `lost` when the console left the account (rather than a
    /// display staying lit).
    console_left: Arc<AtomicBool>,
    filter: Option<native::InputFilter>,
    watcher: Option<std::thread::JoinHandle<()>>,
    blanked: Arc<std::sync::Mutex<BTreeSet<u32>>>,
    outputs_restored: bool,
    /// Changes the Deck has not been told yet, until the stream takes them.
    updates: Option<DeskPrivacyUpdates>,
}

impl std::fmt::Debug for DeskPrivacy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeskPrivacy")
            .field("outputs_restored", &self.outputs_restored)
            .field("lost", &self.lost.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DeskPrivacy {
    /// Drops the desk's input, then darkens its displays, each proved, for
    /// the session of `serving_uid`, under `state`'s plan (the hello already
    /// told its status, `DeskPrivacyState::hello_status`). The input filter
    /// runs only for a Deck whose input the Pier posts itself (`route`).
    /// What cannot be done is recorded in `state` and, unless the plan
    /// requires privacy, the rest goes on and the change is queued for the
    /// Deck ([`Self::take_updates`]). Anything that fails a required plan
    /// gives back what was taken before returning. While held, the displays
    /// are proved dark every [`DISPLAY_CHECK`](desk_privacy::DISPLAY_CHECK);
    /// a display that stays lit ends the Deck's session through
    /// [`Self::lost`] only when privacy is required (otherwise the Deck is
    /// told, and told again when it is dark), and a console that another
    /// account holds always does: it gets the desk at once, its displays and
    /// input belong to whoever is at the console now.
    ///
    /// # Errors
    ///
    /// [`DeskPrivacyRefusal`] naming what could not be made private, under a
    /// required plan.
    pub fn arm(
        serving_uid: u32,
        mut state: DeskPrivacyState,
        route: DeckInputRoute,
    ) -> Result<Self, DeskPrivacyRefusal> {
        let filter = match desk_privacy::admits_input_route(route) {
            Ok(()) => match native::InputFilter::start(FILTER_START) {
                Some(filter) => Some(filter),
                None => {
                    state.attach_failed(DeskPrivacyRefusal::InputFilterUnavailable)?;
                    None
                }
            },
            Err(refusal) => {
                state.attach_failed(refusal)?;
                None
            }
        };
        let blanked = Arc::new(std::sync::Mutex::new(BTreeSet::new()));
        let (updates_tx, updates) = tokio::sync::mpsc::unbounded_channel();
        let mut privacy = Self {
            stop: Arc::new(AtomicBool::new(false)),
            lost: Arc::new(AtomicBool::new(false)),
            console_left: Arc::new(AtomicBool::new(false)),
            filter,
            watcher: None,
            blanked: Arc::clone(&blanked),
            outputs_restored: false,
            updates: Some(updates),
        };
        // On failure `privacy` drops here and gives back the filter and any
        // display already darkened. A best-effort desk keeps trying: the
        // watch darkens every display again each check and reports it dark
        // once it is.
        if let Err(refusal) = darken_displays(&blanked) {
            state.watched(Err(refusal))?;
        }
        tell(&mut state, &updates_tx);
        let stop = Arc::clone(&privacy.stop);
        let lost = Arc::clone(&privacy.lost);
        let console_left = Arc::clone(&privacy.console_left);
        let filter_port = privacy.filter.as_ref().map(native::InputFilter::handle);
        privacy.watcher = std::thread::Builder::new()
            .name("arcen-desk-privacy".to_owned())
            .spawn(move || {
                watch(
                    serving_uid,
                    &blanked,
                    filter_port.as_ref(),
                    &stop,
                    (&lost, &console_left),
                    (state, &updates_tx),
                );
            })
            .ok();
        if privacy.watcher.is_none() {
            return Err(DeskPrivacyRefusal::DisplaysNotDark);
        }
        Ok(privacy)
    }

    /// The desk's privacy changes for the Deck: what arming could not do,
    /// then each change the watch finds. Taken once, by the stream.
    pub fn take_updates(&mut self) -> Option<DeskPrivacyUpdates> {
        self.updates.take()
    }

    /// Whether the desk could not be kept private (a display stayed lit, or
    /// the console went to another account): the Deck's session must end.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }

    /// The flag behind [`Self::is_lost`], for a task that waits on it.
    #[must_use]
    pub fn lost(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.lost)
    }

    /// Why the Deck's session must end, once [`Self::is_lost`]: the console
    /// left the account ([`WatchVerdict::ConsoleLeft`]) or a display stayed
    /// lit ([`WatchVerdict::PrivacyLost`]). The shared
    /// [`WatchVerdict::deck_refusal`] says what the Deck is told.
    ///
    /// [`WatchVerdict::ConsoleLeft`]: desk_privacy::WatchVerdict::ConsoleLeft
    /// [`WatchVerdict::PrivacyLost`]: desk_privacy::WatchVerdict::PrivacyLost
    /// [`WatchVerdict::deck_refusal`]: desk_privacy::WatchVerdict::deck_refusal
    #[must_use]
    pub fn end_verdict(&self) -> Option<desk_privacy::WatchVerdict> {
        if !self.is_lost() {
            None
        } else if self.console_left.load(Ordering::Acquire) {
            Some(desk_privacy::WatchVerdict::ConsoleLeft)
        } else {
            Some(desk_privacy::WatchVerdict::PrivacyLost)
        }
    }

    /// Gives the displays their own picture back (the shared hand-back
    /// order: after the lock, before the sound and the input).
    pub fn restore_outputs(&mut self) {
        if self.outputs_restored {
            return;
        }
        self.outputs_restored = true;
        self.stop.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        native::restore_all_displays();
        let blanked = self
            .blanked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let still_dark: Vec<u32> = blanked
            .into_iter()
            .filter(|display| native::display_is_dark(*display) == Some(true))
            .collect();
        if still_dark.is_empty() {
            tracing::info!(
                target: arcen_telemetry::names::target::SESSION,
                "the desk's displays have their picture back"
            );
        } else {
            tracing::error!(
                target: arcen_telemetry::names::target::SESSION,
                displays = ?still_dark,
                "a desk display stayed dark after its colour settings were restored"
            );
        }
    }

    /// Gives the desk its input back (the last hand-back step), and returns
    /// what privacy did.
    pub fn release_input(mut self) -> PrivacyCounts {
        self.restore_outputs();
        let displays = self
            .blanked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let (events_dropped, filter_reenabled) =
            self.filter.take().map_or((0, 0), native::InputFilter::stop);
        PrivacyCounts {
            displays,
            events_dropped,
            filter_reenabled,
        }
    }
}

impl Drop for DeskPrivacy {
    fn drop(&mut self) {
        // A panic unwinding through the session skipped its hand-back: lock
        // first, so that the desk never gets the Deck's desktop unlocked.
        if std::thread::panicking() && !self.outputs_restored {
            if let Err(error) = crate::screen_lock::lock_and_confirm(PANIC_LOCK) {
                tracing::error!(
                    target: arcen_telemetry::names::target::SESSION,
                    %error,
                    "the session could not be locked before the desk got it back"
                );
            }
        }
        self.restore_outputs();
        if let Some(filter) = self.filter.take() {
            let _ = filter.stop();
        }
    }
}

/// The watcher thread: proves the displays dark every
/// [`DISPLAY_CHECK`](desk_privacy::DISPLAY_CHECK), keeps the filter enabled,
/// and reads the console every
/// [`CONSOLE_CHECK`](desk_privacy::CONSOLE_CHECK), acting on the shared
/// [`PrivacyWatch`](desk_privacy::PrivacyWatch) verdicts.
fn watch(
    serving_uid: u32,
    blanked: &std::sync::Mutex<BTreeSet<u32>>,
    filter: Option<&native::FilterHandle>,
    stop: &AtomicBool,
    (lost, console_left): (&AtomicBool, &AtomicBool),
    (mut state, updates): (
        DeskPrivacyState,
        &tokio::sync::mpsc::UnboundedSender<DeskPrivacyStatus>,
    ),
) {
    use desk_privacy::{CONSOLE_CHECK, DISPLAY_CHECK, PrivacyWatch, WatchVerdict};
    let mut verdicts = PrivacyWatch::default();
    let mut secure_input_seen = false;
    let mut console_read = std::time::Instant::now();
    let mut not_dark_logged = false;
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(DISPLAY_CHECK);
        if stop.load(Ordering::Acquire) {
            break;
        }
        let now = std::time::Instant::now();
        if now.duration_since(console_read) >= CONSOLE_CHECK {
            console_read = now;
            let ours = crate::console::console_holder()
                == arcen_session::agent_relay::ConsoleHolder::User(serving_uid);
            if verdicts.console(ours) == WatchVerdict::ConsoleLeft {
                if let Some(handle) = filter {
                    handle.suspend();
                }
                native::restore_all_displays();
                console_left.store(true, Ordering::Release);
                lost.store(true, Ordering::Release);
                tracing::warn!(
                    target: arcen_telemetry::names::target::SESSION,
                    "the console left this account; the desk has its displays and input back and the Deck's session ends"
                );
                break;
            }
            let secure_input = native::secure_event_input();
            if secure_input && !secure_input_seen {
                tracing::warn!(
                    target: arcen_telemetry::names::target::SESSION,
                    "an application holds Secure Event Input: the desk's keystrokes reach it unfiltered"
                );
            }
            secure_input_seen = secure_input;
        }
        let dark = darken_displays(blanked);
        if let Err(refusal) = dark {
            if !not_dark_logged {
                not_dark_logged = true;
                tracing::warn!(
                    target: arcen_telemetry::names::target::SESSION,
                    reason = refusal.token(),
                    "a desk display could not be kept dark; retrying"
                );
            }
        } else {
            not_dark_logged = false;
        }
        // A display that stays lit past the limit ends a required desk's
        // session; a best-effort desk reports it, and its recovery.
        let verdict = match verdicts.displays(dark.is_ok(), now) {
            WatchVerdict::PrivacyLost => Some(Err(DeskPrivacyRefusal::DisplaysNotDark)),
            _ if dark.is_ok() => Some(Ok(())),
            // Lit, within the limit: nothing decided yet.
            _ => None,
        };
        if verdict.is_some_and(|verdict| state.watched(verdict).is_err())
            && !lost.swap(true, Ordering::AcqRel)
        {
            tracing::error!(
                target: arcen_telemetry::names::target::SESSION,
                "a desk display stayed lit and the administrator requires desk privacy; the Deck's session ends"
            );
        }
        tell(&mut state, updates);
        if let Some(handle) = filter {
            handle.keep_enabled();
        }
    }
}

/// Queues the desk's status for the Deck when it changed since it was last
/// told, logging the change once.
fn tell(
    state: &mut DeskPrivacyState,
    updates: &tokio::sync::mpsc::UnboundedSender<DeskPrivacyStatus>,
) {
    let Some(status) = state.tell() else {
        return;
    };
    if status.is_warning() {
        tracing::warn!(
            target: arcen_telemetry::names::target::SESSION,
            status = status.token(),
            "desk privacy: the desk is not fully private; the session continues and the Deck is told (privacy.desk = \"required\" refuses instead)"
        );
    } else {
        tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            status = status.token(),
            "desk privacy: the Deck is told the desk's state"
        );
    }
    let _ = updates.send(status);
}

/// Darkens every display that goes dark while attached and is not yet,
/// recording each, and proves every one of them dark. Every display is
/// tried before a failure is reported; a display list that cannot be read
/// is a failure.
fn darken_displays(blanked: &std::sync::Mutex<BTreeSet<u32>>) -> Result<(), DeskPrivacyRefusal> {
    let displays = native::online_displays().ok_or(DeskPrivacyRefusal::DisplaysNotDark)?;
    let mut darkened = Vec::new();
    let mut all_dark = true;
    for display in displays {
        let fact = DeskDisplayFact {
            pier_virtual: is_pier_virtual_display(display.vendor, display.product),
        };
        if !desk_privacy::blanks_while_attached(fact) {
            continue;
        }
        if native::display_is_dark(display.id) != Some(true) {
            native::darken(display.id);
        }
        if native::display_is_dark(display.id) != Some(true) {
            all_dark = false;
            continue;
        }
        if blanked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(display.id)
        {
            darkened.push(display);
        }
    }
    if !darkened.is_empty() {
        tracing::info!(
            target: arcen_telemetry::names::target::SESSION,
            displays = ?darkened,
            "desk displays darkened while the Deck is attached"
        );
    }
    if all_dark {
        Ok(())
    } else {
        Err(DeskPrivacyRefusal::DisplaysNotDark)
    }
}

/// The desk's displays darkened by a process that outlives the agent that
/// kept them private (the lock guard, or the agent launchd starts next),
/// until the session it served is locked (`lock_guard`). Displays only: the
/// input filter needs an Accessibility grant that is attributed to the
/// agent, and the lock it waits for takes the desk's input.
#[derive(Debug, Default)]
pub struct DarkDesk {
    blanked: std::sync::Mutex<BTreeSet<u32>>,
}

impl DarkDesk {
    /// Darkens every desk display not yet dark. Returns whether all are.
    pub fn darken(&self) -> bool {
        darken_displays(&self.blanked).is_ok()
    }

    /// Gives every darkened display its own picture back.
    pub fn give_back(&mut self) {
        let blanked = self
            .blanked
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !blanked.is_empty() {
            blanked.clear();
            native::restore_all_displays();
        }
    }
}

/// Whether an input event posted by `poster_pid` reaches a private desktop.
#[must_use]
pub fn event_passes(poster_pid: i64) -> bool {
    desk_privacy::event_passes_while_attached(EventPoster::from_pid(
        poster_pid,
        i64::from(std::process::id()),
    ))
}

/// One online display, as the probe and the darkening read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct OnlineDisplay {
    pub id: u32,
    pub vendor: u32,
    pub product: u32,
}

/// What `probe-desk-privacy` found.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PrivacyProbe {
    /// Every online display, whether it would go dark, and whether its
    /// table reads dark now.
    pub displays: Vec<ProbedDisplay>,
    /// Whether an active input filter could be installed.
    pub filter_available: bool,
    /// Events the listening filter saw, by who posted them.
    pub events_hid_system: u64,
    pub events_this_process: u64,
    pub events_other_process: u64,
    /// The one synthetic pointer move the probe posted was seen as this
    /// process's.
    pub own_post_recognised: bool,
    /// With `--capture`: mean luma of a fresh capture of the main display
    /// before and while it was dark (equal when capture reads before the
    /// table).
    pub capture_mean_before: Option<f64>,
    pub capture_mean_dark: Option<f64>,
    /// With `--blank`: every display went dark and came back.
    pub darkened_and_restored: Option<bool>,
}

/// One display in a [`PrivacyProbe`].
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ProbedDisplay {
    pub display: OnlineDisplay,
    pub darkens: bool,
    pub dark_now: Option<bool>,
}

/// Lists the displays, listens to input for `listen` and posts one pointer
/// move to the pointer's own position to see whether this process's events
/// are told apart, and, with `blank`, darkens every desk display for that
/// long (with `capture`, capturing the main display before and while dark).
#[must_use]
pub fn probe(listen: Duration, blank: Option<Duration>, capture: bool) -> PrivacyProbe {
    let mut report = PrivacyProbe {
        displays: native::online_displays()
            .unwrap_or_default()
            .into_iter()
            .map(|display| ProbedDisplay {
                display,
                darkens: desk_privacy::blanks_while_attached(DeskDisplayFact {
                    pier_virtual: is_pier_virtual_display(display.vendor, display.product),
                }),
                dark_now: native::display_is_dark(display.id),
            })
            .collect(),
        ..PrivacyProbe::default()
    };
    if let Some(seen) = native::listen(listen) {
        report.filter_available = true;
        report.events_hid_system = seen.hid_system;
        report.events_this_process = seen.this_process;
        report.events_other_process = seen.other_process;
        report.own_post_recognised = seen.own_post_recognised;
    }
    if let Some(blank) = blank {
        if capture {
            report.capture_mean_before = native::capture_main_display_mean();
        }
        let blanked = std::sync::Mutex::new(BTreeSet::new());
        let darkened = darken_displays(&blanked).is_ok();
        if capture {
            report.capture_mean_dark = native::capture_main_display_mean();
        }
        std::thread::sleep(blank);
        native::restore_all_displays();
        let restored = blanked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .all(|display| native::display_is_dark(*display) != Some(true));
        report.darkened_and_restored = Some(darkened && restored);
    }
    report
}

#[cfg(target_os = "macos")]
mod native {
    // A handful of C entry points, each with its SAFETY note below.
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
    use std::time::Duration;

    use super::OnlineDisplay;

    type CGEventRef = *mut c_void;
    type CFMachPortRef = *mut c_void;
    type CFRunLoopSourceRef = *mut c_void;
    type CFRunLoopRef = *mut c_void;
    type CFStringRef = *const c_void;
    type TapCallback =
        unsafe extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

    /// `kCGHIDEventTap`: where HID input enters the window server.
    const HID_EVENT_TAP: u32 = 0;
    const HEAD_INSERT: u32 = 0;
    /// `kCGEventTapOptionDefault`: an active filter.
    const ACTIVE_FILTER: u32 = 0;
    const LISTEN_ONLY: u32 = 1;
    /// `kCGEventSourceUnixProcessID`.
    const SOURCE_UNIX_PROCESS_ID: u32 = 41;
    /// `kCGEventTapDisabledByTimeout`, `kCGEventTapDisabledByUserInput`.
    const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
    /// Every event type a person drives the desktop with: mouse buttons,
    /// moves and drags (1-7), keys and modifiers (10-12), system keys
    /// (brightness, sound, media, power: 14), the trackpad's gestures (18-20,
    /// 29-32, 34, 37-38), scrolling (22), tablets (23-24) and the other
    /// buttons (25-27).
    const DESK_EVENT_TYPES: [u32; 27] = [
        1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14, 18, 19, 20, 22, 23, 24, 25, 26, 27, 29, 30, 31, 32,
        34, 37, 38,
    ];

    pub(super) fn desk_event_mask() -> u64 {
        DESK_EVENT_TYPES
            .iter()
            .fold(0_u64, |mask, kind| mask | 1_u64 << kind)
    }

    // SAFETY: signatures match `CGEvent.h`, `CGDirectDisplay.h`,
    // `CGDisplayConfiguration.h`, `CFMachPort.h`, `CFRunLoop.h` and
    // `HIToolbox/CarbonEventsCore.h`; every reference is an opaque pointer.
    // `CGEventTapCreate`, `CGEventTapEnable`, `CGEventGetIntegerValueField`
    // and the run-loop calls are declared with the same signatures as in
    // `virtual_hid.rs`.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            tap: u32,
            place: u32,
            options: u32,
            events_of_interest: u64,
            callback: TapCallback,
            user_info: *mut c_void,
        ) -> CFMachPortRef;
        fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
        fn CGEventTapIsEnabled(tap: CFMachPortRef) -> bool;
        fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
        fn CGGetOnlineDisplayList(
            max_displays: u32,
            online_displays: *mut u32,
            display_count: *mut u32,
        ) -> i32;
        fn CGDisplayVendorNumber(display: u32) -> u32;
        fn CGDisplayModelNumber(display: u32) -> u32;
        fn CGDisplayGammaTableCapacity(display: u32) -> u32;
        fn CGSetDisplayTransferByTable(
            display: u32,
            table_size: u32,
            red: *const f32,
            green: *const f32,
            blue: *const f32,
        ) -> i32;
        fn CGGetDisplayTransferByTable(
            display: u32,
            capacity: u32,
            red: *mut f32,
            green: *mut f32,
            blue: *mut f32,
            sample_count: *mut u32,
        ) -> i32;
        fn CGDisplayRestoreColorSyncSettings();
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: CFStringRef;
        fn CFMachPortCreateRunLoopSource(
            allocator: *const c_void,
            port: CFMachPortRef,
            order: isize,
        ) -> CFRunLoopSourceRef;
        fn CFMachPortInvalidate(port: CFMachPortRef);
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopAddSource(
            run_loop: CFRunLoopRef,
            source: CFRunLoopSourceRef,
            mode: CFStringRef,
        );
        fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
    }

    /// Whether an application holds Secure Event Input.
    pub(super) fn secure_event_input() -> bool {
        crate::input::secure_event_input()
    }

    /// The online displays; `None` when the window server will not list
    /// them.
    pub(super) fn online_displays() -> Option<Vec<OnlineDisplay>> {
        let mut ids = [0_u32; 32];
        let mut count = 0_u32;
        // SAFETY: `ids` holds 32 entries and `count` is a live out-parameter.
        let error = unsafe { CGGetOnlineDisplayList(32, ids.as_mut_ptr(), &raw mut count) };
        if error != 0 {
            return None;
        }
        let displays = ids
            .iter()
            .take(usize::try_from(count).unwrap_or(0).min(ids.len()))
            .map(|id| OnlineDisplay {
                id: *id,
                // SAFETY: plain reads of a display id the system listed.
                vendor: unsafe { CGDisplayVendorNumber(*id) },
                // SAFETY: as above.
                product: unsafe { CGDisplayModelNumber(*id) },
            })
            .collect();
        Some(displays)
    }

    /// Whether the display's transfer table reads back dark; `None` when it
    /// cannot be read.
    pub(super) fn display_is_dark(display: u32) -> Option<bool> {
        // SAFETY: a plain read of a display id.
        let capacity = unsafe { CGDisplayGammaTableCapacity(display) }.clamp(2, 4096);
        let size = usize::try_from(capacity).ok()?;
        let (mut red, mut green, mut blue) =
            (vec![0_f32; size], vec![0_f32; size], vec![0_f32; size]);
        let mut samples = 0_u32;
        // SAFETY: three buffers of `capacity` entries and a live count.
        let error = unsafe {
            CGGetDisplayTransferByTable(
                display,
                capacity,
                red.as_mut_ptr(),
                green.as_mut_ptr(),
                blue.as_mut_ptr(),
                &raw mut samples,
            )
        };
        if error != 0 {
            return None;
        }
        let samples = usize::try_from(samples).ok()?.min(size);
        Some([&red, &green, &blue].iter().all(|table| {
            arcen_session::console_session::desk_privacy::transfer_table_is_dark(&table[..samples])
        }))
    }

    pub(super) fn darken(display: u32) {
        let zeros = [0.0_f32; 256];
        // SAFETY: three tables of 256 entries, read during the call.
        let _ = unsafe {
            CGSetDisplayTransferByTable(
                display,
                256,
                zeros.as_ptr(),
                zeros.as_ptr(),
                zeros.as_ptr(),
            )
        };
    }

    pub(super) fn restore_all_displays() {
        // SAFETY: no arguments; restores every display's ColorSync table.
        unsafe { CGDisplayRestoreColorSyncSettings() };
    }

    /// What the filter's callback reads.
    struct FilterState {
        pid: i64,
        /// Set once the desk is given back while the filter still runs.
        suspended: AtomicBool,
        port: AtomicPtr<c_void>,
        dropped: AtomicU64,
        reenabled: AtomicU64,
    }

    unsafe extern "C" fn filter(
        _proxy: *mut c_void,
        event_type: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef {
        // SAFETY: `user_info` is the `FilterState` the filter thread owns,
        // which outlives its run loop.
        let state = unsafe { &*user_info.cast::<FilterState>() };
        if event_type == TAP_DISABLED_BY_TIMEOUT || event_type == TAP_DISABLED_BY_USER_INPUT {
            let port = state.port.load(Ordering::Acquire);
            if !port.is_null() {
                // SAFETY: the port is live while the run loop runs.
                unsafe { CGEventTapEnable(port, true) };
                state.reenabled.fetch_add(1, Ordering::Relaxed);
            }
            return event;
        }
        if state.suspended.load(Ordering::Acquire) {
            return event;
        }
        // SAFETY: `event` is live for the callback; a field read.
        let poster = unsafe { CGEventGetIntegerValueField(event, SOURCE_UNIX_PROCESS_ID) };
        if arcen_session::console_session::desk_privacy::event_passes_while_attached(
            arcen_session::console_session::desk_privacy::EventPoster::from_pid(poster, state.pid),
        ) {
            event
        } else {
            state.dropped.fetch_add(1, Ordering::Relaxed);
            std::ptr::null_mut()
        }
    }

    /// The running filter: its thread and what it counted.
    pub(super) struct InputFilter {
        stop: Arc<AtomicBool>,
        state: Arc<FilterState>,
        thread: std::thread::JoinHandle<()>,
    }

    /// Re-enables the filter from another thread.
    #[derive(Clone)]
    pub(super) struct FilterHandle(Arc<FilterState>);

    impl FilterHandle {
        /// Lets every event through from now on.
        pub(super) fn suspend(&self) {
            self.0.suspended.store(true, Ordering::Release);
        }

        pub(super) fn keep_enabled(&self) {
            let port = self.0.port.load(Ordering::Acquire);
            // SAFETY: the port is live until the filter thread clears it.
            if !port.is_null() && !unsafe { CGEventTapIsEnabled(port) } {
                // SAFETY: as above.
                unsafe { CGEventTapEnable(port, true) };
                self.0.reenabled.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    impl InputFilter {
        /// Installs the filter on its own thread; `None` when the window
        /// server refuses it (no Accessibility) or it did not start in time.
        pub(super) fn start(timeout: Duration) -> Option<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let state = Arc::new(FilterState {
                pid: i64::from(std::process::id()),
                suspended: AtomicBool::new(false),
                port: AtomicPtr::new(std::ptr::null_mut()),
                dropped: AtomicU64::new(0),
                reenabled: AtomicU64::new(0),
            });
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let thread = {
                let stop = Arc::clone(&stop);
                let state = Arc::clone(&state);
                std::thread::Builder::new()
                    .name("arcen-desk-input".to_owned())
                    .spawn(move || run_filter(&state, &stop, &ready_tx))
                    .ok()?
            };
            let started = ready_rx.recv_timeout(timeout).unwrap_or(false);
            let filter = Self {
                stop,
                state,
                thread,
            };
            if started {
                Some(filter)
            } else {
                let _ = filter.stop();
                None
            }
        }

        pub(super) fn handle(&self) -> FilterHandle {
            FilterHandle(Arc::clone(&self.state))
        }

        /// Removes the filter; returns the events it dropped and how often
        /// it was re-enabled.
        pub(super) fn stop(self) -> (u64, u64) {
            self.stop.store(true, Ordering::Release);
            let _ = self.thread.join();
            (
                self.state.dropped.load(Ordering::Relaxed),
                self.state.reenabled.load(Ordering::Relaxed),
            )
        }
    }

    fn run_filter(
        state: &Arc<FilterState>,
        stop: &AtomicBool,
        ready: &std::sync::mpsc::Sender<bool>,
    ) {
        // SAFETY: an active filter whose callback reads `state`, which this
        // function keeps alive until the port is invalidated below.
        let port = unsafe {
            CGEventTapCreate(
                HID_EVENT_TAP,
                HEAD_INSERT,
                ACTIVE_FILTER,
                desk_event_mask(),
                filter,
                Arc::as_ptr(state).cast_mut().cast(),
            )
        };
        if port.is_null() {
            let _ = ready.send(false);
            return;
        }
        state.port.store(port, Ordering::Release);
        // SAFETY: `port` is a live tap; the source and the run loop are this
        // thread's, and both references are released before returning.
        unsafe {
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
            let _ = ready.send(CGEventTapIsEnabled(port));
            while !stop.load(Ordering::Acquire) {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, 0);
            }
            CGEventTapEnable(port, false);
            state.port.store(std::ptr::null_mut(), Ordering::Release);
            CFMachPortInvalidate(port);
            crate::input::release_core_foundation(source.cast_const());
            crate::input::release_core_foundation(port.cast_const());
        }
    }

    /// Who posted the events a listening filter saw.
    #[derive(Default)]
    pub(super) struct Seen {
        pub hid_system: u64,
        pub this_process: u64,
        pub other_process: u64,
        pub own_post_recognised: bool,
    }

    struct ListenState {
        pid: i64,
        hid_system: AtomicU64,
        this_process: AtomicU64,
        other_process: AtomicU64,
    }

    unsafe extern "C" fn count(
        _proxy: *mut c_void,
        _event_type: u32,
        event: CGEventRef,
        user_info: *mut c_void,
    ) -> CGEventRef {
        // SAFETY: `user_info` is the `ListenState` `listen` owns for the
        // whole run loop.
        let state = unsafe { &*user_info.cast::<ListenState>() };
        // SAFETY: a field read of a live event.
        let poster = unsafe { CGEventGetIntegerValueField(event, SOURCE_UNIX_PROCESS_ID) };
        let counter = match arcen_session::console_session::desk_privacy::EventPoster::from_pid(
            poster, state.pid,
        ) {
            arcen_session::console_session::desk_privacy::EventPoster::HidSystem => {
                &state.hid_system
            }
            arcen_session::console_session::desk_privacy::EventPoster::ThisPier => {
                &state.this_process
            }
            arcen_session::console_session::desk_privacy::EventPoster::OtherProcess => {
                &state.other_process
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
        event
    }

    /// Listens (never filters) for `duration`, after posting one pointer
    /// move to where the pointer already is.
    pub(super) fn listen(duration: Duration) -> Option<Seen> {
        let state = ListenState {
            pid: i64::from(std::process::id()),
            hid_system: AtomicU64::new(0),
            this_process: AtomicU64::new(0),
            other_process: AtomicU64::new(0),
        };
        // SAFETY: a listen-only tap whose callback reads `state`, alive
        // until the port is invalidated below.
        let port = unsafe {
            CGEventTapCreate(
                HID_EVENT_TAP,
                HEAD_INSERT,
                LISTEN_ONLY,
                desk_event_mask(),
                count,
                std::ptr::from_ref(&state).cast_mut().cast(),
            )
        };
        if port.is_null() {
            return None;
        }
        // SAFETY: as in `run_filter`.
        unsafe {
            let source = CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
            let _ = crate::input::post_pointer_nudge();
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, duration.as_secs_f64(), 0);
            CGEventTapEnable(port, false);
            CFMachPortInvalidate(port);
            crate::input::release_core_foundation(source.cast_const());
            crate::input::release_core_foundation(port.cast_const());
        }
        let this_process = state.this_process.load(Ordering::Relaxed);
        Some(Seen {
            hid_system: state.hid_system.load(Ordering::Relaxed),
            this_process,
            other_process: state.other_process.load(Ordering::Relaxed),
            own_post_recognised: this_process > 0,
        })
    }

    /// The mean luma of one fresh capture of the main display.
    pub(super) fn capture_main_display_mean() -> Option<f64> {
        // The main display is the one at the global origin.
        let displays = crate::displays::probe().ok()?;
        let main = displays
            .iter()
            .find(|display| display.origin_x == 0.0 && display.origin_y == 0.0)
            .or_else(|| displays.first())?;
        let config = crate::capture::CaptureConfig::sdr(
            main.display_id,
            main.pixel_width,
            main.pixel_height,
            30,
        );
        let session = crate::capture::CaptureSession::start(config).ok()?;
        let frame = session.next_frame(Duration::from_secs(3)).ok();
        let mean = frame.and_then(|frame| crate::capture::mean_luma(&frame));
        session.stop();
        mean
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use std::time::Duration;

    use super::OnlineDisplay;

    pub(super) struct InputFilter;

    #[derive(Clone)]
    pub(super) struct FilterHandle;

    impl FilterHandle {
        pub(super) fn suspend(&self) {}

        pub(super) fn keep_enabled(&self) {}
    }

    impl InputFilter {
        pub(super) fn start(_timeout: Duration) -> Option<Self> {
            None
        }

        pub(super) fn handle(&self) -> FilterHandle {
            FilterHandle
        }

        pub(super) fn stop(self) -> (u64, u64) {
            (0, 0)
        }
    }

    #[derive(Default)]
    pub(super) struct Seen {
        pub hid_system: u64,
        pub this_process: u64,
        pub other_process: u64,
        pub own_post_recognised: bool,
    }

    pub(super) fn secure_event_input() -> bool {
        false
    }

    pub(super) fn online_displays() -> Option<Vec<OnlineDisplay>> {
        None
    }

    pub(super) fn display_is_dark(_display: u32) -> Option<bool> {
        None
    }

    pub(super) fn darken(_display: u32) {}

    pub(super) fn restore_all_displays() {}

    pub(super) fn listen(_duration: Duration) -> Option<Seen> {
        None
    }

    pub(super) fn capture_main_display_mean() -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pier_s_own_virtual_displays_stay_lit() {
        assert!(is_pier_virtual_display(0x3456, 0x1234));
        assert!(is_pier_virtual_display(0x3456, 0x1235));
        // Another product's virtual display, a desk panel, a projector.
        assert!(!is_pier_virtual_display(0x3456, 0x9999));
        assert!(!is_pier_virtual_display(0x0610, 0x1234));
    }

    #[test]
    fn this_process_s_events_pass_and_the_desk_s_do_not() {
        assert!(event_passes(i64::from(std::process::id())));
        assert!(!event_passes(0));
        assert!(!event_passes(i64::from(std::process::id()) + 1));
    }

    #[test]
    fn the_filter_watches_every_kind_of_desk_input() {
        // Keys, modifiers, pointer, scroll, tablet, gestures, system keys.
        #[cfg(target_os = "macos")]
        {
            let mask = native::desk_event_mask();
            for kind in [1_u32, 5, 10, 11, 12, 14, 22, 23, 24, 29, 30, 31] {
                assert_ne!(mask & 1 << kind, 0, "event type {kind}");
            }
            // Never the window server's own tap-disabled notices or
            // application events.
            for kind in [13_u32, 15, 16] {
                assert_eq!(mask & 1 << kind, 0, "event type {kind}");
            }
        }
    }
}
