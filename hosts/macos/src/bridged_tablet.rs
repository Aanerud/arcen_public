// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Native tablet on a macOS Pier (ADR 0022): the session side.
//!
//! The Deck bridges its tablet as USB; this host has no virtual USB
//! controller it may use, but it may publish HID devices. The session runs
//! the shared USB host driver ([`arcen_usb_bridge::HidHostAdapter`]) over the
//! unchanged URB bridge, and the existing entitled `hid-injector` child
//! (`input::bridged_hid`) owns one `IOHIDUserDevice` per HID interface.
//!
//! This module decides whether Native tablet is offered (and says why not),
//! authorizes the Deck's device with the same shared checks as the Linux Pier
//! before anything exists, and runs one attachment: wire frames in and out,
//! the child's frames in and out, deadlines.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

/// Why a build without the adapter refuses.
pub const NOT_BUILT: &str = "this Pier build does not contain the Native tablet adapter";
/// Why the login window refuses.
pub const LOGIN_WINDOW: &str = "Native tablet is not offered at the login window";
/// Why a host with no injector program refuses.
pub const NO_PROGRAM: &str = "the Pier executable that owns virtual HID devices is missing";
/// Why a host still probing refuses.
pub const PROBING: &str = "still checking whether this Mac can create virtual HID devices";
/// Told to a Deck whose tablet was accepted: this Pier reads the pen itself
/// (ADR 0022 §14), the one Native tablet way on a macOS Pier.
pub const HOST_PEN_NOTICE: &str = "Native tablet is active. This Mac's Pier reads the pen \
     itself and posts tablet events with pressure, tilt, barrel buttons and eraser; the \
     tablet maker's driver settings, ExpressKeys and touch are not used.";
/// Told to a Deck whose tablet has no dedicated reader: this Pier reads the
/// pen by its own HID report descriptor (Q-16: any tablet works by default).
pub const GENERIC_PEN_NOTICE: &str = "Native tablet is active. This Mac's Pier reads the pen \
     from its standard HID reports and posts tablet events with what the tablet reports \
     (pressure, tilt, buttons, eraser); the maker's driver settings, ExpressKeys and touch \
     are not used.";
/// How long after a refused probe the next session may probe again. Short:
/// the probe is one short-lived child, and a grant given since must be seen
/// by the very next connection.
const REPROBE_AFTER: Duration = Duration::from_secs(5);
/// How long a hello waits for a probe that is still running, so that the
/// first connection after the agent starts (or after a grant) is offered
/// Native tablet rather than told "still checking".
pub const PROBE_WAIT: Duration = Duration::from_secs(3);
/// How long the probe child may take before it counts as a refusal.
const PROBE_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    Unknown,
    Running,
    Available,
    Refused(String, Instant),
}

static PROGRAM: OnceLock<PathBuf> = OnceLock::new();
static PROBE: Mutex<Probe> = Mutex::new(Probe::Unknown);

/// Names the executable whose `hid-injector` owns the devices: the installed
/// Pier for an agent, the running binary for `serve`. Starts the first
/// capability probe in the background.
pub fn set_injector_program(program: PathBuf) {
    if PROGRAM.set(program).is_ok() {
        refresh_probe();
    }
}

fn program() -> Option<&'static Path> {
    PROGRAM
        .get()
        .map(PathBuf::as_path)
        .filter(|path| path.is_file())
}

/// Re-probes in the background when the last answer was a refusal old enough
/// to be worth asking again (a grant given since is then seen without a
/// restart). Cheap; call it before each hello.
pub fn refresh_probe() {
    if !cfg!(feature = "native-tablet") {
        return;
    }
    let Some(program) = program().map(Path::to_path_buf) else {
        return;
    };
    {
        let mut state = PROBE.lock().unwrap_or_else(PoisonError::into_inner);
        let due = match &*state {
            Probe::Unknown => true,
            Probe::Refused(_, when) => when.elapsed() >= REPROBE_AFTER,
            Probe::Running | Probe::Available => false,
        };
        if !due {
            return;
        }
        *state = Probe::Running;
    }
    let spawned = std::thread::Builder::new()
        .name("arcen-native-tablet-probe".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let outcome = probe_with(&program);
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                available = outcome.is_ok(),
                refusal = outcome.as_ref().err().map(String::as_str),
                elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "Native tablet: virtual HID capability probe settled"
            );
            *PROBE.lock().unwrap_or_else(PoisonError::into_inner) = match outcome {
                Ok(()) => Probe::Available,
                Err(reason) => Probe::Refused(reason, Instant::now()),
            };
        });
    if spawned.is_err() {
        *PROBE.lock().unwrap_or_else(PoisonError::into_inner) = Probe::Refused(
            "could not start the virtual HID probe".to_owned(),
            Instant::now(),
        );
    }
}

/// Runs `program probe-virtual-hid`: the same executable, signature and
/// responsible app as the child that will own the devices. A probe that does
/// not answer within [`PROBE_DEADLINE`] is killed and counts as a refusal.
fn probe_with(program: &Path) -> Result<(), String> {
    use std::io::Read;
    let mut child = std::process::Command::new(program)
        .arg("probe-virtual-hid")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run the virtual HID probe: {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < PROBE_DEADLINE => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the virtual HID probe did not answer".to_owned());
            }
        }
    };
    let mut text = String::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout.take(4096).read_to_string(&mut text);
    }
    if status.success() && text.contains("virtual_hid=available") {
        return Ok(());
    }
    Err(text
        .lines()
        .find(|line| !line.starts_with("virtual_hid="))
        .map_or_else(
            || "this Mac refused to create a virtual HID device".to_owned(),
            str::to_owned,
        ))
}

/// Whether this session may offer Native tablet, and why not.
///
/// # Errors
///
/// The reason, in words a Deck shows its user.
pub fn availability(login_window: bool) -> Result<(), String> {
    availability_from(
        cfg!(feature = "native-tablet"),
        login_window,
        program().is_some(),
        &PROBE.lock().unwrap_or_else(PoisonError::into_inner),
    )
}

/// [`availability`] as the hello needs it: a refusal old enough is probed
/// again, and a probe still running is waited for up to `wait`. This is what
/// makes Native tablet work on the first connection (ADR 0022 §8): nothing
/// about the session has to be restarted, signed out of or reconnected for
/// the devices to appear.
///
/// # Errors
///
/// The reason, in words a Deck shows its user.
pub async fn settled_availability(login_window: bool, wait: Duration) -> Result<(), String> {
    if login_window {
        return availability(true);
    }
    refresh_probe();
    settle(|| availability(false), wait).await
}

/// Polls `current` until it is no longer [`PROBING`] or `wait` has passed.
async fn settle(
    mut current: impl FnMut() -> Result<(), String>,
    wait: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let offer = current();
        let probing = offer.as_ref().err().is_some_and(|reason| reason == PROBING);
        if !probing || tokio::time::Instant::now() >= deadline {
            return offer;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn availability_from(
    built: bool,
    login_window: bool,
    program: bool,
    probe: &Probe,
) -> Result<(), String> {
    if !built {
        return Err(NOT_BUILT.to_owned());
    }
    if login_window {
        return Err(LOGIN_WINDOW.to_owned());
    }
    if !program {
        return Err(NO_PROGRAM.to_owned());
    }
    match probe {
        Probe::Available => Ok(()),
        Probe::Unknown | Probe::Running => Err(PROBING.to_owned()),
        Probe::Refused(reason, _) => Err(reason.clone()),
    }
}

/// A device this host agreed to present (shared, `arcen_input::bridged_pen`).
pub use arcen_input::bridged_pen::AuthorizedTablet;
/// The tablet event, the shared pen sample and the host pointer of a pen this
/// host reads itself, and the panic guard: shared with every Pier that reads
/// the pen itself (`arcen_input::bridged_pen`).
pub use arcen_input::bridged_pen::{HostPenPointer, contained, host_pen_event, host_pen_sample};

/// What a macOS Pier publishes of a bridged tablet. An `IOHIDUserDevice` is
/// system-wide — there is no seat to confine it to, as the Linux Pier does
/// (ADR 0021) — so a keyboard, keypad or mouse interface would type or
/// click into whatever holds the console, including another user's session
/// or the login window, beyond secure input. Only pen digitizer and the
/// tablet's own vendor interfaces are published (ADR 0022 §5).
pub const PUBLISH_POLICY: arcen_usb_bridge::HidPublishPolicy =
    arcen_usb_bridge::HidPublishPolicy::PenAndVendorOnly;

/// Decides whether the Deck's captured device may be presented here, with
/// the same vendor-neutral checks as the Linux Pier
/// (`arcen_usb_bridge::authorize_hard_usb_tablet`). Vendor and product ids
/// are logged, never trusted. The synthetic lab tablet needs
/// `ARCEN_USB_HARD_SYNTHETIC=1`.
///
/// # Errors
///
/// A bounded reason.
pub fn authorize(
    device: Option<&arcen_protocol::messages::UsbHardDeviceMsg>,
) -> Result<AuthorizedTablet, String> {
    let synthetic = std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() == Some("1");
    let tablet = arcen_input::bridged_pen::authorize(device, PUBLISH_POLICY, synthetic)?;
    // A generic pen is read even when its interface is withheld (a pen and a
    // keyboard in one descriptor): nothing of it is published.
    if tablet.published.is_empty() && tablet.host_pen.generic_interface().is_none() {
        return Err(
            "the captured tablet has no pen or vendor interface this Mac may publish \
             (keyboard, keypad and mouse interfaces are never published on macOS)"
                .to_owned(),
        );
    }
    Ok(tablet)
}

/// The tablet-mode result for a Native tablet request, given what this host
/// can do and what the Deck brought.
///
/// Accepted results keep the shared negotiation's answer and add the
/// vendor-driver notice when no driver is installed; refusals carry the
/// precise reason rather than the generic one.
#[must_use]
pub fn tablet_result(
    result: arcen_protocol::messages::TabletModeResultMsg,
    offer: &Result<(), String>,
    authorization: Option<&Result<AuthorizedTablet, String>>,
) -> arcen_protocol::messages::TabletModeResultMsg {
    arcen_input::bridged_pen::tablet_result(
        result,
        offer,
        authorization,
        arcen_input::bridged_pen::HostPenNotices {
            dedicated: HOST_PEN_NOTICE,
            generic: GENERIC_PEN_NOTICE,
        },
    )
}

/// What a session must send after a Native tablet step: wire frames for the
/// Deck's bulk lane, and a tablet-mode result when Native tablet ended.
#[derive(Debug, Default)]
pub struct SessionOutput {
    pub frames: Vec<Vec<u8>>,
    pub notice: Option<String>,
    /// Tablet events this host read from the bridged pen itself, for the
    /// session's input injector, in order.
    pub pens: Vec<arcen_protocol::messages::PenEventMsg>,
    /// The same reads as shared pen samples, for the host pointer they move
    /// ([`HostPenPointer`]).
    pub positions: Vec<arcen_input::tablet_pointer::PenSample>,
}

/// Why Native tablet ended when the console went to the login window.
pub const CONSOLE_TO_LOGIN_WINDOW: &str = "this Mac's console went to the login window, and bridged devices exist only for the \
     account whose desktop is on screen";
/// Why Native tablet ended when another account took the console.
pub const CONSOLE_TO_OTHER_USER: &str = "another account took this Mac's console, and bridged devices exist only for the account \
     whose desktop is on screen";
/// How often an active attachment re-reads who holds the console when no
/// USB traffic arrives.
pub const CONSOLE_POLL: Duration = Duration::from_millis(100);
/// Traffic re-reads it too, at most this often: no report reaches a device
/// more than this long after the console changed hands.
pub const CONSOLE_RECHECK: Duration = Duration::from_millis(20);

/// The account whose desktop must be on the console for its bridged devices
/// to exist (ADR 0022 §6). `IOHIDUserDevice`s are system-wide: after a Fast
/// User Switch or a return to the login window, a device fed by this
/// session would act on somebody else's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleWatch {
    uid: u32,
}

impl ConsoleWatch {
    /// The account `uid`.
    #[must_use]
    pub const fn for_uid(uid: u32) -> Self {
        Self { uid }
    }

    /// The account this process runs as: an agent serves its own account.
    #[must_use]
    pub fn for_this_process() -> Self {
        // SAFETY: `getuid` has no preconditions and cannot fail.
        #[allow(unsafe_code)]
        let uid = unsafe { libc::getuid() };
        Self { uid }
    }

    /// Whether the devices may exist while `holder` holds the console.
    ///
    /// # Errors
    ///
    /// Why not, in words a Deck shows its user.
    pub fn check(
        self,
        holder: arcen_session::agent_relay::ConsoleHolder,
    ) -> Result<(), &'static str> {
        use arcen_session::agent_relay::ConsoleHolder;
        match holder {
            ConsoleHolder::User(uid) if uid == self.uid => Ok(()),
            ConsoleHolder::User(_) => Err(CONSOLE_TO_OTHER_USER),
            ConsoleHolder::LoginWindow => Err(CONSOLE_TO_LOGIN_WINDOW),
        }
    }
}

/// Who holds the console now.
#[cfg_attr(not(feature = "native-tablet"), allow(dead_code))]
fn console_holder_now() -> arcen_session::agent_relay::ConsoleHolder {
    #[cfg(feature = "native-tablet")]
    if let Some(holder) = *CONSOLE_OVERRIDE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
    {
        return holder;
    }
    crate::console::console_holder()
}

#[cfg(feature = "native-tablet")]
static CONSOLE_OVERRIDE: Mutex<Option<arcen_session::agent_relay::ConsoleHolder>> =
    Mutex::new(None);

/// Lab builds only, for the end-to-end test: pretend `holder` holds the
/// console (`None` reads the real one again). Switching the real console
/// needs a second account at the machine.
#[cfg(feature = "native-tablet")]
#[doc(hidden)]
pub fn set_console_holder_for_lab_tests(holder: Option<arcen_session::agent_relay::ConsoleHolder>) {
    *CONSOLE_OVERRIDE
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = holder;
}

/// Native tablet inside one streaming session: inactive unless the handshake
/// accepted it; ends itself (and tells the Deck) on any failure or when the
/// console leaves this session's account, while the session goes on.
#[derive(Debug)]
pub struct SessionTablet {
    #[cfg(feature = "native-tablet")]
    attachment: Option<BridgedTablet>,
    #[cfg(feature = "native-tablet")]
    console: Console,
    #[cfg_attr(not(feature = "native-tablet"), allow(dead_code))]
    result: arcen_protocol::messages::TabletModeResultMsg,
}

/// The console check of one attachment.
#[cfg(feature = "native-tablet")]
#[derive(Debug)]
struct Console {
    watch: ConsoleWatch,
    holder: fn() -> arcen_session::agent_relay::ConsoleHolder,
    checked: Option<Instant>,
    tick: Option<tokio::time::Interval>,
}

#[cfg(feature = "native-tablet")]
impl Console {
    /// Re-reads the console unless it was read within [`CONSOLE_RECHECK`]
    /// (`force` always reads).
    fn check(&mut self, force: bool) -> Result<(), &'static str> {
        let now = Instant::now();
        if !force
            && self
                .checked
                .is_some_and(|checked| now.duration_since(checked) < CONSOLE_RECHECK)
        {
            return Ok(());
        }
        let verdict = self.watch.check((self.holder)());
        if verdict.is_ok() {
            self.checked = Some(now);
        }
        verdict
    }
}

impl SessionTablet {
    /// Starts the attachment for `tablet`, if any, for this process's
    /// account.
    #[must_use]
    #[cfg_attr(not(feature = "native-tablet"), allow(clippy::needless_pass_by_value))]
    pub fn start(
        tablet: Option<AuthorizedTablet>,
        result: &arcen_protocol::messages::TabletModeResultMsg,
    ) -> (Self, SessionOutput) {
        #[cfg(feature = "native-tablet")]
        {
            Self::start_with(
                tablet,
                result,
                program(),
                ConsoleWatch::for_this_process(),
                console_holder_now,
            )
        }
        #[cfg(not(feature = "native-tablet"))]
        {
            let _ = tablet;
            (
                Self {
                    result: result.clone(),
                },
                SessionOutput::default(),
            )
        }
    }

    /// [`Self::start`] with the injector program, the account and the
    /// console reader given.
    #[cfg(feature = "native-tablet")]
    #[must_use]
    pub fn start_with(
        tablet: Option<AuthorizedTablet>,
        result: &arcen_protocol::messages::TabletModeResultMsg,
        program: Option<&Path>,
        watch: ConsoleWatch,
        holder: fn() -> arcen_session::agent_relay::ConsoleHolder,
    ) -> (Self, SessionOutput) {
        let mut session = Self {
            attachment: None,
            console: Console {
                watch,
                holder,
                checked: None,
                tick: None,
            },
            result: result.clone(),
        };
        let Some(tablet) = tablet else {
            return (session, SessionOutput::default());
        };
        let ended = |session: &Self, reason: &str| SessionOutput {
            notice: session.ended_notice(reason),
            ..SessionOutput::default()
        };
        if let Err(reason) = session.console.check(true) {
            let output = ended(&session, reason);
            return (session, output);
        }
        let Some(program) = program else {
            let output = ended(&session, NO_PROGRAM);
            return (session, output);
        };
        match contained(|| BridgedTablet::start(program, &tablet, fresh_generation())) {
            Ok((attachment, frames)) => {
                let mut tick = tokio::time::interval(CONSOLE_POLL);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                session.console.tick = Some(tick);
                session.attachment = Some(attachment);
                (
                    session,
                    SessionOutput {
                        frames,
                        ..SessionOutput::default()
                    },
                )
            }
            Err(reason) => {
                let output = ended(&session, &reason);
                (session, output)
            }
        }
    }

    /// Whether an attachment is running.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        #[cfg(feature = "native-tablet")]
        {
            self.attachment.is_some()
        }
        #[cfg(not(feature = "native-tablet"))]
        {
            false
        }
    }

    /// Whether `bytes` is a USB completion this session must handle.
    #[must_use]
    pub fn takes(&self, bytes: &[u8]) -> bool {
        self.is_active()
            && bytes.first() == Some(&(arcen_protocol::FrameType::UsbBridgeUrbComplete as u8))
    }

    /// One USB completion from the Deck. Nothing reaches a device unless
    /// this session's account holds the console.
    #[must_use]
    pub fn on_deck_frame(&mut self, bytes: &[u8]) -> SessionOutput {
        #[cfg(feature = "native-tablet")]
        {
            if self.attachment.is_none() {
                return SessionOutput::default();
            }
            if let Err(reason) = self.console.check(false) {
                return self.revoke(reason);
            }
            let Some(attachment) = self.attachment.as_mut() else {
                return SessionOutput::default();
            };
            match contained(|| attachment.on_deck_frame(bytes)) {
                Ok(frames) => {
                    let (pens, positions) = attachment.take_pens();
                    SessionOutput {
                        frames,
                        notice: None,
                        pens,
                        positions,
                    }
                }
                Err(reason) => self.end(&reason, true),
            }
        }
        #[cfg(not(feature = "native-tablet"))]
        {
            let _ = bytes;
            SessionOutput::default()
        }
    }

    /// Waits for the attachment's next step, or for the console to leave
    /// this session's account; never resolves when inactive.
    pub async fn next(&mut self) -> SessionOutput {
        #[cfg(feature = "native-tablet")]
        {
            use futures_util::FutureExt as _;
            enum Woke {
                Step(Result<TabletStep, String>),
                Console,
            }
            let (Some(attachment), Some(tick)) =
                (self.attachment.as_mut(), self.console.tick.as_mut())
            else {
                return std::future::pending().await;
            };
            let woke = tokio::select! {
                biased;
                _ = tick.tick() => Woke::Console,
                step = std::panic::AssertUnwindSafe(attachment.next()).catch_unwind() => {
                    Woke::Step(step.map_err(|_| {
                        "an internal fault in the Native tablet adapter".to_owned()
                    }))
                }
            };
            match woke {
                Woke::Console => match self.console.check(true) {
                    Ok(()) => SessionOutput::default(),
                    Err(reason) => self.revoke(reason),
                },
                Woke::Step(Ok(TabletStep::Frames(frames))) => {
                    let (pens, positions) = self
                        .attachment
                        .as_mut()
                        .map(BridgedTablet::take_pens)
                        .unwrap_or_default();
                    SessionOutput {
                        frames,
                        notice: None,
                        pens,
                        positions,
                    }
                }
                Woke::Step(Ok(TabletStep::Ended(reason)) | Err(reason)) => self.end(&reason, true),
            }
        }
        #[cfg(not(feature = "native-tablet"))]
        {
            std::future::pending().await
        }
    }

    /// Ends Native tablet because the console left this session's account:
    /// the device owner is killed at once, so its devices vanish before
    /// another queued report can reach them.
    #[cfg(feature = "native-tablet")]
    fn revoke(&mut self, reason: &str) -> SessionOutput {
        if let Some(attachment) = self.attachment.as_mut() {
            attachment.kill_device_owner();
        }
        // No pen event either, not even a release: the desktop on the
        // console is no longer this session's.
        self.end(reason, false)
    }

    /// Ends the attachment. `release` lifts a pen this host read itself and
    /// left in proximity, so no press is stranded on this session's desktop.
    #[cfg(feature = "native-tablet")]
    fn end(&mut self, reason: &str, release: bool) -> SessionOutput {
        tracing::warn!(
            target: arcen_telemetry::names::target::HID,
            event = "usb_hard_device_refused",
            %reason,
            "Native tablet ended for this attachment; the session goes on"
        );
        self.console.tick = None;
        let (frames, pens) = self
            .attachment
            .take()
            .map(|mut attachment| {
                // The mode restore goes out before the cancellations and is
                // left to finish: it touches only the Deck's tablet, never
                // this host's desktop, so it is sent on a console change too.
                contained(|| Ok(attachment.end(release))).unwrap_or_default()
            })
            .unwrap_or_default();
        SessionOutput {
            frames,
            notice: self.ended_notice(reason),
            pens,
            positions: Vec::new(),
        }
    }

    /// The tablet-mode result that tells the Deck Native tablet ended, so it
    /// gives the tablet back to its own Mac.
    #[cfg_attr(not(feature = "native-tablet"), allow(dead_code))]
    fn ended_notice(&self, reason: &str) -> Option<String> {
        arcen_input::bridged_pen::ended_notice(&self.result, reason)
    }
}

impl Drop for SessionTablet {
    fn drop(&mut self) {
        #[cfg(feature = "native-tablet")]
        if let Some(attachment) = self.attachment.take() {
            let _ = contained(|| Ok(attachment.shutdown()));
        }
    }
}

/// A generation no earlier attachment of this or a previous agent used.
#[cfg(feature = "native-tablet")]
fn fresh_generation() -> arcen_usb_bridge::AttachmentGeneration {
    arcen_input::bridged_pen::fresh_generation(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default(),
        arcen_transport::quic::next_attachment_generation().get(),
    )
}

#[cfg(feature = "native-tablet")]
pub use attachment::{BridgedTablet, TabletStep};

#[cfg(feature = "native-tablet")]
mod attachment {
    use super::AuthorizedTablet;
    use arcen_input::bridged_pen::{BridgedPenSession, BridgedPenStep};
    use arcen_protocol::messages::PenEventMsg;
    use arcen_usb_bridge::hid_device_ipc::HidDeviceFrame;
    use arcen_usb_bridge::{AttachmentGeneration, HidReportKind, HidReportRequest, HostPenNote};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
    use std::time::{Duration, Instant};

    /// How long the child may take to publish the devices.
    const READY_DEADLINE: Duration = Duration::from_secs(5);
    /// Frames waiting for the child; a child that falls this far behind ends
    /// the attachment rather than losing a report.
    const CHILD_BACKLOG: usize = 512;
    /// How long the child gets to exit after its input closes.
    const CHILD_EXIT: Duration = Duration::from_millis(500);

    enum ChildEvent {
        Frame(HidDeviceFrame),
        Gone(String),
    }

    /// What the session must do after an attachment step.
    #[derive(Debug)]
    pub enum TabletStep {
        /// Wire frames for the Deck, in order (possibly none).
        Frames(Vec<Vec<u8>>),
        /// Native tablet ended for this attachment; the session goes on.
        Ended(String),
    }

    /// One Native tablet attachment: the shared host-read attachment
    /// (`arcen_input::bridged_pen`: enumeration, the pen read here, its mode
    /// switch), the device-owner child that publishes the other interfaces,
    /// and the deadlines.
    pub struct BridgedTablet {
        pen: BridgedPenSession,
        program: std::path::PathBuf,
        child: Option<Child>,
        to_child: Option<SyncSender<HidDeviceFrame>>,
        from_child: Option<tokio::sync::mpsc::Receiver<ChildEvent>>,
        ready_by: Option<Instant>,
        tick: tokio::time::Interval,
        devices: usize,
        pens: Vec<PenEventMsg>,
        positions: Vec<arcen_input::tablet_pointer::PenSample>,
    }

    impl std::fmt::Debug for BridgedTablet {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("BridgedTablet")
                .field("phase", &self.pen.bridge().phase())
                .field("devices", &self.devices)
                .finish_non_exhaustive()
        }
    }

    impl BridgedTablet {
        /// Starts enumerating the authorized device; returns the first frames
        /// for the Deck.
        ///
        /// # Errors
        ///
        /// Why the attachment could not start.
        pub fn start(
            program: &Path,
            tablet: &AuthorizedTablet,
            generation: AttachmentGeneration,
        ) -> Result<(Self, Vec<Vec<u8>>), String> {
            let (pen, step) = BridgedPenSession::start(
                tablet,
                super::PUBLISH_POLICY,
                generation,
                Instant::now(),
            )?;
            let mut tick = tokio::time::interval(arcen_input::bridged_pen::BRIDGED_PEN_POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut attachment = Self {
                pen,
                program: program.to_path_buf(),
                child: None,
                to_child: None,
                from_child: None,
                ready_by: None,
                tick,
                devices: 0,
                pens: Vec::new(),
                positions: Vec::new(),
            };
            let frames = attachment.apply(step)?;
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                generation = generation.get(),
                vendor_id = tablet.identity.vendor_id,
                product_id = tablet.identity.product_id,
                pen_reader = tablet.host_pen.kind(),
                pen_interface = tablet.pen_interface,
                interfaces = tablet.report_descriptors.len(),
                published = ?tablet.published,
                withheld = ?tablet
                    .withheld
                    .iter()
                    .map(|(interface, refusal)| format!("{interface}: {refusal}"))
                    .collect::<Vec<_>>(),
                "Native tablet: enumerating the bridged device"
            );
            Ok((attachment, frames))
        }

        /// One `UsbBridgeUrbComplete` frame from the Deck.
        ///
        /// # Errors
        ///
        /// Why the attachment ended.
        pub fn on_deck_frame(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, String> {
            let step = self.pen.on_deck_frame(bytes, Instant::now())?;
            self.apply(step)
        }

        /// Waits for the next thing the attachment must act on: a frame from
        /// the child, the child going away, or a deadline.
        pub async fn next(&mut self) -> TabletStep {
            let event = if let Some(events) = self.from_child.as_mut() {
                tokio::select! {
                    event = events.recv() => event.or_else(|| Some(ChildEvent::Gone("the device owner stopped".to_owned()))),
                    _ = self.tick.tick() => None,
                }
            } else {
                self.tick.tick().await;
                None
            };
            let result = match event {
                Some(ChildEvent::Frame(frame)) => self.on_child_frame(frame),
                Some(ChildEvent::Gone(reason)) => Err(reason),
                None => self.on_tick(),
            };
            match result {
                Ok(frames) => TabletStep::Frames(frames),
                Err(reason) => TabletStep::Ended(reason),
            }
        }

        /// Tablet events read from the pen since the last call, and the
        /// same reads as shared pen samples.
        pub fn take_pens(
            &mut self,
        ) -> (
            Vec<PenEventMsg>,
            Vec<arcen_input::tablet_pointer::PenSample>,
        ) {
            (
                std::mem::take(&mut self.pens),
                std::mem::take(&mut self.positions),
            )
        }

        /// Kills the device owner now (by its pid, through its handle): the
        /// kernel destroys its devices with it, and no report still queued
        /// for it is delivered.
        pub fn kill_device_owner(&mut self) {
            drop(self.to_child.take());
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
        }

        /// Ends the attachment while the Deck is still there: the frames to
        /// send (the pen's mode put back, then the cancellations) and, with
        /// `release`, the events still owed to the injector, ending with a
        /// proximity exit for a pen left in proximity; without, no event.
        pub fn end(&mut self, release: bool) -> (Vec<Vec<u8>>, Vec<PenEventMsg>) {
            let mut pens = std::mem::take(&mut self.pens);
            self.positions.clear();
            let (frames, owed) = self.pen.end(release, Instant::now());
            if release {
                pens.extend(owed);
            } else {
                pens.clear();
            }
            self.log_end();
            (frames, pens)
        }

        /// Ends the attachment without the Deck: the cancellations. Dropping
        /// closes the child's input, which destroys the devices.
        #[must_use]
        pub fn shutdown(mut self) -> Vec<Vec<u8>> {
            let frames = self.pen.shutdown();
            self.log_end();
            frames
        }

        fn log_end(&self) {
            let bridge = self.pen.bridge();
            let stats = bridge.stats();
            let (pen_packets, pen_ignored, pen_dropped) = bridge.pen_counts();
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                pen_packets,
                pen_ignored,
                pen_dropped,
                reports = stats.reports,
                report_bytes = stats.report_bytes,
                report_requests = stats.report_requests,
                report_requests_failed = stats.report_requests_failed,
                interrupt_errors = stats.interrupt_errors,
                devices = self.devices,
                "Native tablet: attachment ended"
            );
        }

        fn on_tick(&mut self) -> Result<Vec<Vec<u8>>, String> {
            if self
                .ready_by
                .is_some_and(|deadline| Instant::now() > deadline)
            {
                return Err("the bridged HID devices were not published in time".to_owned());
            }
            let step = self.pen.poll(Instant::now())?;
            self.apply(step)
        }

        fn on_child_frame(&mut self, frame: HidDeviceFrame) -> Result<Vec<Vec<u8>>, String> {
            match frame {
                HidDeviceFrame::Ready { devices } if self.ready_by.is_some() => {
                    if usize::from(devices) != self.devices {
                        return Err("the device owner published the wrong devices".to_owned());
                    }
                    self.ready_by = None;
                    tracing::info!(
                        target: arcen_telemetry::names::target::HID,
                        devices,
                        "Native tablet: bridged HID devices published"
                    );
                    let step = self.pen.devices_created(Instant::now())?;
                    self.apply(step)
                }
                HidDeviceFrame::Refused { reason } => Err(reason),
                // The shared attachment refuses a token in its own range and
                // an interface the host reads alone (never the child's).
                HidDeviceFrame::GetReport {
                    token,
                    index,
                    kind,
                    report_id,
                    length,
                } => match HidReportKind::from_request(kind) {
                    Some(kind) => {
                        let request = HidReportRequest::Get {
                            index,
                            kind,
                            report_id,
                            length,
                        };
                        let step = self.pen.device_request(token, request, Instant::now())?;
                        self.apply(step)
                    }
                    None => self.refuse_call(token),
                },
                HidDeviceFrame::SetReport {
                    token,
                    index,
                    kind,
                    report_id,
                    data,
                } => match HidReportKind::from_request(kind) {
                    Some(kind) => {
                        let request = HidReportRequest::Set {
                            index,
                            kind,
                            report_id,
                            data,
                        };
                        let step = self.pen.device_request(token, request, Instant::now())?;
                        self.apply(step)
                    }
                    None => self.refuse_call(token),
                },
                _ => Err("the device owner broke its contract".to_owned()),
            }
        }

        fn refuse_call(&self, token: u32) -> Result<Vec<Vec<u8>>, String> {
            self.to_child(HidDeviceFrame::ReportReply {
                token,
                ok: false,
                data: Vec::new(),
            })?;
            Ok(Vec::new())
        }

        /// Acts on one shared step: the pen's reads are queued for the
        /// session, the devices to publish go to a new device owner, their
        /// reports and replies to it; the wire frames are returned.
        fn apply(&mut self, step: BridgedPenStep) -> Result<Vec<Vec<u8>>, String> {
            for note in &step.notes {
                match note {
                    HostPenNote::ModeRead { original, failure } => tracing::info!(
                        target: arcen_telemetry::names::target::HID,
                        original = ?original,
                        failure = ?failure,
                        "Native tablet: the pen's mode before this host switches it"
                    ),
                    HostPenNote::ModeSwitched { accepted, failure } => tracing::info!(
                        target: arcen_telemetry::names::target::HID,
                        accepted,
                        failure = ?failure,
                        "Native tablet: this host switched the pen to tablet mode"
                    ),
                }
            }
            self.pens.extend(step.pens);
            self.positions.extend(step.positions);
            let mut frames = step.frames;
            for (index, report) in step.device_reports {
                self.to_child(HidDeviceFrame::Report { index, report })?;
            }
            for (token, result) in step.device_replies {
                let (ok, data) = match result {
                    Ok(data) => (true, data),
                    Err(failure) => {
                        tracing::debug!(
                            target: arcen_telemetry::names::target::HID,
                            token,
                            %failure,
                            "Native tablet: a report call failed"
                        );
                        (false, Vec::new())
                    }
                };
                self.to_child(HidDeviceFrame::ReportReply { token, ok, data })?;
            }
            if let Some(plan) = step.publish {
                frames.extend(self.publish(plan)?);
            }
            Ok(frames)
        }

        fn publish(
            &mut self,
            plan: Vec<arcen_usb_bridge::BridgedHidDevice>,
        ) -> Result<Vec<Vec<u8>>, String> {
            for device in &plan {
                tracing::info!(
                    target: arcen_telemetry::names::target::HID,
                    interface = device.interface,
                    vendor_id = device.vendor_id,
                    product_id = device.product_id,
                    usage_page = device.primary_usage_page,
                    usage = device.primary_usage,
                    report_descriptor_bytes = device.report_descriptor.len(),
                    location_id = device.location_id,
                    "Native tablet: publishing a bridged HID interface"
                );
            }
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                pen_interface_index = ?self.pen.bridge().pen_index(),
                pen_reader = self.pen.bridge().reader_kind(),
                "Native tablet: this host reads the pen itself and posts tablet events"
            );
            let mut child = Command::new(&self.program)
                .args(["hid-injector", "--bridged-usb"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .map_err(|error| format!("could not start the device owner: {error}"))?;
            let input = child.stdin.take();
            let output = child.stdout.take();
            let (Some(mut input), Some(mut output)) = (input, output) else {
                let _ = child.kill();
                let _ = child.wait();
                return Err("the device owner has no pipes".to_owned());
            };
            let (to_child, queue) = sync_channel::<HidDeviceFrame>(CHILD_BACKLOG);
            std::thread::Builder::new()
                .name("arcen-native-tablet-writer".to_owned())
                .spawn(move || {
                    while let Ok(frame) = queue.recv() {
                        if frame.write_to(&mut input).is_err() {
                            return;
                        }
                    }
                    // The queue closed: dropping `input` closes the child's
                    // standard input, which is its signal to cancel the
                    // devices and exit.
                })
                .map_err(|error| format!("could not start the device writer: {error}"))?;
            let (events, from_child) = tokio::sync::mpsc::channel(64);
            std::thread::Builder::new()
                .name("arcen-native-tablet-reader".to_owned())
                .spawn(move || {
                    loop {
                        let event = match HidDeviceFrame::read_from(&mut output) {
                            Ok(Some(frame)) => ChildEvent::Frame(frame),
                            Ok(None) => ChildEvent::Gone("the device owner exited".to_owned()),
                            Err(error) => ChildEvent::Gone(format!("the device owner: {error}")),
                        };
                        let gone = matches!(event, ChildEvent::Gone(_));
                        if events.blocking_send(event).is_err() || gone {
                            return;
                        }
                    }
                })
                .map_err(|error| format!("could not start the device reader: {error}"))?;
            self.child = Some(child);
            self.to_child = Some(to_child);
            self.from_child = Some(from_child);
            self.devices = plan.len();
            for device in plan {
                self.to_child(HidDeviceFrame::Create(device))?;
            }
            self.to_child(HidDeviceFrame::Activate)?;
            self.ready_by = Some(Instant::now() + READY_DEADLINE);
            Ok(Vec::new())
        }

        fn to_child(&self, frame: HidDeviceFrame) -> Result<(), String> {
            let sender = self
                .to_child
                .as_ref()
                .ok_or_else(|| "the device owner is not running".to_owned())?;
            sender.try_send(frame).map_err(|error| match error {
                TrySendError::Full(_) => "the device owner fell behind".to_owned(),
                TrySendError::Disconnected(_) => "the device owner stopped".to_owned(),
            })
        }
    }

    impl Drop for BridgedTablet {
        fn drop(&mut self) {
            drop(self.to_child.take());
            if let Some(mut child) = self.child.take() {
                // Off the session's thread: the child normally exits within
                // milliseconds of its input closing.
                let _ = std::thread::Builder::new()
                    .name("arcen-native-tablet-reaper".to_owned())
                    .spawn(move || {
                        let deadline = Instant::now() + CHILD_EXIT;
                        while Instant::now() < deadline {
                            if child.try_wait().is_ok_and(|status| status.is_some()) {
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        let _ = child.kill();
                        let _ = child.wait();
                    });
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use arcen_protocol::messages::{
        TabletModeMsg, TabletModeResultMsg, UsbHardDeviceMsg, UsbHidReportDescriptorMsg,
    };

    fn intuos5() -> UsbHardDeviceMsg {
        UsbHardDeviceMsg {
            vendor_id: 0x056a,
            product_id: 0x0317,
            bcd_device: 0x0100,
            device_class: 0,
            speed: arcen_usb_bridge::UsbSpeed::Full,
            hid_report_descriptors: (0..3)
                .map(|interface| {
                    UsbHidReportDescriptorMsg::new(
                        interface,
                        arcen_usb_bridge::WacomIntuos5::report_descriptor(interface).unwrap(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn native_tablet_is_offered_only_with_every_precondition_and_says_which_is_missing() {
        let available = Probe::Available;
        assert_eq!(
            availability_from(false, false, true, &available),
            Err(NOT_BUILT.to_owned())
        );
        assert_eq!(
            availability_from(true, true, true, &available),
            Err(LOGIN_WINDOW.to_owned()),
            "never at the login window"
        );
        assert_eq!(
            availability_from(true, false, false, &available),
            Err(NO_PROGRAM.to_owned())
        );
        assert_eq!(
            availability_from(true, false, true, &Probe::Running),
            Err(PROBING.to_owned())
        );
        assert_eq!(
            availability_from(
                true,
                false,
                true,
                &Probe::Refused("no entitlement".to_owned(), Instant::now())
            ),
            Err("no entitlement".to_owned())
        );
        assert_eq!(availability_from(true, false, true, &available), Ok(()));
        if !cfg!(feature = "native-tablet") {
            assert_eq!(availability(false), Err(NOT_BUILT.to_owned()));
        }
    }

    #[tokio::test]
    async fn the_hello_waits_for_a_running_probe_so_the_first_connection_is_offered_native_tablet()
    {
        // An agent that has just started is still probing when the first
        // Deck arrives: the hello waits for the answer instead of refusing.
        let mut asked = 0;
        let offer = settle(
            || {
                asked += 1;
                if asked < 10 {
                    Err(PROBING.to_owned())
                } else {
                    Ok(())
                }
            },
            PROBE_WAIT,
        )
        .await;
        assert_eq!(offer, Ok(()));
        // A settled refusal is answered at once, and a probe that never
        // answers is not waited for beyond the bound.
        let bound = Duration::from_millis(200);
        let started = Instant::now();
        assert_eq!(
            settle(|| Err(LOGIN_WINDOW.to_owned()), bound).await,
            Err(LOGIN_WINDOW.to_owned())
        );
        assert!(started.elapsed() < bound);
        assert_eq!(
            settle(|| Err(PROBING.to_owned()), bound).await,
            Err(PROBING.to_owned())
        );
        assert!(started.elapsed() >= bound);
    }

    #[test]
    fn the_deck_device_is_authorized_by_what_it_is_not_by_its_vendor() {
        let tablet = authorize(Some(&intuos5())).unwrap();
        assert_eq!(tablet.identity.vendor_id, 0x056a);
        assert_eq!(tablet.report_descriptors.len(), 3);
        // A keyboard claiming Wacom's vendor id is not a tablet.
        let keyboard = UsbHardDeviceMsg {
            hid_report_descriptors: vec![UsbHidReportDescriptorMsg::new(
                0,
                &[
                    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15,
                    0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xC0,
                ],
            )],
            ..intuos5()
        };
        assert!(authorize(Some(&keyboard)).unwrap_err().contains("refused"));
        assert!(authorize(None).is_err());
        let legacy = UsbHardDeviceMsg {
            hid_report_descriptors: Vec::new(),
            ..intuos5()
        };
        assert!(
            authorize(Some(&legacy)).is_err(),
            "no descriptors, no tablet"
        );
        let storage = UsbHardDeviceMsg {
            device_class: 0x08,
            ..intuos5()
        };
        assert!(authorize(Some(&storage)).is_err());
    }

    fn requested_native(accepted: bool) -> TabletModeResultMsg {
        TabletModeResultMsg {
            requested: TabletModeMsg::UsbTabletBridge,
            active: if accepted {
                TabletModeMsg::UsbTabletBridge
            } else {
                TabletModeMsg::DisabledMouseCompat
            },
            accepted,
            ..TabletModeResultMsg::default()
        }
    }

    #[test]
    fn the_deck_is_told_exactly_why_and_what_it_will_get() {
        // Not offered: the precise reason replaces the generic one.
        let unavailable =
            tablet_result(requested_native(false), &Err(LOGIN_WINDOW.to_owned()), None);
        assert!(String::from(unavailable.reason).contains(LOGIN_WINDOW));
        // Offered, but this device was refused: no Native tablet.
        let refused = tablet_result(
            requested_native(true),
            &Ok(()),
            Some(&Err("not a pen tablet".to_owned())),
        );
        assert!(!refused.accepted);
        assert_eq!(refused.active, TabletModeMsg::DisabledMouseCompat);
        assert!(String::from(refused.reason).contains("not a pen tablet"));
        // Accepted: the pen is read here, and the Deck is told so.
        let tablet = authorize(Some(&intuos5())).unwrap();
        let accepted = tablet_result(requested_native(true), &Ok(()), Some(&Ok(tablet)));
        assert!(accepted.accepted && !accepted.reconnect_required);
        assert_eq!(String::from(accepted.reason), HOST_PEN_NOTICE);
        assert!(HOST_PEN_NOTICE.len() <= arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES);
        // Another mode is left alone.
        let typed = TabletModeResultMsg {
            requested: TabletModeMsg::LocalTermination,
            ..TabletModeResultMsg::default()
        };
        assert_eq!(
            tablet_result(typed.clone(), &Err(NOT_BUILT.to_owned()), None),
            typed
        );
    }

    #[test]
    fn devices_exist_only_while_this_account_holds_the_console() {
        use arcen_session::agent_relay::ConsoleHolder;
        let watch = ConsoleWatch::for_uid(501);
        assert_eq!(watch.check(ConsoleHolder::User(501)), Ok(()));
        assert_eq!(
            watch.check(ConsoleHolder::User(502)),
            Err(CONSOLE_TO_OTHER_USER),
            "Fast User Switch"
        );
        assert_eq!(
            watch.check(ConsoleHolder::LoginWindow),
            Err(CONSOLE_TO_LOGIN_WINDOW)
        );
        for reason in [CONSOLE_TO_OTHER_USER, CONSOLE_TO_LOGIN_WINDOW] {
            assert!(
                reason.len() + "Native tablet (USB bridged) ended: ".len()
                    <= arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES
            );
        }
    }

    #[test]
    fn keyboard_and_mouse_interfaces_are_never_published_on_this_host() {
        // The Intuos5's touch (on 0xFF00, also Apple's own vendor page) and
        // boot mouse are authorized (the Deck bridges the whole device) but
        // withheld here; its pen interface publishes.
        let tablet = authorize(Some(&intuos5())).unwrap();
        assert_eq!(tablet.report_descriptors.len(), 3);
        assert_eq!(tablet.published, vec![0]);
        assert_eq!(
            tablet.withheld,
            vec![
                (
                    1,
                    arcen_usb_bridge::PublishRefusal::ApplePage {
                        usage_page: 0xFF00,
                        usage: 0x01
                    }
                ),
                (
                    2,
                    arcen_usb_bridge::PublishRefusal::Collection {
                        usage_page: 0x01,
                        usage: 0x02
                    }
                )
            ]
        );
    }

    #[cfg(feature = "native-tablet")]
    #[tokio::test]
    async fn native_tablet_ends_the_moment_the_console_changes_hands() {
        use arcen_session::agent_relay::ConsoleHolder;
        use std::sync::atomic::{AtomicU32, Ordering};
        // 0 is the login window; anything else a uid.
        static HOLDER: AtomicU32 = AtomicU32::new(501);
        fn holder() -> ConsoleHolder {
            match HOLDER.load(Ordering::SeqCst) {
                0 => ConsoleHolder::LoginWindow,
                uid => ConsoleHolder::User(uid),
            }
        }
        let accepted = requested_native(true);
        let start = || {
            SessionTablet::start_with(
                Some(authorize(Some(&intuos5())).unwrap()),
                &accepted,
                Some(Path::new("/usr/bin/true")),
                ConsoleWatch::for_uid(501),
                holder,
            )
        };
        let notice = |output: &SessionOutput| -> TabletModeResultMsg {
            serde_json::from_str(output.notice.as_deref().expect("a tablet_mode_result")).unwrap()
        };
        let cancels = |output: &SessionOutput| {
            output
                .frames
                .iter()
                .filter(|frame| {
                    frame.first() == Some(&(arcen_protocol::FrameType::UsbBridgeUrbCancel as u8))
                })
                .count()
        };

        // The console goes to the login window while the device enumerates.
        HOLDER.store(501, Ordering::SeqCst);
        let (mut session, first) = start();
        assert!(session.is_active());
        assert_eq!(first.frames.len(), 1, "GET_DESCRIPTOR(Device)");
        let quiet = tokio::time::timeout(Duration::from_secs(1), session.next())
            .await
            .unwrap();
        assert!(quiet.frames.is_empty() && quiet.notice.is_none());
        HOLDER.store(0, Ordering::SeqCst);
        let switched = Instant::now();
        let ended = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let output = session.next().await;
                if output.notice.is_some() {
                    return output;
                }
            }
        })
        .await
        .unwrap();
        assert!(switched.elapsed() <= CONSOLE_POLL + Duration::from_millis(50));
        assert!(!session.is_active());
        assert_eq!(cancels(&ended), 1, "the outstanding request is cancelled");
        let result = notice(&ended);
        assert!(!result.accepted && result.reconnect_required);
        assert!(String::from(result.reason).contains(CONSOLE_TO_LOGIN_WINDOW));

        // Another account takes the console: the next USB completion is not
        // forwarded, Native tablet ends instead.
        HOLDER.store(501, Ordering::SeqCst);
        let (mut session, _) = start();
        HOLDER.store(502, Ordering::SeqCst);
        tokio::time::sleep(CONSOLE_RECHECK + Duration::from_millis(5)).await;
        let ended = session.on_deck_frame(&[0x42]);
        assert!(!session.is_active());
        assert!(String::from(notice(&ended).reason).contains(CONSOLE_TO_OTHER_USER));

        // Never started when the console is not this account's.
        HOLDER.store(0, Ordering::SeqCst);
        let (session, refused) = start();
        assert!(!session.is_active());
        assert!(refused.frames.is_empty());
        assert!(String::from(notice(&refused).reason).contains(CONSOLE_TO_LOGIN_WINDOW));
    }

    fn generic(vendor_id: u16, descriptor: &[u8]) -> UsbHardDeviceMsg {
        UsbHardDeviceMsg {
            vendor_id,
            product_id: 0x0042,
            hid_report_descriptors: vec![UsbHidReportDescriptorMsg::new(0, descriptor)],
            ..intuos5()
        }
    }

    #[test]
    fn any_tablet_is_read_a_known_model_by_its_dedicated_reader() {
        use arcen_usb_bridge::HostPenRoute;
        use arcen_usb_bridge::descriptor_fixtures::{
            GENERIC_PEN_FIXTURES, HUION_STYLE_PEN, XP_PEN_STYLE_COMPOSITE,
        };
        let tablet = authorize(Some(&intuos5())).unwrap();
        assert!(
            matches!(tablet.host_pen, HostPenRoute::Dedicated(model) if model.product_id == 0x0317)
        );
        // Every standard pen is read by its own descriptor, whoever made it.
        for (label, descriptor) in GENERIC_PEN_FIXTURES {
            let tablet = authorize(Some(&generic(0x256c, descriptor)))
                .unwrap_or_else(|why| panic!("{label}: {why}"));
            assert_eq!(tablet.host_pen.generic_interface(), Some(0), "{label}");
        }
        let huion = authorize(Some(&generic(0x256c, HUION_STYLE_PEN))).unwrap();
        assert_eq!(
            huion.published,
            vec![0],
            "a pen-only interface is published"
        );
        // A pen that shares its descriptor with a keyboard is still read,
        // and nothing of it is published.
        let composite = authorize(Some(&generic(0x28bd, XP_PEN_STYLE_COMPOSITE))).unwrap();
        assert!(composite.published.is_empty());
        assert_eq!(composite.host_pen.generic_interface(), Some(0));
        let accepted = tablet_result(requested_native(true), &Ok(()), Some(&Ok(composite)));
        assert_eq!(String::from(accepted.reason), GENERIC_PEN_NOTICE);
        assert!(GENERIC_PEN_NOTICE.len() <= arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES);
    }

    /// A generic pen whose interface also declares a mouse and a keyboard:
    /// nothing is published (no device owner is started; the program does
    /// not even exist), the Pier polls the pen interface itself and turns
    /// its pen reports into tablet events, through the real adapter and the
    /// Deck's real exporter.
    #[cfg(feature = "native-tablet")]
    #[tokio::test]
    async fn a_generic_pen_sharing_its_interface_with_a_keyboard_is_read_without_publishing() {
        use arcen_protocol::messages::PenToolMsg;
        use arcen_usb_bridge::descriptor_fixtures::XP_PEN_STYLE_COMPOSITE;
        use arcen_usb_bridge::{
            GENERIC_PEN_IN, GenericPenTablet, SyntheticDevice, UrbSubmission, UsbExporter,
        };
        fn holder() -> arcen_session::agent_relay::ConsoleHolder {
            arcen_session::agent_relay::ConsoleHolder::User(501)
        }
        let (device, handle) =
            SyntheticDevice::emulate(GenericPenTablet::with_descriptor(XP_PEN_STYLE_COMPOSITE))
                .unwrap();
        let exporter = UsbExporter::new(Box::new(device))
            .map_err(|(r, _)| r)
            .unwrap();
        let facts = exporter.facts();
        let claim = UsbHardDeviceMsg {
            vendor_id: facts.snapshot.id.vendor_id,
            product_id: facts.snapshot.id.product_id,
            bcd_device: facts.snapshot.id.bcd_device,
            device_class: facts.snapshot.device_class,
            speed: facts.speed,
            hid_report_descriptors: facts
                .report_descriptors
                .iter()
                .map(|r| UsbHidReportDescriptorMsg::new(r.interface, &r.descriptor))
                .collect(),
        };
        let tablet = authorize(Some(&claim)).unwrap();
        assert!(tablet.published.is_empty());
        let (mut session, first) = SessionTablet::start_with(
            Some(tablet),
            &requested_native(true),
            Some(Path::new("/nonexistent/arcen-pier-macos")),
            ConsoleWatch::for_uid(501),
            holder,
        );
        assert!(session.is_active());
        let pen_report = |switches: u8, x: u16, pressure: u16| {
            let mut report = vec![7, switches];
            report.extend_from_slice(&x.to_le_bytes());
            report.extend_from_slice(&12_500_u16.to_le_bytes());
            report.extend_from_slice(&pressure.to_le_bytes());
            report.extend_from_slice(&[0, 0]);
            report
        };
        let mut frames = first.frames;
        let mut pens = Vec::new();
        let mut pushed = false;
        let deadline = Instant::now() + Duration::from_secs(10);
        while pens.len() < 3 && Instant::now() < deadline {
            for frame in std::mem::take(&mut frames) {
                if frame.first() == Some(&(arcen_protocol::FrameType::UsbBridgeUrbSubmit as u8)) {
                    let (header, payload) = arcen_protocol::decode_usb_urb_submit(&frame).unwrap();
                    exporter
                        .submit(
                            UrbSubmission {
                                generation: header.generation,
                                urb_id: header.urb_id,
                                endpoint: header.endpoint,
                                transfer_kind: header.transfer_kind,
                                timeout_ms: header.timeout_ms,
                                declared_length: header.declared_length,
                                setup: header.setup,
                            },
                            payload,
                        )
                        .unwrap();
                }
            }
            if !pushed && handle.held_on(GENERIC_PEN_IN) > 0 {
                // A mouse report first: never a pen event.
                handle.push_report_to(GENERIC_PEN_IN, vec![1, 1, 5, 5]);
                handle.push_report_to(GENERIC_PEN_IN, pen_report(0b1_0000, 10_000, 0));
                handle.push_report_to(GENERIC_PEN_IN, pen_report(0b1_0001, 20_000, 8_192));
                handle.push_report_to(GENERIC_PEN_IN, pen_report(0, 20_000, 0));
                pushed = true;
            }
            if let Ok(Some(done)) = exporter.next_completion_within(Duration::from_millis(20)) {
                let frame = arcen_protocol::encode_usb_urb_complete(
                    arcen_protocol::UsbUrbCompletionHeader {
                        generation: done.generation,
                        urb_id: done.urb_id,
                        status: done.status,
                        actual_length: u32::try_from(done.data.len()).unwrap(),
                    },
                    &done.data,
                )
                .unwrap();
                let output = session.on_deck_frame(&frame);
                assert!(output.notice.is_none(), "{:?}", output.notice);
                frames.extend(output.frames);
                pens.extend(output.pens);
            }
        }
        assert_eq!(pens.len(), 3, "{pens:?}");
        assert!(pens[0].in_proximity && !pens[0].touching);
        assert!(pens[1].touching && (pens[1].pressure - 0.5).abs() < 0.01);
        assert_eq!(pens[1].tool, PenToolMsg::Tip);
        assert!((pens[1].x - 0.5).abs() < 0.001);
        assert!(!pens[2].in_proximity);
        assert!(pens.iter().all(|pen| pen.validate().is_ok()));
    }

    #[test]
    fn only_a_pen_in_a_vendor_format_no_reader_knows_is_refused_and_says_so() {
        let unknown = UsbHardDeviceMsg {
            product_id: 0x0001,
            ..intuos5()
        };
        let refusal = authorize(Some(&unknown)).unwrap_err();
        assert!(refusal.contains("056a:0001") && refusal.contains("Tablet support"));
        assert!(refusal.contains("vendor format"));
        let result = tablet_result(requested_native(true), &Ok(()), Some(&Err(refusal)));
        assert!(!result.accepted);
        assert!(String::from(result.reason).contains("056a:0001"));
    }
}
