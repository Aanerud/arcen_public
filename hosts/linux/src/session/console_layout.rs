// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's layout on the joined console (ADR 0025 §6): the Linux adapter.
//!
//! The decision (which output shows which Deck monitor, in which mode) is
//! `arcen_outputs::console_layout`'s. This module reads the console's
//! outputs through RandR, applies the decision as one NVIDIA MetaMode over
//! NV-CONTROL (`display::nv_control`, requests on the same proven X
//! connection as RandR; no `nvidia-settings` child runs as root against the
//! session's X server), makes the Deck's primary the RandR primary, hides
//! the forced virtual heads the layout does not use (RandR `non-desktop`)
//! and proves the result. Before it changes anything it journals the
//! console's own MetaMode, primary and output visibility in
//! `/run/arcen/console-layout.json`; the hand-back restores exactly that,
//! and so does the Pier's recovery after a launcher that did not. The
//! journal also holds the session's windows before the layout and once it
//! settled, and the restore moves back the windows the layout moved
//! (`console_windows`, GAP-046).

use std::path::Path;

use arcen_media::Rotation;
use arcen_outputs::console_layout::{
    console_monitor_capacity, match_shown_layout, plan_console_layout, takes_a_monitor,
    ConsoleHeadRequest, ConsoleLayoutPlan, ConsoleOutput, OutputMode, ShownLayoutMatch,
    ShownOutput,
};
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::console::{connect, read_root_journal, write_root_journal, SessionDisplay};
use crate::display::nv_control;
use crate::logging::target;

const LAYOUT_JOURNAL: &str = "/run/arcen/console-layout.json";

/// The installer's forced virtual outputs (a headless console): their
/// presence means the console's connected heads are Arcen's, so unused
/// ones are hidden and a single Deck display also lays the console out.
pub(crate) const VIRTUAL_OUTPUTS_CONFIG: &str =
    "/etc/X11/xorg.conf.d/10-arcen-console-outputs.conf";

/// Whether this host's console runs the installer's forced virtual outputs.
pub(crate) fn virtual_outputs_installed() -> bool {
    std::fs::symlink_metadata(VIRTUAL_OUTPUTS_CONFIG).is_ok_and(|metadata| metadata.is_file())
}

/// What the console showed before Arcen laid it out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LayoutJournal {
    session_id: String,
    display: SessionDisplay,
    /// NV-CONTROL `CurrentMetaMode` before the layout.
    metamode: String,
    /// RandR primary output's name before the layout.
    primary: Option<String>,
    /// Outputs Arcen marked `non-desktop`.
    hidden: Vec<String>,
    /// The session's windows before the layout (GAP-046).
    #[serde(default)]
    windows_before: Vec<super::console_windows::SavedWindow>,
    /// Its windows once the Deck's layout had settled; `None` until then.
    #[serde(default)]
    windows_after: Option<Vec<super::console_windows::SavedWindow>>,
}

impl LayoutJournal {
    fn write(&self) -> Result<(), String> {
        let text = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        write_root_journal(Path::new(LAYOUT_JOURNAL), &text)
            .map_err(|error| format!("console layout journal: {error}"))
    }

    fn read() -> Option<Self> {
        serde_json::from_slice(&read_root_journal(Path::new(LAYOUT_JOURNAL))?).ok()
    }

    fn remove() {
        let _ = std::fs::remove_file(LAYOUT_JOURNAL);
    }
}

/// Why the console could not take the Deck's layout. The console is left
/// (or put back) as it was, and the Pier ends the sign-in with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayoutFailure {
    /// Stable token (`console_outputs_insufficient`, …).
    pub(crate) token: &'static str,
    pub(crate) detail: String,
}

impl LayoutFailure {
    fn new(token: &'static str, detail: impl Into<String>) -> Self {
        Self {
            token,
            detail: detail.into(),
        }
    }
}

/// The Deck's layout, applied; [`Self::restore`] puts the console back.
#[derive(Debug)]
pub(crate) struct AppliedLayout {
    journal: LayoutJournal,
    plan: ConsoleLayoutPlan,
}

impl AppliedLayout {
    /// The applied heads, in the order capture addresses them.
    pub(crate) fn plan(&self) -> &ConsoleLayoutPlan {
        &self.plan
    }

    /// Restores the journalled layout; `false` leaves the journal for the
    /// Pier's recovery.
    pub(crate) async fn restore(self) -> bool {
        restore_journal(&self.journal).await
    }

    /// Journals the session's windows as the settled layout left them: a
    /// window placed otherwise when the Deck leaves was moved by the Deck's
    /// user, and stays (shared `plan_window_restore`).
    pub(crate) fn note_settled_windows(&mut self) {
        if self.journal.windows_before.is_empty() || self.journal.windows_after.is_some() {
            return;
        }
        let Ok(connection) = connect(&self.journal.display) else {
            return;
        };
        let Ok(root) = root_of(&connection) else {
            return;
        };
        if let Ok(windows) = super::console_windows::read(&connection, root) {
            self.journal.windows_after = Some(windows);
            if let Err(error) = self.journal.write() {
                tracing::warn!(target: target::SESSION, %error, "the console's settled windows could not be journalled");
            }
        }
    }
}

/// The session's windows, or none (logged) when they cannot be read: the
/// layout goes ahead without them.
fn read_windows(
    connection: &RustConnection,
    root: xproto::Window,
    session_id: &str,
) -> Vec<super::console_windows::SavedWindow> {
    match super::console_windows::read(connection, root) {
        Ok(windows) => windows,
        Err(error) => {
            tracing::warn!(
                target: target::SESSION,
                session_id,
                %error,
                "the console's windows could not be read; they are not moved back after the Deck's layout"
            );
            Vec::new()
        }
    }
}

/// The requests for a committed multi-monitor plan's monitors, in its
/// order.
pub(crate) fn requests_from_plan(
    plan: &crate::display::topology::LinuxTopologyPlan,
) -> Option<Vec<ConsoleHeadRequest>> {
    plan.monitors
        .iter()
        .map(|monitor| {
            let (width, height) = monitor.mode_token.split_once('x')?;
            Some(ConsoleHeadRequest {
                width: width.parse().ok()?,
                height: height.parse().ok()?,
                x: monitor.x,
                y: monitor.y,
                rotation: monitor.rotation,
                refresh_hz: monitor.refresh_hz,
                primary: monitor.primary,
            })
        })
        .collect()
}

/// The request for a single served Deck display.
pub(crate) fn request_for_display(width: u32, height: u32, refresh_hz: u32) -> ConsoleHeadRequest {
    ConsoleHeadRequest {
        width,
        height,
        x: 0,
        y: 0,
        rotation: Rotation::Degrees0,
        refresh_hz,
        primary: true,
    }
}

/// How long reading a live X server's outputs may take.
const LIVE_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// The outputs of a console's X server that show a picture now (a CRTC
/// with a mode), in RandR order.
fn read_shown(
    connection: &RustConnection,
    root: xproto::Window,
) -> Result<Vec<ShownOutput>, String> {
    let resources = connection
        .randr_get_screen_resources_current(root)
        .map_err(|error| format!("request RandR resources: {error}"))?
        .reply()
        .map_err(|error| format!("RandR resources: {error}"))?;
    let (_, facts) = read_outputs(connection, root)?;
    let primary = primary_name(connection, root, &facts);
    let mut shown = Vec::new();
    for fact in &facts {
        if fact.crtc == 0 {
            continue;
        }
        let crtc = connection
            .randr_get_crtc_info(fact.crtc, resources.config_timestamp)
            .map_err(|error| format!("request RandR CRTC: {error}"))?
            .reply()
            .map_err(|error| format!("RandR CRTC: {error}"))?;
        let Some(mode) = resources.modes.iter().find(|mode| mode.id == crtc.mode) else {
            continue;
        };
        let rotation = if crtc.rotation.contains(randr::Rotation::ROTATE270) {
            Rotation::Degrees90
        } else if crtc.rotation.contains(randr::Rotation::ROTATE180) {
            Rotation::Degrees180
        } else if crtc.rotation.contains(randr::Rotation::ROTATE90) {
            Rotation::Degrees270
        } else {
            Rotation::Degrees0
        };
        shown.push(ShownOutput {
            name: fact.name.clone(),
            width: u32::from(mode.width),
            height: u32::from(mode.height),
            x: i32::from(crtc.x),
            y: i32::from(crtc.y),
            rotation,
            refresh_mhz: refresh_mhz(mode),
            primary: primary.as_deref() == Some(fact.name.as_str()),
        });
    }
    Ok(shown)
}

/// Reads `display`'s outputs off the async runtime, bounded: an X server
/// that does not answer is no answer.
async fn read_live<T: Send + 'static>(
    display: &SessionDisplay,
    read: fn(&RustConnection, xproto::Window) -> Result<T, String>,
) -> Result<T, String> {
    on_display(display, LIVE_READ_TIMEOUT, read)
        .await
        .map_err(|error| match error {
            DisplayWorkError::Failed(error) => error,
            DisplayWorkError::Unanswered => "the X server did not answer in time".to_owned(),
        })
}

/// How long one NV-CONTROL request may take: a MetaMode change is a mode
/// set on every head.
const NV_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Why work on a display did not finish.
enum DisplayWorkError {
    Failed(String),
    /// The X server did not answer within the bound.
    Unanswered,
}

/// The display's current MetaMode, read over NV-CONTROL (the joined
/// console's, and the single-display resize of `display::nvctrl`).
pub(crate) async fn nv_snapshot(display: &SessionDisplay) -> Result<String, String> {
    on_display(display, NV_CONTROL_TIMEOUT, |connection, _| {
        nv_control::current_metamode(connection, 0).map_err(|error| error.to_string())
    })
    .await
    .map_err(nv_work_error)
}

/// Switches the display to `metamode` over NV-CONTROL.
pub(crate) async fn nv_assign(display: &SessionDisplay, metamode: &str) -> Result<(), String> {
    let metamode = metamode.to_owned();
    on_display(display, NV_CONTROL_TIMEOUT, move |connection, _| {
        nv_control::set_current_metamode(connection, 0, &metamode)
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(nv_work_error)
}

/// [`nv_assign`] for a thread with no async runtime (a restore run from a
/// `Drop`), bounded the same way.
pub(crate) fn nv_assign_blocking(display: &SessionDisplay, metamode: &str) -> Result<(), String> {
    let metamode = metamode.to_owned();
    on_display_blocking(display, NV_CONTROL_TIMEOUT, move |connection, _| {
        nv_control::set_current_metamode(connection, 0, &metamode)
            .map_err(|error| error.to_string())
    })
    .map_err(nv_work_error)
}

fn nv_work_error(error: DisplayWorkError) -> String {
    match error {
        DisplayWorkError::Failed(error) => error,
        DisplayWorkError::Unanswered => format!(
            "NV-CONTROL did not answer within {} ms",
            NV_CONTROL_TIMEOUT.as_millis()
        ),
    }
}

/// The X socket of one piece of display work, for the caller that gives up
/// waiting: it shuts the socket down so the blocked thread wakes and ends,
/// and a socket connected only after that is shut down at once.
#[derive(Default)]
struct WorkSocket(std::sync::Mutex<WorkSocketState>);

#[derive(Default)]
enum WorkSocketState {
    #[default]
    Connecting,
    Connected(std::os::unix::net::UnixStream),
    Abandoned,
}

impl WorkSocket {
    fn connected(&self, stream: &std::os::unix::net::UnixStream) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, WorkSocketState::Abandoned) {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        } else if let Ok(clone) = stream.try_clone() {
            *state = WorkSocketState::Connected(clone);
        }
    }

    fn abandon(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let WorkSocketState::Connected(stream) =
            std::mem::replace(&mut *state, WorkSocketState::Abandoned)
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Connects to `display` and runs `work`, reporting the socket to `socket`.
fn display_work<T>(
    display: &SessionDisplay,
    socket: &WorkSocket,
    work: impl FnOnce(&RustConnection, xproto::Window) -> Result<T, String>,
) -> Result<T, String> {
    let connection = super::console::connect_reporting(display, |stream| socket.connected(stream))?;
    let root = root_of(&connection)?;
    work(&connection, root)
}

/// Runs `work` on its own connection to `display`, off the async runtime
/// and bounded: an X server that does not answer in `timeout` is no answer
/// (its socket is shut down so the blocked thread ends).
async fn on_display<T: Send + 'static>(
    display: &SessionDisplay,
    timeout: std::time::Duration,
    work: impl FnOnce(&RustConnection, xproto::Window) -> Result<T, String> + Send + 'static,
) -> Result<T, DisplayWorkError> {
    let display = display.clone();
    let socket = std::sync::Arc::new(WorkSocket::default());
    let reported = std::sync::Arc::clone(&socket);
    let work = tokio::task::spawn_blocking(move || display_work(&display, &reported, work));
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(result)) => result.map_err(DisplayWorkError::Failed),
        Ok(Err(error)) => Err(DisplayWorkError::Failed(format!(
            "work on the X server: {error}"
        ))),
        Err(_) => {
            socket.abandon();
            Err(DisplayWorkError::Unanswered)
        }
    }
}

/// [`on_display`] without an async runtime: the work runs on a thread of its
/// own and is abandoned (its socket shut down) after `timeout`.
fn on_display_blocking<T: Send + 'static>(
    display: &SessionDisplay,
    timeout: std::time::Duration,
    work: impl FnOnce(&RustConnection, xproto::Window) -> Result<T, String> + Send + 'static,
) -> Result<T, DisplayWorkError> {
    let display = display.clone();
    let socket = std::sync::Arc::new(WorkSocket::default());
    let reported = std::sync::Arc::clone(&socket);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("arcen-x-work".to_owned())
        .spawn(move || {
            let _ = sender.send(display_work(&display, &reported, work));
        })
        .map_err(|error| DisplayWorkError::Failed(format!("start the X work: {error}")))?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result.map_err(DisplayWorkError::Failed),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(DisplayWorkError::Failed(
            "the X work ended without an answer".to_owned(),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            socket.abandon();
            Err(DisplayWorkError::Unanswered)
        }
    }
}

/// The outputs of a console's X server that show a picture now, and the
/// size of its screen (the root window: what an absolute pointer spans).
fn read_shown_screen(
    connection: &RustConnection,
    root: xproto::Window,
) -> Result<(Vec<ShownOutput>, (u32, u32)), String> {
    let shown = read_shown(connection, root)?;
    let geometry = connection
        .get_geometry(root)
        .map_err(|error| format!("request root geometry: {error}"))?
        .reply()
        .map_err(|error| format!("root geometry: {error}"))?;
    Ok((
        shown,
        (u32::from(geometry.width), u32::from(geometry.height)),
    ))
}

/// One Deck monitor's output on a console shown as it is, as the launcher
/// reports it to the Pier: the output's own mode (native, pre-rotation),
/// rotation, refresh and place in the X screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ShownHead {
    pub(crate) name: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rotation: Rotation,
    pub(crate) refresh_mhz: u32,
}

impl ShownHead {
    fn extent(&self) -> (u32, u32) {
        match self.rotation {
            Rotation::Degrees90 | Rotation::Degrees270 => (self.height, self.width),
            Rotation::Degrees0 | Rotation::Degrees180 => (self.width, self.height),
        }
    }
}

/// A Deck's monitors mapped onto another product's desktop, which is only
/// read (ADR 0025 §8): what each monitor of the committed plan is shown
/// by, the X screen's size, and the mapping in words for the log and the
/// Deck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GuestLayout {
    /// One entry per monitor of the committed plan, in plan order: the
    /// output that shows it, or `None` (no window for it).
    pub(crate) heads: Vec<Option<ShownHead>>,
    /// The X screen (root window) size.
    pub(crate) screen: (u32, u32),
    /// `ShownLayoutMatch::describe`.
    pub(crate) note: String,
    /// The desktop is exactly the Deck's layout.
    pub(crate) exact: bool,
}

impl GuestLayout {
    fn from_match(
        matched: &ShownLayoutMatch,
        requests: &[ConsoleHeadRequest],
        screen: (u32, u32),
    ) -> Self {
        let mut heads = vec![None; requests.len()];
        for found in &matched.matches {
            heads[found.request] = Some(ShownHead {
                name: found.output.name.clone(),
                x: found.output.x,
                y: found.output.y,
                width: found.output.width,
                height: found.output.height,
                rotation: found.output.rotation,
                refresh_mhz: found.output.refresh_mhz,
            });
        }
        Self {
            heads,
            screen,
            note: matched.describe(requests),
            exact: matched.exact(requests),
        }
    }

    /// The outputs captured, one per served monitor, in plan order.
    pub(crate) fn outputs(&self) -> Vec<String> {
        self.heads
            .iter()
            .flatten()
            .map(|head| head.name.clone())
            .collect()
    }

    /// The committed plan as this desktop shows it: each Deck monitor that
    /// has an output takes that output's place, extent, native size and
    /// rotation (it is streamed as it is and the Deck fits the picture into
    /// the monitor's window), a monitor without one is dropped, and the
    /// absolute pointer spans the whole X screen. The Deck's logical
    /// geometry stays, so its pointer maps proportionally onto the output.
    ///
    /// # Errors
    ///
    /// When the mapping does not belong to `plan` (another monitor count),
    /// the Deck's primary has no output, or an output's geometry is not a
    /// valid stream inside the screen.
    pub(crate) fn fit(
        &self,
        plan: &crate::display::topology::LinuxTopologyPlan,
    ) -> Result<crate::display::topology::LinuxTopologyPlan, String> {
        if self.heads.len() != plan.monitors.len() {
            return Err(format!(
                "the mapping has {} monitors, the plan {}",
                self.heads.len(),
                plan.monitors.len()
            ));
        }
        let (screen_width, screen_height) = self.screen;
        let mut monitors = Vec::with_capacity(plan.monitors.len());
        for (monitor, head) in plan.monitors.iter().zip(&self.heads) {
            let Some(head) = head else {
                if monitor.primary {
                    return Err("the Deck's primary monitor has no output".to_owned());
                }
                continue;
            };
            let (width, height) = head.extent();
            let fits = |origin: i32, extent: u32, limit: u32| {
                u32::try_from(origin)
                    .ok()
                    .and_then(|origin| origin.checked_add(extent))
                    .is_some_and(|end| end <= limit)
            };
            if !fits(head.x, width, screen_width) || !fits(head.y, height, screen_height) {
                return Err(format!(
                    "{} {width}x{height}+{}+{} lies outside the {screen_width}x{screen_height} screen",
                    head.name, head.x, head.y
                ));
            }
            let mut fitted = monitor.clone();
            fitted.x = head.x;
            fitted.y = head.y;
            fitted.width = width;
            fitted.height = height;
            fitted.rotation = head.rotation;
            fitted.physical_size = arcen_media::PhysicalSize::new(head.width, head.height)
                .map_err(|error| format!("{}: {error}", head.name))?;
            fitted.mode_token = format!("{}x{}", head.width, head.height);
            monitors.push(fitted);
        }
        let mut fitted = plan.clone();
        fitted.monitors = monitors;
        fitted.virtual_width = screen_width;
        fitted.virtual_height = screen_height;
        fitted
            .region_sets()
            .map_err(|error| format!("the mapped plan is not a valid region set: {error}"))?;
        Ok(fitted)
    }
}

/// Maps the Deck's monitors onto a console shown as it is (another
/// product's desktop): its outputs are only read, never changed; each Deck
/// monitor gets the output in the same place of that desktop's own layout
/// (`arcen_outputs::console_layout::match_shown_layout`).
pub(crate) async fn match_shown(
    display: &SessionDisplay,
    requests: &[ConsoleHeadRequest],
) -> Result<GuestLayout, LayoutFailure> {
    let (shown, screen) = read_live(display, read_shown_screen)
        .await
        .map_err(|error| LayoutFailure::new("console_layout_unproven", error))?;
    let matched = match_shown_layout(requests, &shown).map_err(|error| {
        let detail = format!(
            "{error}; the desktop shows {}",
            shown
                .iter()
                .map(|output| format!(
                    "{} {}x{}+{}+{}",
                    output.name, output.width, output.height, output.x, output.y
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        LayoutFailure::new(error.token(), detail)
    })?;
    Ok(GuestLayout::from_match(&matched, requests, screen))
}

/// What a console's live X server can show a Deck, read at each sign-in
/// (ADR 0025 §6): the multi-monitor offer comes from it, never from a probe
/// X server started beside a desktop that holds the GPU.
#[derive(Debug, Clone)]
pub(crate) struct LiveConsole {
    pub(crate) session_id: String,
    pub(crate) origin: arcen_session::console_session::ConsoleOrigin,
    /// How many Deck monitors it can show: outputs that take a monitor
    /// for a console Arcen lays out; for one shown as it is (another
    /// product's), as many as a Deck may have while it shows a picture at
    /// all ([`guest_offer`]).
    pub(crate) monitors: usize,
    /// For the log: the outputs it counted.
    pub(crate) outputs: Vec<String>,
}

/// Reads the console's live X server: the `seat0` X11 desktop in front
/// (any account's, a login screen's too), else any on `seat0`.
///
/// # Errors
///
/// When no console X server answers (nothing graphical runs, or its
/// display cannot be opened).
pub(crate) async fn live_console() -> Result<LiveConsole, String> {
    use arcen_session::console_session::{ConsoleOrigin, ARCEN_TTY, CONSOLE_SEAT};
    let sessions = super::console::read_sessions()
        .await
        .map_err(|unreadable| {
            format!(
                "logind's sessions could not be read ({})",
                unreadable.token()
            )
        })?;
    let mut candidates: Vec<_> = sessions
        .into_iter()
        .filter(|fact| {
            fact.seat == CONSOLE_SEAT
                && fact.session_type == "x11"
                && fact.tty != ARCEN_TTY
                && fact.state != "closing"
        })
        .collect();
    candidates.sort_by_key(|fact| (!fact.active, fact.class != "user"));
    let mut last_error = "no X11 session on the console seat".to_owned();
    for fact in candidates {
        let Some(home) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(fact.uid))
            .ok()
            .flatten()
            .map(|user| user.dir)
        else {
            continue;
        };
        let Some(display) = super::console::find_session_display(&fact.id, fact.uid, &home) else {
            last_error = format!("session {} has no display Arcen can open", fact.id);
            continue;
        };
        let origin = if fact.class == "user" {
            fact.origin()
        } else {
            ConsoleOrigin::DisplayManager
        };
        let result = if origin == ConsoleOrigin::OtherProduct {
            read_live(&display, read_shown).await.map(|shown| {
                (
                    guest_offer(shown.len()),
                    shown.into_iter().map(|output| output.name).collect(),
                )
            })
        } else {
            read_live(&display, |connection, root| {
                read_outputs(connection, root).map(|(outputs, _)| outputs)
            })
            .await
            .map(|outputs| {
                (
                    console_monitor_capacity(&outputs),
                    outputs
                        .into_iter()
                        .filter(takes_a_monitor)
                        .map(|output| output.name)
                        .collect(),
                )
            })
        };
        match result {
            Ok((monitors, outputs)) => {
                return Ok(LiveConsole {
                    session_id: fact.id,
                    origin,
                    monitors,
                    outputs,
                })
            }
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// The outputs of joined console session `session_id` (owned by `uid`)
/// that show a picture now, in RandR order, with the refresh each runs at:
/// what a capture of them can deliver.
///
/// # Errors
///
/// When the session's display cannot be found or does not answer.
pub(crate) async fn shown_outputs(session_id: &str, uid: u32) -> Result<Vec<ShownOutput>, String> {
    let home = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|user| user.dir)
        .ok_or_else(|| "the session user has no home".to_owned())?;
    let display = super::console::find_session_display(session_id, uid, &home)
        .ok_or_else(|| format!("session {session_id} has no display Arcen can open"))?;
    read_live(&display, read_shown).await
}

/// [`shown_outputs`] with the size of the X screen they are part of, for a
/// single-display Deck shown one of them (its pointer is placed on the
/// whole screen).
///
/// # Errors
///
/// As [`shown_outputs`].
pub(crate) async fn shown_desktop(
    session_id: &str,
    uid: u32,
) -> Result<(Vec<ShownOutput>, (u32, u32)), String> {
    let home = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|user| user.dir)
        .ok_or_else(|| "the session user has no home".to_owned())?;
    let display = super::console::find_session_display(session_id, uid, &home)
        .ok_or_else(|| format!("session {session_id} has no display Arcen can open"))?;
    read_live(&display, |connection, root| {
        let geometry = connection
            .get_geometry(root)
            .map_err(|error| format!("request the screen size: {error}"))?
            .reply()
            .map_err(|error| format!("screen size: {error}"))?;
        Ok((
            read_shown(connection, root)?,
            (u32::from(geometry.width), u32::from(geometry.height)),
        ))
    })
    .await
}

/// How many Deck monitors a desktop shown as it is (another product's)
/// is offered for, given how many outputs it shows: every Deck layout is
/// mapped onto what it shows (`match_shown_layout`: a Deck monitor without
/// an output gets no window), so the offer never makes a Deck with more
/// displays than the desktop shows refuse Match my layout.
fn guest_offer(shown: usize) -> usize {
    if shown == 0 {
        0
    } else {
        arcen_outputs::console_layout::MAX_VIRTUAL_OUTPUTS
    }
}

fn root_of(connection: &RustConnection) -> Result<xproto::Window, String> {
    connection
        .setup()
        .roots
        .first()
        .map(|screen| screen.root)
        .ok_or_else(|| "the X display has no screen".to_owned())
}

struct OutputFacts {
    id: randr::Output,
    name: String,
    crtc: randr::Crtc,
}

/// The console's outputs in RandR order, with their modes.
fn read_outputs(
    connection: &RustConnection,
    root: xproto::Window,
) -> Result<(Vec<ConsoleOutput>, Vec<OutputFacts>), String> {
    let resources = connection
        .randr_get_screen_resources_current(root)
        .map_err(|error| format!("request RandR resources: {error}"))?
        .reply()
        .map_err(|error| format!("RandR resources: {error}"))?;
    let mut outputs = Vec::new();
    let mut facts = Vec::new();
    for output in &resources.outputs {
        let info = connection
            .randr_get_output_info(*output, resources.config_timestamp)
            .map_err(|error| format!("request RandR output: {error}"))?
            .reply()
            .map_err(|error| format!("RandR output: {error}"))?;
        let name = String::from_utf8_lossy(&info.name).into_owned();
        let modes: Vec<OutputMode> = info
            .modes
            .iter()
            .filter_map(|id| resources.modes.iter().find(|mode| mode.id == *id))
            .map(|mode| OutputMode {
                width: u32::from(mode.width),
                height: u32::from(mode.height),
                refresh_mhz: refresh_mhz(mode),
            })
            .collect();
        let preferred = (info.num_preferred > 0 && !modes.is_empty()).then_some(0);
        outputs.push(ConsoleOutput {
            name: name.clone(),
            connected: info.connection == randr::Connection::CONNECTED,
            modes,
            preferred,
        });
        facts.push(OutputFacts {
            id: *output,
            name,
            crtc: info.crtc,
        });
    }
    Ok((outputs, facts))
}

/// The largest X screen this server can show (`RandR`
/// `GetScreenSizeRange`).
fn screen_maximum(connection: &RustConnection, root: xproto::Window) -> Result<(u32, u32), String> {
    let range = connection
        .randr_get_screen_size_range(root)
        .map_err(|error| format!("request RandR screen range: {error}"))?
        .reply()
        .map_err(|error| format!("RandR screen range: {error}"))?;
    Ok((u32::from(range.max_width), u32::from(range.max_height)))
}

fn refresh_mhz(mode: &randr::ModeInfo) -> u32 {
    let total = u64::from(mode.htotal) * u64::from(mode.vtotal);
    if total == 0 {
        return 0;
    }
    u32::try_from(u64::from(mode.dot_clock) * 1000 / total).unwrap_or(0)
}

fn atom(connection: &RustConnection, name: &[u8]) -> Result<xproto::Atom, String> {
    connection
        .intern_atom(false, name)
        .map_err(|error| format!("request atom: {error}"))?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|error| format!("atom: {error}"))
}

fn set_non_desktop(
    connection: &RustConnection,
    output: randr::Output,
    hidden: bool,
) -> Result<(), String> {
    let property = atom(connection, b"non-desktop")?;
    let value = u32::from(hidden).to_ne_bytes();
    connection
        .randr_change_output_property(
            output,
            property,
            u32::from(xproto::AtomEnum::INTEGER),
            32,
            xproto::PropMode::REPLACE,
            1,
            &value,
        )
        .map_err(|error| format!("request non-desktop: {error}"))?
        .check()
        .map_err(|error| format!("non-desktop: {error}"))
}

fn primary_name(
    connection: &RustConnection,
    root: xproto::Window,
    facts: &[OutputFacts],
) -> Option<String> {
    let primary = connection
        .randr_get_output_primary(root)
        .ok()?
        .reply()
        .ok()?
        .output;
    facts
        .iter()
        .find(|fact| fact.id == primary)
        .map(|fact| fact.name.clone())
}

fn set_primary(
    connection: &RustConnection,
    root: xproto::Window,
    facts: &[OutputFacts],
    name: &str,
) -> Result<(), String> {
    let output = facts
        .iter()
        .find(|fact| fact.name == name)
        .map(|fact| fact.id)
        .ok_or_else(|| format!("output {name} is gone"))?;
    connection
        .randr_set_output_primary(root, output)
        .map_err(|error| format!("request primary: {error}"))?
        .check()
        .map_err(|error| format!("primary: {error}"))
}

fn rotation_bits(rotation: Rotation) -> randr::Rotation {
    // RandR rotates counter-clockwise: a clockwise quarter turn is
    // `ROTATE270`.
    match rotation {
        Rotation::Degrees0 => randr::Rotation::ROTATE0,
        Rotation::Degrees90 => randr::Rotation::ROTATE270,
        Rotation::Degrees180 => randr::Rotation::ROTATE180,
        Rotation::Degrees270 => randr::Rotation::ROTATE90,
    }
}

/// Proves the console shows `plan`: every head's CRTC at its place, size
/// and rotation, and the active outputs in RandR order exactly the plan's
/// heads (capture addresses heads by that position).
fn verify(
    connection: &RustConnection,
    root: xproto::Window,
    plan: &ConsoleLayoutPlan,
) -> Result<(), String> {
    let (_, facts) = read_outputs(connection, root)?;
    let timestamp = connection
        .randr_get_screen_resources_current(root)
        .map_err(|error| error.to_string())?
        .reply()
        .map_err(|error| error.to_string())?
        .config_timestamp;
    let active: Vec<&OutputFacts> = facts.iter().filter(|fact| fact.crtc != 0).collect();
    let active_names: Vec<&str> = active.iter().map(|fact| fact.name.as_str()).collect();
    let planned: Vec<&str> = plan.heads.iter().map(|head| head.output.as_str()).collect();
    if active_names != planned {
        return Err(format!(
            "active outputs {active_names:?}, planned {planned:?}"
        ));
    }
    for (head, fact) in plan.heads.iter().zip(active) {
        let crtc = connection
            .randr_get_crtc_info(fact.crtc, timestamp)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| error.to_string())?;
        let (width, height) = head.screen_extent();
        let rotation = crtc.rotation
            & (randr::Rotation::ROTATE0
                | randr::Rotation::ROTATE90
                | randr::Rotation::ROTATE180
                | randr::Rotation::ROTATE270);
        if i32::from(crtc.x) != head.x
            || i32::from(crtc.y) != head.y
            || u32::from(crtc.width) != width
            || u32::from(crtc.height) != height
            || rotation != rotation_bits(head.rotation)
        {
            return Err(format!(
                "{} shows {}x{}+{}+{} rotation {:?}, planned {width}x{height}+{}+{} {:?}",
                head.output,
                crtc.width,
                crtc.height,
                crtc.x,
                crtc.y,
                crtc.rotation,
                head.x,
                head.y,
                head.rotation
            ));
        }
    }
    Ok(())
}

/// Lays the console out for the Deck: journals what it showed, applies the
/// plan, makes the Deck's primary primary, hides unused forced heads, and
/// proves it. A failure after the change puts the console back.
pub(crate) async fn apply(
    display: &SessionDisplay,
    session_id: &str,
    requests: &[ConsoleHeadRequest],
) -> Result<AppliedLayout, LayoutFailure> {
    let connection = connect(display)
        .map_err(|error| LayoutFailure::new("console_display_unreachable", error))?;
    let root = root_of(&connection)
        .map_err(|error| LayoutFailure::new("console_display_unreachable", error))?;
    let (outputs, facts) = read_outputs(&connection, root)
        .map_err(|error| LayoutFailure::new("console_randr_failed", error))?;
    let plan = plan_console_layout(requests, &outputs)
        .map_err(|error| LayoutFailure::new(error.token(), error.to_string()))?;
    let (max_width, max_height) = screen_maximum(&connection, root)
        .map_err(|error| LayoutFailure::new("console_randr_failed", error))?;
    if !plan.fits_screen(max_width, max_height) {
        let (width, height) = plan.screen_size();
        return Err(LayoutFailure::new(
            "console_screen_too_large",
            format!("the layout needs a {width}x{height} screen; this X server's largest is {max_width}x{max_height}"),
        ));
    }
    // A journal left by an earlier attachment of this console holds the
    // console's own layout; never replace it with Arcen's.
    let mut journal = match LayoutJournal::read() {
        // Its windows settle afresh in this layout.
        Some(journal) if journal.display == *display => LayoutJournal {
            windows_after: None,
            ..journal
        },
        _ => LayoutJournal {
            session_id: session_id.to_owned(),
            display: display.clone(),
            metamode: nv_snapshot(display)
                .await
                .map_err(|error| LayoutFailure::new("console_layout_unavailable", error))?,
            primary: primary_name(&connection, root, &facts),
            hidden: Vec::new(),
            windows_before: read_windows(&connection, root, session_id),
            windows_after: None,
        },
    };
    journal
        .write()
        .map_err(|error| LayoutFailure::new("console_layout_unavailable", error))?;
    let metamode = plan.nvidia_metamode();
    if let Err(error) = nv_assign(display, &metamode).await {
        restore_journal(&journal).await;
        return Err(LayoutFailure::new("console_layout_refused", error));
    }
    let outcome = finish_apply(&connection, root, &plan, &mut journal);
    if let Err(error) = outcome {
        restore_journal(&journal).await;
        return Err(LayoutFailure::new("console_layout_unproven", error));
    }
    tracing::info!(
        target: target::SESSION,
        session_id,
        heads = plan.heads.len(),
        metamode = %metamode,
        primary = plan.primary_output().unwrap_or(""),
        hidden = ?journal.hidden,
        scaled = plan.heads.iter().filter(|head| head.scaled).count(),
        "the console takes the Deck's layout"
    );
    Ok(AppliedLayout { journal, plan })
}

fn finish_apply(
    connection: &RustConnection,
    root: xproto::Window,
    plan: &ConsoleLayoutPlan,
    journal: &mut LayoutJournal,
) -> Result<(), String> {
    let (_, facts) = read_outputs(connection, root)?;
    if let Some(primary) = plan.primary_output() {
        set_primary(connection, root, &facts, primary)?;
    }
    if virtual_outputs_installed() {
        for name in &plan.unused {
            let Some(fact) = facts.iter().find(|fact| &fact.name == name) else {
                continue;
            };
            if !journal.hidden.contains(name) {
                journal.hidden.push(name.clone());
                journal.write()?;
            }
            set_non_desktop(connection, fact.id, true)?;
        }
    }
    connection.flush().map_err(|error| error.to_string())?;
    verify(connection, root, plan)
}

/// Puts the console back as `journal` recorded it, then forgets the
/// journal. `false` keeps it for the next try.
async fn restore_journal(journal: &LayoutJournal) -> bool {
    // Off the active VT NVIDIA applies nothing: keep the journal for the
    // Pier's recovery, which restores once the session is on screen.
    if super::console::session_fact(&journal.session_id)
        .await
        .is_some_and(|fact| fact.state != "closing" && !fact.active)
    {
        tracing::info!(
            target: target::SESSION,
            session_id = %journal.session_id,
            "the console session is not on screen; its own layout is restored when it is"
        );
        return false;
    }
    let connection = match connect(&journal.display) {
        Ok(connection) => connection,
        Err(error) => {
            tracing::warn!(
                target: target::SESSION,
                %error,
                "the console's display is gone; its layout went with it"
            );
            LayoutJournal::remove();
            return true;
        }
    };
    let Ok(root) = root_of(&connection) else {
        LayoutJournal::remove();
        return true;
    };
    // Where the windows are as the Deck leaves, still in its layout.
    let departing = if journal.windows_before.is_empty() {
        Vec::new()
    } else {
        super::console_windows::read(&connection, root).unwrap_or_default()
    };
    if let Ok((_, facts)) = read_outputs(&connection, root) {
        for name in &journal.hidden {
            if let Some(fact) = facts.iter().find(|fact| &fact.name == name) {
                if let Err(error) = set_non_desktop(&connection, fact.id, false) {
                    tracing::warn!(target: target::SESSION, output = %name, %error, "a hidden console output could not be shown again");
                }
            }
        }
    }
    if let Err(error) = nv_assign(&journal.display, &journal.metamode).await {
        tracing::error!(
            target: target::SESSION,
            %error,
            metamode = %journal.metamode,
            "the console's own layout could not be restored; it is retried"
        );
        return false;
    }
    if let (Some(primary), Ok((_, facts))) = (&journal.primary, read_outputs(&connection, root)) {
        if let Err(error) = set_primary(&connection, root, &facts, primary) {
            tracing::warn!(target: target::SESSION, %error, "the console's own primary output could not be restored");
        }
    }
    let _ = connection.flush();
    super::console_windows::restore(
        &connection,
        root,
        &journal.session_id,
        &journal.windows_before,
        journal.windows_after.as_deref(),
        &departing,
    )
    .await;
    LayoutJournal::remove();
    tracing::info!(
        target: target::SESSION,
        session_id = %journal.session_id,
        metamode = %journal.metamode,
        primary = journal.primary.as_deref().unwrap_or(""),
        "the console's own layout is restored"
    );
    true
}

/// How long a desktop that is still starting may take before the layout
/// is applied: GNOME Shell applies its own monitor configuration once while
/// it starts and would undo one applied under it (the reference product
/// that serves a console delays its layout on GNOME for the same reason).
const DESKTOP_START_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long the shell must have been running.
const DESKTOP_SETTLED_AFTER: std::time::Duration = std::time::Duration::from_secs(4);
/// Checks after applying; a layout the starting desktop changed is applied
/// again, at most this many times.
const SETTLE_CHECKS: u32 = 3;

/// Waits (bounded) until the session's GNOME Shell (any process named
/// `gnome-shell` of `uid`) has run for a few seconds. A desktop without
/// one is not waited for.
pub(crate) async fn wait_for_desktop_settled(uid: u32) {
    let started = tokio::time::Instant::now();
    let deadline = started + DESKTOP_START_WAIT;
    loop {
        let age = shell_age(uid);
        match settle_wait(age, started.elapsed()) {
            None => return,
            Some(wait) => tokio::time::sleep(wait).await,
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!(target: target::SESSION, uid, "the desktop shell did not settle in time; laying the console out anyway");
            return;
        }
    }
}

/// How long to wait before looking again, given how long the session's
/// shell has run (`None`: no shell) and how long we have waited; `None`
/// when the desktop may be laid out now. A session without a GNOME Shell
/// after a short grace (another desktop) is not waited for.
fn settle_wait(
    shell_age: Option<std::time::Duration>,
    waited: std::time::Duration,
) -> Option<std::time::Duration> {
    const NO_SHELL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
    match shell_age {
        Some(age) if age >= DESKTOP_SETTLED_AFTER => None,
        Some(age) => Some(DESKTOP_SETTLED_AFTER.saturating_sub(age)),
        None if waited >= NO_SHELL_GRACE => None,
        None => Some(std::time::Duration::from_millis(500)),
    }
}

/// How long the oldest `gnome-shell` of `uid` has run.
fn shell_age(uid: u32) -> Option<std::time::Duration> {
    use std::os::unix::fs::MetadataExt as _;
    // /proc counts process start times in USER_HZ, 100 on Linux.
    let ticks: u64 = 100;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter(|entry| entry.metadata().is_ok_and(|metadata| metadata.uid() == uid))
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("comm"))
                .is_ok_and(|comm| comm.trim() == "gnome-shell")
        })
        .filter_map(|entry| {
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // Field 22 (starttime) counts from the end of the command name.
            let after = stat.rsplit_once(')')?.1;
            let start: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
            #[allow(clippy::cast_precision_loss, reason = "process start ticks")]
            let started = start as f64 / ticks as f64;
            Some(std::time::Duration::from_secs_f64(
                (uptime - started).max(0.0),
            ))
        })
        .max()
}

/// What [`settle_with`] needs from a console: whether it still shows the
/// layout, and applying the layout again.
pub(crate) trait SettleDesk {
    async fn verify(&mut self) -> Result<(), String>;
    async fn reapply(&mut self) -> Result<(), LayoutFailure>;
}

/// Proves a just-applied layout holds over `checks` pauses and applies it
/// again whenever a starting desktop changed it (GNOME Shell applies its own
/// monitor configuration once while it starts). Returns how many times it
/// was applied again; the layout must hold at the end.
pub(crate) async fn settle_with<D: SettleDesk>(
    desk: &mut D,
    checks: u32,
    pause: std::time::Duration,
) -> Result<u32, LayoutFailure> {
    let mut reapplied = 0;
    for _ in 0..checks {
        tokio::time::sleep(pause).await;
        if let Err(drift) = desk.verify().await {
            tracing::warn!(
                target: target::SESSION,
                %drift,
                "the desktop changed the console's layout while it started; applying the Deck's again"
            );
            reapplied += 1;
            desk.reapply().await?;
        }
    }
    tokio::time::sleep(pause).await;
    desk.verify()
        .await
        .map_err(|error| LayoutFailure::new("console_layout_unproven", error))?;
    Ok(reapplied)
}

struct LiveSettleDesk<'a> {
    layout: &'a AppliedLayout,
}

impl LiveSettleDesk<'_> {
    fn connection(&self) -> Result<(RustConnection, xproto::Window), LayoutFailure> {
        let connection = connect(&self.layout.journal.display)
            .map_err(|error| LayoutFailure::new("console_display_unreachable", error))?;
        let root = root_of(&connection)
            .map_err(|error| LayoutFailure::new("console_display_unreachable", error))?;
        Ok((connection, root))
    }
}

impl SettleDesk for LiveSettleDesk<'_> {
    async fn verify(&mut self) -> Result<(), String> {
        let (connection, root) = self.connection().map_err(|failure| failure.detail)?;
        verify(&connection, root, &self.layout.plan)
    }

    async fn reapply(&mut self) -> Result<(), LayoutFailure> {
        nv_assign(
            &self.layout.journal.display,
            &self.layout.plan.nvidia_metamode(),
        )
        .await
        .map_err(|error| LayoutFailure::new("console_layout_refused", error))?;
        let (connection, root) = self.connection()?;
        let (_, facts) = read_outputs(&connection, root)
            .map_err(|error| LayoutFailure::new("console_randr_failed", error))?;
        if let Some(primary) = self.layout.plan.primary_output() {
            set_primary(&connection, root, &facts, primary)
                .map_err(|error| LayoutFailure::new("console_layout_unproven", error))?;
        }
        connection
            .flush()
            .map_err(|error| LayoutFailure::new("console_randr_failed", error.to_string()))
    }
}

impl AppliedLayout {
    /// Proves the layout holds for a few seconds after it was applied, and
    /// applies it again (bounded) when a starting desktop changed it.
    /// Returns how many times it was applied again.
    pub(crate) async fn settle(&self) -> Result<u32, LayoutFailure> {
        settle_with(
            &mut LiveSettleDesk { layout: self },
            SETTLE_CHECKS,
            std::time::Duration::from_secs(1),
        )
        .await
    }
}

/// The session a layout journal left by a launcher that did not restore it
/// belongs to.
pub(crate) fn journalled_session() -> Option<String> {
    LayoutJournal::read().map(|journal| journal.session_id)
}

/// Restores a layout a launcher left applied (it died, or was killed):
/// called by the Pier's console recovery once the session is secure.
pub(crate) async fn recover() -> bool {
    let restored = try_recover(true).await;
    if !restored {
        restore_when_shown();
    }
    restored
}

/// One recovery attempt: `true` once nothing is left to restore. NVIDIA
/// applies no mode while its X server is off the active VT (a login screen
/// in front of the session, as after a killed launcher), so a session that
/// is not on screen keeps its journal and is restored when it is again.
async fn try_recover(announce: bool) -> bool {
    let Some(journal) = LayoutJournal::read() else {
        return true;
    };
    match super::console::session_fact(&journal.session_id).await {
        Some(fact) if fact.state != "closing" && fact.active => {}
        Some(fact) if fact.state != "closing" => {
            if announce {
                tracing::info!(
                    target: target::SESSION,
                    session_id = %journal.session_id,
                    "the console session is not on screen; its own layout is restored when it is"
                );
            }
            return false;
        }
        Some(_) => {
            tracing::info!(
                target: target::SESSION,
                session_id = %journal.session_id,
                "the console session is gone; its layout went with it"
            );
            LayoutJournal::remove();
            return true;
        }
        // logind did not answer: the display decides (restore_journal drops
        // the journal only when it is gone).
        None => {}
    }
    tracing::warn!(
        target: target::SESSION,
        session_id = %journal.session_id,
        "a console was left in a Deck's layout by a launcher that did not restore it; restoring its own"
    );
    restore_journal(&journal).await
}

/// Bumped by every console attachment (Pier side).
static ATTACHMENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The attachment count when a restore was last asked for: while the count
/// differs, an attachment started since and the retry stands aside.
static RESTORE_ARMED_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RESTORE_WAITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A Deck is about to attach to the console (Pier side): a deferred restore
/// stands aside until the Pier asks again when that attachment ends; the
/// attachment keeps the journal's original layout and restores it itself.
pub(crate) fn attachment_starting() {
    ATTACHMENTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Retries the restore every few seconds until the session is on screen
/// (restored) or gone (journal dropped), standing aside while a later
/// attachment has not ended.
fn restore_when_shown() {
    use std::sync::atomic::Ordering;
    RESTORE_ARMED_AT.store(ATTACHMENTS.load(Ordering::SeqCst), Ordering::SeqCst);
    if RESTORE_WAITING.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        RESTORE_WAITING.store(false, Ordering::SeqCst);
        return;
    };
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if LayoutJournal::read().is_none() {
                break;
            }
            if ATTACHMENTS.load(Ordering::SeqCst) != RESTORE_ARMED_AT.load(Ordering::SeqCst) {
                continue;
            }
            if try_recover(false).await {
                break;
            }
        }
        RESTORE_WAITING.store(false, Ordering::SeqCst);
        // A restore asked for while this one was finishing found it busy.
        if LayoutJournal::read().is_some()
            && ATTACHMENTS.load(Ordering::SeqCst) == RESTORE_ARMED_AT.load(Ordering::SeqCst)
        {
            restore_when_shown();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_starting_shell_is_waited_for_and_no_shell_is_not() {
        use std::time::Duration;
        let secs = Duration::from_secs;
        // A shell that started a second ago: wait the rest of its settle time.
        assert_eq!(settle_wait(Some(secs(1)), secs(0)), Some(secs(3)));
        assert_eq!(settle_wait(Some(secs(4)), secs(0)), None);
        // No shell yet: a short grace (it may be about to start), then go.
        assert_eq!(settle_wait(None, secs(1)), Some(Duration::from_millis(500)));
        assert_eq!(settle_wait(None, secs(2)), None);
    }

    /// A console whose desktop changes the layout `drifts` times, the
    /// first `refuse_from`.. re-applies failing.
    struct FakeSettleDesk {
        drifts: u32,
        reapplied: u32,
        refuse: bool,
    }

    impl SettleDesk for FakeSettleDesk {
        async fn verify(&mut self) -> Result<(), String> {
            if self.drifts > 0 {
                Err("DVI-D-1 off".to_owned())
            } else {
                Ok(())
            }
        }

        async fn reapply(&mut self) -> Result<(), LayoutFailure> {
            if self.refuse {
                return Err(LayoutFailure::new("console_layout_refused", "refused"));
            }
            self.reapplied += 1;
            self.drifts = self.drifts.saturating_sub(1);
            Ok(())
        }
    }

    fn settle(desk: &mut FakeSettleDesk) -> Result<u32, LayoutFailure> {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime")
            .block_on(settle_with(desk, 3, std::time::Duration::ZERO))
    }

    #[test]
    fn a_layout_the_starting_shell_changed_once_is_applied_again() {
        let mut desk = FakeSettleDesk {
            drifts: 1,
            reapplied: 0,
            refuse: false,
        };
        assert_eq!(settle(&mut desk).expect("settled"), 1);
        assert_eq!(desk.reapplied, 1);
        let mut steady = FakeSettleDesk {
            drifts: 0,
            reapplied: 0,
            refuse: false,
        };
        assert_eq!(settle(&mut steady).expect("settled"), 0);
    }

    #[test]
    fn a_desktop_that_keeps_changing_it_or_a_refused_reapply_fails() {
        let mut stubborn = FakeSettleDesk {
            drifts: 10,
            reapplied: 0,
            refuse: false,
        };
        let failure = settle(&mut stubborn).expect_err("never settles");
        assert_eq!(failure.token, "console_layout_unproven");
        assert_eq!(stubborn.reapplied, 3, "bounded");
        let mut refused = FakeSettleDesk {
            drifts: 1,
            reapplied: 0,
            refuse: true,
        };
        assert_eq!(
            settle(&mut refused).expect_err("refused").token,
            "console_layout_refused"
        );
    }

    #[test]
    fn rotation_bits_follow_randr_counter_clockwise() {
        assert_eq!(
            rotation_bits(Rotation::Degrees90),
            randr::Rotation::ROTATE270
        );
        assert_eq!(
            rotation_bits(Rotation::Degrees270),
            randr::Rotation::ROTATE90
        );
    }
}
