// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's layout on the joined console (ADR 0025 §6): the Linux adapter.
//!
//! The decision (which output shows which Deck monitor, in which mode) is
//! `arcen_outputs::console_layout`'s. This module reads the console's
//! outputs through RandR, applies the decision as one NVIDIA MetaMode over
//! NV-CONTROL (`nvidia-settings`, as root with the session's cookie, like
//! the privacy code), makes the Deck's primary the RandR primary, hides
//! the forced virtual heads the layout does not use (RandR `non-desktop`)
//! and proves the result. Before it changes anything it journals the
//! console's own MetaMode, primary and output visibility in
//! `/run/arcen/console-layout.json`; the hand-back restores exactly that,
//! and so does the Pier's recovery after a launcher that did not.

use std::path::Path;

use arcen_media::Rotation;
use arcen_outputs::console_layout::{
    plan_console_layout, ConsoleHeadRequest, ConsoleLayoutPlan, ConsoleOutput, OutputMode,
};
use serde::{Deserialize, Serialize};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::console::{connect, read_root_journal, write_root_journal, SessionDisplay};
use crate::display::nvctrl::NvControl;
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
    let nvcontrol = NvControl::new(
        display.display.clone(),
        Some(display.xauthority.to_string_lossy().into_owned()),
    );
    // A journal left by an earlier attachment of this console holds the
    // console's own layout; never replace it with Arcen's.
    let mut journal = match LayoutJournal::read() {
        Some(journal) if journal.display == *display => journal,
        _ => LayoutJournal {
            session_id: session_id.to_owned(),
            display: display.clone(),
            metamode: nvcontrol.snapshot().await.map_err(|error| {
                LayoutFailure::new("console_layout_unavailable", error.to_string())
            })?,
            primary: primary_name(&connection, root, &facts),
            hidden: Vec::new(),
        },
    };
    journal
        .write()
        .map_err(|error| LayoutFailure::new("console_layout_unavailable", error))?;
    let metamode = plan.nvidia_metamode();
    if let Err(error) = nvcontrol.assign(&metamode).await {
        restore_journal(&journal).await;
        return Err(LayoutFailure::new(
            "console_layout_refused",
            error.to_string(),
        ));
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
    if let Ok((_, facts)) = read_outputs(&connection, root) {
        for name in &journal.hidden {
            if let Some(fact) = facts.iter().find(|fact| &fact.name == name) {
                if let Err(error) = set_non_desktop(&connection, fact.id, false) {
                    tracing::warn!(target: target::SESSION, output = %name, %error, "a hidden console output could not be shown again");
                }
            }
        }
    }
    let nvcontrol = NvControl::new(
        journal.display.display.clone(),
        Some(journal.display.xauthority.to_string_lossy().into_owned()),
    );
    if let Err(error) = nvcontrol.assign(&journal.metamode).await {
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
        NvControl::new(
            self.layout.journal.display.display.clone(),
            Some(
                self.layout
                    .journal
                    .display
                    .xauthority
                    .to_string_lossy()
                    .into_owned(),
            ),
        )
        .assign(&self.layout.plan.nvidia_metamode())
        .await
        .map_err(|error| LayoutFailure::new("console_layout_refused", error.to_string()))?;
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
