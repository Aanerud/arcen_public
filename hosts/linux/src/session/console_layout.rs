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
//!
//! Another product's desktop laid out to the Deck (shared
//! `console_session::guest_layout`, ADR 0025 §8) is journalled the same
//! way, and once the layout is proved and settled its journal says the
//! Deck's layout stays (`LayoutDeparture::LeaveDecks`; until then it says
//! the console's own comes back): the hand-back and the recovery keep the
//! layout and move the journal to `/run/arcen/console-layout-before-deck.json`
//! as the record of what the desktop showed before. A layout that fails or
//! cannot be proved is still put back. Such a desktop is never scaled
//! (shared `HeadScaling::NativeOnly`): a size an output does not list is
//! added as a mode of that size (`RRCreateMode`, `RRAddOutputMode`, kept on
//! the X server as the product keeps its own), every head states an
//! identity transform, and the proof reads each CRTC's transform back.

use std::path::Path;

use arcen_media::Rotation;
use arcen_outputs::console_layout::{
    console_monitor_capacity, match_shown_layout, missing_native_modes, plan_console_layout,
    plan_native_console_layout, shown_size, takes_a_monitor, ConsoleHeadRequest,
    ConsoleLayoutError, ConsoleLayoutPlan, ConsoleOutput, HeadScaling, ModeTiming, NativeModeNeed,
    OutputMode, ShownLayoutMatch, ShownOutput,
};
use arcen_outputs::display_truth::{
    restore_differences, ColourMode, DisplayTruth, DisplayTruthReason, ModeTruth, OutputTruth,
    RestoreDifference,
};
use arcen_session::console_session::guest_layout::LayoutDeparture;
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::console::{connect, read_root_journal, write_root_journal, SessionDisplay};
use crate::display::nv_control;
use crate::logging::target;

const LAYOUT_JOURNAL: &str = "/run/arcen/console-layout.json";
/// What another product's desktop showed before the Deck's layout, which
/// stayed when the Deck left: a record for the administrator, never
/// restored.
const LEFT_LAYOUT_RECORD: &str = "/run/arcen/console-layout-before-deck.json";

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
    /// What happens to the layout when the Deck leaves; absent (an older
    /// journal) restores the console's own.
    #[serde(default)]
    departure: LayoutDeparture,
    /// Every output as RandR read it before the layout (shared
    /// `display_truth`): what the restore is proved against. Empty in an
    /// older journal, or when it could not be read; the restore is then not
    /// proved.
    #[serde(default)]
    before: Vec<OutputRecord>,
    /// Set when a restore put the MetaMode back but the outputs still read
    /// back differently from `before`: the journal stays as the record of the
    /// console's own displays, the next layout starts from it rather than
    /// from what was left, and nothing retries it in a loop.
    #[serde(default)]
    restore_incomplete: Option<String>,
}

/// One output of [`LayoutJournal::before`]: the shared `OutputTruth` as the
/// journal stores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OutputRecord {
    id: String,
    active: bool,
    primary: bool,
    x: i32,
    y: i32,
    /// Width, height and refresh in millihertz.
    mode: Option<(u32, u32, u32)>,
    rotation_degrees: u16,
    edid: Option<Vec<u8>>,
    bits_per_channel: Option<u8>,
}

impl OutputRecord {
    fn of(output: &OutputTruth) -> Self {
        Self {
            id: output.id.clone(),
            active: output.active,
            primary: output.primary,
            x: output.x,
            y: output.y,
            mode: output
                .mode
                .map(|mode| (mode.width, mode.height, mode.refresh_mhz)),
            rotation_degrees: match output.rotation {
                Rotation::Degrees0 => 0,
                Rotation::Degrees90 => 90,
                Rotation::Degrees180 => 180,
                Rotation::Degrees270 => 270,
            },
            edid: output.edid.clone(),
            bits_per_channel: output.bits_per_channel,
        }
    }

    fn truth(&self) -> OutputTruth {
        OutputTruth {
            active: self.active,
            primary: self.primary,
            x: self.x,
            y: self.y,
            mode: self.mode.map(|(width, height, refresh_mhz)| ModeTruth {
                width,
                height,
                refresh_mhz,
            }),
            rotation: match self.rotation_degrees {
                90 => Rotation::Degrees90,
                180 => Rotation::Degrees180,
                270 => Rotation::Degrees270,
                _ => Rotation::Degrees0,
            },
            edid: self.edid.clone(),
            colour: if self.active {
                ColourMode::Sdr
            } else {
                ColourMode::Unknown
            },
            bits_per_channel: self.bits_per_channel,
            ..OutputTruth::new(self.id.clone(), self.id.clone())
        }
    }
}

fn records(truth: &DisplayTruth) -> Vec<OutputRecord> {
    truth.outputs.iter().map(OutputRecord::of).collect()
}

fn truth_of(records: &[OutputRecord]) -> DisplayTruth {
    DisplayTruth {
        source: TRUTH_SOURCE,
        outputs: records.iter().map(OutputRecord::truth).collect(),
    }
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

    /// Whether a restore is still owed. A journal whose restore was
    /// incomplete is a record: the next layout of the console starts from
    /// it, and nothing retries it in a loop.
    fn pending(&self) -> bool {
        self.restore_incomplete.is_none()
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
    /// What the layout's departure is once it is proved and settled
    /// ([`Self::note_proven`]); until then the journal restores the
    /// console's own, so a layout that fails, or a launcher that dies
    /// before it is proved, leaves the Pier's recovery to put it back.
    departure: LayoutDeparture,
}

impl AppliedLayout {
    /// The applied heads, in the order capture addresses them.
    pub(crate) fn plan(&self) -> &ConsoleLayoutPlan {
        &self.plan
    }

    /// Restores the journalled layout; `false` leaves the journal for the
    /// Pier's recovery. Puts the console back whatever its departure says:
    /// for a layout that failed or could not be proved.
    pub(crate) async fn restore(mut self) -> bool {
        // A restore that fails keeps the journal for the Pier's recovery,
        // which must then put it back too, not leave it.
        if prove_departure(&mut self.journal, LayoutDeparture::RestoreOwn) {
            if let Err(error) = self.journal.write() {
                tracing::warn!(target: target::SESSION, %error, "the console layout journal could not be marked for restore");
            }
        }
        restore_journal(&self.journal).await
    }

    /// The layout is proved and settled: from now on the journal says what
    /// happens when the Deck leaves (another product's desktop keeps the
    /// Deck's layout).
    pub(crate) fn note_proven(&mut self) {
        if prove_departure(&mut self.journal, self.departure) {
            if let Err(error) = self.journal.write() {
                tracing::warn!(target: target::SESSION, %error, "the console layout journal could not record its departure; the Pier's recovery would put the console's own layout back");
            }
        }
    }

    /// Whether the Deck's layout stays when the Deck leaves.
    pub(crate) fn leaves_decks(&self) -> bool {
        self.journal.departure == LayoutDeparture::LeaveDecks
    }

    /// The Deck leaves: the console's own layout is restored, or, for a
    /// desktop whose journal keeps the Deck's layout, the layout stays and
    /// the journal becomes the record. `false` leaves the journal for the
    /// Pier's recovery.
    pub(crate) async fn depart(self) -> bool {
        if self.leaves_decks() {
            leave_journal(&self.journal);
            return true;
        }
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
        let rotation = crtc_rotation(crtc.rotation);
        let (width, height) = shown_size((u32::from(crtc.width), u32::from(crtc.height)), rotation);
        if (width, height) != (u32::from(mode.width), u32::from(mode.height)) {
            tracing::warn!(
                target: target::SESSION,
                output = %fact.name,
                mode = %format!("{}x{}", mode.width, mode.height),
                shows = %format!("{width}x{height}"),
                "an output of this desktop is scaled (a RandR transform): it is captured as the area it shows, not its mode"
            );
        }
        shown.push(ShownOutput {
            name: fact.name.clone(),
            width,
            height,
            x: i32::from(crtc.x),
            y: i32::from(crtc.y),
            rotation,
            refresh_mhz: refresh_mhz(mode),
            primary: primary.as_deref() == Some(fact.name.as_str()),
        });
    }
    Ok(shown)
}

/// A CRTC's rotation as the Deck's picture turns (`RandR` counts the
/// other way).
fn crtc_rotation(rotation: randr::Rotation) -> Rotation {
    if rotation.contains(randr::Rotation::ROTATE270) {
        Rotation::Degrees90
    } else if rotation.contains(randr::Rotation::ROTATE180) {
        Rotation::Degrees180
    } else if rotation.contains(randr::Rotation::ROTATE90) {
        Rotation::Degrees270
    } else {
        Rotation::Degrees0
    }
}

/// The shared display truth's source name for an Xorg console.
const TRUTH_SOURCE: &str = "xorg-randr";

/// Every output of the console's X server that is connected or shows a
/// picture, as `RandR` reads it now (shared `display_truth`): name, place,
/// mode and refresh, rotation, primary, EDID, and the screen's bits per
/// channel. Xorg composes SDR only, so an active output reads `sdr` and no
/// output has an HDR switch.
fn read_truth(connection: &RustConnection, root: xproto::Window) -> Result<DisplayTruth, String> {
    let resources = connection
        .randr_get_screen_resources_current(root)
        .map_err(|error| format!("request RandR resources: {error}"))?
        .reply()
        .map_err(|error| format!("RandR resources: {error}"))?;
    let (_, facts) = read_outputs(connection, root)?;
    let primary = primary_name(connection, root, &facts);
    let edid_atom = atom(connection, b"EDID")?;
    let bits_per_channel = connection
        .setup()
        .roots
        .iter()
        .find(|screen| screen.root == root)
        .and_then(|screen| match screen.root_depth {
            24 => Some(8),
            30 => Some(10),
            _ => None,
        });
    let mut truth = DisplayTruth::new(TRUTH_SOURCE);
    for fact in &facts {
        let info = connection
            .randr_get_output_info(fact.id, resources.config_timestamp)
            .map_err(|error| format!("request RandR output: {error}"))?
            .reply()
            .map_err(|error| format!("RandR output: {error}"))?;
        if info.connection != randr::Connection::CONNECTED && fact.crtc == 0 {
            continue;
        }
        let mut output = OutputTruth::new(fact.name.clone(), fact.name.clone());
        output.edid = connection
            .randr_get_output_property(
                fact.id,
                edid_atom,
                xproto::AtomEnum::ANY,
                0,
                256,
                false,
                false,
            )
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|reply| reply.data)
            .filter(|edid| !edid.is_empty());
        if fact.crtc != 0 {
            let crtc = connection
                .randr_get_crtc_info(fact.crtc, resources.config_timestamp)
                .map_err(|error| format!("request RandR CRTC: {error}"))?
                .reply()
                .map_err(|error| format!("RandR CRTC: {error}"))?;
            if let Some(mode) = resources.modes.iter().find(|mode| mode.id == crtc.mode) {
                output.active = true;
                output.primary = primary.as_deref() == Some(fact.name.as_str());
                output.x = i32::from(crtc.x);
                output.y = i32::from(crtc.y);
                output.mode = Some(ModeTruth {
                    width: u32::from(mode.width),
                    height: u32::from(mode.height),
                    refresh_mhz: refresh_mhz(mode),
                });
                output.rotation = crtc_rotation(crtc.rotation);
                output.colour = ColourMode::Sdr;
                output.bits_per_channel = bits_per_channel;
            }
        }
        truth.outputs.push(output);
    }
    Ok(truth)
}

/// The console's displays as `RandR` reads them, for `arcen-pier diagnose
/// displays`: the `seat0` X11 desktop in front, as [`live_console`] finds
/// it.
///
/// # Errors
///
/// When no console X server answers.
pub(crate) async fn console_truth() -> Result<DisplayTruth, String> {
    let mut last_error = "no X11 session on the console seat".to_owned();
    for (fact, display) in console_candidates().await? {
        match read_live(&display, read_truth).await {
            Ok(truth) => return Ok(truth),
            Err(error) => last_error = format!("session {}: {error}", fact.id),
        }
    }
    Err(last_error)
}

/// How long the outputs may take to read back as they were after a restore.
const RESTORE_SETTLE: std::time::Duration = std::time::Duration::from_secs(3);

/// Reads the outputs after a restore until they equal `before`, for at most
/// [`RESTORE_SETTLE`]; the provable differences left, or the read error.
async fn restore_left(
    display: &SessionDisplay,
    before: &DisplayTruth,
) -> Result<Vec<RestoreDifference>, String> {
    let deadline = tokio::time::Instant::now() + RESTORE_SETTLE;
    loop {
        let differences = read_live(display, read_truth)
            .await
            .map(|after| restore_differences(before, &after))?;
        let provable = differences
            .iter()
            .filter(|difference| difference.provable)
            .cloned()
            .collect::<Vec<_>>();
        if provable.is_empty() || tokio::time::Instant::now() >= deadline {
            return Ok(provable);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
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

/// The `seat0` X11 desktops Arcen can open, the one in front first (any
/// account's, a login screen's too), each with its display.
///
/// # Errors
///
/// When logind's sessions cannot be read, or none of them can be opened.
async fn console_candidates() -> Result<
    Vec<(
        arcen_session::console_session::ConsoleSessionFact,
        SessionDisplay,
    )>,
    String,
> {
    use arcen_session::console_session::{ARCEN_TTY, CONSOLE_SEAT};
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
    let mut found = Vec::new();
    for fact in candidates {
        let Some(home) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(fact.uid))
            .ok()
            .flatten()
            .map(|user| user.dir)
        else {
            continue;
        };
        match super::console::find_session_display(&fact.id, fact.uid, &home) {
            Some(display) => found.push((fact, display)),
            None => last_error = format!("session {} has no display Arcen can open", fact.id),
        }
    }
    if found.is_empty() {
        return Err(last_error);
    }
    Ok(found)
}

/// Reads the console's live X server: the `seat0` X11 desktop in front
/// (any account's, a login screen's too), else any on `seat0`.
///
/// # Errors
///
/// When no console X server answers (nothing graphical runs, or its
/// display cannot be opened).
pub(crate) async fn live_console() -> Result<LiveConsole, String> {
    use arcen_session::console_session::ConsoleOrigin;
    let mut last_error = "no X11 session on the console seat".to_owned();
    for (fact, display) in console_candidates().await? {
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

/// Each mode's name, in the order of `resources.modes` (the names are
/// packed one after another, each `name_len` bytes).
fn mode_names(resources: &randr::GetScreenResourcesCurrentReply) -> Vec<String> {
    let mut names = resources.names.as_slice();
    resources
        .modes
        .iter()
        .map(|mode| {
            let length = usize::from(mode.name_len).min(names.len());
            let (name, rest) = names.split_at(length);
            names = rest;
            String::from_utf8_lossy(name).into_owned()
        })
        .collect()
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
    let names = mode_names(&resources);
    let mut outputs = Vec::new();
    let mut facts = Vec::new();
    for output in &resources.outputs {
        let info = connection
            .randr_get_output_info(*output, resources.config_timestamp)
            .map_err(|error| format!("request RandR output: {error}"))?
            .reply()
            .map_err(|error| format!("RandR output: {error}"))?;
        let name = String::from_utf8_lossy(&info.name).into_owned();
        let (modes, mode_names): (Vec<OutputMode>, Vec<String>) = info
            .modes
            .iter()
            .filter_map(|id| {
                let index = resources.modes.iter().position(|mode| mode.id == *id)?;
                Some((&resources.modes[index], names[index].clone()))
            })
            .map(|(mode, name)| {
                (
                    OutputMode {
                        width: u32::from(mode.width),
                        height: u32::from(mode.height),
                        refresh_mhz: refresh_mhz(mode),
                    },
                    name,
                )
            })
            .unzip();
        let preferred = (info.num_preferred > 0 && !modes.is_empty()).then_some(0);
        outputs.push(ConsoleOutput {
            name: name.clone(),
            connected: info.connection == randr::Connection::CONNECTED,
            modes,
            mode_names,
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
        if plan.scaling == HeadScaling::NativeOnly {
            let transform = connection
                .randr_get_crtc_transform(fact.crtc)
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?
                .current_transform;
            if !is_identity(&transform) {
                return Err(format!(
                    "{} keeps a scaling transform ({:?}); another product's desktop is never scaled",
                    head.output, transform
                ));
            }
        }
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

/// Whether a CRTC's `RandR` transform is the identity (16.16 fixed point).
fn is_identity(transform: &x11rb::protocol::render::Transform) -> bool {
    const ONE: i32 = 1 << 16;
    [
        transform.matrix11,
        transform.matrix12,
        transform.matrix13,
        transform.matrix21,
        transform.matrix22,
        transform.matrix23,
        transform.matrix31,
        transform.matrix32,
        transform.matrix33,
    ] == [ONE, 0, 0, 0, ONE, 0, 0, 0, ONE]
}

/// How a console's heads may show the Deck's monitors: another product's
/// desktop, whose Deck layout stays, only in modes of their own sizes
/// (ADR 0025 §8); a console Arcen puts back may scale (§6).
fn head_scaling(departure: LayoutDeparture) -> HeadScaling {
    match departure {
        LayoutDeparture::LeaveDecks => HeadScaling::NativeOnly,
        LayoutDeparture::RestoreOwn => HeadScaling::ViewPortIn,
    }
}

/// The `RandR` mode for `timing` under its own name.
fn mode_info(timing: &ModeTiming, name: &str) -> Option<randr::ModeInfo> {
    let short = |value: u32| u16::try_from(value).ok();
    Some(randr::ModeInfo {
        id: 0,
        width: short(timing.width)?,
        height: short(timing.height)?,
        dot_clock: timing.dot_clock_khz.checked_mul(1000)?,
        hsync_start: short(timing.hsync_start)?,
        hsync_end: short(timing.hsync_end)?,
        htotal: short(timing.htotal)?,
        hskew: 0,
        vsync_start: short(timing.vsync_start)?,
        vsync_end: short(timing.vsync_end)?,
        vtotal: short(timing.vtotal)?,
        name_len: u16::try_from(name.len()).ok()?,
        mode_flags: randr::ModeFlag::HSYNC_POSITIVE | randr::ModeFlag::VSYNC_NEGATIVE,
    })
}

/// Adds each needed mode to its output (`RRCreateMode`, or the mode of
/// that name an earlier attachment made, then `RRAddOutputMode`). The
/// driver may still refuse to list one; the caller plans again from what
/// the outputs list now. Returns what could not be added.
fn add_native_modes(
    connection: &RustConnection,
    root: xproto::Window,
    facts: &[OutputFacts],
    needs: &[NativeModeNeed],
) -> Vec<String> {
    let mut failures = Vec::new();
    let mut existing = match connection
        .randr_get_screen_resources_current(root)
        .map_err(|error| error.to_string())
        .and_then(|cookie| cookie.reply().map_err(|error| error.to_string()))
    {
        Ok(resources) => mode_names(&resources)
            .into_iter()
            .zip(resources.modes.iter().map(|mode| mode.id))
            .collect::<Vec<_>>(),
        Err(error) => return vec![format!("RandR resources: {error}")],
    };
    for need in needs {
        let name = need.timing.name();
        let Some(output) = facts.iter().find(|fact| fact.name == need.output) else {
            failures.push(format!("{}: output gone", need.output));
            continue;
        };
        let mode = match existing.iter().find(|(known, _)| *known == name) {
            Some((_, id)) => *id,
            None => {
                let Some(info) = mode_info(&need.timing, &name) else {
                    failures.push(format!("{name}: timing out of range"));
                    continue;
                };
                match connection
                    .randr_create_mode(root, info, name.as_bytes())
                    .map_err(|error| error.to_string())
                    .and_then(|cookie| cookie.reply().map_err(|error| error.to_string()))
                {
                    // Two outputs may need the same mode: the second finds
                    // it (RandR refuses a second mode of one name).
                    Ok(reply) => {
                        existing.push((name.clone(), reply.mode));
                        reply.mode
                    }
                    Err(error) => {
                        failures.push(format!("create {name}: {error}"));
                        continue;
                    }
                }
            }
        };
        match connection
            .randr_add_output_mode(output.id, mode)
            .map_err(|error| error.to_string())
            .and_then(|cookie| cookie.check().map_err(|error| error.to_string()))
        {
            Ok(()) => tracing::info!(
                target: target::SESSION,
                output = %need.output,
                mode = %name,
                dot_clock_khz = need.timing.dot_clock_khz,
                "a mode of the Deck monitor's own size is added to another product's output"
            ),
            Err(error) => failures.push(format!("add {name} to {}: {error}", need.output)),
        }
    }
    failures
}

/// Plans `requests` on the console's outputs as `scaling` allows; for a
/// native layout, adds the modes of the Deck monitors' sizes the outputs
/// lack and plans again from what they list then.
fn plan_on(
    connection: &RustConnection,
    root: xproto::Window,
    requests: &[ConsoleHeadRequest],
    scaling: HeadScaling,
) -> Result<(ConsoleLayoutPlan, Vec<OutputFacts>), LayoutFailure> {
    let read = || {
        read_outputs(connection, root)
            .map_err(|error| LayoutFailure::new("console_randr_failed", error))
    };
    let (outputs, facts) = read()?;
    let planned = match scaling {
        HeadScaling::ViewPortIn => plan_console_layout(requests, &outputs),
        HeadScaling::NativeOnly => plan_native_console_layout(requests, &outputs),
    };
    let failure = |error: ConsoleLayoutError| LayoutFailure::new(error.token(), error.to_string());
    match planned {
        Ok(plan) => Ok((plan, facts)),
        Err(ConsoleLayoutError::NoNativeMode { .. }) => {
            let refused = add_native_modes(
                connection,
                root,
                &facts,
                &missing_native_modes(requests, &outputs),
            );
            let (outputs, facts) = read()?;
            match plan_native_console_layout(requests, &outputs) {
                Ok(plan) => Ok((plan, facts)),
                Err(error) => {
                    let mut failure = failure(error);
                    if !refused.is_empty() {
                        failure.detail = format!("{}; {}", failure.detail, refused.join("; "));
                    }
                    Err(failure)
                }
            }
        }
        Err(error) => Err(failure(error)),
    }
}

/// Lays the console out for the Deck: journals what it showed, applies the
/// plan, makes the Deck's primary primary, hides unused forced heads, and
/// proves it. A failure after the change puts the console back.
pub(crate) async fn apply(
    display: &SessionDisplay,
    session_id: &str,
    requests: &[ConsoleHeadRequest],
    departure: LayoutDeparture,
) -> Result<AppliedLayout, LayoutFailure> {
    let connection = connect(display)
        .map_err(|error| LayoutFailure::new("console_display_unreachable", error))?;
    let root = root_of(&connection)
        .map_err(|error| LayoutFailure::new("console_display_unreachable", error))?;
    let (plan, facts) = plan_on(&connection, root, requests, head_scaling(departure))?;
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
    // That includes one whose restore left the outputs different from
    // before (`restore_incomplete`): the layout starts from the console's
    // own, never from what was left.
    let mut journal = match LayoutJournal::read() {
        // Its windows settle afresh in this layout.
        Some(journal) if journal.display == *display => {
            if let Some(left) = &journal.restore_incomplete {
                tracing::warn!(
                    target: target::SESSION,
                    reason_code = DisplayTruthReason::RestoreIncomplete.code(),
                    left = %left,
                    "an earlier restore left the console's outputs different; this layout is restored to the console's own as journalled before it"
                );
            }
            LayoutJournal {
                windows_after: None,
                departure: LayoutDeparture::RestoreOwn,
                restore_incomplete: None,
                ..journal
            }
        }
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
            departure: LayoutDeparture::RestoreOwn,
            before: read_truth(&connection, root).map_or_else(
                |error| {
                    tracing::warn!(
                        target: target::SESSION,
                        %error,
                        "the console's outputs could not be read before the layout; its restore cannot be proved"
                    );
                    Vec::new()
                },
                |truth| {
                    for output in truth.to_text().lines().skip(1) {
                        tracing::info!(target: target::SESSION, output, "the console's outputs before the layout");
                    }
                    records(&truth)
                },
            ),
            restore_incomplete: None,
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
        before = %journal.metamode,
        departure = ?departure,
        "the console takes the Deck's layout"
    );
    Ok(AppliedLayout {
        journal,
        plan,
        departure,
    })
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
    // Only Arcen's own forced heads are hidden: another product's outputs
    // keep their properties (the MetaMode already switched them off), as
    // its desktop keeps the Deck's layout after the Deck.
    if virtual_outputs_installed() && plan.scaling == HeadScaling::ViewPortIn {
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

/// Sets `journal`'s departure; whether it changed (and is to be written).
fn prove_departure(journal: &mut LayoutJournal, departure: LayoutDeparture) -> bool {
    let changed = journal.departure != departure;
    journal.departure = departure;
    changed
}

/// The Deck's layout stays on a console whose journal says so: the journal
/// becomes the record of the layout before it ([`LEFT_LAYOUT_RECORD`]),
/// and nothing of the console is changed.
fn leave_journal(journal: &LayoutJournal) {
    if let Err(error) = std::fs::rename(LAYOUT_JOURNAL, LEFT_LAYOUT_RECORD) {
        tracing::warn!(
            target: target::SESSION,
            %error,
            "the layout record could not be kept; the journal is dropped"
        );
        LayoutJournal::remove();
    }
    tracing::info!(
        target: target::SESSION,
        session_id = %journal.session_id,
        before = %journal.metamode,
        primary_before = journal.primary.as_deref().unwrap_or(""),
        record = LEFT_LAYOUT_RECORD,
        "the Deck's layout stays on another product's desktop, as that product leaves its own; the layout before it is recorded"
    );
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
    if !journal.before.is_empty() {
        let left = restore_left(&journal.display, &truth_of(&journal.before)).await;
        if let Some(left) = restore_incomplete(&left) {
            tracing::error!(
                target: target::SESSION,
                session_id = %journal.session_id,
                reason_code = DisplayTruthReason::RestoreIncomplete.code(),
                left = %left,
                "the console's outputs do not read back as before the layout; the journal is kept as the record of its own"
            );
            for difference in left.split("; ") {
                tracing::error!(target: target::SESSION, difference, "console output restore difference");
            }
            let kept = LayoutJournal {
                restore_incomplete: Some(left),
                ..journal.clone()
            };
            if let Err(error) = kept.write() {
                tracing::warn!(target: target::SESSION, %error, "the console layout journal could not record the incomplete restore");
            }
            return true;
        }
    }
    LayoutJournal::remove();
    tracing::info!(
        target: target::SESSION,
        session_id = %journal.session_id,
        metamode = %journal.metamode,
        primary = journal.primary.as_deref().unwrap_or(""),
        proved = !journal.before.is_empty(),
        "the console's own layout is restored"
    );
    true
}

/// What a restore left different, as the journal records it; `None` when
/// the outputs read back as before.
fn restore_incomplete(left: &Result<Vec<RestoreDifference>, String>) -> Option<String> {
    match left {
        Ok(differences) if differences.is_empty() => None,
        Ok(differences) => Some(
            differences
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ),
        Err(error) => Some(format!("the outputs could not be read: {error}")),
    }
}

/// How long a desktop that is still starting may take before the layout
/// is applied: GNOME Shell applies its own monitor configuration once while
/// it starts and would undo one applied under it.
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
    LayoutJournal::read()
        .filter(LayoutJournal::pending)
        .map(|journal| journal.session_id)
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
    let Some(journal) = LayoutJournal::read().filter(LayoutJournal::pending) else {
        return true;
    };
    // A launcher that died leaves a proved Deck layout as any departure
    // does; one not yet proved is journalled to be put back.
    if journal.departure == LayoutDeparture::LeaveDecks {
        leave_journal(&journal);
        return true;
    }
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
            if !LayoutJournal::read().is_some_and(|journal| journal.pending()) {
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
        if LayoutJournal::read().is_some_and(|journal| journal.pending())
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
    fn a_journal_says_whether_the_decks_layout_stays_and_an_older_one_restores() {
        let older = r#"{"session_id":"7","display":{"display":":100","number":100,"xauthority":"/run/user/1000/xauth"},"metamode":"DPY-0: 3840x2160 @3840x2160 +0+0","primary":"DP-0","hidden":[]}"#;
        let journal: LayoutJournal = serde_json::from_str(older).expect("older journal");
        assert_eq!(journal.departure, LayoutDeparture::RestoreOwn);
        // Applied but not yet proved: a recovery would put it back.
        let mut applied = AppliedLayout {
            journal,
            plan: ConsoleLayoutPlan {
                heads: Vec::new(),
                unused: Vec::new(),
                scaling: HeadScaling::NativeOnly,
            },
            departure: LayoutDeparture::LeaveDecks,
        };
        assert!(!applied.leaves_decks());
        assert!(prove_departure(
            &mut applied.journal,
            LayoutDeparture::LeaveDecks
        ));
        assert!(applied.leaves_decks());
        assert!(!prove_departure(
            &mut applied.journal,
            LayoutDeparture::LeaveDecks
        ));
        let written = serde_json::to_string(&applied.journal).expect("journal");
        let read: LayoutJournal = serde_json::from_str(&written).expect("read back");
        assert_eq!(read.departure, LayoutDeparture::LeaveDecks);
    }

    // The audit finding: the restore put the MetaMode back and removed the
    // journal without reading the outputs again, so a refresh, rotation or
    // primary left different went unseen and the next layout started from
    // it.
    #[test]
    fn a_restore_is_proved_against_the_journalled_outputs_and_an_incomplete_one_is_kept() {
        let mut output = OutputTruth::new("DP-0", "DP-0");
        output.active = true;
        output.primary = true;
        output.x = 1920;
        output.mode = Some(ModeTruth::at_hz(2560, 1440, 60));
        output.rotation = Rotation::Degrees90;
        output.edid = Some(vec![0, 0xff, 0xff]);
        output.colour = ColourMode::Sdr;
        output.bits_per_channel = Some(10);
        let mut before = DisplayTruth::new(TRUTH_SOURCE);
        before.outputs = vec![output, OutputTruth::new("DP-1", "DP-1")];
        let journalled = truth_of(&records(&before));
        assert!(restore_differences(&before, &journalled).is_empty());

        let mut after = before.clone();
        after.outputs[0].mode = Some(ModeTruth::at_hz(2560, 1440, 30));
        let left = restore_incomplete(&Ok(restore_differences(&journalled, &after)));
        assert_eq!(
            left.as_deref(),
            Some("DP-0 mode: 2560x1440@60000mHz -> 2560x1440@30000mHz")
        );
        assert_eq!(restore_incomplete(&Ok(Vec::new())), None);
        assert!(restore_incomplete(&Err("gone".into())).is_some());

        let older = r#"{"session_id":"7","display":{"display":":100","number":100,"xauthority":"/run/user/1000/xauth"},"metamode":"","primary":null,"hidden":[]}"#;
        let mut journal: LayoutJournal = serde_json::from_str(older).expect("older journal");
        assert!(journal.before.is_empty() && journal.pending());
        journal.restore_incomplete = left;
        assert!(!journal.pending(), "a record is not retried in a loop");
    }

    #[test]
    fn a_crtc_left_scaled_is_not_proved_native_and_added_modes_carry_their_timing() {
        use x11rb::protocol::render::Transform;
        let identity = Transform {
            matrix11: 1 << 16,
            matrix12: 0,
            matrix13: 0,
            matrix21: 0,
            matrix22: 1 << 16,
            matrix23: 0,
            matrix31: 0,
            matrix32: 0,
            matrix33: 1 << 16,
        };
        assert!(is_identity(&identity));
        // The field's stale transform: 0.666656 by 0.740738.
        let left_scaled = Transform {
            matrix11: 43_690,
            matrix22: 48_545,
            ..identity
        };
        assert!(!is_identity(&left_scaled));
        assert_eq!(
            head_scaling(LayoutDeparture::LeaveDecks),
            HeadScaling::NativeOnly
        );
        assert_eq!(
            head_scaling(LayoutDeparture::RestoreOwn),
            HeadScaling::ViewPortIn
        );
        let timing =
            arcen_outputs::console_layout::cvt_reduced_blanking(2560, 1600, 60).expect("timing");
        let name = timing.name();
        let info = mode_info(&timing, &name).expect("mode");
        assert_eq!(
            (info.width, info.height, info.dot_clock),
            (2560, 1600, 268_500_000)
        );
        assert_eq!((info.htotal, info.vtotal), (2720, 1646));
        assert_eq!(usize::from(info.name_len), name.len());
    }

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
