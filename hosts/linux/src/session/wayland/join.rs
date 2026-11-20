// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The account's side of a joined GNOME Wayland session (ADR 0025 §9, S4b).
//!
//! Runs as the session's account, in the session agent of a console
//! attachment, against the session's own user bus
//! (`/run/user/<uid>/bus`): it starts no bus, no compositor and no
//! `PipeWire`, and it changes nothing of the session's monitors. Before it
//! uses the bus it checks again, from inside, that the one connection
//! owning Mutter's names is the compositor the launcher proved (by the
//! bus's own report of its process and account). Then it records the
//! session's enabled monitors on a `ScreenCast` session paired with a
//! `RemoteDesktop` session (the shared [`MutterSessions`]), proves the
//! input target, proves the clipboard on its own `RemoteDesktop` session,
//! and serves input and clipboard on sockets in the account's runtime
//! directory, exactly as for a desktop Arcen created. Every decision is
//! `arcen_session::console_session::wayland_attach`'s.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arcen_session::console_session::wayland_attach::{
    joined_outputs, monitor_change_step, JoinMonitor, JoinedOutput, MonitorChangeStep,
};
use arcen_session::console_session::wayland_join::MUTTER_NAMES;
use arcen_session::wayland_outputs::{OutputDensity, PlacedOutput, VirtualOutputMode};
use arcen_session::wayland_session::VirtualOutputRequest;
use thiserror::Error;

use super::clipboard::{ClipboardService, MutterClipboard, CLIPBOARD_INTERFACE};
use super::input::{InputService, SessionFacts};
use super::mutter::{MutterError, MutterSessions};
use super::reconfigure::LiveOutputs;
use super::remote::RemoteSet;
use super::{
    InputBackendKind, InputSessionFacts, JoinedReady, ReadyOutput, StreamCursorMode,
    WaylandAgentReady, WaylandClipboardReady, CLIPBOARD_SOCKET_NAME, INITIAL_OUTPUT_GENERATION,
    INPUT_SOCKET_NAME,
};
use crate::logging::target;

/// GNOME Shell on the session bus: the shell that puts up the lock screen
/// when logind asks the session to lock.
pub const GNOME_SHELL: &str = "org.gnome.Shell";
/// How long the join may take, from the bus to the proven input target.
const JOIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// How long the join waits for the system bus.
const SYSTEM_BUS_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// How long an immediate lock may take before the hand-back takes over.
const LOCK_NOW_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// A join that could not be made.
#[derive(Debug, Error)]
pub enum JoinError {
    #[error("the agent does not run in the account's runtime directory and user bus")]
    Environment,
    #[error("Mutter's names are not served by the proven compositor: {0}")]
    Owner(&'static str),
    #[error("the session shows nothing to record")]
    NoOutputs,
    #[error("Mutter session failed: {0}")]
    Mutter(#[from] MutterError),
    #[error("the bus could not be asked: {0}")]
    Bus(#[from] zbus::Error),
    #[error("the bus refused a question: {0}")]
    BusCall(#[from] zbus::fdo::Error),
    #[error("input socket failed: {0}")]
    Input(std::io::Error),
    #[error("the join did not finish within {0:?}")]
    Budget(std::time::Duration),
}

/// The joined session while the agent serves it.
pub struct JoinedSession {
    connection: zbus::Connection,
    /// The system bus, connected at the join so a lock needs no connect
    /// ([`Self::lock_now`]); `None` leaves the lock to the hand-back.
    system: Option<zbus::Connection>,
    /// The session's logind id.
    session_id: String,
    live: LiveOutputs,
    input: Arc<InputService>,
    facts: Arc<SessionFacts>,
    clipboard: Option<(ClipboardService, MutterClipboard)>,
    ready: WaylandAgentReady,
    joined: JoinedReady,
    sockets: Vec<PathBuf>,
}

/// The account's runtime directory and user bus address, from the
/// environment the launcher gave the agent: only the account's own.
fn user_bus(uid: u32) -> Result<(PathBuf, String), JoinError> {
    let runtime_dir = PathBuf::from(format!("/run/user/{uid}"));
    let bus = format!("unix:path={}/bus", runtime_dir.display());
    let given_runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let given_bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
    if given_runtime.as_deref() != Some(runtime_dir.as_path()) || given_bus.as_deref() != Some(&bus)
    {
        return Err(JoinError::Environment);
    }
    Ok((runtime_dir, bus))
}

/// The session's `WAYLAND_DISPLAY`: a plain socket name in the runtime
/// directory.
fn wayland_display() -> Option<String> {
    let name = std::env::var("WAYLAND_DISPLAY").ok()?;
    (!name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')))
    .then_some(name)
}

/// Who serves `name` on the bus: its unique name, process and account.
async fn owner_of(
    bus: &zbus::fdo::DBusProxy<'_>,
    name: &'static str,
) -> Result<Option<(String, u32, u32)>, JoinError> {
    let Ok(bus_name) = zbus::names::BusName::try_from(name) else {
        return Ok(None);
    };
    let Ok(owner) = bus.get_name_owner(bus_name.clone()).await else {
        return Ok(None);
    };
    let pid = bus.get_connection_unix_process_id(bus_name.clone()).await?;
    let uid = bus.get_connection_unix_user(bus_name).await?;
    Ok(Some((owner.to_string(), pid, uid)))
}

/// Checks that one connection, the proven compositor's (`pid`) and this
/// account's, owns every Mutter name; whether that connection is also
/// GNOME Shell's (the lock screen's).
async fn verify_compositor(
    connection: &zbus::Connection,
    pid: u32,
    uid: u32,
) -> Result<bool, JoinError> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let mut unique: Option<String> = None;
    for name in MUTTER_NAMES {
        let Some((owner, owner_pid, owner_uid)) = owner_of(&bus, name).await? else {
            return Err(JoinError::Owner("a Mutter name has no owner"));
        };
        if owner_pid != pid || owner_uid != uid {
            return Err(JoinError::Owner(
                "a Mutter name is served by another process or account",
            ));
        }
        match &unique {
            Some(first) if *first != owner => {
                return Err(JoinError::Owner(
                    "Mutter's names are split across connections",
                ));
            }
            _ => unique = Some(owner),
        }
    }
    let shell = owner_of(&bus, GNOME_SHELL)
        .await?
        .is_some_and(|(owner, owner_pid, owner_uid)| {
            Some(&owner) == unique.as_ref() && owner_pid == pid && owner_uid == uid
        });
    Ok(shell)
}

/// Whether the account's settings let its session lock
/// (`org.gnome.desktop.lockdown disable-lock-screen` is not set).
async fn lock_screen_allowed() -> bool {
    let mut command = tokio::process::Command::new("gsettings");
    command
        .args(["get", "org.gnome.desktop.lockdown", "disable-lock-screen"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(std::time::Duration::from_secs(3), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim() == "false"
        }
        _ => false,
    }
}

/// The session's monitors as the shared decision reads them.
fn join_monitors(state: &super::mutter::DisplayState) -> Vec<JoinMonitor> {
    state
        .monitors
        .iter()
        .map(|monitor| JoinMonitor {
            connector: monitor.connector.clone(),
            virtual_monitor: monitor.virtual_monitor,
            mode: monitor
                .current
                .as_ref()
                .map(|(_, width, height, millihertz)| (*width, *height, *millihertz)),
            placement: state
                .placements
                .iter()
                .find(|placement| placement.connector == monitor.connector)
                .map(|placement| (placement.x, placement.y)),
            scale: state
                .scales
                .iter()
                .find(|(connector, _)| *connector == monitor.connector)
                .map_or(1.0, |(_, scale)| *scale),
        })
        .collect()
}

fn placed(output: &JoinedOutput) -> Option<PlacedOutput> {
    Some(PlacedOutput {
        request: VirtualOutputRequest::new(output.width, output.height, output.refresh_hz).ok()?,
        x: output.x,
        y: output.y,
        density: OutputDensity::default(),
    })
}

/// Removes a socket of ours left in the runtime directory by an agent that
/// did not finish; anything else at that path is left alone (and the bind
/// then fails).
fn remove_stale_socket(path: &Path, uid: u32) {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_socket() && metadata.uid() == uid {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl JoinedSession {
    /// Joins the session served by compositor `compositor_pid`, as `uid`.
    ///
    /// # Errors
    ///
    /// [`JoinError`]; nothing of the session was changed, and every
    /// sharing session that was created is stopped again.
    pub async fn start(
        compositor_pid: u32,
        uid: u32,
        username: &str,
        cursor_mode: StreamCursorMode,
    ) -> Result<Self, JoinError> {
        tokio::time::timeout(
            JOIN_BUDGET,
            Self::start_within(compositor_pid, uid, username, cursor_mode),
        )
        .await
        .map_err(|_| JoinError::Budget(JOIN_BUDGET))?
    }

    async fn start_within(
        compositor_pid: u32,
        uid: u32,
        username: &str,
        cursor_mode: StreamCursorMode,
    ) -> Result<Self, JoinError> {
        let (runtime_dir, bus) = user_bus(uid)?;
        let display = wayland_display().ok_or(JoinError::Environment)?;
        let connection = super::mutter::connect(&bus).await?;
        // GNOME Shell puts up the lock screen when logind asks the session to
        // lock; the hand-back still checks that it did.
        let shell = verify_compositor(&connection, compositor_pid, uid).await?;
        let lock = shell && lock_screen_allowed().await;
        let state = super::mutter::display_state(&connection).await?;
        let outputs = joined_outputs(&join_monitors(&state)).map_err(|_| JoinError::NoOutputs)?;
        let placed = outputs
            .iter()
            .map(placed)
            .collect::<Option<Vec<_>>>()
            .ok_or(JoinError::NoOutputs)?;
        let connectors = outputs
            .iter()
            .map(|output| output.connector.clone())
            .collect::<Vec<_>>();
        let sessions = MutterSessions::start(
            &connection,
            &connectors,
            cursor_mode.screencast().mutter_value(),
        )
        .await?;
        let set = RemoteSet::new(sessions, placed, connectors, cursor_mode);
        let streams = set.ready_streams();
        let ready_outputs: Vec<ReadyOutput> = set.ready_outputs();
        let capture = streams.len() == outputs.len() && streams.iter().all(|s| s.node_id != 0);
        let device_types = set.sessions.device_types();
        let live = LiveOutputs(Arc::new(tokio::sync::RwLock::new(set)));
        let input_proven = match live.0.read().await.sessions.probe_input().await {
            Ok(evidence) => {
                let capabilities = evidence.capabilities();
                evidence.started
                    && evidence.linked_to_screencast
                    && evidence.absolute_pointer_accepted
                    && capabilities.keyboard
                        == arcen_session::desktop_provider::CapabilityFact::Available
                    && capabilities.pointer
                        == arcen_session::desktop_provider::CapabilityFact::Available
            }
            Err(error) => {
                tracing::warn!(target: target::INPUT, %error, "the joined session's input target was not accepted");
                false
            }
        };
        let input_socket = runtime_dir.join(INPUT_SOCKET_NAME);
        remove_stale_socket(&input_socket, uid);
        let session_text = Arc::new(super::text::SessionText::new(Some(Box::new(
            super::text::GsettingsInputSources,
        ))));
        super::wl_keymap::spawn(runtime_dir.join(&display), Arc::clone(&session_text));
        let input = match InputService::bind(&input_socket, uid) {
            Ok(service) => Arc::new(service.with_text(Arc::clone(&session_text)).with_locks(
                super::LockState::at_session_start(super::input::read_remembered_num_lock()),
            )),
            Err(error) => {
                live.0.write().await.sessions.stop().await;
                return Err(JoinError::Input(error));
            }
        };
        let facts = Arc::new(SessionFacts::new(InputSessionFacts {
            generation: INITIAL_OUTPUT_GENERATION,
            streams: streams.clone(),
            device_types,
            remote_desktop_started: input_proven,
            backend: InputBackendKind::RemoteDesktopDbus,
            eis_offered: None,
            stylus: None,
            connection: 0,
            text: None,
        }));
        let clipboard_socket = runtime_dir.join(CLIPBOARD_SOCKET_NAME);
        remove_stale_socket(&clipboard_socket, uid);
        let clipboard =
            super::agent::prove_clipboard(&connection, &runtime_dir, uid, username).await;
        let joined = JoinedReady {
            compositor_pid,
            capture,
            input: input_proven,
            lock,
            at_desk: arcen_session::console_session::wayland_attach::shown_at_desk(&outputs),
        };
        tracing::info!(
            target: target::SESSION,
            compositor_pid,
            outputs = ?outputs.iter().map(|output| output.connector.as_str()).collect::<Vec<_>>(),
            at_desk = joined.at_desk,
            capture,
            input = input_proven,
            lock,
            clipboard = clipboard.is_some(),
            "joined the account's Wayland session: its own monitors recorded, nothing of it changed"
        );
        let ready = WaylandAgentReady {
            compositor: "gnome-shell".to_owned(),
            runtime_dir: runtime_dir.clone(),
            wayland_display: display,
            outputs: ready_outputs,
            streams,
            input_device_types: device_types,
            input_socket: input_socket.clone(),
            generation: INITIAL_OUTPUT_GENERATION,
            compositor_render_node: None,
            compositor_gpu_pci: None,
            compositor_gpu_vendor: None,
            output_mode: VirtualOutputMode::StartupMonitors,
            output_socket: None,
            x11: None,
            cursor_mode,
            clipboard: clipboard.as_ref().map(|_| WaylandClipboardReady {
                socket: clipboard_socket.clone(),
                interface: CLIPBOARD_INTERFACE.to_owned(),
                dedicated_session: true,
            }),
        };
        // The session's own logind id (the launcher checks the ready
        // message's against it), and the system bus to lock it through.
        let session_id = std::env::var("XDG_SESSION_ID")
            .ok()
            .filter(|id| {
                !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
            })
            .ok_or(JoinError::Environment)?;
        let system = tokio::time::timeout(SYSTEM_BUS_BUDGET, zbus::Connection::system())
            .await
            .ok()
            .and_then(Result::ok);
        if system.is_none() {
            tracing::warn!(target: target::SESSION, "the system bus is unreachable; a desk monitor plugged in during the attachment is locked by the hand-back only");
        }
        Ok(Self {
            connection,
            system,
            session_id,
            live,
            input,
            facts,
            clipboard,
            ready,
            joined,
            sockets: vec![input_socket, clipboard_socket],
        })
    }

    /// The ready message's Wayland part.
    #[must_use]
    pub const fn ready(&self) -> &WaylandAgentReady {
        &self.ready
    }

    /// What the join proved.
    #[must_use]
    pub const fn joined(&self) -> &JoinedReady {
        &self.joined
    }

    /// Serves input and clipboard until Mutter closes the sharing sessions
    /// (the session locked, or its compositor went away).
    pub async fn serve(&self) {
        let clipboard = async {
            match &self.clipboard {
                Some((service, compositor)) => service.serve(compositor).await,
                None => std::future::pending::<()>().await,
            }
        };
        let closed = async {
            let signals = self.live.0.read().await.sessions.closed_signals().await;
            match signals {
                Some(mut signals) => {
                    use futures_util::StreamExt as _;
                    let _ = signals.next().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        let step = tokio::select! {
            () = self.input.serve(&self.live, &self.facts) => None,
            () = clipboard => None,
            () = closed => {
                tracing::info!(target: target::SESSION, "Mutter closed the joined session's sharing (locked, a recorded monitor moved or went away, or the compositor ended); the attachment ends, the session continues");
                None
            }
            step = self.monitors_changed() => {
                tracing::warn!(target: target::SESSION, "the joined session's monitors changed (one enabled, disabled or reconfigured); the attachment ends, the session is locked and continues");
                Some(step)
            }
        };
        // Whichever ended the attachment: Mutter closes a recorded
        // monitor's stream, and so the sharing, before it signals
        // `MonitorsChanged`, so a desk monitor plugged in often ends it
        // through `closed`. The monitors are judged once more after any
        // ending, outside the select that could drop the lock.
        let step = match step {
            Some(step) => step,
            // Unanswered in time counts as unreadable.
            None => tokio::time::timeout(LOCK_NOW_BUDGET, self.monitor_change())
                .await
                .unwrap_or(MonitorChangeStep::LockNow),
        };
        if step == MonitorChangeStep::LockNow {
            self.lock_now().await;
        }
    }

    /// Resolves when the session's monitors are no longer the ones the
    /// join recorded and judged for the desk (a monitor plugged in at the
    /// desk, one disabled or reconfigured): the recorded set is fixed, and
    /// desk privacy was decided for it. A change that cannot be watched
    /// ends the attachment too.
    async fn monitors_changed(&self) -> MonitorChangeStep {
        use futures_util::StreamExt as _;
        let Ok(mut changes) = super::mutter::monitors_changed(&self.connection).await else {
            return MonitorChangeStep::End;
        };
        // A change between the join and the subscription, then each one
        // as the compositor signals it: judged at once, not polled.
        let mut step = self.monitor_change().await;
        while step == MonitorChangeStep::Keep {
            if changes.next().await.is_none() {
                step = MonitorChangeStep::End;
                break;
            }
            step = self.monitor_change().await;
        }
        step
    }

    /// Locks the session through logind at once, as the account (logind
    /// lets a session's owner lock it), before the attachment winds down;
    /// the hand-back locks again and checks it. A failure is logged and
    /// left to the hand-back.
    async fn lock_now(&self) {
        let Some(system) = &self.system else {
            return;
        };
        let locked = tokio::time::timeout(
            LOCK_NOW_BUDGET,
            system.call_method(
                Some("org.freedesktop.login1"),
                "/org/freedesktop/login1",
                Some("org.freedesktop.login1.Manager"),
                "LockSession",
                &(self.session_id.as_str(),),
            ),
        )
        .await;
        match locked {
            Ok(Ok(_)) => {
                tracing::warn!(target: target::SESSION, session_id = %self.session_id, "the joined session may show on a monitor at the desk; it was locked at once");
            }
            Ok(Err(error)) => {
                tracing::error!(target: target::SESSION, session_id = %self.session_id, %error, "the joined session could not be locked at once; the hand-back locks it");
            }
            Err(_) => {
                tracing::error!(target: target::SESSION, session_id = %self.session_id, "locking the joined session at once timed out; the hand-back locks it");
            }
        }
    }

    /// The shared step for the session's monitors as they are now
    /// ([`monitor_change_step`]).
    async fn monitor_change(&self) -> MonitorChangeStep {
        let outputs = super::mutter::display_state(&self.connection)
            .await
            .ok()
            .and_then(|state| joined_outputs(&join_monitors(&state)).ok());
        let Some(outputs) = outputs else {
            return monitor_change_step(false, self.joined.at_desk, None);
        };
        let recorded = self
            .ready
            .outputs
            .iter()
            .map(|output| (output.connector.as_str(), output.width, output.height));
        let same = outputs
            .iter()
            .map(|output| (output.connector.as_str(), output.width, output.height))
            .eq(recorded);
        monitor_change_step(
            same,
            self.joined.at_desk,
            Some(arcen_session::console_session::wayland_attach::shown_at_desk(&outputs)),
        )
    }

    /// Ends the sharing sessions and removes the sockets; the session
    /// itself is untouched.
    pub async fn stop(self) {
        self.input.release_held(&self.live).await;
        self.live.0.write().await.sessions.stop().await;
        for socket in &self.sockets {
            let _ = std::fs::remove_file(socket);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_session::wayland_outputs::{LayoutMode, ObservedPlacement};

    fn monitor(connector: &str, virtual_monitor: bool) -> super::super::mutter::DisplayMonitor {
        super::super::mutter::DisplayMonitor {
            connector: connector.to_owned(),
            remote: false,
            virtual_monitor,
            current: Some(("mode".to_owned(), 2560, 1440, 59_951)),
            supported_scales: vec![1.0, 2.0],
        }
    }

    #[test]
    fn the_sessions_monitors_are_read_with_their_place_and_scale() {
        let state = super::super::mutter::DisplayState {
            serial: 1,
            monitors: vec![
                monitor("DP-1", false),
                monitor("Meta-0", true),
                monitor("DP-2", false),
            ],
            placements: vec![
                ObservedPlacement {
                    connector: "DP-1".to_owned(),
                    x: 0,
                    y: 0,
                },
                ObservedPlacement {
                    connector: "Meta-0".to_owned(),
                    x: 2560,
                    y: 0,
                },
            ],
            scales: vec![("DP-1".to_owned(), 1.0), ("Meta-0".to_owned(), 2.0)],
            layout: LayoutMode::Physical,
        };
        let monitors = join_monitors(&state);
        assert_eq!(monitors.len(), 3);
        assert_eq!(monitors[1].placement, Some((2560, 0)));
        assert_eq!(monitors[1].scale, 2.0);
        assert!(monitors[1].virtual_monitor && !monitors[0].virtual_monitor);
        // DP-2 is not placed: disabled, never recorded.
        assert_eq!(monitors[2].placement, None);
        let outputs = joined_outputs(&monitors).expect("outputs");
        assert_eq!(
            outputs
                .iter()
                .map(|output| (output.connector.as_str(), output.at_desk))
                .collect::<Vec<_>>(),
            [("DP-1", true), ("Meta-0", false)]
        );
        assert!(outputs.iter().all(|output| placed(output).is_some()));
    }
}
