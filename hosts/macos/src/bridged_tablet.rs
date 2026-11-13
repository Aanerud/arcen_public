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

/// A device this host agreed to present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedTablet {
    pub identity: arcen_usb_bridge::UsbDeviceId,
    /// Every authorized interface: the guard holds the device to all of
    /// them.
    pub report_descriptors: Vec<arcen_usb_bridge::InterfaceReportDescriptor>,
    pub profile: arcen_usb_bridge::PenTabletProfile,
    /// The interfaces this host publishes ([`PUBLISH_POLICY`]).
    pub published: Vec<u8>,
    /// The interfaces it does not, and why.
    pub withheld: Vec<(u8, arcen_usb_bridge::PublishRefusal)>,
    /// The model whose pen this host reads itself ([`host_pen_model`]).
    pub host_pen: arcen_usb_bridge::IntuosPenModel,
}

/// The model of `identity` whose pen this host reads itself and turns into
/// tablet events with pressure: only a model the shared reader knows. Any
/// other tablet is refused (ADR 0022 §14.2): one Native
/// tablet way, never a proximity-only half.
#[must_use]
pub fn host_pen_model(
    identity: arcen_usb_bridge::UsbDeviceId,
) -> Option<arcen_usb_bridge::IntuosPenModel> {
    arcen_usb_bridge::host_decoded_intuos_model(identity.vendor_id, identity.product_id)
}

/// The plan entry whose reports are Intuos pen packets: the interface that
/// declares input report 2 at the pen packet's length.
#[must_use]
pub fn host_pen_interface(plan: &[arcen_usb_bridge::BridgedHidDevice]) -> Option<u8> {
    plan.iter()
        .find(|device| {
            arcen_usb_bridge::declared_report_lengths(
                &device.report_descriptor,
                arcen_usb_bridge::HidReportKind::Input,
            )
            .get(&arcen_usb_bridge::INTUOS5_PEN_REPORT_ID)
                == Some(&arcen_usb_bridge::INTUOS5_PEN_REPORT_LENGTH)
        })
        .map(|device| device.index)
}

/// The tablet event for one sample this host read, in the same message the
/// Deck sends in Tablet support, so one injector serves both.
#[must_use]
pub fn host_pen_event(
    sample: &arcen_usb_bridge::HostPenSample,
) -> arcen_protocol::messages::PenEventMsg {
    use arcen_protocol::messages::{PenEventMsg, PenToolMsg};
    PenEventMsg {
        x: sample.x.clamp(0.0, 1.0),
        y: sample.y.clamp(0.0, 1.0),
        pressure: sample.pressure.clamp(0.0, 1.0),
        tilt_x_degrees: sample.tilt_x_degrees.clamp(-90.0, 90.0),
        tilt_y_degrees: sample.tilt_y_degrees.clamp(-90.0, 90.0),
        tool: if sample.eraser {
            PenToolMsg::Eraser
        } else {
            PenToolMsg::Tip
        },
        in_proximity: sample.in_proximity,
        touching: sample.touching,
        buttons: sample.barrel_buttons,
        ..PenEventMsg::default()
    }
}

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
    let device = device.ok_or_else(|| "the Deck did not capture a USB device".to_owned())?;
    let synthetic = device.vendor_id == arcen_usb_bridge::ARCEN_LAB_VENDOR_ID
        && device.product_id == arcen_usb_bridge::ARCEN_LAB_PRODUCT_ID;
    if synthetic && std::env::var("ARCEN_USB_HARD_SYNTHETIC").ok().as_deref() != Some("1") {
        return Err("the synthetic lab tablet is not enabled on this host".to_owned());
    }
    let report_descriptors = device
        .report_descriptors()
        .map_err(|error| format!("the Deck's HID report descriptors are malformed: {error}"))?;
    let profile =
        arcen_usb_bridge::authorize_hard_usb_tablet(device.device_class, &report_descriptors)
            .map_err(|denial| format!("the captured USB device was refused: {denial}"))?;
    let (mut published, mut withheld) = (Vec::new(), Vec::new());
    for claim in &report_descriptors {
        match arcen_usb_bridge::publishable(PUBLISH_POLICY, &claim.descriptor) {
            Ok(()) => published.push(claim.interface),
            Err(refusal) => withheld.push((claim.interface, refusal)),
        }
    }
    let identity = arcen_usb_bridge::UsbDeviceId {
        vendor_id: device.vendor_id,
        product_id: device.product_id,
        bcd_device: device.bcd_device,
    };
    let Some(host_pen) = host_pen_model(identity) else {
        return Err(format!(
            "this Mac's Pier reads a tablet's pen itself and does not know this model \
             ({:04x}:{:04x}) yet; use Tablet support",
            device.vendor_id, device.product_id
        ));
    };
    if published.is_empty() {
        return Err(
            "the captured tablet has no pen or vendor interface this Mac may publish \
             (keyboard, keypad and mouse interfaces are never published on macOS)"
                .to_owned(),
        );
    }
    Ok(AuthorizedTablet {
        identity,
        host_pen,
        report_descriptors,
        profile,
        published,
        withheld,
    })
}

/// The tablet-mode result for a Native tablet request, given what this host
/// can do and what the Deck brought.
///
/// Accepted results keep the shared negotiation's answer and add the
/// vendor-driver notice when no driver is installed; refusals carry the
/// precise reason rather than the generic one.
#[must_use]
pub fn tablet_result(
    mut result: arcen_protocol::messages::TabletModeResultMsg,
    offer: &Result<(), String>,
    authorization: Option<&Result<AuthorizedTablet, String>>,
) -> arcen_protocol::messages::TabletModeResultMsg {
    use arcen_protocol::messages::{TabletModeMsg, TabletModeReason};
    if result.requested != TabletModeMsg::UsbTabletBridge {
        return result;
    }
    let reason = |text: String| TabletModeReason::try_from(text).unwrap_or_default();
    match (offer, authorization) {
        (Err(why), _) => {
            if !result.accepted {
                result.reason = reason(format!("Native tablet (USB bridged) unavailable: {why}"));
            }
        }
        (Ok(()), Some(Err(why))) => {
            result.accepted = false;
            if result.active == TabletModeMsg::UsbTabletBridge {
                result.active = TabletModeMsg::DisabledMouseCompat;
            }
            result.reconnect_required = true;
            result.reason = reason(format!(
                "Native tablet (USB bridged) refused: {why}. Use Tablet support instead."
            ));
        }
        (Ok(()), Some(Ok(_))) if result.accepted => {
            result.reason = reason(HOST_PEN_NOTICE.to_owned());
        }
        _ => {}
    }
    result
}

/// The shared pen sample (ADR 0023) for one read of a pen this host reads
/// itself: the reader's device units over the model's whole range, the
/// tool, and when it was read. The macOS adapter of the one pen position
/// path; the injector and the position use the same scaling.
#[must_use]
pub fn host_pen_sample(
    sample: &arcen_usb_bridge::HostPenSample,
    model: arcen_usb_bridge::IntuosPenModel,
    at: Instant,
) -> arcen_input::tablet_pointer::PenSample {
    use arcen_input::tablet_pointer::{AxisRange, PenSample, PenTool};
    let axis = |maximum: u32| AxisRange {
        minimum: 0,
        maximum: i32::try_from(maximum).unwrap_or(i32::MAX),
    };
    let unit = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    PenSample {
        tool: sample.in_proximity.then_some(if sample.eraser {
            PenTool::Eraser
        } else {
            PenTool::Pen
        }),
        x: unit(sample.device_x),
        y: unit(sample.device_y),
        x_range: axis(model.max_x),
        y_range: axis(model.max_y),
        at,
    }
}

/// The host pointer a pen this host reads itself moves, reported to a Deck
/// that draws its cursor at host positions (`cursor_position_v1`).
///
/// A bridged tablet moves only this host's pointer, so without these
/// records the person drawing sees no cursor on the Deck. The pen events
/// this host posts land on the captured display at the samples' own
/// fractions, so the position is known without reading the pointer back.
/// Glue only: which source leads and where the pen points are the shared
/// [`arcen_input::tablet_pointer::PenPointerLead`]'s, echo suppression,
/// rate and the record the shared
/// [`arcen_input::host_pointer::HostPointerFeed`]'s.
#[derive(Debug)]
pub struct HostPenPointer {
    lead: arcen_input::tablet_pointer::PenPointerLead,
    feed: arcen_input::host_pointer::HostPointerFeed,
}

impl HostPenPointer {
    /// A reporter for a single captured display of `width` × `height`
    /// pixels; `None` for a size a record cannot carry.
    #[must_use]
    pub fn new(width: usize, height: usize) -> Option<Self> {
        let width = u16::try_from(width).ok().filter(|width| *width > 0)?;
        let height = u16::try_from(height).ok().filter(|height| *height > 0)?;
        let mut lead = arcen_input::tablet_pointer::PenPointerLead::new(
            arcen_input::tablet_pointer::PenMapping::WholeArea,
        );
        let _ = lead.set_desktop_size(u32::from(width), u32::from(height));
        Some(Self {
            lead,
            feed: arcen_input::host_pointer::HostPointerFeed::new(Some(
                arcen_input::host_pointer::HostPointerLayout::single(
                    u32::from(width),
                    u32::from(height),
                    width,
                    height,
                ),
            )),
        })
    }

    /// The pen samples this host just read, in order.
    pub fn observe(&mut self, samples: &[arcen_input::tablet_pointer::PenSample]) {
        for sample in samples {
            if let Some(position) = self.lead.pen(*sample) {
                self.feed.observe(&position);
            }
        }
    }

    /// When the waiting record may be sent; `None` when none waits.
    #[must_use]
    pub fn due_at(&self) -> Option<Instant> {
        self.feed.due_at()
    }

    /// The record to send now, if one waits and the rate allows it; it
    /// counts as sent.
    pub fn take_record(
        &mut self,
        now: Instant,
    ) -> Option<[u8; arcen_protocol::cursor_position::CURSOR_POSITION_SIZE]> {
        self.feed.take_record(now)
    }

    /// Records sent so far.
    #[must_use]
    pub const fn reports(&self) -> u64 {
        self.feed.reports()
    }
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

/// Runs `step`, turning a panic into an error: a fault in the Native tablet
/// code ends Native tablet, never the session that streams the desktop.
///
/// # Errors
///
/// `step`'s own error, or the panic as one.
pub fn contained<T>(step: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(step))
        .unwrap_or_else(|_| Err("an internal fault in the Native tablet adapter".to_owned()))
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
                let pens = attachment.release_pen(release);
                // The mode restore goes out before the cancellations and is
                // left to finish: it touches only the Deck's tablet, never
                // this host's desktop, so it is sent on a console change too.
                let (mut frames, keep) =
                    contained(|| Ok(attachment.restore_pen_mode())).unwrap_or_default();
                frames.extend(contained(|| Ok(attachment.shutdown(keep))).unwrap_or_default());
                (frames, pens)
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
        use arcen_protocol::messages::{TabletModeMsg, TabletModeReason};
        let mut result = self.result.clone();
        result.accepted = false;
        result.active = TabletModeMsg::DisabledMouseCompat;
        result.reconnect_required = true;
        let text: String = format!("Native tablet (USB bridged) ended: {reason}")
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        let mut text = text;
        while text.len() > arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES {
            text.pop();
        }
        result.reason = TabletModeReason::try_from(text).unwrap_or_default();
        serde_json::to_string(&result).ok()
    }
}

impl Drop for SessionTablet {
    fn drop(&mut self) {
        #[cfg(feature = "native-tablet")]
        if let Some(attachment) = self.attachment.take() {
            let _ = contained(|| Ok(attachment.shutdown(None)));
        }
    }
}

/// A generation no earlier attachment of this or a previous agent used.
#[cfg(feature = "native-tablet")]
fn fresh_generation() -> arcen_usb_bridge::AttachmentGeneration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |elapsed| {
            u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX)
        });
    let counter = arcen_transport::quic::next_attachment_generation().get();
    let value = std::num::NonZeroU64::new(nanos.wrapping_add(counter) | 1)
        .unwrap_or(std::num::NonZeroU64::MIN);
    arcen_usb_bridge::AttachmentGeneration::new(value)
}

#[cfg(feature = "native-tablet")]
pub use attachment::{BridgedTablet, TabletStep};

#[cfg(feature = "native-tablet")]
mod attachment {
    use super::AuthorizedTablet;
    use arcen_protocol::messages::PenEventMsg;
    use arcen_protocol::{
        UsbUrbSubmitHeader, decode_usb_urb_complete, encode_usb_urb_cancel, encode_usb_urb_submit,
    };
    use arcen_usb_bridge::hid_device_ipc::HidDeviceFrame;
    use arcen_usb_bridge::{
        AttachmentGeneration, HidHostAdapter, HidHostOutput, HidReportKind, HidReportRequest,
        INTUOS_MODE_SWITCH, IntuosPenReader,
    };
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
    use std::time::{Duration, Instant};

    /// How long the child may take to publish the devices.
    const READY_DEADLINE: Duration = Duration::from_secs(5);
    /// Frames waiting for the child; a child that falls this far behind ends
    /// the attachment rather than losing a report.
    const CHILD_BACKLOG: usize = 512;
    /// How often deadlines are checked.
    const TICK: Duration = Duration::from_millis(100);
    /// How long the child gets to exit after its input closes.
    const CHILD_EXIT: Duration = Duration::from_millis(500);
    /// Report-request tokens this host uses for its own requests to a pen it
    /// reads itself. The child numbers its requests from 1, and a child
    /// request with a token at or above [`HOST_TOKENS`] is refused, so a
    /// reply is never handed to the wrong side.
    const HOST_TOKENS: u32 = u32::MAX - 2;
    /// `GET_REPORT` of the mode the tablet was in before this host switched it.
    const HOST_PEN_READ_MODE: u32 = u32::MAX - 2;
    /// `SET_REPORT` of Wacom mode.
    const HOST_PEN_SET_MODE: u32 = u32::MAX - 1;
    /// `SET_REPORT` of the mode read before, when the attachment ends.
    const HOST_PEN_RESTORE_MODE: u32 = u32::MAX;
    /// Pen events waiting for the session; a session that falls this far
    /// behind loses the oldest.
    const PEN_BACKLOG: usize = 256;

    /// Where this host's mode switch of a pen it reads itself stands.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PenMode {
        /// Nothing sent yet.
        Untouched,
        /// The current mode is being read.
        Reading,
        /// Wacom mode is being set; the mode read before, when known.
        Switching(Option<u8>),
        /// The tablet accepted Wacom mode.
        Switched(Option<u8>),
        /// The tablet refused the switch; nothing to restore.
        Refused,
    }

    /// A pen this host reads itself (ADR 0022 §14).
    struct HostPen {
        reader: IntuosPenReader,
        /// The plan index of the pen interface, once published.
        index: Option<u8>,
        pending: std::collections::VecDeque<PenEventMsg>,
        positions: std::collections::VecDeque<arcen_input::tablet_pointer::PenSample>,
        last: Option<PenEventMsg>,
        mode: PenMode,
    }

    /// The mode a `GET_REPORT(Feature 2)` answer names: `[id, mode, ...]`.
    pub(super) fn read_pen_mode(answer: &[u8]) -> Option<u8> {
        match answer {
            [id, mode, ..] if *id == INTUOS_MODE_SWITCH.0 => Some(*mode),
            _ => None,
        }
    }

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

    /// One Native tablet attachment: the adapter, the child, the deadlines.
    pub struct BridgedTablet {
        adapter: HidHostAdapter,
        program: std::path::PathBuf,
        child: Option<Child>,
        to_child: Option<SyncSender<HidDeviceFrame>>,
        from_child: Option<tokio::sync::mpsc::Receiver<ChildEvent>>,
        ready_by: Option<Instant>,
        tick: tokio::time::Interval,
        devices: usize,
        host_pen: Option<HostPen>,
    }

    impl std::fmt::Debug for BridgedTablet {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("BridgedTablet")
                .field("phase", &self.adapter.phase())
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
            let mut adapter = HidHostAdapter::new(
                tablet.identity,
                tablet.report_descriptors.clone(),
                generation,
            )
            .with_publish_policy(super::PUBLISH_POLICY);
            let outputs = adapter
                .start(Instant::now())
                .map_err(|error| error.to_string())?;
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut attachment = Self {
                adapter,
                program: program.to_path_buf(),
                child: None,
                to_child: None,
                from_child: None,
                ready_by: None,
                tick,
                devices: 0,
                host_pen: Some(HostPen {
                    reader: IntuosPenReader::new(tablet.host_pen),
                    index: None,
                    pending: std::collections::VecDeque::new(),
                    positions: std::collections::VecDeque::new(),
                    last: None,
                    mode: PenMode::Untouched,
                }),
            };
            let frames = attachment.apply(outputs)?;
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                generation = generation.get(),
                vendor_id = tablet.identity.vendor_id,
                product_id = tablet.identity.product_id,
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
            let (header, payload) = decode_usb_urb_complete(bytes)
                .map_err(|error| format!("malformed USB completion: {error:?}"))?;
            if header.generation != self.adapter.generation() {
                return Err("a USB completion for another attachment".to_owned());
            }
            let outputs = self
                .adapter
                .on_completion(header.urb_id, header.status, payload, Instant::now())
                .map_err(|error| error.to_string())?;
            self.apply(outputs)
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
            self.host_pen
                .as_mut()
                .map(|pen| {
                    (
                        std::mem::take(&mut pen.pending).into(),
                        std::mem::take(&mut pen.positions).into(),
                    )
                })
                .unwrap_or_default()
        }

        /// What is still owed to the session when the attachment ends: with
        /// `release`, the pending events and a proximity exit for a pen left
        /// in proximity; without, nothing.
        pub fn release_pen(&mut self, release: bool) -> Vec<PenEventMsg> {
            let Some(pen) = self.host_pen.as_mut() else {
                return Vec::new();
            };
            let mut pens: Vec<PenEventMsg> = std::mem::take(&mut pen.pending).into();
            if !release {
                return Vec::new();
            }
            let last = pens.last().or(pen.last.as_ref()).cloned();
            if let Some(last) = last
                && last.in_proximity
            {
                pens.push(PenEventMsg {
                    in_proximity: false,
                    touching: false,
                    pressure: 0.0,
                    buttons: 0,
                    ..last
                });
            }
            pens
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

        /// Puts a pen this host switched to Wacom mode back into the mode it
        /// read before, as the attachment ends while the Deck is still
        /// there: the frames to send, and the request to leave running (not
        /// cancelled by [`Self::shutdown`]). Nothing when the mode was never
        /// switched, was not readable, or was already Wacom mode; the Deck's
        /// hand-back (a USB reset) restores the power-on mode in any case.
        pub fn restore_pen_mode(&mut self) -> (Vec<Vec<u8>>, Option<arcen_usb_bridge::UrbId>) {
            let Some((index, original)) =
                self.host_pen
                    .as_ref()
                    .and_then(|pen| match (pen.index, pen.mode) {
                        (Some(index), PenMode::Switched(Some(original)))
                            if original != INTUOS_MODE_SWITCH.1[1] =>
                        {
                            Some((index, original))
                        }
                        _ => None,
                    })
            else {
                return (Vec::new(), None);
            };
            let outputs = self.adapter.request_report(
                HOST_PEN_RESTORE_MODE,
                HidReportRequest::Set {
                    index,
                    kind: HidReportKind::Feature,
                    report_id: INTUOS_MODE_SWITCH.0,
                    data: vec![INTUOS_MODE_SWITCH.0, original],
                },
                Instant::now(),
            );
            let keep = outputs.iter().find_map(|output| match output {
                HidHostOutput::Submit(urb) => Some(urb.urb_id),
                _ => None,
            });
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                original,
                sent = keep.is_some(),
                "Native tablet: this host puts the pen back into the mode it found"
            );
            if keep.is_none() {
                return (Vec::new(), None);
            }
            match self.apply(outputs) {
                Ok(frames) => (frames, keep),
                Err(_) => (Vec::new(), None),
            }
        }

        /// Ends the attachment: the cancellations for the Deck, except for
        /// `keep` (a mode restore left to finish). Dropping closes the
        /// child's input, which destroys the devices.
        #[must_use]
        pub fn shutdown(mut self, keep: Option<arcen_usb_bridge::UrbId>) -> Vec<Vec<u8>> {
            let outputs = self.adapter.shutdown();
            let generation = self.adapter.generation();
            let stats = self.adapter.stats();
            let (pen_packets, pen_ignored) = self
                .host_pen
                .as_ref()
                .map_or((0, 0), |pen| pen.reader.counts());
            tracing::info!(
                target: arcen_telemetry::names::target::HID,
                pen_packets,
                pen_ignored,
                reports = stats.reports,
                report_bytes = stats.report_bytes,
                report_requests = stats.report_requests,
                report_requests_failed = stats.report_requests_failed,
                interrupt_errors = stats.interrupt_errors,
                devices = self.devices,
                "Native tablet: attachment ended"
            );
            outputs
                .into_iter()
                .filter_map(|output| match output {
                    HidHostOutput::Cancel(urb) if Some(urb) != keep => {
                        Some(encode_usb_urb_cancel(generation, urb).to_vec())
                    }
                    _ => None,
                })
                .collect()
        }

        fn on_tick(&mut self) -> Result<Vec<Vec<u8>>, String> {
            if self
                .ready_by
                .is_some_and(|deadline| Instant::now() > deadline)
            {
                return Err("the bridged HID devices were not published in time".to_owned());
            }
            let outputs = self
                .adapter
                .poll(Instant::now())
                .map_err(|error| error.to_string())?;
            self.apply(outputs)
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
                    let mut outputs = self
                        .adapter
                        .devices_created(Instant::now())
                        .map_err(|error| error.to_string())?;
                    // The vendor driver switches the tablet out of HID mouse
                    // mode at probe; reading the pen here, this host does,
                    // after reading the mode it is in so it can put it back.
                    if let Some(pen) = self.host_pen.as_mut()
                        && let Some(index) = pen.index
                        && pen.mode == PenMode::Untouched
                    {
                        pen.mode = PenMode::Reading;
                        outputs.extend(self.adapter.request_report(
                            HOST_PEN_READ_MODE,
                            HidReportRequest::Get {
                                index,
                                kind: HidReportKind::Feature,
                                report_id: INTUOS_MODE_SWITCH.0,
                                length: 2,
                            },
                            Instant::now(),
                        ));
                    }
                    self.apply(outputs)
                }
                HidDeviceFrame::Refused { reason } => Err(reason),
                HidDeviceFrame::GetReport { token, .. }
                | HidDeviceFrame::SetReport { token, .. }
                    if token >= HOST_TOKENS =>
                {
                    self.refuse_call(token)
                }
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
                        let outputs = self.adapter.request_report(token, request, Instant::now());
                        self.apply(outputs)
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
                        let outputs = self.adapter.request_report(token, request, Instant::now());
                        self.apply(outputs)
                    }
                    None => self.refuse_call(token),
                },
                _ => Err("the device owner broke its contract".to_owned()),
            }
        }

        /// The answer to one of this host's own requests to the pen.
        fn on_host_pen_reply(
            &mut self,
            token: u32,
            result: Result<Vec<u8>, arcen_usb_bridge::HidReportFailure>,
        ) -> Result<Vec<Vec<u8>>, String> {
            let Some(pen) = self.host_pen.as_mut() else {
                return Ok(Vec::new());
            };
            match (token, pen.mode, pen.index) {
                (HOST_PEN_READ_MODE, PenMode::Reading, Some(index)) => {
                    let original = result.as_deref().ok().and_then(read_pen_mode);
                    tracing::info!(
                        target: arcen_telemetry::names::target::HID,
                        original = ?original,
                        failure = ?result.as_ref().err(),
                        "Native tablet: the pen's mode before this host switches it"
                    );
                    pen.mode = PenMode::Switching(original);
                    let (report_id, data) = INTUOS_MODE_SWITCH;
                    let outputs = self.adapter.request_report(
                        HOST_PEN_SET_MODE,
                        HidReportRequest::Set {
                            index,
                            kind: HidReportKind::Feature,
                            report_id,
                            data: data.to_vec(),
                        },
                        Instant::now(),
                    );
                    self.apply(outputs)
                }
                (HOST_PEN_SET_MODE, PenMode::Switching(original), _) => {
                    pen.mode = if result.is_ok() {
                        PenMode::Switched(original)
                    } else {
                        PenMode::Refused
                    };
                    tracing::info!(
                        target: arcen_telemetry::names::target::HID,
                        accepted = result.is_ok(),
                        failure = ?result.err(),
                        "Native tablet: this host switched the pen to tablet mode"
                    );
                    Ok(Vec::new())
                }
                _ => {
                    tracing::debug!(
                        target: arcen_telemetry::names::target::HID,
                        token,
                        accepted = result.is_ok(),
                        "Native tablet: an answer to this host's pen request"
                    );
                    Ok(Vec::new())
                }
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

        fn apply(&mut self, outputs: Vec<HidHostOutput>) -> Result<Vec<Vec<u8>>, String> {
            let generation = self.adapter.generation();
            let mut frames = Vec::new();
            for output in outputs {
                match output {
                    HidHostOutput::Submit(urb) => frames.push(
                        encode_usb_urb_submit(
                            UsbUrbSubmitHeader {
                                generation,
                                urb_id: urb.urb_id,
                                endpoint: urb.endpoint,
                                transfer_kind: urb.transfer_kind,
                                timeout_ms: urb.timeout_ms,
                                declared_length: urb.declared_length,
                                setup: urb.setup,
                            },
                            &urb.payload,
                        )
                        .map_err(|error| format!("encode a USB request: {error:?}"))?,
                    ),
                    HidHostOutput::Cancel(urb) => {
                        frames.push(encode_usb_urb_cancel(generation, urb).to_vec());
                    }
                    HidHostOutput::CreateDevices(plan) => self.publish(plan)?,
                    HidHostOutput::Report { index, report } => {
                        if let Some(pen) = self
                            .host_pen
                            .as_mut()
                            .filter(|pen| pen.index == Some(index))
                        {
                            // Read here, not handed to the published device:
                            // one consumer, so a driver that binds it one day
                            // cannot double the pen.
                            if let Some(sample) = pen.reader.read(&report) {
                                let event = super::host_pen_event(&sample);
                                if pen.pending.len() >= PEN_BACKLOG {
                                    pen.pending.pop_front();
                                }
                                if pen.positions.len() >= PEN_BACKLOG {
                                    pen.positions.pop_front();
                                }
                                pen.last = Some(event.clone());
                                pen.pending.push_back(event);
                                pen.positions.push_back(super::host_pen_sample(
                                    &sample,
                                    pen.reader.model(),
                                    Instant::now(),
                                ));
                            }
                            continue;
                        }
                        self.to_child(HidDeviceFrame::Report { index, report })?;
                    }
                    HidHostOutput::ReportReply { token, result } if token >= HOST_TOKENS => {
                        frames.extend(self.on_host_pen_reply(token, result)?);
                    }
                    HidHostOutput::ReportReply { token, result } => {
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
                }
            }
            Ok(frames)
        }

        fn publish(&mut self, plan: Vec<arcen_usb_bridge::BridgedHidDevice>) -> Result<(), String> {
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
            if let Some(pen) = self.host_pen.as_mut() {
                pen.index = super::host_pen_interface(&plan);
                tracing::info!(
                    target: arcen_telemetry::names::target::HID,
                    pen_interface_index = ?pen.index,
                    "Native tablet: this host reads the pen itself and posts tablet events"
                );
            }
            for device in plan {
                self.to_child(HidDeviceFrame::Create(device))?;
            }
            self.to_child(HidDeviceFrame::Activate)?;
            self.ready_by = Some(Instant::now() + READY_DEADLINE);
            Ok(())
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
    fn a_panic_in_the_adapter_becomes_an_error() {
        let panicked: Result<(), String> = contained(|| panic!("a bug"));
        assert!(panicked.unwrap_err().contains("internal fault"));
        assert_eq!(contained(|| Ok(7)), Ok(7));
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

    #[test]
    fn an_unknown_model_is_refused_with_its_ids_rather_than_half_working() {
        let tablet = authorize(Some(&intuos5())).unwrap();
        assert_eq!(tablet.host_pen.product_id, 0x0317);
        let unknown = UsbHardDeviceMsg {
            product_id: 0x0001,
            ..intuos5()
        };
        let refusal = authorize(Some(&unknown)).unwrap_err();
        assert!(refusal.contains("056a:0001") && refusal.contains("Tablet support"));
        assert!(refusal.len() <= arcen_protocol::messages::MAX_TABLET_MODE_REASON_BYTES);
    }

    #[test]
    fn the_pen_interface_is_found_by_its_packet_not_its_number() {
        let device = |index: u8, interface: u8| arcen_usb_bridge::BridgedHidDevice {
            index,
            interface,
            report_descriptor: arcen_usb_bridge::WacomIntuos5::report_descriptor(interface)
                .unwrap()
                .to_vec(),
            vendor_id: 0x056a,
            product_id: 0x0317,
            version: 0x0100,
            country_code: 0,
            primary_usage_page: 0,
            primary_usage: 0,
            location_id: 0,
            manufacturer: String::new(),
            product: String::new(),
            serial: String::new(),
            input_endpoint: arcen_usb_bridge::INTUOS5_PEN_IN,
            input_length: 10,
        };
        assert_eq!(host_pen_interface(&[device(0, 1), device(1, 0)]), Some(1));
        assert_eq!(host_pen_interface(&[device(0, 1), device(1, 2)]), None);
    }

    #[test]
    fn the_pointer_a_pen_read_here_moves_goes_through_the_shared_pen_path() {
        use arcen_protocol::cursor_position::decode_cursor_position;
        assert!(HostPenPointer::new(0, 1080).is_none());
        assert!(HostPenPointer::new(70_000, 1080).is_none());
        let model = arcen_usb_bridge::host_decoded_intuos_model(0x056a, 0x0317).unwrap();
        let mut pointer = HostPenPointer::new(1920, 1080).unwrap();
        let start = Instant::now();
        let read = |x: u32, y: u32, in_proximity| {
            host_pen_sample(
                &arcen_usb_bridge::HostPenSample {
                    x: 0.0,
                    y: 0.0,
                    device_x: x,
                    device_y: y,
                    pressure: 0.0,
                    tilt_x_degrees: 0.0,
                    tilt_y_degrees: 0.0,
                    eraser: false,
                    in_proximity,
                    touching: false,
                    barrel_buttons: 0,
                },
                model,
                start,
            )
        };
        // The first position is where the pointer rests, not a move.
        pointer.observe(&[read(16_000, 20_000, true)]);
        assert_eq!(pointer.due_at(), None);
        // A pen out of proximity moves nothing.
        pointer.observe(&[read(model.max_x, model.max_y, false)]);
        assert_eq!(pointer.due_at(), None);
        // Several moves in one step: only the newest is reported, at the
        // shared mapping's pixel.
        // 32_513 / 65_025 and 10_161 / 40_641 of the tablet, as libinput
        // and the X server scale it.
        pointer.observe(&[read(16_000, 20_000, true), read(32_513, 10_161, true)]);
        let record = decode_cursor_position(&pointer.take_record(start).unwrap()).unwrap();
        assert!(record.on_stream);
        assert_eq!((record.space_width, record.space_height), (1920, 1080));
        assert_eq!((record.x, record.y), (960, 270));
        assert_eq!(record.sequence, 1);
        // At most one record per 8.3 ms; the newest waits, then goes.
        pointer.observe(&[read(model.max_x, 0, true)]);
        assert!(pointer.take_record(start).is_none());
        let due = pointer.due_at().unwrap();
        let record = decode_cursor_position(&pointer.take_record(due).unwrap()).unwrap();
        assert_eq!((record.x, record.y, record.sequence), (1919, 0, 2));
        assert_eq!(pointer.reports(), 2);
        assert_eq!(pointer.due_at(), None);
    }

    #[cfg(feature = "native-tablet")]
    #[test]
    fn the_pen_mode_read_back_is_bounded_and_names_only_report_two() {
        use super::attachment::read_pen_mode;
        assert_eq!(read_pen_mode(&[2, 0]), Some(0));
        assert_eq!(read_pen_mode(&[2, 2, 9]), Some(2));
        assert_eq!(read_pen_mode(&[3, 0]), None);
        assert_eq!(read_pen_mode(&[2]), None);
        assert_eq!(read_pen_mode(&[]), None);
    }

    #[test]
    fn a_pen_read_here_becomes_the_same_tablet_event_a_deck_sends() {
        use arcen_protocol::messages::PenToolMsg;
        let model = arcen_usb_bridge::host_decoded_intuos_model(0x056a, 0x0317).unwrap();
        let mut reader = arcen_usb_bridge::IntuosPenReader::new(model);
        let events: Vec<_> = arcen_usb_bridge::intuos5_scripted_stroke()
            .iter()
            .filter_map(|step| reader.read(&step.report.encode()))
            .map(|sample| host_pen_event(&sample))
            .collect();
        assert!(events.iter().all(|event| event.validate().is_ok()));
        assert!(events.iter().all(|event| event.tool == PenToolMsg::Tip));
        let pressed = events.iter().filter(|event| event.touching).count();
        assert!(pressed > 20, "the stroke presses ({pressed})");
        assert!(events.iter().any(|event| event.pressure > 0.99));
        assert!(events.iter().any(|event| event.buttons == 1));
        let last = events.last().unwrap();
        assert!(!last.in_proximity && !last.touching);
        let eraser = host_pen_event(&arcen_usb_bridge::HostPenSample {
            x: 2.0,
            y: -1.0,
            device_x: 0,
            device_y: 0,
            pressure: 0.5,
            tilt_x_degrees: 63.0,
            tilt_y_degrees: -64.0,
            eraser: true,
            in_proximity: true,
            touching: true,
            barrel_buttons: 0,
        });
        assert_eq!(eraser.tool, PenToolMsg::Eraser);
        assert!(eraser.validate().is_ok(), "clamped into the wire's ranges");
    }
}
