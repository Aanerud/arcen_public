// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Privileged per-session launcher process.
//!
//! The machine broker never owns a PAM handle or a logind user session. It
//! starts this process in `system.slice`, authenticates over private pipes, and
//! retains the child for the graphical desktop lifetime. The launcher owns the
//! PAM transaction, validates pam_systemd/logind ownership, starts the
//! unprivileged session agent, and closes PAM only after the complete user
//! process tree has stopped.

#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arcen_session::restore_lease::IanaTimeZone;
use arcen_telemetry::CorrelationId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
#[cfg(target_os = "linux")]
use tokio::process::Command;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

#[cfg(target_os = "linux")]
use super::agent::SessionAgent;
use super::agent::{AgentAudioArguments, AgentError, AgentReady};
use super::identity::{IdentityError, SessionEnvironment, UserExecution, UserIdentity};
use crate::logging::target;

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the broker waits for the launcher's ready response. A headless
/// Wayland start is bounded by its own budget, which is longer than an Xorg
/// start; the broker must outwait it, or it would kill a launcher that is
/// still inside a bounded start.
const fn open_timeout(provider: arcen_session::desktop_provider::DesktopProvider) -> Duration {
    match provider {
        arcen_session::desktop_provider::DesktopProvider::Xorg => OPEN_TIMEOUT,
        arcen_session::desktop_provider::DesktopProvider::Wayland => {
            super::wayland::WAYLAND_OPEN_TIMEOUT
        }
    }
}
const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(12);
/// How long a dedicated X server may take to answer, well inside the
/// broker's open timeout.
#[cfg(target_os = "linux")]
const XORG_READY_BUDGET: Duration = Duration::from_secs(12);
/// How long the broker waits for a console launcher's ready response: a
/// remote-first sign-in through the login screen (bounded at 30 s) and the
/// new session's display (bounded at 15 s) come first; the wait for GDM to
/// settle after the sign-in gets what is left, less [`CONSOLE_READY_MARGIN`].
const CONSOLE_OPEN_TIMEOUT: Duration = Duration::from_secs(55);
/// What a console launcher keeps of [`CONSOLE_OPEN_TIMEOUT`] for unlocking
/// the session and starting its agent.
const CONSOLE_READY_MARGIN: Duration = Duration::from_secs(5);
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(12);
#[cfg(target_os = "linux")]
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const AGENT_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_AUTH_REQUEST_BYTES: usize = 64 * 1024;
const MAX_OPEN_REQUEST_BYTES: usize = 32 * 1024;
#[cfg(target_os = "linux")]
const LOGIND_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("session launcher is unavailable")]
    BinaryUnavailable,
    #[error("session launcher rejected authentication")]
    Rejected,
    #[error("session launcher protocol failed")]
    Protocol,
    #[error("session launcher timed out")]
    Timeout,
    #[error("session launcher process failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("authenticated OS identity is invalid: {0}")]
    Identity(#[from] IdentityError),
    #[error("session agent failed: {0}")]
    Agent(#[from] AgentError),
    #[error("PAM initialization failed")]
    PamInitialization,
    #[error("PAM session opening failed")]
    PamSession,
    #[error("pam_systemd did not create the required logind session")]
    LogindSession,
    #[error("session launcher must run as root")]
    RootRequired,
    #[error("dedicated Xorg configuration is invalid")]
    XorgConfig,
    #[error("dedicated Xorg failed to start")]
    XorgStart,
    #[error("dedicated Xorg topology verification failed")]
    XorgVerify,
    #[error("dedicated Xorg could not be proved released")]
    XorgRelease,
    #[error("logind session could not be activated on seat0")]
    LogindActivation,
    #[error("another remote desktop is active on this host (one X11 desktop per host)")]
    SeatOccupied,
    #[error("another remote-desktop product holds this host's display (one X11 desktop per host)")]
    SeatOccupiedOtherProduct(Option<arcen_session::console_session::ForeignService>),
    #[error("the console sign-in was refused: {}", .0.token())]
    Console(arcen_session::console_session::ConsoleNotice),
    #[error("authenticated logind session could not be unlocked")]
    LogindUnlock,
    #[error("deskside protection failed")]
    Deskside,
    #[error("the headless Wayland provider is not built into this Pier")]
    WaylandUnavailable,
    #[error("headless Wayland session failed to become ready")]
    WaylandStart,
    #[error("the host could not prove its own speakers silent")]
    LocalPlaybackRefused,
    #[error("headless Wayland session release could not be proven")]
    WaylandRelease,
    #[error(
        "SELinux policy does not let the launcher start the session in the context PAM assigned"
    )]
    SelinuxContext,
    #[error("polkit would not keep this remote desktop from the console's rights ({0})")]
    PolkitUnprotected(arcen_session::polkit_remote::PolkitUnprotected),
    #[error("wayland.session_mode = native_seat, and this desktop cannot have its own seat: {}", .0.explanation())]
    NativeSeatUnavailable(arcen_session::native_seat::NativeSeatUnavailable),
}

#[derive(Debug, Clone)]
pub struct LauncherConfig {
    pub binary: PathBuf,
    pub pam_service: String,
    pub display: String,
    pub gpu_head: String,
    pub xorg_binary: PathBuf,
    pub xorg_config_template: PathBuf,
    pub runtime_root: PathBuf,
    pub agent_binary: PathBuf,
    pub desktop: String,
    /// `platform.session.default_desktop` for console sign-ins.
    pub default_desktop: Option<arcen_session::console_session::account_desktop::DesktopId>,
    pub deskside: crate::deskside::LinuxDesksideConfig,
    /// `privacy.desk`: a joined console's desk is private while a Deck is
    /// attached unless the administrator turned it off.
    pub desk_privacy: arcen_session::console_session::desk_privacy::DeskPrivacyPolicy,
    /// Desktop provider the privileged launcher creates.
    pub provider: arcen_session::desktop_provider::DesktopProvider,
    /// Whether a Wayland desktop runs the compositor's Xwayland for X11
    /// applications (the Pier's resolved `platform.session.wayland_x11_apps`).
    pub x11_apps: bool,
    /// The operator's `audio.local_playback` and the `pactl` the session
    /// agent drives it with.
    pub audio: AgentAudioArguments,
    /// `wayland.first_login_setup` for a headless Wayland desktop.
    pub first_login_setup: arcen_session::desktop_session::first_login::FirstLoginSetup,
    /// `platform.session.mode`: join the user's console session (the
    /// default, ADR 0025) or create a dedicated desktop.
    pub desktop_mode: arcen_session::console_session::DesktopMode,
}

#[derive(Serialize)]
struct AuthenticateRequest<'a> {
    command: &'static str,
    username: &'a str,
    password: &'a str,
    remote_host: &'a str,
}

/// The broker's `authenticate` line. Deliberately not `Debug`: it carries
/// the password.
#[derive(Deserialize)]
struct OwnedAuthenticateRequest {
    command: String,
    username: String,
    password: String,
    remote_host: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timezone: Option<IanaTimeZone>,
    #[serde(default)]
    deskside: crate::deskside::LinuxDesksideConfig,
    /// Committed `multi_monitor_v1` topology this launcher must render into
    /// a generated multi-head Xorg configuration instead of the single-head
    /// template substitution, or `None` for every session that never
    /// requested (or was never admitted for) multi-monitor — including every
    /// session while the operator's own `--multi-monitor` gate stays off,
    /// which remains this host's default and the sole production safety
    /// switch (`media::multi_capenc::MULTI_MONITOR_CARRIER_READY` is `true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    multi_monitor: Option<MultiHeadPlanMsg>,
    /// The Deck display a single-head session stands for. The launcher
    /// synthesizes that head's EDID from it with the shared generator, so
    /// the head's native mode, physical size and gamut are the Deck
    /// display's. `None` keeps the template's EDID-less head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<SessionDisplayMsg>,
    /// The operator's `audio.local_playback`; absent in a request from an
    /// older Pier, which means the safe default.
    #[serde(default)]
    local_playback: arcen_session::pier_config::LocalPlayback,
    /// The `pactl` the agent drives the session's audio server with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pactl: Option<PathBuf>,
    /// How a headless Wayland desktop records its cursor, chosen from the
    /// opening Deck's facts. Absent means embedded; an Xorg desktop ignores
    /// it (its cursor is never in a `ScreenCast` stream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream_cursor: Option<super::wayland::StreamCursorMode>,
    /// The route to a wide composition a headless Wayland session starts
    /// with (`force-hdr`): only for a Grading or HDR session on a host whose
    /// qualification proved it. `None` keeps the compositor's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wayland_fidelity_route: Option<String>,
    /// The side-by-side prefix a headless Wayland session runs its
    /// compositor from (`capture.wayland_compositor_prefix`); absent for the
    /// distribution's `/usr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wayland_compositor_prefix: Option<String>,
    /// Lab (`wayland.lab_seat_pen`): the udev seat of the Pier's seat pen,
    /// on which the compositor runs libinput.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wayland_lab_input_seat: Option<String>,
    /// A native desktop (`wayland.session_mode`): the logind session opens
    /// on `wayland_lab_input_seat` and the compositor runs natively there.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    wayland_native_seat: bool,
    /// The X11 depth (24 or 30) a dedicated Xorg desktop is created at,
    /// decided by the Pier from the creating session
    /// (`arcen_media::video::choose_desktop_depth`). Absent keeps the
    /// template's own depth, as every desktop had before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    x11_desktop_depth: Option<u8>,
    /// Join the user's console session instead of creating a desktop
    /// (`platform.session.mode = console`, ADR 0025).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    console: bool,
    /// The administrator turned desk privacy off (`privacy.desk = "off"`):
    /// a joined console's desk is given its monitors and input once the
    /// session is joined, and again after each resume. Absent is private.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    desk_live: bool,
}

/// A served Deck display's facts, carried to the privileged launcher.
///
/// Facts rather than EDID bytes: the launcher builds the EDID itself with
/// `arcen_outputs::edid::generate`, so what reaches a root-owned Xorg
/// configuration is always a validated, generated block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SessionDisplayMsg {
    width: u32,
    height: u32,
    refresh_hz: u32,
    width_mm: f32,
    height_mm: f32,
    scale: f32,
    product_id: u16,
    serial: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<arcen_protocol::messages::DisplayColorMsg>,
}

impl SessionDisplayMsg {
    /// The served display, at the size the session streams.
    pub(crate) fn served(
        monitor: &arcen_protocol::messages::ClientMonitor,
        width: u32,
        height: u32,
    ) -> Self {
        let finite = |value: f32| {
            if value.is_finite() && value > 0.0 {
                value
            } else {
                0.0
            }
        };
        Self {
            width,
            height,
            refresh_hz: monitor.refresh_hz.clamp(1, 240),
            width_mm: finite(monitor.width_mm),
            height_mm: finite(monitor.height_mm),
            scale: if monitor.scale.is_finite() && monitor.scale > 0.0 {
                monitor.scale
            } else {
                1.0
            },
            // The same identity the Windows Pier gives a Deck display.
            product_id: if monitor.model == 0 {
                0x0001
            } else {
                monitor.model as u16
            },
            serial: monitor.serial,
            color: monitor.color,
        }
    }

    /// The display's refresh rate.
    pub(crate) const fn refresh_hz(&self) -> u32 {
        self.refresh_hz
    }

    /// The display's physical size, for the virtual output's UI scale.
    pub(crate) fn density(&self) -> arcen_session::wayland_outputs::OutputDensity {
        let millimetres = |value: f32| {
            if value.is_finite() && value > 0.0 && value < 100_000.0 {
                // Range-checked above.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let rounded = value.round() as u32;
                rounded
            } else {
                0
            }
        };
        arcen_session::wayland_outputs::OutputDensity {
            width_mm: millimetres(self.width_mm),
            height_mm: millimetres(self.height_mm),
            logical_width: 0,
        }
    }

    /// The same display with its head created at `refresh_hz`: a headless
    /// Wayland session's virtual output, or the refresh a dedicated Xorg
    /// head's EDID describes.
    #[must_use]
    pub(crate) const fn with_refresh_hz(mut self, refresh_hz: u32) -> Self {
        self.refresh_hz = refresh_hz;
        self
    }

    /// The size the session streams.
    pub(crate) const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn edid(&self) -> Result<[u8; 128], String> {
        arcen_outputs::edid::generate(arcen_outputs::edid::EdidRequest {
            width: self.width,
            height: self.height,
            refresh_hz: self.refresh_hz,
            width_mm: self.width_mm,
            height_mm: self.height_mm,
            scale: self.scale,
            product_id: self.product_id,
            serial: self.serial,
            color: self
                .color
                .as_ref()
                .map(arcen_media::display_color::DisplayColor::from_msg),
        })
    }
}

/// Wire-serializable mirror of a committed
/// [`crate::display::topology::LinuxTopologyPlan`], carried across the
/// privileged `session-launcher` subprocess IPC boundary inside
/// [`OpenRequest`].
///
/// `LinuxTopologyPlan` itself is not `serde`-serializable: its
/// [`arcen_media::SessionMonitorId`]/[`arcen_media::TopologyGeneration`]
/// newtypes intentionally do not derive `serde` traits, keeping host-internal
/// identifiers out of any wire surface by default. This DTO exists purely
/// for this one same-host, same-build-version local process boundary, never
/// sent to a client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MultiHeadPlanMsg {
    #[serde(default = "dedicated_xorg_backend")]
    backend: crate::display::topology::LinuxTopologyBackend,
    generation: u64,
    virtual_width: u32,
    virtual_height: u32,
    monitors: Vec<MultiHeadMonitorPlanMsg>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct MultiHeadMonitorPlanMsg {
    session_monitor_id: u16,
    client_display_id: String,
    /// Assigned RandR/NV-CONTROL output, e.g. `"DFP-0"`.
    head: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    logical_x: i64,
    logical_y: i64,
    logical_width: u64,
    logical_height: u64,
    physical_width: u32,
    physical_height: u32,
    scale_120: u32,
    rotation: arcen_media::Rotation,
    primary: bool,
    #[serde(default)]
    quality_intent: arcen_protocol::messages::MonitorQualityIntentMsg,
    mode_token: String,
    #[serde(default = "default_refresh_hz")]
    refresh_hz: u32,
    #[serde(default)]
    width_mm: u32,
    #[serde(default)]
    height_mm: u32,
}

const fn dedicated_xorg_backend() -> crate::display::topology::LinuxTopologyBackend {
    crate::display::topology::LinuxTopologyBackend::DedicatedXorg
}

const fn default_refresh_hz() -> u32 {
    60
}

impl MultiHeadPlanMsg {
    fn from_plan(plan: &crate::display::topology::LinuxTopologyPlan) -> Self {
        Self {
            backend: plan.backend,
            generation: plan.generation.get(),
            virtual_width: plan.virtual_width,
            virtual_height: plan.virtual_height,
            monitors: plan
                .monitors
                .iter()
                .map(MultiHeadMonitorPlanMsg::from_plan)
                .collect(),
        }
    }

    /// Reconstructs the committed [`crate::display::topology::LinuxTopologyPlan`]
    /// this message mirrors.
    ///
    /// # Errors
    ///
    /// Returns [`LauncherError::XorgConfig`] when a stored session monitor id
    /// or generation is `0` — impossible for a plan this host itself
    /// produced via [`MultiHeadPlanMsg::from_plan`], but rejected
    /// defensively rather than panicking on malformed input crossing this
    /// privileged process's stdin boundary.
    fn into_plan(self) -> Result<crate::display::topology::LinuxTopologyPlan, LauncherError> {
        let generation = arcen_media::TopologyGeneration::new(self.generation)
            .map_err(|_| LauncherError::XorgConfig)?;
        let monitors = self
            .monitors
            .into_iter()
            .map(MultiHeadMonitorPlanMsg::into_plan)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::display::topology::LinuxTopologyPlan {
            backend: self.backend,
            generation,
            virtual_width: self.virtual_width,
            virtual_height: self.virtual_height,
            monitors,
        })
    }
}

impl MultiHeadMonitorPlanMsg {
    fn from_plan(monitor: &crate::display::topology::LinuxMonitorPlan) -> Self {
        Self {
            session_monitor_id: monitor.session_monitor_id.get(),
            client_display_id: monitor.client_display_id.clone(),
            head: monitor.head.clone(),
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
            logical_x: monitor.logical_rect.origin().x,
            logical_y: monitor.logical_rect.origin().y,
            logical_width: monitor.logical_rect.size().width(),
            logical_height: monitor.logical_rect.size().height(),
            physical_width: monitor.physical_size.width(),
            physical_height: monitor.physical_size.height(),
            scale_120: monitor.scale.get(),
            rotation: monitor.rotation,
            primary: monitor.primary,
            quality_intent: monitor.quality_intent,
            mode_token: monitor.mode_token.clone(),
            refresh_hz: monitor.refresh_hz,
            width_mm: monitor.width_mm,
            height_mm: monitor.height_mm,
        }
    }

    fn into_plan(self) -> Result<crate::display::topology::LinuxMonitorPlan, LauncherError> {
        let session_monitor_id = arcen_media::SessionMonitorId::new(self.session_monitor_id)
            .map_err(|_| LauncherError::XorgConfig)?;
        let logical_size = arcen_media::LogicalSize::new(self.logical_width, self.logical_height)
            .map_err(|_| LauncherError::XorgConfig)?;
        let logical_rect = arcen_media::LogicalRect::new(
            arcen_media::LogicalPoint::new(self.logical_x, self.logical_y),
            logical_size,
        )
        .map_err(|_| LauncherError::XorgConfig)?;
        let physical_size =
            arcen_media::PhysicalSize::new(self.physical_width, self.physical_height)
                .map_err(|_| LauncherError::XorgConfig)?;
        let scale =
            arcen_media::Scale120::new(self.scale_120).map_err(|_| LauncherError::XorgConfig)?;
        Ok(crate::display::topology::LinuxMonitorPlan {
            refresh_hz: self.refresh_hz,
            width_mm: self.width_mm,
            height_mm: self.height_mm,
            session_monitor_id,
            client_display_id: self.client_display_id,
            head: self.head,
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            logical_rect,
            physical_size,
            scale,
            rotation: self.rotation,
            primary: self.primary,
            quality_intent: self.quality_intent,
            mode_token: self.mode_token,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseRequest {
    command: String,
}

impl OpenRequest {
    fn new(
        timezone: Option<IanaTimeZone>,
        deskside: crate::deskside::LinuxDesksideConfig,
        multi_monitor: Option<MultiHeadPlanMsg>,
        display: Option<SessionDisplayMsg>,
        audio: &AgentAudioArguments,
        wayland_fidelity_route: Option<String>,
    ) -> Self {
        Self {
            command: "open".to_string(),
            timezone,
            deskside,
            multi_monitor,
            display,
            local_playback: audio.local_playback,
            pactl: Some(audio.pactl.clone()),
            stream_cursor: None,
            wayland_fidelity_route,
            wayland_compositor_prefix: super::wayland::compositor_prefix().recorded(),
            wayland_lab_input_seat: None,
            wayland_native_seat: false,
            x11_desktop_depth: None,
            console: false,
            desk_live: false,
        }
    }

    /// The agent's audio arguments. A `pactl` that is not an absolute path
    /// to a file falls back to the default rather than running something
    /// relative to the agent's working directory.
    fn audio(&self) -> AgentAudioArguments {
        let pactl = self
            .pactl
            .clone()
            .filter(|path| path.is_absolute() && path.is_file())
            .unwrap_or_else(|| PathBuf::from(crate::media::local_playback::DEFAULT_PACTL));
        AgentAudioArguments {
            local_playback: self.local_playback,
            pactl,
        }
    }

    fn with_stream_cursor(
        mut self,
        stream_cursor: Option<super::wayland::StreamCursorMode>,
    ) -> Self {
        self.stream_cursor = stream_cursor;
        self
    }

    fn with_lab_input_seat(mut self, seat: Option<String>) -> Self {
        self.wayland_lab_input_seat = seat;
        self
    }

    fn with_native_seat(mut self, native: bool) -> Self {
        self.wayland_native_seat = native && self.wayland_lab_input_seat.is_some();
        self
    }

    /// The seat the logind session opens on: only a valid desktop seat of a
    /// native-seat request.
    fn native_seat(&self) -> Option<&str> {
        self.wayland_native_seat
            .then_some(self.wayland_lab_input_seat.as_deref())
            .flatten()
            .filter(|seat| arcen_input::seat_pen::is_desktop_seat(seat))
    }

    fn with_x11_desktop_depth(mut self, depth: Option<arcen_media::video::DesktopDepth>) -> Self {
        self.x11_desktop_depth = depth.map(arcen_media::video::DesktopDepth::x11_depth);
        self
    }

    /// The requested depth, refusing anything but the two this launcher
    /// renders: a request from the Pier is trusted no further than that.
    fn x11_desktop_depth(&self) -> Result<Option<arcen_media::video::DesktopDepth>, LauncherError> {
        self.x11_desktop_depth
            .map(|depth| {
                arcen_media::video::DesktopDepth::from_x11_depth(depth)
                    .ok_or(LauncherError::Protocol)
            })
            .transpose()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct LauncherResponse {
    status: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    uid: u32,
    #[serde(default)]
    gid: u32,
    #[serde(default)]
    supplementary_groups: Vec<u32>,
    #[serde(default)]
    home: PathBuf,
    #[serde(default)]
    shell: PathBuf,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    session_type: String,
    #[serde(default)]
    desktop: String,
    #[serde(default)]
    display: String,
    #[serde(default)]
    xauthority: String,
    #[serde(default)]
    agent_pid: u32,
    /// `lspci -D` form of the GPU the dedicated X server drives, or empty.
    #[serde(default)]
    capture_gpu_pci: String,
    /// The headless Wayland session's outputs, streams and input socket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    wayland: Option<super::wayland::WaylandAgentReady>,
    /// The session's audio, as its agent established it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<super::agent::SessionAudioReady>,
    /// The root depth the dedicated Xorg reported once ready; absent for a
    /// Wayland session and from an older launcher.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    x11_root_depth: Option<u8>,
    /// Who started the console session this launcher joined (ADR 0025);
    /// absent for a desktop the launcher created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_origin: Option<String>,
    /// The joined session was started through the login screen for this
    /// sign-in (remote-first).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    console_signed_in: bool,
    /// The other remote-desktop service that started the joined session, or
    /// that a refusal names, as the host reports it (checked again here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_service: Option<arcen_session::console_session::ForeignService>,
    /// The Deck's layout on the joined console (ADR 0025 §6): `applied`,
    /// or the token of why not (the console is then served as one
    /// picture). Absent when no layout was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_layout: Option<String>,
    /// A console shown as it is (`console_layout` = `shown`): the RandR
    /// output each committed monitor is captured from, in plan order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_outputs: Option<Vec<String>>,
    /// A console shown as it is: what shows each monitor of the committed
    /// plan (the Pier serves the plan as that desktop shows it).
    #[cfg(target_os = "linux")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_shown: Option<super::console_layout::GuestLayout>,
    /// What `platform.session.default_desktop` did for a sign-in that
    /// started the account's session (`default_applied`,
    /// `account_choice`…); absent for a joined session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_default_desktop: Option<String>,
    /// The joined session's desktop as logind recorded it
    /// (`XDG_SESSION_DESKTOP`: `gnome-classic`, `gnome-xorg`…). The agent
    /// keeps the configured desktop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    console_desktop: Option<String>,
}

/// The helper environment that pins capture to the dedicated X server's GPU.
///
/// The launcher reports the GPU its Xorg drives; capenc reads
/// [`arcen_capenc::CAPTURE_GPU_PCI_ENV`] and opens CUDA on that PCI device
/// instead of CUDA ordinal 0, so NvFBC, CUDA and NVENC stay on the GPU that
/// owns the screen. Anything but a valid address is dropped with a warning:
/// capture then keeps its previous single-GPU behaviour.
fn capture_gpu_environment(reported: &str) -> Option<(String, String)> {
    if reported.is_empty() {
        return None;
    }
    match arcen_outputs::PciAddress::parse_sysfs(reported) {
        Ok(address) => {
            tracing::info!(
                target: target::SESSION,
                capture_gpu_pci = %address,
                "dedicated X server GPU; capture will use the same GPU"
            );
            Some((
                arcen_capenc::CAPTURE_GPU_PCI_ENV.to_owned(),
                address.to_string(),
            ))
        }
        Err(error) => {
            tracing::warn!(
                target: target::SESSION,
                %error,
                "launcher reported an unusable capture GPU; capture keeps CUDA's default device"
            );
            None
        }
    }
}

impl LauncherResponse {
    fn error(status: &str) -> Self {
        Self {
            status: status.to_string(),
            username: String::new(),
            uid: 0,
            gid: 0,
            supplementary_groups: Vec::new(),
            home: PathBuf::new(),
            shell: PathBuf::new(),
            session_id: String::new(),
            session_type: String::new(),
            desktop: String::new(),
            display: String::new(),
            xauthority: String::new(),
            agent_pid: 0,
            capture_gpu_pci: String::new(),
            wayland: None,
            audio: None,
            x11_root_depth: None,
            console_origin: None,
            console_signed_in: false,
            console_service: None,
            console_layout: None,
            console_outputs: None,
            #[cfg(target_os = "linux")]
            console_shown: None,
            console_default_desktop: None,
            console_desktop: None,
        }
    }

    /// A refusal that names the service holding the workstation.
    fn error_naming(
        status: &str,
        service: Option<&arcen_session::console_session::ForeignService>,
    ) -> Self {
        let mut response = Self::error(status);
        response.console_service = service.cloned();
        response
    }

    fn console_refusal(notice: &arcen_session::console_session::ConsoleNotice) -> Self {
        Self::error_naming(notice.token(), notice.service())
    }

    fn authenticated(identity: &UserIdentity) -> Self {
        Self {
            status: "authenticated".into(),
            username: identity.username.clone(),
            uid: identity.uid,
            gid: identity.gid,
            supplementary_groups: identity.supplementary_groups.clone(),
            home: identity.home.clone(),
            shell: identity.shell.clone(),
            ..Self::error("")
        }
    }

    fn ready(
        identity: &UserIdentity,
        environment: &SessionEnvironment,
        agent: &AgentReady,
        capture_gpu_pci: Option<arcen_outputs::PciAddress>,
        x11_root_depth: Option<u8>,
    ) -> Self {
        Self {
            status: "ready".into(),
            username: identity.username.clone(),
            uid: identity.uid,
            gid: identity.gid,
            supplementary_groups: identity.supplementary_groups.clone(),
            home: identity.home.clone(),
            shell: identity.shell.clone(),
            session_id: environment.session_id().unwrap_or_default().to_string(),
            session_type: environment.session_type().to_string(),
            desktop: environment.desktop().to_string(),
            display: environment.display().to_string(),
            xauthority: environment
                .get("XAUTHORITY")
                .unwrap_or_default()
                .to_string(),
            agent_pid: agent.pid,
            capture_gpu_pci: capture_gpu_pci
                .map(|address| address.to_string())
                .unwrap_or_default(),
            wayland: agent.wayland.clone(),
            audio: agent.audio.clone(),
            x11_root_depth,
            console_origin: None,
            console_signed_in: false,
            console_service: None,
            console_layout: None,
            console_outputs: None,
            #[cfg(target_os = "linux")]
            console_shown: None,
            console_default_desktop: None,
            console_desktop: None,
        }
    }

    fn identity(&self) -> UserIdentity {
        UserIdentity {
            username: self.username.clone(),
            uid: self.uid,
            gid: self.gid,
            supplementary_groups: self.supplementary_groups.clone(),
            home: self.home.clone(),
            shell: self.shell.clone(),
        }
    }
}

pub struct AuthenticatedLauncher {
    pub identity: UserIdentity,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<Lines<BufReader<ChildStdout>>>,
    stderr_task: Option<JoinHandle<()>>,
    pam_permit: Option<OwnedSemaphorePermit>,
    /// A native seat built for this desktop, kept here until the desktop
    /// is handed over: when opening fails, it is taken apart only after the
    /// launcher (and with it the session's compositor) has stopped.
    #[cfg(target_os = "linux")]
    native_pen: Option<Arc<crate::input::seat_pen::SeatPen>>,
    session_log_id: Arc<Mutex<CorrelationId>>,
    timezone: Option<IanaTimeZone>,
    deskside: crate::deskside::LinuxDesksideConfig,
    desk_privacy: arcen_session::console_session::desk_privacy::DeskPrivacyPolicy,
    multi_monitor_plan: Option<MultiHeadPlanMsg>,
    session_display: Option<SessionDisplayMsg>,
    stream_cursor: Option<super::wayland::StreamCursorMode>,
    wayland_fidelity_route: Option<String>,
    /// Lab (`wayland.lab_seat_pen`): a desktop this sign-in creates gets its
    /// own pen on its own seat.
    lab_seat_pen: bool,
    /// This launcher was asked to join the console: if it fails, it may
    /// have been stopped before handing the console back.
    console_open_sent: bool,
    /// The Pier-wide console gate while this launcher joins a console.
    #[cfg(target_os = "linux")]
    console_gate: Option<super::console::ConsoleGate>,
    /// How a desktop this sign-in creates gets its seat
    /// (`wayland.session_mode`).
    desktop_seat_plan: arcen_session::native_seat::DesktopSeatPlan,
    /// What the desktop's compositor does to its seat pen's pressure,
    /// resolved when the seat pen is built.
    seat_pen_pressure: crate::input::pressure_curve::SeatPenPressurePlan,
    x11_desktop_depth: Option<arcen_media::video::DesktopDepth>,
    provider: arcen_session::desktop_provider::DesktopProvider,
    audio: AgentAudioArguments,
    desktop_mode: arcen_session::console_session::DesktopMode,
    /// A host that creates its own Wayland desktops found the account's own
    /// proven Wayland session: this sign-in joins it instead (S4b).
    joins_wayland_console: bool,
}

impl AuthenticatedLauncher {
    pub fn session_log_id(&self) -> CorrelationId {
        self.session_log_id
            .lock()
            .expect("session log id lock")
            .clone()
    }

    pub fn timezone(&self) -> Option<&IanaTimeZone> {
        self.timezone.as_ref()
    }

    pub fn set_timezone(&mut self, timezone: Option<IanaTimeZone>) {
        self.timezone = timezone;
    }

    /// Attaches (or clears) the committed `multi_monitor_v1` topology this
    /// launcher's privileged subprocess must render as a generated
    /// multi-head Xorg configuration for its `open` call, in place of the
    /// existing single-head template substitution.
    ///
    /// Must be called before [`AuthenticatedLauncher::open`] to take effect;
    /// `None` (the default) preserves today's single-monitor behavior
    /// exactly.
    pub fn set_multi_monitor_plan(
        &mut self,
        plan: Option<&crate::display::topology::LinuxTopologyPlan>,
    ) {
        self.multi_monitor_plan = plan.map(MultiHeadPlanMsg::from_plan);
    }

    /// Attaches the Deck display a single-head session stands for, so its
    /// head gets that display's EDID. Must be called before
    /// [`AuthenticatedLauncher::open`] to take effect.
    pub(crate) fn set_session_display(&mut self, display: Option<SessionDisplayMsg>) {
        self.session_display = display;
    }

    /// Sets how a headless Wayland desktop opened by this launcher records
    /// its cursor. Must be called before [`AuthenticatedLauncher::open`].
    pub(crate) fn set_stream_cursor_mode(&mut self, mode: super::wayland::StreamCursorMode) {
        self.stream_cursor = Some(mode);
    }

    /// The outputs this sign-in asks a headless Wayland desktop for: a
    /// committed virtual-output topology's (primary first, placed), or the
    /// one served display at the origin; `None` when it named neither.
    #[must_use]
    pub(crate) fn requested_wayland_outputs(
        &self,
    ) -> Option<Vec<arcen_session::wayland_outputs::PlacedOutput>> {
        if let Some(plan) = self.multi_monitor_plan.clone() {
            let plan = plan.into_plan().ok()?;
            if plan.backend != crate::display::topology::LinuxTopologyBackend::VirtualOutputs {
                return None;
            }
            return plan
                .placed_outputs()
                .ok()
                .map(|outputs| outputs.into_iter().map(|(_, placed)| placed).collect());
        }
        let display = self.session_display.as_ref()?;
        arcen_session::wayland_session::VirtualOutputRequest::new(
            display.width,
            display.height,
            display.refresh_hz,
        )
        .ok()
        .map(|request| {
            vec![
                arcen_session::wayland_outputs::PlacedOutput::at_origin(request)
                    .with_density(display.density()),
            ]
        })
    }

    /// Starts a headless Wayland session's compositor on `route` to a wide
    /// composition (`force-hdr`), for a Grading or HDR session. Must be
    /// called before [`AuthenticatedLauncher::open`] to take effect.
    pub(crate) fn set_wayland_fidelity_route(&mut self, route: Option<&str>) {
        self.wayland_fidelity_route = route.map(str::to_owned);
    }

    /// Lab (`wayland.lab_seat_pen`): a headless Wayland desktop this
    /// sign-in creates gets its own pen ([`crate::input::seat_pen::SeatPen`])
    /// and its compositor runs libinput on the pen's seat. A reattachment
    /// keeps the desktop's own pen, or none. Must be called before
    /// [`AuthenticatedLauncher::open`].
    pub(crate) fn set_lab_seat_pen(&mut self, enabled: bool) {
        self.lab_seat_pen = enabled;
    }

    /// How a Wayland desktop this sign-in creates gets its seat
    /// (`wayland.session_mode`, [`arcen_session::native_seat::plan_desktop_seat`]).
    /// A native desktop gets its own seat
    /// ([`crate::input::seat_pen::SeatPen::create_native`]): its logind
    /// session opens on that seat and its stock compositor runs natively
    /// there, with the desktop's pen as a libinput tablet. Must be called
    /// before [`AuthenticatedLauncher::open`].
    pub(crate) fn set_desktop_seat_plan(
        &mut self,
        plan: arcen_session::native_seat::DesktopSeatPlan,
    ) {
        self.desktop_seat_plan = plan;
    }

    /// What the compositor of a Wayland desktop this sign-in creates does
    /// to its seat pen's pressure (from the qualified compositor version and
    /// its prefix's pressure-curve marker), so the pen writes the pressure
    /// its applications see as sent. Must be called before
    /// [`AuthenticatedLauncher::open`].
    pub(crate) fn set_seat_pen_pressure(
        &mut self,
        pressure: crate::input::pressure_curve::SeatPenPressurePlan,
    ) {
        self.seat_pen_pressure = pressure;
    }

    /// Creates a dedicated Xorg desktop at `depth` (a headless Wayland
    /// desktop ignores it). Must be called before
    /// [`AuthenticatedLauncher::open`] to take effect; a reattachment keeps
    /// the depth the desktop was created at.
    pub(crate) fn set_x11_desktop_depth(&mut self, depth: arcen_media::video::DesktopDepth) {
        self.x11_desktop_depth = Some(depth);
    }

    pub async fn authenticate(
        config: &LauncherConfig,
        username: &str,
        password: Zeroizing<String>,
        remote_host: &str,
        pam_permit: OwnedSemaphorePermit,
        session_log_id: CorrelationId,
    ) -> Result<Self, LauncherError> {
        if !config.binary.is_file() {
            return Err(LauncherError::BinaryUnavailable);
        }
        let mut command = crate::command_for_helper(&config.binary, "session-launcher");
        command
            .arg("--pam-service")
            .arg(&config.pam_service)
            .arg("--display")
            .arg(&config.display)
            .arg("--gpu-head")
            .arg(&config.gpu_head)
            .arg("--xorg-bin")
            .arg(&config.xorg_binary)
            .arg("--xorg-config-template")
            .arg(&config.xorg_config_template)
            .arg("--runtime-root")
            .arg(&config.runtime_root)
            .arg("--agent-bin")
            .arg(&config.agent_binary)
            .arg("--desktop-session")
            .arg(&config.desktop)
            .args(
                config
                    .default_desktop
                    .as_ref()
                    .map(|desktop| ["--default-desktop", desktop.as_str()])
                    .into_iter()
                    .flatten(),
            )
            .arg("--desktop-provider")
            .arg(config.provider.token())
            .args(config.x11_apps.then_some(super::wayland::LAUNCHER_X11_FLAG))
            .arg(super::wayland::FIRST_LOGIN_SETUP_FLAG)
            .arg(config.first_login_setup.token())
            .arg("--session-log-id")
            .arg(session_log_id.as_str())
            .env("ARCEN_SESSION_LOG_ID", session_log_id.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
            // SAFETY: the launcher keeps root credentials, so no later
            // credential transition clears this parent-death signal.
            unsafe {
                command
                    .as_std_mut()
                    .pre_exec(super::identity::arm_parent_death_signal);
            }
        }
        let mut child = command.spawn()?;
        let mut stdin = child.stdin.take().ok_or(LauncherError::Protocol)?;
        let stdout = child.stdout.take().ok_or(LauncherError::Protocol)?;
        let stderr = child.stderr.take().ok_or(LauncherError::Protocol)?;
        let session_log_id = Arc::new(Mutex::new(session_log_id));
        let stderr_task = tokio::spawn(read_stderr(stderr, Arc::clone(&session_log_id)));

        let request = AuthenticateRequest {
            command: "authenticate",
            username,
            password: password.as_str(),
            remote_host,
        };
        // One allocation sized for the worst-case escaping, so serializing
        // never reallocates and leaves a stray copy of the password behind.
        let mut request_json = Zeroizing::new(Vec::with_capacity(authenticate_request_capacity(
            username,
            &password,
            remote_host,
        )));
        serde_json::to_writer(&mut *request_json, &request).map_err(|_| LauncherError::Protocol)?;
        request_json.push(b'\n');
        stdin.write_all(&request_json).await?;
        stdin.flush().await?;

        let mut stdout = BufReader::new(stdout).lines();
        let response = read_response(&mut stdout, AUTH_TIMEOUT).await?;
        match response.status.as_str() {
            "authenticated" => Ok(Self {
                identity: response.identity(),
                child: Some(child),
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr_task: Some(stderr_task),
                pam_permit: Some(pam_permit),
                #[cfg(target_os = "linux")]
                native_pen: None,
                session_log_id,
                timezone: None,
                deskside: config.deskside.clone(),
                desk_privacy: config.desk_privacy,
                multi_monitor_plan: None,
                session_display: None,
                stream_cursor: None,
                wayland_fidelity_route: None,
                lab_seat_pen: false,
                console_open_sent: false,
                #[cfg(target_os = "linux")]
                console_gate: None,
                desktop_seat_plan: arcen_session::native_seat::DesktopSeatPlan::Headless {
                    fallback: None,
                },
                seat_pen_pressure: crate::input::pressure_curve::SeatPenPressurePlan::linear(),
                x11_desktop_depth: None,
                provider: config.provider,
                audio: config.audio.clone(),
                desktop_mode: config.desktop_mode,
                joins_wayland_console: false,
            }),
            "rejected" => Err(LauncherError::Rejected),
            status => arcen_session::console_session::ConsoleNotice::from_status(status, None)
                .map_or(Err(LauncherError::Protocol), |notice| {
                    Err(LauncherError::Console(notice))
                }),
        }
    }

    /// Builds this desktop's native seat when its plan asks for one: the
    /// pen on the seat and whether the desktop is native. A failure falls
    /// back to headless with the reason logged, or refuses the desktop when
    /// the mode requires a native seat. A native desktop's logind session
    /// sits on its Arcen seat, so polkit's `state` must allow one (Arcen's
    /// rule installed, polkit >= 0.106) before the seat is built.
    #[cfg(target_os = "linux")]
    async fn native_seat(
        &self,
        polkit: arcen_session::polkit_remote::PolkitState,
    ) -> Result<(Option<Arc<crate::input::seat_pen::SeatPen>>, bool), LauncherError> {
        use arcen_session::native_seat::{after_seat_failure, polkit_blocker, DesktopSeatPlan};
        let mut plan = self.desktop_seat_plan;
        if let (DesktopSeatPlan::Native { required }, Some(reason)) =
            (plan, NATIVE_SEATS_DISABLED.get())
        {
            plan = after_seat_failure(required, *reason);
        }
        if let (DesktopSeatPlan::Native { required }, Some(reason)) = (plan, polkit_blocker(polkit))
        {
            plan = after_seat_failure(required, reason);
        }
        if let DesktopSeatPlan::Native { required } = plan {
            let uid = self.identity.uid;
            let pressure = self.seat_pen_pressure.clone();
            #[allow(
                clippy::disallowed_methods,
                reason = "awaited to completion, never under a timeout; the seat build is bounded (udevadm settle --timeout, SEAT_TIMEOUT polls)"
            )]
            let created = tokio::task::spawn_blocking(move || {
                crate::input::seat_pen::SeatPen::create_native(uid, pressure.resolve())
            })
            .await;
            match created {
                Ok(Ok(pen)) => return Ok((Some(pen), true)),
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: target::SESSION,
                        reason = error.reason.token(),
                        detail = %error.detail,
                        required,
                        "this Wayland desktop cannot have its own seat"
                    );
                    plan = after_seat_failure(required, error.reason);
                }
                Err(_) => {
                    plan = after_seat_failure(
                        required,
                        arcen_session::native_seat::NativeSeatUnavailable::SeatSetupFailed,
                    );
                }
            }
        }
        match plan {
            DesktopSeatPlan::Refuse(reason) => {
                tracing::warn!(
                    target: target::SESSION,
                    reason = reason.token(),
                    explanation = reason.explanation(),
                    "wayland.session_mode = native_seat: the desktop is refused"
                );
                Err(LauncherError::NativeSeatUnavailable(reason))
            }
            DesktopSeatPlan::Headless {
                fallback: Some(reason),
            } => {
                tracing::info!(
                    target: target::SESSION,
                    session_mode = "auto",
                    reason = reason.token(),
                    explanation = reason.explanation(),
                    "this Wayland desktop is headless (no real pen)"
                );
                Ok((None, false))
            }
            DesktopSeatPlan::Headless { fallback: None } | DesktopSeatPlan::Native { .. } => {
                Ok((None, false))
            }
        }
    }

    pub async fn open(mut self) -> Result<SessionLauncher, LauncherError> {
        let result = self.open_inner().await;
        if result.is_err() {
            self.shutdown().await;
        }
        result
    }

    /// Whether this sign-in joins the user's console session (ADR 0025):
    /// `platform.session.mode = console` on an Xorg host. A headless Wayland
    /// provider keeps creating its own desktops.
    pub(crate) fn joins_console(&self) -> bool {
        self.joins_wayland_console
            || (self.desktop_mode == arcen_session::console_session::DesktopMode::Console
                && self.provider == arcen_session::desktop_provider::DesktopProvider::Xorg)
    }

    /// Whether this sign-in is served by a Wayland desktop of the Pier's
    /// own (the headless or native-seat provider).
    pub(crate) fn creates_wayland_desktop(&self) -> bool {
        !self.joins_wayland_console
            && self.provider == arcen_session::desktop_provider::DesktopProvider::Wayland
    }

    /// This sign-in joins the account's own proven Wayland session as a
    /// console session, on a host that would otherwise create a desktop.
    pub(crate) fn join_wayland_console(&mut self) {
        self.joins_wayland_console = true;
    }

    /// Whether [`Self::join_wayland_console`] switched this sign-in.
    pub(crate) const fn joins_wayland_console(&self) -> bool {
        self.joins_wayland_console
    }

    /// Joins the user's console session through the privileged launcher:
    /// no PAM session, no X server and no desktop of Arcen's own.
    async fn open_console(&mut self) -> Result<SessionLauncher, LauncherError> {
        // Held until this launcher is cleaned up: the Pier's retries of an
        // unsecured console never run beside a console launcher. A deferred
        // layout restore stands aside from here on (after the gate, so a
        // recovery in progress cannot arm one under this attachment).
        #[cfg(target_os = "linux")]
        {
            let gate = super::console::console_gate().await;
            gate.held_by(self.child.as_ref().and_then(Child::id));
            self.console_gate = Some(gate);
            super::console_layout::attachment_starting();
        }
        let stdin = self.stdin.as_mut().ok_or(LauncherError::Protocol)?;
        // The committed plan travels too: the console lays it on its own
        // outputs while the Deck is attached (ADR 0025 §6).
        let mut request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            self.multi_monitor_plan.clone(),
            self.session_display.clone(),
            &self.audio,
            None,
        );
        request.console = true;
        request.desk_live = !self.desk_privacy.enabled;
        // A joined Wayland session's streams report the cursor as the
        // Deck asked.
        request.stream_cursor = self.stream_cursor;
        self.console_open_sent = true;
        let mut request_json = serde_json::to_vec(&request).map_err(|_| LauncherError::Protocol)?;
        request_json.push(b'\n');
        stdin.write_all(&request_json).await?;
        stdin.flush().await?;
        let response = read_response(
            self.stdout.as_mut().ok_or(LauncherError::Protocol)?,
            CONSOLE_OPEN_TIMEOUT,
        )
        .await?;
        // A joined Wayland session (S4b) answers with its own monitors'
        // streams in the account's runtime directory, and nothing else.
        let joined_wayland = response
            .wayland
            .as_ref()
            .filter(|wayland| wayland.is_consistent_joined(self.identity.uid));
        // A sign-in switched to join the account's Wayland session is
        // served only that session.
        if response.status == "ready" && self.joins_wayland_console && joined_wayland.is_none() {
            return Err(LauncherError::Console(
                arcen_session::console_session::ConsoleRefusal::SessionChanged.into(),
            ));
        }
        if response.status != "ready"
            || response.username != self.identity.username
            || response.uid != self.identity.uid
            || (response.wayland.is_some() && joined_wayland.is_none())
        {
            return Err(open_response_error(
                response.status.as_str(),
                response.console_service.clone(),
            ));
        }
        let pam_environment = vec![
            ("XDG_SESSION_ID".into(), response.session_id.clone()),
            (
                "XDG_SEAT".into(),
                arcen_session::console_session::CONSOLE_SEAT.into(),
            ),
            (
                "ARCEN_SESSION_LOG_ID".into(),
                self.session_log_id
                    .lock()
                    .expect("session log id lock")
                    .to_string(),
            ),
        ];
        let environment = match joined_wayland {
            Some(wayland) => {
                let handoff = wayland
                    .primary_handoff()
                    .map_err(|_| LauncherError::Protocol)?;
                SessionEnvironment::build_wayland(
                    &self.identity,
                    &response.desktop,
                    None,
                    pam_environment,
                )
                .with_wayland_session(wayland, &handoff)
                .with_session_audio(response.audio.as_ref())
            }
            None => SessionEnvironment::build(
                &self.identity,
                &response.display,
                (!response.xauthority.is_empty()).then_some(response.xauthority.as_str()),
                &response.desktop,
                None,
                pam_environment,
            )
            .with_session_audio(response.audio.as_ref()),
        };
        let depth = response
            .x11_root_depth
            .and_then(arcen_media::video::DesktopDepth::from_x11_depth)
            .unwrap_or(arcen_media::video::DesktopDepth::Eight);
        tracing::info!(
            target: target::SESSION,
            session_id = %response.session_id,
            display = %response.display,
            origin = response.console_origin.as_deref().unwrap_or("unknown"),
            signed_in_through_login_screen = response.console_signed_in,
            desktop = response.console_desktop.as_deref().unwrap_or("unknown"),
            default_desktop = response.console_default_desktop.as_deref().unwrap_or("not_applicable"),
            layout = response.console_layout.as_deref().unwrap_or("not_requested"),
            root_depth = ?response.x11_root_depth,
            "joined the user's console session"
        );
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        let writer = tokio::spawn(write_console_commands(
            self.stdin.take().ok_or(LauncherError::Protocol)?,
            self.stdout.take().ok_or(LauncherError::Protocol)?,
            receiver,
        ));
        Ok(SessionLauncher {
            identity: self.identity.clone(),
            environment,
            x11_desktop_depth: joined_wayland.is_none().then_some(depth),
            wayland: joined_wayland.cloned(),
            audio: response.audio,
            agent_pid: response.agent_pid,
            child: self.child.take().ok_or(LauncherError::Protocol)?,
            stdin: None,
            stderr_task: self.stderr_task.take().ok_or(LauncherError::Protocol)?,
            _pam_permit: self.pam_permit.take().ok_or(LauncherError::Protocol)?,
            session_log_id: Arc::clone(&self.session_log_id),
            #[cfg(target_os = "linux")]
            seat_pen: None,
            console: Some(ConsoleLink { commands, writer }),
            console_origin: response.console_origin.clone(),
            console_service: response.console_service.clone(),
            #[cfg(target_os = "linux")]
            _console_gate: self.console_gate.take(),
            console_layout_applied: matches!(
                response.console_layout.as_deref(),
                Some(CONSOLE_LAYOUT_APPLIED | CONSOLE_LAYOUT_SHOWN)
            ),
            console_guest: response.console_origin.as_deref()
                == Some(arcen_session::console_session::ConsoleOrigin::OtherProduct.token()),
            console_shown_outputs: response
                .console_outputs
                .clone()
                .filter(|_| response.console_layout.as_deref() == Some(CONSOLE_LAYOUT_SHOWN)),
            console_shown: response
                .console_shown
                .clone()
                .filter(|_| response.console_layout.as_deref() == Some(CONSOLE_LAYOUT_SHOWN)),
        })
    }

    async fn open_inner(&mut self) -> Result<SessionLauncher, LauncherError> {
        if self.joins_console() {
            return self.open_console().await;
        }
        // polkit's state is read before anything is built: it decides
        // whether this desktop may have a seat at all (below), and whether
        // a native desktop may have its Arcen seat.
        #[cfg(target_os = "linux")]
        #[allow(
            clippy::disallowed_methods,
            reason = "polkit tools are bounded by their own timeout (polkit_remote::TOOL_TIMEOUT)"
        )]
        let polkit_state = tokio::task::spawn_blocking(crate::session::polkit_remote::state)
            .await
            .ok();
        // Created before the compositor starts (it opens its seat's devices
        // once) and owned by the desktop from here: dropped with it, or right
        // away if the desktop does not open.
        #[cfg(target_os = "linux")]
        let (native_pen, native_seat) = self.native_seat(polkit_state.unwrap_or_default()).await?;
        #[cfg(target_os = "linux")]
        self.native_pen.clone_from(&native_pen);
        #[cfg(not(target_os = "linux"))]
        let native_seat = false;
        #[cfg(target_os = "linux")]
        let seat_pen = if native_pen.is_some() {
            native_pen
        } else if self.lab_seat_pen {
            let uid = self.identity.uid;
            let pressure = self.seat_pen_pressure.clone();
            match tokio::task::spawn_blocking(move || {
                crate::input::seat_pen::SeatPen::create(uid, pressure.resolve())
            })
            .await
            {
                Ok(Ok(pen)) => Some(pen),
                Ok(Err(error)) => {
                    tracing::warn!(
                        target: target::INPUT,
                        %error,
                        "LAB: this desktop gets no seat pen; pen input stays on the emulated seat"
                    );
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let lab_input_seat = seat_pen.as_ref().map(|pen| pen.seat());
        #[cfg(not(target_os = "linux"))]
        let lab_input_seat = None;
        // Before anything is launched: a refusal polkit's state alone decides
        // (no rule for a seated desktop) is made while the user has no
        // session, so a refused remote user never holds a seat. An X11
        // desktop sits on the console seat and a native Wayland desktop on
        // its own Arcen seat; only a headless Wayland desktop is seatless.
        #[cfg(target_os = "linux")]
        let polkit_seat = crate::session::polkit_remote::desktop_seat(self.provider, native_seat);
        #[cfg(target_os = "linux")]
        let polkit_state = {
            let (state, refusal) = match polkit_state {
                Some(state) => (
                    state,
                    crate::session::polkit_remote::refusal_logged(state, polkit_seat),
                ),
                None => (
                    arcen_session::polkit_remote::PolkitState::Unknown,
                    (polkit_seat != arcen_session::polkit_remote::DesktopSeat::Seatless)
                        .then_some(arcen_session::polkit_remote::PolkitUnprotected::Unverified),
                ),
            };
            if let Some(reason) = refusal {
                return Err(LauncherError::PolkitUnprotected(reason));
            }
            state
        };
        let stdin = self.stdin.as_mut().ok_or(LauncherError::Protocol)?;
        let request = OpenRequest::new(
            self.timezone.clone(),
            self.deskside.clone(),
            self.multi_monitor_plan.clone(),
            self.session_display.clone(),
            &self.audio,
            self.wayland_fidelity_route.clone(),
        )
        .with_stream_cursor(self.stream_cursor)
        .with_x11_desktop_depth(self.x11_desktop_depth)
        .with_lab_input_seat(lab_input_seat)
        .with_native_seat(native_seat);
        let mut request_json = serde_json::to_vec(&request).map_err(|_| LauncherError::Protocol)?;
        if request_json.len() > MAX_OPEN_REQUEST_BYTES {
            return Err(LauncherError::Protocol);
        }
        request_json.push(b'\n');
        stdin.write_all(&request_json).await?;
        stdin.flush().await?;
        let response = read_response(
            self.stdout.as_mut().ok_or(LauncherError::Protocol)?,
            open_timeout(self.provider),
        )
        .await?;
        if response.status != "ready"
            || response.username != self.identity.username
            || response.uid != self.identity.uid
        {
            return Err(open_response_error(
                response.status.as_str(),
                response.console_service.clone(),
            ));
        }
        let mut extra_environment = vec![
            ("XDG_SESSION_ID".into(), response.session_id.clone()),
            (
                "ARCEN_SESSION_LOG_ID".into(),
                self.session_log_id
                    .lock()
                    .expect("session log id lock")
                    .to_string(),
            ),
        ];
        let environment = match response.wayland.as_ref() {
            Some(wayland) => {
                if response.session_type != "wayland" || !wayland.is_consistent() {
                    return Err(LauncherError::Protocol);
                }
                let handoff = wayland
                    .primary_handoff()
                    .map_err(|_| LauncherError::Protocol)?;
                tracing::info!(
                    target: target::SESSION,
                    compositor = %wayland.compositor,
                    runtime_dir = %wayland.runtime_dir.display(),
                    pipewire_node = handoff.node_id(),
                    output = handoff.output().as_str(),
                    compositor_render_node = ?handoff.compositor_render_node(),
                    width = handoff.size().0,
                    height = handoff.size().1,
                    "headless Wayland session ready for capture hand-off"
                );
                SessionEnvironment::build_wayland(
                    &self.identity,
                    &response.desktop,
                    self.timezone.as_ref(),
                    extra_environment,
                )
                .with_wayland_session(wayland, &handoff)
                .with_session_audio(response.audio.as_ref())
            }
            None => {
                extra_environment.extend(capture_gpu_environment(&response.capture_gpu_pci));
                SessionEnvironment::build(
                    &self.identity,
                    &response.display,
                    (!response.xauthority.is_empty()).then_some(response.xauthority.as_str()),
                    &response.desktop,
                    self.timezone.as_ref(),
                    extra_environment,
                )
                .with_session_audio(response.audio.as_ref())
            }
        };
        if self.child.is_none()
            || self.stdin.is_none()
            || self.stderr_task.is_none()
            || self.pam_permit.is_none()
        {
            return Err(LauncherError::Protocol);
        }
        // The verified depth of an Xorg desktop; a Wayland desktop has none.
        let x11_desktop_depth = if response.wayland.is_some() {
            None
        } else {
            let depth = response
                .x11_root_depth
                .and_then(arcen_media::video::DesktopDepth::from_x11_depth);
            if depth.is_none() {
                tracing::warn!(
                    target: target::SESSION,
                    root_depth = ?response.x11_root_depth,
                    "the dedicated Xorg's root depth is not one this host serves ten-bit from; the desktop is treated as eight-bit"
                );
            }
            Some(depth.unwrap_or(arcen_media::video::DesktopDepth::Eight))
        };
        // The desktop's session is active now: it is handed over only when
        // polkit refuses it the console's grants (an X11 session has the
        // console seat, a native Wayland session its own Arcen seat).
        // Between activation and this answer the session's own processes run
        // with whatever polkit gives them; with the rule installed (checked
        // before launch) that is the remote answer, and a refusal here kills
        // the desktop's processes at once, before the launcher tears the
        // session down (and releases a native seat).
        #[cfg(target_os = "linux")]
        {
            use arcen_session::polkit_remote::{Admission, DesktopSeat};
            let seat = polkit_seat;
            let pid = response.agent_pid;
            let session_id = response.session_id.clone();
            #[allow(
                clippy::disallowed_methods,
                reason = "polkit tools are bounded by their own timeout (polkit_remote::TOOL_TIMEOUT)"
            )]
            let decision = tokio::task::spawn_blocking(move || {
                crate::session::polkit_remote::prove_desktop(pid, &session_id, seat, polkit_state)
            })
            .await
            .unwrap_or(if seat == DesktopSeat::Seatless {
                Admission::AdmitUnverified
            } else {
                Admission::Refuse(arcen_session::polkit_remote::PolkitUnprotected::Unverified)
            });
            if let Admission::Refuse(reason) = decision {
                if response.agent_pid > 1 {
                    signal_process_group(response.agent_pid, nix::sys::signal::Signal::SIGKILL);
                }
                // A native seat polkit could not keep remote stays off for
                // this Pier's later desktops (headless under `auto`,
                // refused under `native_seat`).
                if native_seat {
                    disable_native_seats(reason.into());
                }
                return Err(LauncherError::PolkitUnprotected(reason));
            }
            // The desktop owns its native seat from here (released with it).
            self.native_pen = None;
        }
        Ok(SessionLauncher {
            identity: self.identity.clone(),
            environment,
            x11_desktop_depth,
            wayland: response.wayland,
            audio: response.audio,
            agent_pid: response.agent_pid,
            child: self.child.take().expect("launcher child checked"),
            stdin: self.stdin.take(),
            stderr_task: self.stderr_task.take().expect("stderr task checked"),
            _pam_permit: self.pam_permit.take().expect("PAM permit checked"),
            session_log_id: Arc::clone(&self.session_log_id),
            #[cfg(target_os = "linux")]
            seat_pen,
            console: None,
            console_origin: None,
            console_service: None,
            #[cfg(target_os = "linux")]
            _console_gate: None,
            console_layout_applied: false,
            console_guest: false,
            console_shown_outputs: None,
            #[cfg(target_os = "linux")]
            console_shown: None,
        })
    }

    pub async fn discard(mut self) {
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            close_and_stop_launcher(&mut self.stdin, &mut child).await;
        }
        self.stdin.take();
        self.stdout.take();
        if let Some(mut task) = self.stderr_task.take() {
            finish_stderr(&mut task).await;
        }
        self.pam_permit.take();
        // A console open that failed may have been stopped before it
        // handed the console back: lock and restore from its journal. Only
        // a launcher that was asked to join (a discarded sign-in that
        // reattached a held console must not touch that console's journal).
        #[cfg(target_os = "linux")]
        if self.console_open_sent {
            super::console::recover_console().await;
        }
        #[cfg(target_os = "linux")]
        if let Some(pen) = self.native_pen.take() {
            release_native_seat(pen).await;
        }
    }
}

impl Drop for AuthenticatedLauncher {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        if let Some(task) = self.stderr_task.as_ref() {
            task.abort();
        }
    }
}

pub struct SessionLauncher {
    pub identity: UserIdentity,
    pub environment: SessionEnvironment,
    /// The headless Wayland session's outputs, streams and input socket;
    /// `None` for dedicated Xorg.
    pub wayland: Option<super::wayland::WaylandAgentReady>,
    /// The depth the dedicated Xorg desktop runs at, as its server reported
    /// it once ready (an unknown depth is eight-bit: nothing proves more).
    /// `None` for a headless Wayland desktop.
    pub x11_desktop_depth: Option<arcen_media::video::DesktopDepth>,
    /// The session's audio, as its agent established it.
    pub audio: Option<super::agent::SessionAudioReady>,
    pub agent_pid: u32,
    child: Child,
    stdin: Option<ChildStdin>,
    stderr_task: JoinHandle<()>,
    _pam_permit: OwnedSemaphorePermit,
    session_log_id: Arc<Mutex<CorrelationId>>,
    /// Lab (`wayland.lab_seat_pen`): this desktop's own pen, dropped (grant
    /// revoked, device destroyed, rule removed) with the desktop.
    #[cfg(target_os = "linux")]
    pub seat_pen: Option<Arc<crate::input::seat_pen::SeatPen>>,
    /// A joined console session's command line to its launcher; `None`
    /// for a desktop the launcher created.
    console: Option<ConsoleLink>,
    /// Who started the joined console session (`ConsoleOrigin::token`).
    console_origin: Option<String>,
    /// The other remote-desktop service that started it, as the host
    /// reported it.
    console_service: Option<arcen_session::console_session::ForeignService>,
    /// The Pier-wide console gate, held until this launcher is cleaned up
    /// (`console::console_gate`).
    #[cfg(target_os = "linux")]
    _console_gate: Option<super::console::ConsoleGate>,
    /// The joined console took the committed multi-monitor plan's layout,
    /// or showed it as it is (a guest console whose layout is the Deck's).
    console_layout_applied: bool,
    /// The joined console belongs to another remote-desktop product: it is
    /// shown as it is and never changed (ADR 0025 §8).
    console_guest: bool,
    /// Such a console's outputs, one per committed monitor, in plan order.
    console_shown_outputs: Option<Vec<String>>,
    /// Such a console's mapping of the committed plan's monitors onto the
    /// outputs it shows ([`Self::fit_committed_plan`]).
    #[cfg(target_os = "linux")]
    console_shown: Option<super::console_layout::GuestLayout>,
}

/// [`LauncherResponse::console_layout`] when the console took the layout.
const CONSOLE_LAYOUT_APPLIED: &str = "applied";
/// [`LauncherResponse::console_layout`] when a console shown as it is
/// already has the Deck's layout: nothing is changed, each monitor is
/// captured from the output that shows it.
const CONSOLE_LAYOUT_SHOWN: &str = "shown";

/// The layout a joined console takes while the Deck is attached (ADR 0025
/// §6): a committed multi-monitor plan's monitors; on a host whose console
/// runs the installer's forced virtual outputs, a single Deck display as
/// one head too. `None`: the console keeps its own layout.
#[cfg(target_os = "linux")]
fn console_layout_requests(
    open: &OpenRequest,
) -> Option<Vec<arcen_outputs::console_layout::ConsoleHeadRequest>> {
    use super::console_layout;
    if let Some(plan) = open.multi_monitor.clone() {
        let plan = plan.into_plan().ok()?;
        if plan.backend == crate::display::topology::LinuxTopologyBackend::ConsoleOutputs {
            return console_layout::requests_from_plan(&plan);
        }
        return None;
    }
    let display = open.display.as_ref()?;
    console_layout::virtual_outputs_installed().then(|| {
        vec![console_layout::request_for_display(
            display.width,
            display.height,
            display.refresh_hz,
        )]
    })
}

/// What the broker tells a console launcher while the session is joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsoleCommand {
    /// The Deck left (transport lost, held for its resume): hand the
    /// console back locked.
    Detach,
    /// The Deck is back: make the console private again and unlock it.
    Attach,
}

impl ConsoleCommand {
    const fn token(self) -> &'static str {
        match self {
            Self::Detach => "detach",
            Self::Attach => "attach",
        }
    }
}

/// How long the broker waits for a console launcher to say whether a
/// returning Deck is attached: bringing a session forward (5 s), blanking,
/// and a hand-back that has to secure the session (lock 4 s, a login
/// screen 4 s) come first.
const CONSOLE_ATTACH_TIMEOUT: Duration = Duration::from_secs(25);

/// What a returning Deck's console attachment came to: attached, or the
/// typed refusal the Deck is shown.
pub(crate) type ConsoleAttachOutcome = Result<(), arcen_session::console_session::ConsoleNotice>;

struct ConsoleRequest {
    command: ConsoleCommand,
    reply: Option<tokio::sync::oneshot::Sender<ConsoleAttachOutcome>>,
}

struct ConsoleLink {
    commands: tokio::sync::mpsc::UnboundedSender<ConsoleRequest>,
    writer: JoinHandle<()>,
}

/// The answer a console launcher gives to `attach`: `attached`, or a
/// console refusal (naming the service when another one holds the
/// workstation). Anything else, silence or a closed pipe is a session
/// that could not be reached.
fn console_attach_outcome(line: Option<&str>) -> ConsoleAttachOutcome {
    use arcen_session::console_session::{ConsoleNotice, ConsoleRefusal};
    let response = line.and_then(|line| serde_json::from_str::<LauncherResponse>(line).ok());
    match response {
        Some(response) if response.status == "attached" => Ok(()),
        Some(response) => Err(ConsoleNotice::from_status(
            &response.status,
            response.console_service,
        )
        .unwrap_or_else(|| ConsoleRefusal::SessionUnreachable.into())),
        None => Err(ConsoleRefusal::SessionUnreachable.into()),
    }
}

/// Writes console commands to the launcher and reads its answer to each
/// `attach`; when the broker lets go of the link it says `close`, which
/// hands the console back and ends the launcher (never the session).
async fn write_console_commands(
    mut stdin: ChildStdin,
    mut stdout: Lines<BufReader<ChildStdout>>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<ConsoleRequest>,
) {
    // After an answer that never came, a late one must not be taken for
    // the next attachment's: every later attachment is refused.
    let mut answers_lost = false;
    while let Some(ConsoleRequest { command, reply }) = commands.recv().await {
        let line = format!("{{\"command\":\"{}\"}}\n", command.token());
        if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
            if let Some(reply) = reply {
                let _ = reply.send(console_attach_outcome(None));
            }
            return;
        }
        let Some(reply) = reply else {
            continue;
        };
        let outcome = if answers_lost {
            console_attach_outcome(None)
        } else {
            match tokio::time::timeout(CONSOLE_ATTACH_TIMEOUT, stdout.next_line()).await {
                Ok(Ok(line)) => console_attach_outcome(line.as_deref()),
                Ok(Err(_)) | Err(_) => {
                    answers_lost = true;
                    console_attach_outcome(None)
                }
            }
        };
        let _ = reply.send(outcome);
    }
    let _ = stdin.write_all(b"{\"command\":\"close\"}\n").await;
    let _ = stdin.shutdown().await;
}

impl SessionLauncher {
    /// Whether this is a joined console session (ADR 0025).
    pub fn is_console(&self) -> bool {
        self.console.is_some()
    }

    /// Whether a committed multi-monitor plan is served as planned: always
    /// for a desktop the launcher created; for a joined console only when
    /// the console took the plan's layout (otherwise it is one picture).
    pub(crate) fn serves_committed_monitors(&self) -> bool {
        self.console.is_none() || self.console_layout_applied
    }

    /// Whether this joined console belongs to another remote-desktop
    /// product (shown as it is, never changed).
    pub(crate) fn is_console_guest(&self) -> bool {
        self.console.is_some() && self.console_guest
    }

    /// The outputs a console shown as it is captures, in plan order.
    pub(crate) fn console_shown_outputs(&self) -> Option<Vec<String>> {
        self.console_shown_outputs.clone()
    }

    /// The committed plan as the joined console serves it: unchanged,
    /// except on another product's desktop, which is never laid out for the
    /// Deck: there each Deck monitor takes the output that shows it
    /// (`console_layout::GuestLayout::fit`). `None` when that mapping does
    /// not fit the plan (the sign-in then ends with the reason).
    pub(crate) fn fit_committed_plan(
        &self,
        plan: crate::display::topology::LinuxTopologyPlan,
    ) -> Option<crate::display::topology::LinuxTopologyPlan> {
        #[cfg(target_os = "linux")]
        if let Some(shown) = self.console_shown.as_ref() {
            return match shown.fit(&plan) {
                Ok(fitted) => {
                    tracing::info!(
                        target: target::SESSION,
                        mapping = %shown.note,
                        monitors = fitted.monitors.len(),
                        requested_monitors = plan.monitors.len(),
                        screen_w = fitted.virtual_width,
                        screen_h = fitted.virtual_height,
                        "the Deck's monitors are served as another product's desktop shows them"
                    );
                    Some(fitted)
                }
                Err(error) => {
                    tracing::error!(
                        target: target::SESSION,
                        %error,
                        mapping = %shown.note,
                        "the Deck's monitors could not be served as another product's desktop shows them"
                    );
                    None
                }
            };
        }
        Some(plan)
    }

    /// How a console shown as it is maps the Deck's monitors, in words.
    pub(crate) fn console_layout_note(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        if let Some(shown) = self.console_shown.as_ref() {
            return Some(shown.note.clone());
        }
        None
    }

    /// What another account is told while this joined console session is
    /// held for its Deck: the console belongs to this session's user
    /// (through another product when the display manager did not start
    /// it). `None` for a desktop the launcher created.
    pub(crate) fn console_refusal_for_another_user(
        &self,
    ) -> Option<arcen_session::console_session::ConsoleNotice> {
        use arcen_session::console_session::{ConsoleNotice, ConsoleOrigin, ConsoleRefusal};
        self.console.as_ref()?;
        let origin = if self.console_origin.as_deref() == Some(ConsoleOrigin::OtherProduct.token())
        {
            ConsoleOrigin::OtherProduct
        } else {
            ConsoleOrigin::DisplayManager
        };
        Some(ConsoleNotice::naming(
            ConsoleRefusal::other_user(origin),
            self.console_service.clone(),
        ))
    }

    /// Tells a console launcher that the Deck came back; the receiver
    /// answers once the launcher made the console private again and
    /// unlocked it, or with the typed reason it did not. `None` for a
    /// desktop the launcher created.
    pub(crate) fn console_attach(
        &self,
    ) -> Option<tokio::sync::oneshot::Receiver<ConsoleAttachOutcome>> {
        let link = self.console.as_ref()?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        if link
            .commands
            .send(ConsoleRequest {
                command: ConsoleCommand::Attach,
                reply: Some(reply),
            })
            .is_err()
        {
            tracing::warn!(
                target: target::SESSION,
                "the console launcher is gone; the returning Deck is not attached"
            );
        }
        Some(answer)
    }

    /// Tells a console launcher that the Deck left; a no-op for a desktop
    /// the launcher created.
    pub(crate) fn console_command(&self, command: ConsoleCommand) {
        if let Some(link) = self.console.as_ref() {
            if link
                .commands
                .send(ConsoleRequest {
                    command,
                    reply: None,
                })
                .is_err()
            {
                tracing::warn!(
                    target: target::SESSION,
                    command = command.token(),
                    "the console launcher is gone; the console is recovered when it is cleaned up"
                );
            }
        }
    }

    pub fn execution(&self) -> UserExecution {
        UserExecution::new(self.identity.clone(), self.environment.clone())
    }

    pub fn replace_session_log_id(&self, replacement: CorrelationId) -> CorrelationId {
        std::mem::replace(
            &mut *self.session_log_id.lock().expect("session log id lock"),
            replacement,
        )
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Whether this desktop runs on its own native seat.
    pub fn has_native_seat(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.seat_pen.as_ref().is_some_and(|pen| pen.is_native())
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    pub fn agent_is_running(&self) -> bool {
        if self.console.is_some() {
            // A joined console opens no PAM session (ADR 0025 §2): its
            // agent runs as the user beside the launcher, never in the
            // session's own scope.
            return agent_pid_is_users(self.agent_pid, self.identity.uid);
        }
        agent_pid_matches(
            self.agent_pid,
            self.identity.uid,
            self.environment.session_id().unwrap_or_default(),
        )
    }

    pub async fn shutdown(mut self) -> bool {
        let console = self.console.take();
        if let Some(ConsoleLink {
            commands,
            mut writer,
        }) = console
        {
            drop(commands);
            if tokio::time::timeout(Duration::from_secs(2), &mut writer)
                .await
                .is_err()
            {
                writer.abort();
            }
            close_and_stop_launcher(&mut self.stdin, &mut self.child).await;
            finish_stderr(&mut self.stderr_task).await;
            let exited = ensure_agent_exited(self.agent_pid).await;
            // A launcher that died, or was killed before it handed the
            // console back, left its journal: lock and restore from it.
            #[cfg(target_os = "linux")]
            super::console::recover_console().await;
            return exited;
        }
        close_and_stop_launcher(&mut self.stdin, &mut self.child).await;
        finish_stderr(&mut self.stderr_task).await;
        let exited = ensure_agent_exited(self.agent_pid).await;
        #[cfg(target_os = "linux")]
        if let Some(pen) = self.seat_pen.take().filter(|pen| pen.is_native()) {
            release_native_seat(pen).await;
        }
        exited
    }
}

impl Drop for SessionLauncher {
    fn drop(&mut self) {
        if let Some(link) = self.console.take() {
            link.writer.abort();
        }
        self.stdin.take();
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}

/// Takes a native seat apart on a blocking thread (it waits for the
/// compositor to let go of `vkms`), after its desktop's session has ended.
#[cfg(target_os = "linux")]
#[allow(
    clippy::disallowed_methods,
    reason = "awaited to completion, never under a timeout; the vkms unload retries end after 15 s"
)]
async fn release_native_seat(pen: Arc<crate::input::seat_pen::SeatPen>) {
    let _ = tokio::task::spawn_blocking(move || pen.release_native_now()).await;
}

/// Set when a native desktop's session was found not to be kept from local
/// users' polkit grants: every later desktop of this Pier is headless
/// (`wayland.session_mode = auto`) or refused (`native_seat`).
#[cfg(target_os = "linux")]
static NATIVE_SEATS_DISABLED: std::sync::OnceLock<
    arcen_session::native_seat::NativeSeatUnavailable,
> = std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
fn disable_native_seats(reason: arcen_session::native_seat::NativeSeatUnavailable) {
    if NATIVE_SEATS_DISABLED.set(reason).is_ok() {
        tracing::error!(
            target: target::SESSION,
            reason = reason.token(),
            explanation = reason.explanation(),
            "native desktops are disabled until the Pier restarts; this desktop is refused, and later ones are headless (session_mode auto) or refused (session_mode native_seat)"
        );
    }
}

async fn read_response(
    stdout: &mut Lines<BufReader<ChildStdout>>,
    timeout: Duration,
) -> Result<LauncherResponse, LauncherError> {
    let line = tokio::time::timeout(timeout, stdout.next_line())
        .await
        .map_err(|_| LauncherError::Timeout)??
        .ok_or(LauncherError::Protocol)?;
    serde_json::from_str(&line).map_err(|_| LauncherError::Protocol)
}

async fn read_stderr<R>(stderr: R, session_log_id: Arc<Mutex<CorrelationId>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = crate::bounded_io::BoundedLineReader::new(BufReader::new(stderr));
    while let Ok(Some(line)) = reader.next_bounded_line().await {
        let sid = session_log_id.lock().expect("session log id lock").clone();
        if line.truncated {
            tracing::warn!(
                target: target::AUTH,
                launcher = true,
                %sid,
                "session-launcher stderr line exceeded the bounded read limit; excess bytes discarded"
            );
        }
        let text = crate::eventlog::bounded_diagnostic_line(&line.text);
        // The launcher's own errors and warnings keep their severity, so a
        // host at the `critical` profile still records why a desktop failed.
        match launcher_line_severity(&text) {
            tracing::Level::ERROR => {
                tracing::error!(target: target::AUTH, launcher = true, %sid, "{text}");
            }
            tracing::Level::WARN => {
                tracing::warn!(target: target::AUTH, launcher = true, %sid, "{text}");
            }
            _ => tracing::info!(target: target::AUTH, launcher = true, %sid, "{text}"),
        }
    }
}

/// The severity a launcher stderr line carries (its canonical JSON
/// `severity`, or a plain-text `ERROR`/`WARN` prefix); `INFO` otherwise.
fn launcher_line_severity(line: &str) -> tracing::Level {
    let lower = line.trim_start().to_ascii_lowercase();
    if lower.contains("\"severity\":\"error\"") || lower.starts_with("error") {
        tracing::Level::ERROR
    } else if lower.contains("\"severity\":\"warn\"") || lower.starts_with("warn") {
        tracing::Level::WARN
    } else {
        tracing::Level::INFO
    }
}

async fn finish_stderr(task: &mut JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(1), &mut *task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn close_and_stop_launcher(stdin: &mut Option<ChildStdin>, child: &mut Child) {
    if let Some(mut stdin) = stdin.take() {
        let _ = stdin.write_all(b"{\"command\":\"close\"}\n").await;
        let _ = stdin.shutdown().await;
    }
    if wait_for_child_exit(child, GRACEFUL_CLOSE_TIMEOUT).await {
        return;
    }
    tracing::warn!(
        target: target::SESSION,
        "session launcher did not exit after close request; forcing process-tree teardown"
    );
    terminate_child_tree(child).await;
}

async fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, child.wait()).await, Ok(Ok(_)))
}

async fn terminate_child_tree(child: &mut Child) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id() {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    if !wait_for_child_exit(child, CHILD_STOP_TIMEOUT).await {
        #[cfg(target_os = "linux")]
        if let Some(pid) = child.id() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill().await;
    }
}

#[cfg(target_os = "linux")]
async fn ensure_agent_exited(agent_pid: u32) -> bool {
    if wait_for_pid_exit(agent_pid, AGENT_EXIT_GRACE).await {
        return true;
    }

    tracing::warn!(
        target: target::SESSION,
        agent_pid,
        "session agent remained after launcher exit; running process-group fallback before timezone teardown"
    );
    signal_process_group(agent_pid, nix::sys::signal::Signal::SIGTERM);
    if wait_for_pid_exit(agent_pid, AGENT_STOP_TIMEOUT).await {
        return true;
    }
    signal_process_group(agent_pid, nix::sys::signal::Signal::SIGKILL);
    let exited = wait_for_pid_exit(agent_pid, AGENT_STOP_TIMEOUT).await;
    if !exited {
        tracing::warn!(
            target: target::SESSION,
            agent_pid,
            "session agent exit could not be proven after process-group fallback"
        );
    }
    exited
}

#[cfg(not(target_os = "linux"))]
async fn ensure_agent_exited(_agent_pid: u32) -> bool {
    true
}

#[cfg(target_os = "linux")]
async fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    if !pid_is_live(pid) {
        return true;
    }
    tokio::time::timeout(timeout, async {
        while pid_is_live(pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .is_ok()
}

#[cfg(target_os = "linux")]
fn pid_is_live(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(") ")
        .and_then(|(_, fields)| fields.as_bytes().first())
        .is_some_and(|state| *state != b'Z')
}

#[cfg(target_os = "linux")]
fn agent_pid_matches(pid: u32, expected_uid: u32, expected_session_id: &str) -> bool {
    if !safe_session_id(expected_session_id) {
        return false;
    }
    let uid_matches = agent_pid_is_users(pid, expected_uid);
    let cgroup_matches = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .is_ok_and(|cgroup| cgroup.contains(&format!("session-{expected_session_id}.scope")));
    uid_matches && cgroup_matches
}

/// Whether `pid` is alive and runs entirely as `expected_uid`.
#[cfg(target_os = "linux")]
fn agent_pid_is_users(pid: u32, expected_uid: u32) -> bool {
    if !pid_is_live(pid) {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let expected_uid = expected_uid.to_string();
    status.lines().find_map(|line| {
        let fields = line
            .strip_prefix("Uid:")?
            .split_whitespace()
            .collect::<Vec<_>>();
        Some(fields.len() == 4 && fields.iter().all(|uid| *uid == expected_uid))
    }) == Some(true)
}

#[cfg(not(target_os = "linux"))]
fn agent_pid_is_users(_pid: u32, _expected_uid: u32) -> bool {
    true
}

#[cfg(not(target_os = "linux"))]
fn agent_pid_matches(_pid: u32, _expected_uid: u32, _expected_session_id: &str) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn signal_process_group(pid: u32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal);
}

/// Takes the SELinux exec context `pam_selinux open` set on this thread so
/// the launcher's own root helpers keep its domain, and refuses the session
/// at once when the policy would not let the session user's first process
/// enter that context (see [`super::selinux`]).
#[cfg(target_os = "linux")]
pub(super) fn take_session_exec_context(
) -> Result<Option<super::selinux::SessionSecurityContext>, LauncherError> {
    use super::selinux::{
        current_context, take_pam_exec_context, transition_decision, AccessDecision,
    };

    let exec_context = take_pam_exec_context().map_err(|error| {
        tracing::error!(target: target::SESSION, %error, "the SELinux exec context PAM assigned could not be taken over");
        LauncherError::SelinuxContext
    })?;
    let Some(context) = exec_context.as_ref() else {
        return Ok(None);
    };
    let launcher_context = current_context();
    let launcher_context = launcher_context
        .as_ref()
        .map_or("unknown", super::selinux::SessionSecurityContext::as_str);
    match transition_decision(context) {
        Some(AccessDecision::Denied) => {
            tracing::error!(
                target: target::SESSION,
                launcher_context,
                session_context = %context,
                "SELinux policy does not let the launcher's domain start the session user's processes in the context PAM assigned; install Arcen's SELinux policy module (docs/security/linux-selinux.md)"
            );
            Err(LauncherError::SelinuxContext)
        }
        decision => {
            tracing::info!(
                target: target::SESSION,
                launcher_context,
                session_context = %context,
                checked = decision.is_some(),
                "the session user's processes will run in the SELinux context PAM assigned; the launcher's own helpers keep its domain"
            );
            Ok(exec_context)
        }
    }
}

/// The session user's first `execve` was refused while a PAM-assigned
/// SELinux context was being entered: the policy, not the binary.
#[cfg(target_os = "linux")]
fn agent_exec_refused_by_selinux(error: &AgentError, environment: &SessionEnvironment) -> bool {
    let refused = environment.exec_context().is_some()
        && matches!(error, AgentError::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied);
    if refused {
        tracing::error!(
            target: target::SESSION,
            session_context = environment.exec_context().map_or("", |context| context.as_str()),
            %error,
            "the session user's first process could not enter the SELinux context PAM assigned; install Arcen's SELinux policy module (docs/security/linux-selinux.md)"
        );
    }
    refused
}

pub fn find_launcher_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("ARCEN_SESSION_LAUNCHER") {
        candidates.push(path.into());
    }
    candidates.push("/opt/arcen/bin/arcen-session-launcher".into());
    candidates.push("/usr/local/libexec/arcen/arcen-session-launcher".into());
    if let Ok(mut executable) = std::env::current_exe() {
        executable.pop();
        candidates.push(executable.join("arcen-session-launcher"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(target_os = "linux")]
pub async fn run_launcher(args: &[String]) -> Result<(), LauncherError> {
    use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
    use std::ffi::{CStr, CString};

    if !nix::unistd::Uid::effective().is_root() {
        return Err(LauncherError::RootRequired);
    }
    let pam_service = required_argument(args, "--pam-service")?;
    let display = required_argument(args, "--display")?;
    let gpu_head = required_argument(args, "--gpu-head")?;
    let xorg_binary = PathBuf::from(required_argument(args, "--xorg-bin")?);
    let xorg_config_template = PathBuf::from(required_argument(args, "--xorg-config-template")?);
    let runtime_root = PathBuf::from(required_argument(args, "--runtime-root")?);
    let agent_binary = PathBuf::from(required_argument(args, "--agent-bin")?);
    let desktop = required_argument(args, "--desktop-session")?;
    let default_desktop = match argument_value(args, "--default-desktop") {
        None => None,
        Some(id) => Some(
            arcen_session::console_session::account_desktop::DesktopId::parse(&id)
                .ok_or(LauncherError::Protocol)?,
        ),
    };
    let session_log_id = CorrelationId::parse_uuid(required_argument(args, "--session-log-id")?)
        .map_err(|_| LauncherError::Protocol)?;
    let provider = match argument_value(args, "--desktop-provider") {
        None => arcen_session::desktop_provider::DesktopProvider::Xorg,
        Some(token) => arcen_session::desktop_provider::DesktopProvider::from_token(&token)
            .ok_or(LauncherError::Protocol)?,
    };
    let wayland = provider == arcen_session::desktop_provider::DesktopProvider::Wayland;
    if wayland && !super::wayland::WAYLAND_ADAPTER_BUILT {
        return Err(LauncherError::WaylandUnavailable);
    }
    let first_login_setup = match argument_value(args, super::wayland::FIRST_LOGIN_SETUP_FLAG) {
        None => arcen_session::desktop_session::first_login::FirstLoginSetup::Run,
        Some(token) => {
            arcen_session::desktop_session::first_login::FirstLoginSetup::from_token(&token)
                .ok_or(LauncherError::Protocol)?
        }
    };

    // Before the password arrives: no other process may dump or trace the
    // launcher's memory. A child that drops to the session user is
    // dumpable again only through its own credential change and exec.
    if let Err(error) = nix::sys::prctl::set_dumpable(false) {
        tracing::warn!(target: target::SESSION, %error, "the session launcher could not make itself non-dumpable");
    }
    // The password line is read straight from file descriptor 0 into one
    // pre-allocated zeroizing buffer, before any standard-library or runtime
    // stdin buffer exists, so no reader keeps a copy of it.
    let (auth_line, pending) =
        read_auth_request_line(0, MAX_AUTH_REQUEST_BYTES, AUTH_TIMEOUT).map_err(|error| {
            tracing::warn!(target: target::SESSION, %error, "the broker's authenticate request could not be read");
            error
        })?;
    let mut auth: OwnedAuthenticateRequest =
        serde_json::from_slice(&auth_line).map_err(|_| LauncherError::Protocol)?;
    drop(auth_line);
    let mut commands = BufReader::new(AsyncReadExt::chain(
        std::io::Cursor::new(pending),
        tokio::io::stdin(),
    ));
    if auth.command != "authenticate" {
        return Err(LauncherError::Protocol);
    }

    struct SecretConversation {
        username: String,
        password: Zeroizing<String>,
        /// What the PAM modules said (an access-control module's reason),
        /// bounded, for the log only.
        messages: Vec<String>,
    }
    impl SecretConversation {
        fn note(&mut self, message: &CStr) {
            if self.messages.len() < 8 {
                let text: String = message
                    .to_string_lossy()
                    .chars()
                    .filter(|c| {
                        !c.is_control()
                            && !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
                    })
                    .take(200)
                    .collect();
                self.messages.push(text);
            }
        }
    }
    impl ConversationHandler for SecretConversation {
        fn init(&mut self, default_user: Option<&str>) {
            if self.username.is_empty() {
                if let Some(default_user) = default_user {
                    self.username = default_user.to_string();
                }
            }
        }
        fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
            CString::new(self.username.as_str()).map_err(|_| ErrorCode::CONV_ERR)
        }
        fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
            CString::new(self.password.as_str()).map_err(|_| ErrorCode::CONV_ERR)
        }
        fn text_info(&mut self, message: &CStr) {
            self.note(message);
        }
        fn error_msg(&mut self, message: &CStr) {
            self.note(message);
        }
    }

    // A headless Wayland desktop hands the login password to its own
    // keyring (see `session::wayland::keyring`); nothing else keeps a copy.
    let keyring_password = wayland.then(|| Zeroizing::new(auth.password.as_bytes().to_vec()));
    // A console sign-in GDM's login screen or Arcen's login session
    // completes never shows the password to `pam_gnome_keyring`: the
    // launcher unlocks the login keyring with it (`login_keyring`).
    let console_keyring_password =
        (!wayland).then(|| Zeroizing::new(auth.password.as_bytes().to_vec()));
    let conversation = SecretConversation {
        username: auth.username.clone(),
        password: Zeroizing::new(std::mem::take(&mut auth.password)),
        messages: Vec::new(),
    };
    let mut context = Context::new(&pam_service, Some(&auth.username), conversation)
        .map_err(|_| LauncherError::PamInitialization)?;
    if wayland {
        // A seatless `user` session of type wayland: pam_systemd gives it
        // its own logind session, scope and runtime directory, and logind
        // grants it no seat devices (no DRM card, no input nodes).
        context
            .set_rhost(Some(&auth.remote_host))
            .and_then(|()| context.set_tty(Some("arcen")))
            .and_then(|()| context.putenv("XDG_SESSION_TYPE=wayland"))
            .and_then(|()| context.putenv("XDG_SESSION_CLASS=user"))
            .map_err(|_| LauncherError::PamInitialization)?;
    } else {
        context
            .set_rhost(Some(&auth.remote_host))
            .and_then(|()| context.set_tty(Some("arcen")))
            .and_then(|()| context.set_xdisplay(Some(&display)))
            .and_then(|()| context.putenv(format!("DISPLAY={display}")))
            .and_then(|()| context.putenv("XDG_SESSION_TYPE=x11"))
            .and_then(|()| context.putenv("XDG_SESSION_CLASS=user"))
            .and_then(|()| context.putenv("XDG_SEAT=seat0"))
            .and_then(|()| context.putenv("XDG_VTNR=1"))
            .map_err(|_| LauncherError::PamInitialization)?;
    }
    if context.authenticate(Flag::NONE).is_err() {
        write_response(&LauncherResponse::error("rejected"))?;
        return Err(LauncherError::Rejected);
    }
    if let Err(error) = context.acct_mgmt(Flag::NONE) {
        // The password was right; the account check refused. Access
        // control (a domain's Group Policy logon right through SSSD,
        // pam_access, an HBAC rule) answers "permission denied": the Deck
        // is told so in words, the log keeps the module's own reason.
        if error.code() == ErrorCode::PERM_DENIED {
            let refusal = arcen_session::console_session::ConsoleRefusal::AccountDenied;
            tracing::warn!(
                target: target::SESSION,
                pam_service = %pam_service,
                pam_error = %error,
                module_messages = %context.conversation().messages.join(" | "),
                reason_class = refusal.token(),
                "the account check refused this sign-in (access policy)"
            );
            write_response(&LauncherResponse::error(refusal.token()))?;
            return Err(LauncherError::Console(refusal.into()));
        }
        write_response(&LauncherResponse::error("rejected"))?;
        return Err(LauncherError::Rejected);
    }
    let canonical_username = context
        .user()
        .map_err(|_| LauncherError::PamInitialization)?;
    let identity = UserIdentity::resolve(&canonical_username)?;
    write_response(&LauncherResponse::authenticated(&identity))?;

    let command = read_bounded_line(&mut commands, MAX_OPEN_REQUEST_BYTES).await?;
    if parse_close_request(&command)? {
        return Ok(());
    }
    let open = parse_open_request(&command)?;
    let x11_desktop_depth = open.x11_desktop_depth()?;
    let mut commands = commands.lines();
    // A launcher of a host that creates Wayland desktops joins too: the
    // account's own Wayland session (S4b).
    if open.console {
        // No PAM session: the session already exists (or the login screen
        // creates it). PAM proved the password and the account above.
        drop(context);
        let launch = super::login_session::LoginLaunch {
            xorg_binary: xorg_binary.clone(),
            xorg_config_template: xorg_config_template.clone(),
            runtime_root: runtime_root.clone(),
            gpu_head: gpu_head.clone(),
            reserved_display: display.clone(),
            desktop: desktop.clone(),
            default_desktop: default_desktop.clone(),
            depth: x11_desktop_depth.map(arcen_media::video::DesktopDepth::x11_depth),
            display: open.display.clone(),
        };
        return run_console(
            &identity,
            &open,
            &agent_binary,
            &desktop,
            &session_log_id,
            &mut commands,
            &launch,
            console_keyring_password,
            wayland,
        )
        .await;
    }
    drop(console_keyring_password);

    context
        .putenv(format!("XDG_SESSION_DESKTOP={desktop}"))
        .and_then(|()| {
            context.putenv(format!(
                "XDG_CURRENT_DESKTOP={}",
                if desktop == "gnome-classic" {
                    "GNOME-Classic:GNOME"
                } else {
                    "GNOME"
                }
            ))
        })
        .and_then(|()| {
            if let Some(timezone) = open.timezone.as_ref() {
                context.putenv(format!("TZ={}", timezone.as_str()))
            } else {
                Ok(())
            }
        })
        .and_then(|()| {
            // A native desktop (`wayland.session_mode`): pam_systemd opens the logind
            // session on the desktop's own seat (no VT; logind activates it).
            if let Some(seat) = open.native_seat().filter(|_| wayland) {
                context.putenv(format!("XDG_SEAT={seat}"))
            } else {
                Ok(())
            }
        })
        .map_err(|_| LauncherError::PamSession)?;
    // A dedicated desktop never starts beside a console session that holds
    // the GPU's display on any VT (another remote-desktop product's desktop
    // runs its own Xorg on VT 7): the second X server hangs, or takes the
    // console away.
    if !wayland {
        // A console nobody can see is never taken for a free one.
        let sessions = match super::console::read_sessions().await {
            Ok(sessions) => sessions,
            Err(unreadable) => {
                tracing::warn!(
                    target: target::SESSION,
                    logind = unreadable.token(),
                    "logind's sessions could not be read; a dedicated desktop is not started beside a console it cannot see"
                );
                let notice: arcen_session::console_session::ConsoleNotice =
                    arcen_session::console_session::ConsoleRefusal::SessionsUnreadable.into();
                write_response(&LauncherResponse::console_refusal(&notice))?;
                return Err(LauncherError::Console(notice));
            }
        };
        if let Some((holder, origin)) = arcen_session::x11_seat::x11_seat_holder(&sessions) {
            tracing::warn!(
                target: target::SESSION,
                holder_session = %holder.id,
                holder_service = %holder.service,
                holder_vt = holder.vtnr,
                origin = origin.token(),
                "a console session holds this host's display; a dedicated desktop is not started beside it"
            );
            let error = if origin == arcen_session::console_session::ConsoleOrigin::OtherProduct {
                let service = super::console::foreign_service(holder);
                tracing::warn!(
                    target: target::SESSION,
                    pam_service = service.pam_service().unwrap_or("unknown"),
                    unit = service.unit().unwrap_or("unknown"),
                    process = service.process().unwrap_or("unknown"),
                    "another remote-desktop service holds this host's display"
                );
                LauncherError::SeatOccupiedOtherProduct(Some(service))
            } else {
                LauncherError::SeatOccupied
            };
            let service = match &error {
                LauncherError::SeatOccupiedOtherProduct(service) => service.as_ref(),
                _ => None,
            };
            write_response(&LauncherResponse::error_naming(
                logind_error_status(&error),
                service,
            ))?;
            return Err(error);
        }
    }
    let pam_session = match context.open_session(Flag::NONE) {
        Ok(session) => session,
        Err(_) => {
            write_response(&LauncherResponse::error("pam_error"))?;
            return Err(LauncherError::PamSession);
        }
    };
    // Synchronously, on the thread PAM ran on, before anything is spawned.
    let exec_context = match take_session_exec_context() {
        Ok(exec_context) => exec_context,
        Err(error) => {
            write_response(&LauncherResponse::error("selinux_error"))?;
            drop(pam_session);
            return Err(error);
        }
    };
    let pam_environment = pam_session
        .envlist()
        .iter_tuples()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>();
    let Some(session_id) = pam_environment
        .iter()
        .find_map(|(key, value)| (key == "XDG_SESSION_ID").then_some(value.clone()))
    else {
        // pam_systemd succeeds without a logind session when logind answers
        // "already occupied": another desktop holds seat0's X11 position.
        let error = if !wayland && x11_seat_is_occupied().await {
            LauncherError::SeatOccupied
        } else {
            LauncherError::LogindSession
        };
        write_response(&LauncherResponse::error(logind_error_status(&error)))?;
        drop(pam_session);
        return Err(error);
    };
    // Held until this function returns, on every path: the host never
    // suspends under a desktop it serves.
    let _sleep_inhibitor = super::sleep_inhibitor::SleepInhibitor::acquire(&session_id).await;
    #[cfg(not(feature = "wayland-provider"))]
    let _ = (keyring_password, first_login_setup);
    #[cfg(feature = "wayland-provider")]
    if wayland {
        let x11_apps = args
            .iter()
            .any(|argument| argument == super::wayland::LAUNCHER_X11_FLAG);
        let result = run_wayland_desktop(
            &identity,
            &open,
            pam_environment,
            exec_context,
            &session_id,
            &agent_binary,
            &desktop,
            &session_log_id,
            x11_apps,
            &mut commands,
            super::agent::WaylandSessionServices {
                keyring_password,
                first_login: first_login_setup,
            },
        )
        .await;
        drop(pam_session);
        return result;
    }
    let multi_monitor_plan = open
        .multi_monitor
        .clone()
        .map(MultiHeadPlanMsg::into_plan)
        .transpose()?;
    // A display the generator refuses keeps the template's EDID-less head,
    // as every session had before: the EDID refines the session, it is not
    // a condition of it.
    let session_edid = open
        .display
        .as_ref()
        .and_then(|served| match served.edid() {
            Ok(edid) => Some(edid),
            Err(error) => {
                tracing::warn!(
                    target: target::SESSION,
                    %error,
                    width = served.width,
                    height = served.height,
                    refresh_hz = served.refresh_hz,
                    "the Deck display's EDID could not be generated; the head keeps no EDID"
                );
                None
            }
        });
    let provider = DedicatedXorgProvider::new(
        &xorg_binary,
        &xorg_config_template,
        &runtime_root,
        &display,
        &gpu_head,
        &session_id,
        &identity,
        session_edid.as_ref(),
        arcen_outputs::SessionScreen {
            depth: x11_desktop_depth,
            served_size: open
                .display
                .as_ref()
                .map(|served| (served.width, served.height)),
        },
        XorgSeat::Dedicated,
    );
    let transaction = arcen_outputs::OutputTransaction::acquire(
        provider,
        &multi_monitor_plan,
        &arcen_outputs::OutputContext::new(session_log_id.clone()),
    )
    .await
    .map_err(output_provision_error);
    let mut transaction = match transaction {
        Ok(transaction) => transaction,
        Err(error) => {
            write_response(&LauncherResponse::error(output_error_status(&error)))?;
            drop(pam_session);
            return Err(error);
        }
    };
    if let Err(error) = transaction.commit().await.map_err(output_provision_error) {
        write_response(&LauncherResponse::error(output_error_status(&error)))?;
        drop(pam_session);
        return Err(error);
    }
    let committed = transaction
        .into_committed()
        // Unreachable: `commit` returned `Ok`, so the transaction is
        // committed. Refusing rather than unwrapping keeps the launcher
        // fail-closed if that ever stops being true, and the binding's `Drop`
        // still terminates the private Xorg.
        .map_err(|_| LauncherError::XorgStart);
    let committed = match committed {
        Ok(committed) => committed,
        Err(error) => {
            write_response(&LauncherResponse::error(output_error_status(&error)))?;
            drop(pam_session);
            return Err(error);
        }
    };
    let (_provider, binding) = committed.into_parts();
    let mut xorg = binding.into_output();
    let environment = SessionEnvironment::build(
        &identity,
        &display,
        Some(xorg.xauthority()),
        &desktop,
        open.timezone.as_ref(),
        pam_environment,
    )
    .with_locale(&super::locale::session_locale(&identity.username))
    .with_exec_context(exec_context);
    if environment.validate_runtime(&identity).is_err()
        || validate_logind_ownership(&environment, &identity)
            .await
            .is_err()
    {
        write_response(&LauncherResponse::error("logind_error"))?;
        drop(pam_session);
        return Err(LauncherError::LogindSession);
    }
    if let Err(error) = activate_logind_session(&environment).await {
        write_response(&LauncherResponse::error(logind_error_status(&error)))?;
        xorg.shutdown().await;
        drop(pam_session);
        return Err(error);
    }
    let mut deskside = if open.deskside.enabled {
        Some(
            crate::deskside::LinuxDesksideGuard::arm(
                &open.deskside,
                &session_id,
                identity.uid,
                &display,
                &gpu_head,
            )
            .await
            .map_err(|_| LauncherError::Deskside)?,
        )
    } else {
        None
    };
    let audio = open.audio();
    let mut agent = match SessionAgent::spawn(
        &agent_binary,
        &identity,
        &environment,
        &desktop,
        &session_log_id,
        &audio,
    )
    .await
    {
        Ok(agent) => agent,
        Err(error) => {
            let refused = agent_exec_refused_by_selinux(&error, &environment);
            if let Some(deskside) = deskside.as_mut() {
                let _ = deskside.restore().await;
            }
            recover_local_playback(
                &agent_binary,
                &identity,
                &environment,
                &session_log_id,
                &audio,
            )
            .await;
            xorg.shutdown().await;
            drop(pam_session);
            if refused {
                write_response(&LauncherResponse::error("selinux_error"))?;
                return Err(LauncherError::SelinuxContext);
            }
            if matches!(error, AgentError::LocalPlaybackRefused(_)) {
                write_response(&LauncherResponse::error("local_playback_refused"))?;
                return Err(LauncherError::LocalPlaybackRefused);
            }
            return Err(error.into());
        }
    };
    let ready = LauncherResponse::ready(
        &identity,
        &environment,
        &agent.ready,
        xorg.gpu_pci(),
        xorg.root_depth(),
    );
    if let Err(error) = write_response(&ready) {
        agent.shutdown().await;
        if let Some(deskside) = deskside.as_mut() {
            let _ = deskside.restore().await;
        }
        xorg.shutdown().await;
        drop(pam_session);
        return Err(error);
    }

    tokio::select! {
        command = commands.next_line() => {
            let _ = command;
        }
        _ = launcher_shutdown_signal() => {}
        _ = wait_for_deskside_failure(&mut deskside) => {}
        _ = agent.wait_for_exit() => {}
        _ = xorg.wait_for_exit() => {}
    }

    let deskside_restore = shutdown_desktop_resources(
        || async {
            agent.shutdown().await;
            recover_local_playback(
                &agent_binary,
                &identity,
                &environment,
                &session_log_id,
                &audio,
            )
            .await;
        },
        || async {
            if let Some(deskside) = deskside.as_mut() {
                deskside.restore().await
            } else {
                Ok(())
            }
        },
        || async { xorg.shutdown().await },
    )
    .await;
    drop(pam_session);
    deskside_restore.map_err(|_| LauncherError::Deskside)
}

/// Joins the user's console session (ADR 0025) and keeps it private until
/// the broker lets go; then hands the console back locked. Never starts or
/// ends a session of its own: a remote-first sign-in is completed by GDM's
/// login screen, and the session lives on after the Deck leaves.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)] // The console launch context and the password it may need.
async fn run_console(
    identity: &UserIdentity,
    open: &OpenRequest,
    agent_binary: &Path,
    desktop: &str,
    session_log_id: &CorrelationId,
    commands: &mut Lines<LauncherCommands>,
    launch: &super::login_session::LoginLaunch,
    // Only a sign-in that starts the session uses it.
    mut keyring_password: Option<Zeroizing<Vec<u8>>>,
    // A launcher of a host that creates Wayland desktops: only the
    // account's own Wayland session is joined (S4b); it never takes an X11
    // console, a login screen or a login session of its own.
    wayland_join_only: bool,
) -> Result<(), LauncherError> {
    use super::console;
    use arcen_session::console_session::{self as model, ConsoleDecision, ConsoleRefusal};

    let refuse = |notice: model::ConsoleNotice| -> LauncherError {
        tracing::warn!(
            target: target::SESSION,
            reason_class = notice.token(),
            service = notice
                .service()
                .and_then(model::ForeignService::label)
                .as_deref()
                .unwrap_or(""),
            "the console sign-in is refused"
        );
        let _ = write_response(&LauncherResponse::console_refusal(&notice));
        LauncherError::Console(notice)
    };
    let ready_by = tokio::time::Instant::now() + CONSOLE_OPEN_TIMEOUT - CONSOLE_READY_MARGIN;
    let sessions = console::read_sessions().await;
    let host = super::login_session::console_host();
    // Only a complete read of logind's sessions proves the account has
    // none; an unreadable one refuses rather than start a second session.
    let decision = model::decide_on_read(&sessions, identity.uid, host);
    tracing::info!(
        target: target::SESSION,
        decision = ?decision,
        sessions = sessions.as_ref().map_or(0, Vec::len),
        logind = sessions.as_ref().err().map_or("read", |unreadable| unreadable.token()),
        "console session decision"
    );
    if wayland_join_only
        && !matches!(
            decision,
            ConsoleDecision::JoinWayland(_) | ConsoleDecision::Refuse(_)
        )
    {
        tracing::error!(
            target: target::SESSION,
            decision = ?decision,
            "the account's Wayland session is no longer the console session this sign-in was to join"
        );
        return Err(refuse(ConsoleRefusal::SessionChanged.into()));
    }
    let mut privacy = console::ConsolePrivacy::default();
    let mut signed_in_through: Option<String> = None;
    // What `platform.session.default_desktop` did for a sign-in that
    // starts the account's session (ADR 0025); none for a joined session.
    let mut default_desktop: Option<&'static str> = None;
    // A login session this launcher started that cannot be made private is
    // ended: nobody has seen it, and nothing could cover it at the desk.
    let mut started_login_session = false;
    let (session_id, origin, signed_in) = match decision {
        ConsoleDecision::Attach {
            session,
            activate,
            origin,
        } => {
            if activate {
                // As for a returning Deck: the desk's input is disabled and
                // its monitors blanked (best effort, behind the login
                // screen) before the session is shown.
                if let Err(refusal) = privacy.disable_input() {
                    return Err(refuse(refusal.into()));
                }
                if let Some(display) =
                    console::find_session_display(&session, identity.uid, &identity.home)
                {
                    let _ = privacy.blank(&display, &session);
                }
                privacy.mark_brought_forward();
                if !console::activate_session(&session).await {
                    secure_before_leaving(&mut privacy, None, &session).await;
                    return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
                }
            }
            (session, origin, false)
        }
        ConsoleDecision::SignInThroughGreeter { greeter } => {
            // The desk's input goes first: nobody at the desk may use the
            // session the login screen is about to start.
            if let Err(refusal) = privacy.disable_input() {
                return Err(refuse(refusal.into()));
            }
            // Before the login screen reads the account's settings.
            default_desktop = Some(
                super::account_desktop::prepare(
                    &identity.username,
                    identity.uid,
                    launch.default_desktop.as_ref(),
                )
                .await
                .token(),
            );
            // A login screen GDM left waiting for its terminal is brought
            // there first; one still starting is waited for.
            let greeter = match console::ready_login_screen(&greeter).await {
                Ok(greeter) => greeter,
                Err(refusal) => return Err(refuse(refusal.into())),
            };
            match console::sign_in_through_greeter(&greeter, &identity.username, identity.uid).await
            {
                Ok(session) => {
                    privacy.mark_brought_forward();
                    signed_in_through = Some(greeter);
                    let origin = console::session_fact(&session)
                        .await
                        .map_or(model::ConsoleOrigin::DisplayManager, |fact| fact.origin());
                    (session, origin, true)
                }
                Err(refusal) => return Err(refuse(refusal.into())),
            }
        }
        ConsoleDecision::StartLoginSession { vt } => {
            // As for a login screen: nobody at the desk may use the session
            // about to start on its monitors.
            if let Err(refusal) = privacy.disable_input() {
                return Err(refuse(refusal.into()));
            }
            let prepared = super::account_desktop::prepare(
                &identity.username,
                identity.uid,
                launch.default_desktop.as_ref(),
            )
            .await;
            default_desktop = Some(prepared.token());
            let mut launch = launch.clone();
            let chosen = arcen_session::console_session::account_desktop::login_session_desktop(
                &prepared.choice,
                &prepared.decision,
                launch.default_desktop.as_ref(),
                &launch.desktop,
            );
            if chosen != launch.desktop {
                if super::login_session::has_x_session(&chosen) {
                    launch.desktop = chosen;
                } else {
                    tracing::warn!(target: target::SESSION, desktop = %chosen, configured = %launch.desktop, "the account's desktop has no X session entry; the configured desktop starts");
                }
            }
            match super::login_session::start(identity, vt, &launch).await {
                Ok(session) => {
                    privacy.mark_brought_forward();
                    started_login_session = true;
                    (session, model::ConsoleOrigin::Arcen, true)
                }
                // A session that did not start was stopped: nothing of it
                // is left on the desk's monitors.
                Err(refusal) => {
                    privacy.release_input();
                    return Err(refuse(refusal.into()));
                }
            }
        }
        // The account's own Wayland session: identified and its compositor
        // proven read-only, then refused visibly until this Pier can join
        // one (GAP-042).
        // The account's own Wayland session: identified and its compositor
        // proven read-only, then joined (S4b) when this Pier has the
        // adapter, refused visibly otherwise (GAP-042).
        ConsoleDecision::JoinWayland(target) => {
            let join = super::console_wayland::identify(&target, identity.gid).await;
            if model::wayland_attach::lock_step(&join).is_none() {
                let refusal = join
                    .refusal()
                    .unwrap_or(model::wayland_join::WaylandJoin::REFUSAL_WITHOUT_ADAPTER);
                return Err(refuse(console::notice_for(
                    refusal,
                    &sessions,
                    identity.uid,
                )));
            }
            return run_wayland_join(
                WaylandJoinContext {
                    identity,
                    open,
                    agent_binary,
                    desktop,
                    session_log_id,
                    ready_by,
                },
                commands,
                &target,
                &join,
                &refuse,
            )
            .await;
        }
        ConsoleDecision::Refuse(refusal) => {
            return Err(refuse(console::notice_for(
                refusal,
                &sessions,
                identity.uid,
            )))
        }
    };
    // Another product's desktop is joined only while that product's own
    // client is not using it: one remote product at a time (ADR 0025 §8).
    let guest = origin == model::ConsoleOrigin::OtherProduct;
    if guest {
        if let Some(device) = console::another_products_client().await {
            tracing::warn!(
                target: target::SESSION,
                session_id = %session_id,
                device = %device,
                "another remote-desktop product's client is using this desktop"
            );
            if privacy.is_brought_forward() {
                secure_before_leaving(&mut privacy, None, &session_id).await;
            } else {
                privacy.release_input();
            }
            let service = console::session_fact(&session_id)
                .await
                .map(|fact| console::foreign_service(&fact));
            return Err(refuse(model::ConsoleNotice::naming(
                ConsoleRefusal::InUseOtherProduct,
                service,
            )));
        }
    }
    // A session this sign-in brought to the desk's monitors (started by
    // the login screen, or brought forward) is secured before any refusal
    // gives the desk back; one the desk was already using is left as it
    // was (nothing of it was shown or unlocked).
    let Some((session_display, depth)) =
        console::wait_for_session_display(&session_id, identity.uid, &identity.home).await
    else {
        if started_login_session {
            end_unseen_login_session(&mut privacy, None, &session_id).await;
        } else if privacy.is_brought_forward() {
            secure_before_leaving(&mut privacy, None, &session_id).await;
        }
        return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
    };
    if let Err(refusal) = privacy.arm(&session_display, &session_id) {
        if started_login_session {
            end_unseen_login_session(&mut privacy, Some(&session_display), &session_id).await;
        } else if privacy.is_brought_forward() {
            secure_before_leaving(&mut privacy, Some(&session_display), &session_id).await;
        }
        return Err(refuse(refusal.into()));
    }
    // GDM finishes the login screen that took the sign-in only now, and may
    // put a new one in front of the session; with the desk private, the
    // session is brought forward again before anything is streamed.
    if let Some(greeter) = signed_in_through.as_deref() {
        if !console::settle_signed_in_session(
            greeter,
            identity.uid,
            &session_id,
            &mut privacy,
            ready_by,
        )
        .await
        {
            secure_before_leaving(&mut privacy, Some(&session_display), &session_id).await;
            return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
        }
        console::end_finished_login_screen(greeter).await;
    }
    // The Deck's layout, while the monitors are blank and the session not
    // yet open (ADR 0025 §6). A console that cannot take it is left as it
    // is, and the Pier is told why (it ends the sign-in with the reason).
    let layout_requests = if guest {
        // Shown as it is: never laid out; a single picture never asks.
        open.multi_monitor
            .clone()
            .and_then(|plan| plan.into_plan().ok())
            .and_then(|plan| super::console_layout::requests_from_plan(&plan))
    } else {
        console_layout_requests(open)
    };
    let mut console_outputs = None;
    let mut console_shown = None;
    let console_layout = match layout_requests.as_deref() {
        // Read, never changed: each Deck monitor is shown by the output in
        // the same place of the desktop's own layout.
        Some(requests) if guest => Some(
            match super::console_layout::match_shown(&session_display, requests).await {
                Ok(shown) => {
                    if shown.exact {
                        tracing::info!(
                            target: target::SESSION,
                            mapping = %shown.note,
                            "another product's desktop has the Deck's layout; each monitor is captured as it is shown"
                        );
                    } else {
                        tracing::warn!(
                            target: target::SESSION,
                            mapping = %shown.note,
                            "another product's desktop is laid out differently from the Deck's monitors; it is never changed, each Deck monitor shows the output in its place, fitted"
                        );
                    }
                    console_outputs = Some(shown.outputs());
                    console_shown = Some(shown);
                    CONSOLE_LAYOUT_SHOWN.to_owned()
                }
                Err(failure) => {
                    tracing::warn!(
                        target: target::SESSION,
                        reason_class = failure.token,
                        detail = %failure.detail,
                        monitors = requests.len(),
                        "another product's desktop shows nothing the Deck's monitors can be mapped to; it is never changed"
                    );
                    failure.token.to_owned()
                }
            },
        ),
        Some(requests) => Some(
            match privacy
                .apply_layout(&session_display, &session_id, identity.uid, requests)
                .await
            {
                Ok(()) => CONSOLE_LAYOUT_APPLIED.to_owned(),
                Err(failure) => {
                    tracing::warn!(
                        target: target::SESSION,
                        reason_class = failure.token,
                        detail = %failure.detail,
                        monitors = requests.len(),
                        "the console cannot take the Deck's layout; it is left as it is"
                    );
                    failure.token.to_owned()
                }
            },
        ),
        None => None,
    };
    // Only now, with the desk private, may a locked session open.
    if let Err(error) = unlock_logind_session_id(&session_id).await {
        tracing::warn!(target: target::SESSION, %error, "the joined console session could not be unlocked; the Deck sees its lock screen");
    }
    // The sign-in was private (nobody at the desk could take the session as
    // it opened); an administrator who turned desk privacy off gets the desk
    // back for the attachment.
    if open.desk_live {
        privacy.show_desk();
    }
    let vtnr = console::session_fact(&session_id)
        .await
        .map_or(0, |fact| fact.vtnr);
    // The desktop the session runs, as logind recorded it.
    let started_desktop = super::account_desktop::session_desktop(&session_id, identity.uid);
    tracing::info!(
        target: target::SESSION,
        session_id = %session_id,
        desktop = started_desktop.as_deref().unwrap_or("unknown"),
        default_desktop = default_desktop.unwrap_or("not_applicable"),
        "the console session's desktop"
    );
    let environment = SessionEnvironment::build(
        identity,
        &session_display.display,
        Some(&session_display.xauthority.to_string_lossy()),
        desktop,
        None,
        [
            ("XDG_SESSION_ID".to_owned(), session_id.clone()),
            ("XDG_SEAT".to_owned(), model::CONSOLE_SEAT.to_owned()),
            ("XDG_VTNR".to_owned(), vtnr.to_string()),
        ],
    )
    .with_locale(&super::locale::session_locale(&identity.username));
    if environment.validate_runtime(identity).is_err() {
        secure_before_leaving(&mut privacy, Some(&session_display), &session_id).await;
        return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
    }
    let audio = open.audio();
    let mut agent = match SessionAgent::spawn_console(
        agent_binary,
        identity,
        &environment,
        desktop,
        session_log_id,
        &audio,
    )
    .await
    {
        Ok(agent) => agent,
        Err(error) => {
            secure_before_leaving(&mut privacy, Some(&session_display), &session_id).await;
            if matches!(error, AgentError::LocalPlaybackRefused(_)) {
                write_response(&LauncherResponse::error("local_playback_refused"))?;
                return Err(LauncherError::LocalPlaybackRefused);
            }
            // Every failure after authentication answers with a status.
            write_response(&LauncherResponse::error("agent_error"))?;
            return Err(error.into());
        }
    };
    if signed_in {
        if let Some(password) = keyring_password.take() {
            super::login_keyring::unlock_in_background(
                agent_binary,
                identity,
                &environment,
                session_log_id.as_str(),
                &session_id,
                password,
            );
        }
    }
    drop(keyring_password.take());
    // The desk's sound goes back with the desk when the Deck leaves.
    privacy.hold_audio_with(agent.take_audio_control());
    let mut ready =
        LauncherResponse::ready(identity, &environment, &agent.ready, None, Some(depth));
    ready.console_origin = Some(origin.token().to_owned());
    ready.console_signed_in = signed_in;
    ready.console_default_desktop = default_desktop.map(str::to_owned);
    ready.console_desktop = started_desktop;
    if origin == model::ConsoleOrigin::OtherProduct {
        // Named to another account the Deck's absence would otherwise
        // leave guessing (`console_refusal_for_another_user`).
        ready.console_service = console::session_fact(&session_id)
            .await
            .map(|fact| console::foreign_service(&fact));
    }
    ready.console_layout = console_layout.clone();
    ready.console_outputs = console_outputs;
    ready.console_shown = console_shown;
    if let Err(error) = write_response(&ready) {
        agent.shutdown().await;
        secure_before_leaving(&mut privacy, Some(&session_display), &session_id).await;
        return Err(error);
    }
    tracing::info!(
        target: target::SESSION,
        session_id = %session_id,
        display = %session_display.display,
        origin = origin.token(),
        signed_in,
        desk_private = !open.desk_live,
        "the console session is joined"
    );

    // Installed once: a stop that arrives while a command or a hand-back is
    // being handled is kept for the next wait, never lost.
    let mut stop = StopSignals::install()?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u32 = 0;
    let mut attached = true;
    // A look logind does not answer is not a logout; enough of them in a
    // row hand the console back (locked), never end the session.
    let mut watch = model::session_watch::SessionWatch::new(identity.uid);
    loop {
        tokio::select! {
            line = commands.next_line() => {
                let command = match line {
                    Ok(Some(line)) => serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|value| value.get("command").and_then(|c| c.as_str()).map(str::to_owned)),
                    _ => None,
                };
                match command.as_deref() {
                    Some("detach") => {
                        attached = false;
                        // Held for its resume: the console is locked and
                        // given back with its sound (the default sink and
                        // mutes the desk had), but keeps the Deck's layout
                        // until the hold ends (the resumed capture finds its
                        // outputs).
                        privacy.hold_layout();
                        let given_back = !privacy.is_armed()
                            || console::hand_back(&mut privacy, &session_id).await;
                        if given_back {
                            privacy.restore_audio().await;
                        }
                    }
                    Some("attach") => {
                        if guest {
                            if let Some(device) = console::another_products_client().await {
                                tracing::warn!(
                                    target: target::SESSION,
                                    device = %device,
                                    "another remote-desktop product's client is using this desktop; the returning Deck is refused"
                                );
                                let service = console::session_fact(&session_id)
                                    .await
                                    .map(|fact| console::foreign_service(&fact));
                                let notice = model::ConsoleNotice::naming(
                                    ConsoleRefusal::InUseOtherProduct,
                                    service,
                                );
                                let _ = write_response(&LauncherResponse::console_refusal(&notice));
                                break;
                            }
                        }
                        // The desk's speakers are silent again before
                        // anything of the session is captured; a desk that
                        // cannot be proven silent refuses the returning Deck
                        // without touching the console.
                        if !privacy.retake_audio().await {
                            tracing::warn!(
                                target: target::SESSION,
                                session_id = %session_id,
                                "the desk's speakers could not be proven silent; the returning Deck is refused (audio.local_playback)"
                            );
                            let _ = write_response(&LauncherResponse::console_refusal(
                                &ConsoleRefusal::PrivacyUnavailable.into(),
                            ));
                            break;
                        }
                        attached = true;
                        let mut desk = console::LiveDesk {
                            privacy: &mut privacy,
                            display: &session_display,
                            session_id: &session_id,
                            layout: layout_requests
                                .as_deref()
                                .filter(|_| console_layout.as_deref() == Some(CONSOLE_LAYOUT_APPLIED))
                                .map(|requests| (requests, identity.uid)),
                            layout_failed: false,
                        };
                        // The broker waits for this answer before it
                        // streams, so the returning Deck is told why.
                        match console::reattach(
                            &mut desk,
                            &session_id,
                            identity.uid,
                            super::login_session::console_host(),
                        )
                        .await
                        {
                            // The returning Deck's layout could not be laid
                            // again: the console is handed back (unlock) and
                            // the Deck is not attached.
                            Ok(()) if desk.layout_failed => {
                                let _ = write_response(&LauncherResponse::error(
                                    "console_layout_unproven",
                                ));
                                break;
                            }
                            Ok(()) => {
                                if open.desk_live {
                                    privacy.show_desk();
                                }
                                let _ = write_response(&LauncherResponse::error("attached"));
                            }
                            Err(refusal) => {
                                let notice = console::notice_for(
                                    refusal,
                                    &console::read_sessions().await,
                                    identity.uid,
                                );
                                let _ = write_response(&LauncherResponse::console_refusal(&notice));
                                break;
                            }
                        }
                    }
                    _ => break,
                }
            }
            () = stop.recv() => break,
            _ = agent.wait_for_exit() => {
                tracing::warn!(target: target::SESSION, "the console session agent exited");
                break;
            }
            _ = tick.tick() => {
                ticks = ticks.wrapping_add(1);
                if privacy.is_armed() {
                    privacy.enforce();
                    // A hand-back whose lock failed keeps the desk private
                    // and tries again.
                    if !attached && ticks.is_multiple_of(5) {
                        console::hand_back(&mut privacy, &session_id).await;
                    }
                }
                if ticks & 1 == 0 {
                    match console::watch_joined(&mut watch, &session_id).await {
                        model::session_watch::WatchStep::Hold => {}
                        model::session_watch::WatchStep::Ended => {
                            tracing::info!(
                                target: target::SESSION,
                                session_id = %session_id,
                                "the console session ended (the user logged out)"
                            );
                            break;
                        }
                        step @ model::session_watch::WatchStep::HandBack => {
                            tracing::warn!(
                                target: target::SESSION,
                                session_id = %session_id,
                                reason_class = step.token(),
                                unanswered = watch.unanswered(),
                                "logind has not answered about the joined console session; the attachment ends and the console is handed back locked (the session is not ended)"
                            );
                            break;
                        }
                    }
                }
            }
        }
    }
    // Leaving: the console's own layout comes back. After a resume hold the
    // desk already has the session (locked when the Deck left, perhaps
    // unlocked at the desk since): only its layout is restored, the session
    // is not locked again.
    privacy.release_layout_hold();
    if !privacy.is_armed() && !privacy.holds_input() && !privacy.is_brought_forward() {
        privacy.restore_layout().await;
    }
    // Armed, or with the desk's input still held: the session is handed
    // back locked before the desk gets anything back. A session that will
    // neither lock nor go behind a login screen keeps the desk private
    // (the Deck's stream ends first) until it does, or ends.
    // The desk's speakers stay silent with its monitors: the agent (and so
    // its audio lease) ends only after the session is secured, or when this
    // launcher is stopped.
    if privacy.needs_hand_back() && !console::hand_back(&mut privacy, &session_id).await {
        keep_private_until_secured(&mut privacy, Some(&session_display), &session_id, &mut stop)
            .await;
        agent.shutdown().await;
        return Ok(());
    }
    agent.shutdown().await;
    Ok(())
}

/// What a joined Wayland session's launcher works with.
#[cfg(target_os = "linux")]
struct WaylandJoinContext<'a> {
    identity: &'a UserIdentity,
    open: &'a OpenRequest,
    agent_binary: &'a Path,
    desktop: &'a str,
    session_log_id: &'a CorrelationId,
    /// When the Pier stops waiting for the ready answer.
    ready_by: tokio::time::Instant,
}

/// Joins the account's own proven Wayland session (ADR 0025 §9, S4b) and
/// holds it while the Deck is attached.
///
/// The desk's input is disabled first; a session waiting behind another is
/// brought forward and a locked one unlocked (GNOME shares nothing of a
/// locked session; the account's password was just proven). The session
/// agent then joins the session as the account
/// (`session/wayland/join.rs`); the Deck is served only when the shared
/// decisions accept what it proved (`wayland_attach::{desk_privacy,
/// decide_serve}`). When the Deck leaves, or anything fails after the
/// session was touched, the session is handed back locked and the
/// attachment ends; the session is never ended.
#[cfg(target_os = "linux")]
async fn run_wayland_join(
    context: WaylandJoinContext<'_>,
    commands: &mut Lines<LauncherCommands>,
    target: &arcen_session::console_session::wayland_join::WaylandTarget,
    join: &arcen_session::console_session::wayland_join::WaylandJoin,
    refuse: &dyn Fn(arcen_session::console_session::ConsoleNotice) -> LauncherError,
) -> Result<(), LauncherError> {
    use super::console;
    use arcen_session::console_session::wayland_attach::{self, LockStep};
    use arcen_session::console_session::wayland_join::WaylandJoin;
    use arcen_session::console_session::{self as model, ConsoleRefusal};

    let WaylandJoinContext {
        identity,
        open,
        agent_binary,
        desktop,
        session_log_id,
        ready_by,
    } = context;
    let session_id = target.session.clone();
    let (step, compositor_pid) = match (wayland_attach::lock_step(join), join) {
        (
            Some(step),
            WaylandJoin::Join { compositor, .. } | WaylandJoin::HandleLock { compositor, .. },
        ) => (step, *compositor),
        _ => return Err(refuse(ConsoleRefusal::SessionUnreachable.into())),
    };
    // Desk privacy is decided before anything of the session is brought
    // forward or unlocked: a session that shows on a monitor at the desk is
    // never put on it for a join that would be refused.
    if !open.desk_live {
        let shown = super::console_wayland::desk_shows_session(target, identity.gid).await;
        if let Err(refusal) = wayland_attach::desk_privacy(shown.unwrap_or(true), !open.desk_live) {
            tracing::error!(
                target: target::SESSION,
                session_id = %session_id,
                read = shown.is_some(),
                "the joined Wayland session shows on a monitor at the desk (or its monitors could not be read), and desk privacy is required: a Wayland desk cannot be blanked without changing the user's compositor, so nothing of the session is shown"
            );
            return Err(refuse(refusal.into()));
        }
    }
    // Journalled before anything of the session is touched: a Pier that
    // loses this launcher locks the session (`console::recover_console`).
    if let Err(error) = console::journal_wayland_join(&session_id) {
        tracing::error!(target: target::SESSION, session_id = %session_id, %error, "the joined Wayland session could not be journalled; it is not touched");
        return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
    }
    let mut privacy = console::ConsolePrivacy::default();
    // Nobody at the desk may use the session while it is shown remotely.
    if let Err(refusal) = privacy.disable_input() {
        console::forget_wayland_join();
        return Err(refuse(refusal.into()));
    }
    if target.activate {
        privacy.mark_brought_forward();
        if !console::activate_session(&session_id).await {
            tracing::error!(target: target::SESSION, session_id = %session_id, "the joined Wayland session could not be brought forward");
            leave_unserved(&mut privacy, &session_id).await;
            return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
        }
    }
    if step == LockStep::UnlockFirst {
        privacy.mark_brought_forward();
        let unlocked = unlock_logind_session_id(&session_id).await.is_ok();
        if let Err(refusal) = wayland_attach::after_unlock(!unlocked) {
            tracing::error!(
                target: target::SESSION,
                session_id = %session_id,
                "the joined Wayland session did not unlock; the sign-in is refused"
            );
            leave_unserved(&mut privacy, &session_id).await;
            return Err(refuse(refusal.into()));
        }
    }
    let display = console::session_environ(&session_id, identity.uid, "WAYLAND_DISPLAY")
        .unwrap_or_else(|| super::wayland::WAYLAND_DISPLAY_NAME.to_owned());
    let environment = SessionEnvironment::build_wayland(
        identity,
        desktop,
        None,
        [
            ("XDG_SESSION_ID".to_owned(), session_id.clone()),
            ("XDG_SEAT".to_owned(), model::CONSOLE_SEAT.to_owned()),
        ],
    )
    .with_wayland_display(&display)
    .with_locale(&super::locale::session_locale(&identity.username));
    if let Err(error) = environment.validate_runtime(identity) {
        tracing::error!(target: target::SESSION, session_id = %session_id, %error, "the joined Wayland session's runtime directory or user bus is not the account's");
        leave_unserved(&mut privacy, &session_id).await;
        return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
    }
    let mut agent = match SessionAgent::spawn_wayland_join(
        agent_binary,
        identity,
        &environment,
        desktop,
        session_log_id,
        &open.audio(),
        compositor_pid,
        open.stream_cursor.unwrap_or_default(),
        ready_by.saturating_duration_since(tokio::time::Instant::now()),
    )
    .await
    {
        Ok(agent) => agent,
        Err(error) => {
            tracing::error!(target: target::SESSION, session_id = %session_id, %error, "the joined Wayland session's agent did not join it");
            leave_unserved(&mut privacy, &session_id).await;
            if matches!(error, AgentError::LocalPlaybackRefused(_)) {
                write_response(&LauncherResponse::error("local_playback_refused"))?;
                return Err(LauncherError::LocalPlaybackRefused);
            }
            return Err(refuse(ConsoleRefusal::SessionUnreachable.into()));
        }
    };
    let joined = agent.ready.joined.clone();
    let served = joined.as_ref().map_or(
        Err((
            ConsoleRefusal::SessionUnreachable,
            wayland_attach::RouteGap::Capture,
        )),
        |joined| wayland_attach::decide_serve(joined.proof()),
    );
    let private = joined.as_ref().map_or(Ok(()), |joined| {
        wayland_attach::desk_privacy(joined.at_desk, !open.desk_live).inspect_err(|_| {
            tracing::error!(target: target::SESSION, session_id = %session_id, "the joined Wayland session shows on a monitor at the desk, and desk privacy is required: a Wayland desk cannot be blanked without changing the user's compositor, so the Deck is not served");
        })
    });
    if let Err(refusal) = served
        .map_err(|(refusal, gap)| {
            tracing::error!(target: target::SESSION, session_id = %session_id, gap = gap.token(), "the joined Wayland session's route is not proved; the Deck is not served");
            refusal
        })
        .and(private)
    {
        agent.shutdown().await;
        leave_unserved(&mut privacy, &session_id).await;
        return Err(refuse(refusal.into()));
    }
    // The desk's sound goes back with the desk when the Deck leaves.
    privacy.hold_audio_with(agent.take_audio_control());
    let mut ready = LauncherResponse::ready(identity, &environment, &agent.ready, None, None);
    ready.console_origin = Some(target.origin.token().to_owned());
    ready.console_desktop = Some(target.desktop.clone()).filter(|desktop| !desktop.is_empty());
    if let Err(error) = write_response(&ready) {
        agent.shutdown().await;
        leave_unserved(&mut privacy, &session_id).await;
        return Err(error);
    }
    if open.desk_live {
        privacy.show_desk();
    }
    tracing::info!(
        target: target::SESSION,
        session_id = %session_id,
        compositor_pid,
        unlocked = step == LockStep::UnlockFirst,
        desk_private = !open.desk_live,
        "the account's Wayland session is joined"
    );
    let mut stop = StopSignals::install()?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u32 = 0;
    let mut watch = model::session_watch::SessionWatch::new(identity.uid);
    loop {
        tokio::select! {
            line = commands.next_line() => {
                let command = match line {
                    Ok(Some(line)) => serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .and_then(|value| value.get("command").and_then(|c| c.as_str()).map(str::to_owned)),
                    _ => None,
                };
                match command.as_deref() {
                    // The Deck left: the session is locked and the
                    // attachment ends (`wayland_attach::AFTER_DECK_LEFT`).
                    Some("detach") | None => break,
                    // A joined Wayland session is never held for a resume.
                    Some("attach") => {
                        let _ = write_response(&LauncherResponse::console_refusal(
                            &ConsoleRefusal::SessionChanged.into(),
                        ));
                        break;
                    }
                    Some(_) => {}
                }
            }
            () = stop.recv() => break,
            _ = agent.wait_for_exit() => {
                tracing::info!(target: target::SESSION, session_id = %session_id, "the joined Wayland session's sharing ended (locked or its compositor gone); the attachment ends");
                break;
            }
            _ = tick.tick() => {
                ticks = ticks.wrapping_add(1);
                privacy.enforce();
                if ticks & 1 == 0 {
                    match console::watch_joined(&mut watch, &session_id).await {
                        model::session_watch::WatchStep::Hold => {}
                        model::session_watch::WatchStep::Ended => {
                            tracing::info!(target: target::SESSION, session_id = %session_id, "the console session ended (the user logged out)");
                            break;
                        }
                        step @ model::session_watch::WatchStep::HandBack => {
                            tracing::warn!(target: target::SESSION, session_id = %session_id, reason_class = step.token(), unanswered = watch.unanswered(), "logind has not answered about the joined Wayland session; it is handed back locked (the session is not ended)");
                            break;
                        }
                    }
                }
            }
        }
    }
    // The session is locked (GNOME ends the sharing sessions), then the
    // desk gets its input and its sound back through the agent, which ends
    // last.
    if !give_back_joined(&mut privacy, &session_id).await {
        keep_private_until_secured(&mut privacy, None, &session_id, &mut stop).await;
        if console::session_fact(&session_id)
            .await
            .is_some_and(|fact| fact.locked)
        {
            console::forget_wayland_join();
        }
    }
    agent.shutdown_gracefully().await;
    Ok(())
}

/// Leaves a joined Wayland session the Deck was never served: one this
/// launcher brought forward or unlocked is handed back locked; one the desk
/// was already using is left as it was (only the desk's input comes back).
#[cfg(target_os = "linux")]
async fn leave_unserved(privacy: &mut super::console::ConsolePrivacy, session_id: &str) {
    if privacy.is_brought_forward() {
        let _ = give_back_joined(privacy, session_id).await;
    } else {
        privacy.release_input();
        super::console::forget_wayland_join();
    }
}

/// Hands a joined Wayland session back (locked first) and forgets its
/// journal once that is done.
#[cfg(target_os = "linux")]
async fn give_back_joined(privacy: &mut super::console::ConsolePrivacy, session_id: &str) -> bool {
    let given_back = super::console::hand_back(privacy, session_id).await;
    if given_back {
        super::console::forget_wayland_join();
    }
    given_back
}

/// Hands the console back before the launcher leaves, when it must
/// ([`console::ConsolePrivacy::needs_hand_back`]; callers secure first
/// and answer the broker after, so its teardown timer cannot cut the
/// hand-back short); a session that will not
/// be secured keeps the desk private until it is.
async fn secure_before_leaving(
    privacy: &mut super::console::ConsolePrivacy,
    display: Option<&super::console::SessionDisplay>,
    session_id: &str,
) {
    if privacy.needs_hand_back() && !super::console::hand_back(privacy, session_id).await {
        // Arcen's own login session is kept private by design: once the
        // desk is journalled private (the Pier holds it from here), the
        // refusal goes out now instead of after the broker's timeout.
        if privacy.is_armed() && super::console::kept_private_by_design(session_id).await {
            privacy.keep_desk_disabled_beyond_launcher();
            return;
        }
        match StopSignals::install() {
            Ok(mut stop) => {
                keep_private_until_secured(privacy, display, session_id, &mut stop).await
            }
            Err(error) => tracing::error!(
                target: target::SESSION,
                %error,
                "stop signals could not be watched; the console stays as the failed hand-back left it"
            ),
        }
    }
}

/// Ends Arcen's own login session that this launcher started and could
/// not make private (its display did not answer, or the desk could not be
/// blanked): nobody has seen it, and without a lock screen nothing could
/// cover it once the desk's input is given back. Kept private meanwhile.
async fn end_unseen_login_session(
    privacy: &mut super::console::ConsolePrivacy,
    display: Option<&super::console::SessionDisplay>,
    session_id: &str,
) {
    let ended = super::console::terminate_session(session_id).await;
    if ended {
        tracing::warn!(
            target: target::SESSION,
            session_id,
            "the new login session could not be made private; it was ended before anyone saw it"
        );
        privacy.release_input();
    } else {
        tracing::error!(
            target: target::SESSION,
            session_id,
            "the new login session could not be made private or ended; the desk stays private"
        );
        secure_before_leaving(privacy, display, session_id).await;
    }
}

/// Keeps the desk private (monitors blank where the display is known,
/// input disabled) and retries the hand-back every 5 s until the session
/// is locked, behind a login screen, or gone, or the launcher is stopped.
/// From the first failed hand-back on, the session's X server also ignores
/// the desk's devices (journalled): a grab ends with this process, so a
/// stop, or a kill, still leaves the desk unusable and its monitors blank;
/// the Pier keeps retrying (`console::guard_unsecured_console`) and gives
/// the desk back only once the session is secure.
async fn keep_private_until_secured(
    privacy: &mut super::console::ConsolePrivacy,
    display: Option<&super::console::SessionDisplay>,
    session_id: &str,
    stop: &mut StopSignals,
) {
    let mut disabled_beyond = false;
    loop {
        match display {
            Some(_) if privacy.is_armed() => privacy.enforce(),
            Some(display) => {
                let _ = privacy.arm(display, session_id);
            }
            None => {
                let _ = privacy.disable_input();
            }
        }
        if !disabled_beyond && privacy.is_armed() {
            privacy.keep_desk_disabled_beyond_launcher();
            disabled_beyond = true;
        }
        tokio::select! {
            () = stop.recv() => {
                if disabled_beyond {
                    tracing::error!(
                        target: target::SESSION,
                        session_id,
                        "stopped before the console session could be locked; its monitors stay blank and the desk's input disabled in its X server until the Pier secures it"
                    );
                } else {
                    tracing::error!(
                        target: target::SESSION,
                        session_id,
                        "stopped before the console session could be locked, and its display is unknown; the desk's input is released"
                    );
                }
                return;
            }
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        if super::console::hand_back(privacy, session_id).await {
            return;
        }
    }
}

/// The virtual output a headless Wayland session stands for: the served
/// Deck display when it named one, the default otherwise.
#[cfg(any(feature = "wayland-provider", test))]
fn wayland_output_request(
    display: Option<&SessionDisplayMsg>,
) -> Result<arcen_session::wayland_session::VirtualOutputRequest, LauncherError> {
    let (width, height, refresh_hz) = display.map_or(super::wayland::DEFAULT_OUTPUT, |served| {
        (served.width, served.height, served.refresh_hz)
    });
    arcen_session::wayland_session::VirtualOutputRequest::new(width, height, refresh_hz)
        .map_err(|_| LauncherError::WaylandStart)
}

/// The placed outputs a headless Wayland session opens with: a committed
/// virtual-output topology's outputs (primary first, at their planned
/// positions), or the one served display.
#[cfg(any(feature = "wayland-provider", test))]
fn wayland_session_outputs(
    open: &OpenRequest,
) -> Result<Vec<arcen_session::wayland_outputs::PlacedOutput>, LauncherError> {
    let Some(plan) = open.multi_monitor.clone() else {
        return Ok(vec![
            arcen_session::wayland_outputs::PlacedOutput::at_origin(wayland_output_request(
                open.display.as_ref(),
            )?)
            .with_density(
                open.display
                    .as_ref()
                    .map(SessionDisplayMsg::density)
                    .unwrap_or_default(),
            ),
        ]);
    };
    let plan = plan.into_plan().map_err(|_| LauncherError::WaylandStart)?;
    if plan.backend != crate::display::topology::LinuxTopologyBackend::VirtualOutputs {
        tracing::warn!(
            target: target::SESSION,
            backend = plan.backend.token(),
            "a headless Wayland session serves virtual outputs; this topology was planned for another backend"
        );
        return Err(LauncherError::WaylandStart);
    }
    let outputs = plan
        .placed_outputs()
        .map_err(|_| LauncherError::WaylandStart)?
        .into_iter()
        .map(|(_, placed)| placed)
        .collect::<Vec<_>>();
    tracing::info!(
        target: target::SESSION,
        outputs = outputs.len(),
        generation = plan.generation.get(),
        "headless Wayland session opens the committed multi-monitor topology"
    );
    Ok(outputs)
}

/// Whether `/tmp/.X11-unix` may take this desktop's X11 sockets (a missing
/// one, like a missing `/tmp/.ICE-unix`, is recreated). Otherwise the
/// desktop starts without Xwayland: GNOME Shell aborts at start on a
/// directory another user owns, so serving it with X11 would cost the whole
/// desktop. An unsafe `/tmp/.ICE-unix` is only reported.
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
fn x11_socket_directory_ready() -> bool {
    let results = super::socket_dirs::prepare_production();
    for (_, result) in &results {
        if let Err(fault) = result {
            tracing::warn!(
                target: target::SESSION,
                fault = fault.token(),
                %fault,
                "shared socket directory is unsafe"
            );
        }
    }
    let ready = super::socket_dirs::x11_result(&results).is_ok();
    if !ready {
        tracing::warn!(
            target: target::SESSION,
            "this desktop starts without X11 applications"
        );
    }
    ready
}

/// Runs one headless Wayland desktop inside the PAM session the caller
/// opened, and proves its release before the caller closes PAM.
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
#[allow(clippy::too_many_arguments)] // The launcher's existing open context.
async fn run_wayland_desktop(
    identity: &UserIdentity,
    open: &OpenRequest,
    pam_environment: Vec<(String, String)>,
    exec_context: Option<super::selinux::SessionSecurityContext>,
    session_id: &str,
    agent_binary: &Path,
    desktop: &str,
    session_log_id: &CorrelationId,
    x11_apps: bool,
    commands: &mut Lines<LauncherCommands>,
    services: super::agent::WaylandSessionServices,
) -> Result<(), LauncherError> {
    // Stop requests are honoured from here on, including while the desktop
    // is still starting: the Pier closing the pipe or giving up on the open,
    // SIGTERM, or logind's SIGTERM+SIGHUP. Every one of them goes through
    // release and PAM close rather than killing the launcher.
    let mut stop = StopSignals::install()?;
    let outputs = match wayland_session_outputs(open) {
        Ok(outputs) => outputs,
        Err(error) => {
            write_response(&LauncherResponse::error("wayland_start_error"))?;
            return Err(error);
        }
    };
    if open.deskside.enabled {
        tracing::info!(
            target: target::SESSION,
            "deskside protection guards the physical console; a seatless headless Wayland session never drives it, so it is not armed"
        );
    }
    let x11_apps = x11_apps && x11_socket_directory_ready();
    let environment = SessionEnvironment::build_wayland(
        identity,
        desktop,
        open.timezone.as_ref(),
        pam_environment,
    )
    .with_locale(&super::locale::session_locale(&identity.username))
    .with_exec_context(exec_context);
    let runtime_dir = super::wayland::private_runtime_dir(identity.uid, session_id);
    let audio = open.audio();
    let start = async {
        if environment.validate_runtime(identity).is_err()
            || validate_logind_ownership(&environment, identity)
                .await
                .is_err()
            || validate_wayland_session(session_id, open.native_seat())
                .await
                .is_err()
        {
            return Err("logind_error");
        }
        let agent = SessionAgent::spawn_wayland(
            agent_binary,
            identity,
            &environment,
            desktop,
            session_log_id,
            &runtime_dir,
            &outputs,
            &audio,
            open.wayland_fidelity_route.as_deref(),
            open.wayland_compositor_prefix.as_deref(),
            open.wayland_lab_input_seat.as_deref(),
            open.native_seat().is_some(),
            open.stream_cursor.unwrap_or_default(),
            services,
            x11_apps,
        )
        .await
        .map_err(|error| {
            if agent_exec_refused_by_selinux(&error, &environment) {
                "selinux_error"
            } else if matches!(error, AgentError::LocalPlaybackRefused(_)) {
                "local_playback_refused"
            } else {
                tracing::warn!(target: target::SESSION, %error, "headless Wayland session failed to become ready");
                "wayland_start_error"
            }
        })?;
        if private_runtime_dir_is_safe(&runtime_dir, identity.uid) {
            Ok(agent)
        } else {
            agent.shutdown_gracefully().await;
            Err("wayland_start_error")
        }
    };
    // Dropping `start` on a stop request kills a still-starting agent; its
    // stack dies with it (parent-death signal) and the release below sweeps
    // whatever the agent's session bus had activated.
    let started = tokio::select! {
        started = start => Some(started),
        command = commands.next_line() => {
            tracing::info!(target: target::SESSION, closed = !matches!(command, Ok(Some(_))), "stop requested while the headless Wayland session was starting");
            None
        }
        () = stop.recv() => {
            tracing::info!(target: target::SESSION, "stop signal while the headless Wayland session was starting");
            None
        }
    };
    let mut agent = match started {
        Some(Ok(agent)) => agent,
        Some(Err(status)) => {
            let _ = release_wayland_session(session_id, identity, &environment, &runtime_dir, None)
                .await;
            recover_local_playback(agent_binary, identity, &environment, session_log_id, &audio)
                .await;
            write_response(&LauncherResponse::error(status))?;
            return Err(open_response_error(status, None));
        }
        None => {
            let _ = release_wayland_session(session_id, identity, &environment, &runtime_dir, None)
                .await;
            let _ = write_response(&LauncherResponse::error("wayland_start_error"));
            return Err(LauncherError::WaylandStart);
        }
    };
    // The X11 displays the compositor locked, so a compositor that died
    // without its own shutdown has its sockets and lock files removed too.
    let x11_displays = agent
        .ready
        .wayland
        .as_ref()
        .and_then(|wayland| wayland.x11.as_ref())
        .and_then(|x11| x11.displays);
    if let Err(error) = write_response(&LauncherResponse::ready(
        identity,
        &environment,
        &agent.ready,
        None,
        None,
    )) {
        agent.shutdown_gracefully().await;
        let _ = release_wayland_session(
            session_id,
            identity,
            &environment,
            &runtime_dir,
            x11_displays,
        )
        .await;
        return Err(error);
    }

    tokio::select! {
        command = commands.next_line() => {
            let _ = command;
        }
        () = stop.recv() => {}
        _ = agent.wait_for_exit() => {
            tracing::warn!(target: target::SESSION, "headless Wayland session agent exited");
        }
    }
    agent.shutdown_gracefully().await;
    let released = release_wayland_session(
        session_id,
        identity,
        &environment,
        &runtime_dir,
        x11_displays,
    )
    .await;
    recover_local_playback(agent_binary, identity, &environment, session_log_id, &audio).await;
    released
}

/// logind must report the session as `wayland`, class `user`, with no seat:
/// a seatless session receives no device ACLs, so the compositor cannot
/// open a DRM card or input node through it.
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
async fn validate_wayland_session(
    session_id: &str,
    native_seat: Option<&str>,
) -> Result<(), LauncherError> {
    if !safe_session_id(session_id) {
        return Err(LauncherError::LogindSession);
    }
    let mut show = Command::new("/usr/bin/loginctl");
    show.args([
        "show-session",
        session_id,
        "-p",
        "Type",
        "-p",
        "Class",
        "-p",
        "Seat",
    ]);
    let output = command_output_with_timeout(show, LauncherError::LogindSession).await?;
    let properties = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && wayland_properties_match(&properties, native_seat) {
        if let Some(seat) = native_seat {
            tracing::info!(
                target: target::SESSION,
                session_id,
                seat,
                "logind wayland session created on the desktop's own seat (native desktop)"
            );
        } else {
            tracing::info!(
                target: target::SESSION,
                session_id,
                "seatless logind wayland session created for the headless desktop"
            );
        }
        Ok(())
    } else {
        Err(LauncherError::LogindSession)
    }
}

#[cfg(any(all(target_os = "linux", feature = "wayland-provider"), test))]
fn wayland_properties_match(properties: &str, native_seat: Option<&str>) -> bool {
    let lines = properties.lines().map(str::trim).collect::<Vec<_>>();
    let seat = format!("Seat={}", native_seat.unwrap_or_default());
    lines.contains(&"Type=wayland")
        && lines.contains(&"Class=user")
        && lines.contains(&seat.as_str())
}

#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
fn private_runtime_dir_is_safe(path: &Path, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0
    })
}

/// The root side of teardown: whatever the agent could not release — a
/// process of the session user left in this logind session's scope, a FUSE
/// mount or the private runtime directory — is released here, and the result
/// is proven before PAM closes the session.
///
/// The runtime directory is writable by the session user, so root never acts
/// on paths inside it: unmounting (`fusermount -u -z`, own FUSE mounts only)
/// and removal (`rm -rf --one-file-system`) run as the session user with the
/// agent's privilege drop. Root only signals scope processes by pid and
/// checks the result without following anything.
///
/// The X11 sockets and lock files of `x11_displays` (the displays the
/// session's compositor locked, from the agent's ready message) live in the
/// shared `/tmp`, not the runtime directory. Root removes them only through
/// a descriptor of the root-owned sticky directory, after an
/// `fstatat(AT_SYMLINK_NOFOLLOW)` proves a socket or regular file owned by
/// the session user, named for one of those displays, and (for a lock)
/// holding the compositor's pid (`xwayland::remove_session_artifacts`).
#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
async fn release_wayland_session(
    session_id: &str,
    identity: &UserIdentity,
    environment: &SessionEnvironment,
    runtime_dir: &Path,
    x11_displays: Option<arcen_session::wayland_x11::SessionDisplays>,
) -> Result<(), LauncherError> {
    use super::wayland::stack::{
        mounts_inside, own_session_scope, scope_processes, signal_scope_process,
        unmount_user_fuse_inside, ScopeSignal,
    };
    let uid = identity.uid;
    let Some(scope) = own_session_scope(session_id) else {
        return Err(LauncherError::WaylandRelease);
    };
    for signal in [ScopeSignal::Terminate, ScopeSignal::Kill] {
        let remaining = scope_processes(&scope, uid);
        if remaining.is_empty() {
            break;
        }
        tracing::warn!(
            target: target::SESSION,
            count = remaining.len(),
            ?signal,
            "session-user processes outlived the headless Wayland agent; signalling them"
        );
        for pid in remaining {
            signal_scope_process(&scope, uid, pid, signal);
        }
        for _ in 0..60 {
            if scope_processes(&scope, uid).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let x11_artifacts = x11_displays.map_or(0, |displays| {
        super::wayland::xwayland::remove_session_artifacts(
            &super::wayland::xwayland::X11Dirs::production(),
            uid,
            &displays,
        )
        .remaining
        .len()
    });
    let as_user = |command: &mut Command| {
        super::identity::configure_user_command(command, identity, environment).is_ok()
    };
    unmount_user_fuse_inside(runtime_dir, uid, as_user).await;
    if std::fs::symlink_metadata(runtime_dir).is_ok() {
        let mut remove = Command::new("/usr/bin/rm");
        remove
            .args(["-rf", "--one-file-system", "--"])
            .arg(runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if as_user(&mut remove) {
            let _ = command_status_with_timeout(remove, LauncherError::WaylandRelease).await;
        }
    }
    let remaining = scope_processes(&scope, uid).len();
    let runtime_dir_present = std::fs::symlink_metadata(runtime_dir).is_ok();
    let mounts = mounts_inside(runtime_dir).len();
    if remaining == 0 && !runtime_dir_present && mounts == 0 && x11_artifacts == 0 {
        tracing::info!(
            target: target::SESSION,
            session_id,
            "headless Wayland session released: no session-user process, mount, runtime directory or X11 socket remains"
        );
        Ok(())
    } else {
        tracing::error!(
            target: target::SESSION,
            session_id,
            remaining,
            runtime_dir_present,
            mounts,
            x11_artifacts,
            "headless Wayland session release could not be proven; logind removes the runtime directory with the user's last session"
        );
        Err(LauncherError::WaylandRelease)
    }
}

/// Restores a local-playback lease the session's agent left on the user's
/// own audio server (it was killed before it could release it), as the
/// session user. Nothing runs when no journal was left.
#[cfg(target_os = "linux")]
async fn recover_local_playback(
    agent_binary: &Path,
    identity: &UserIdentity,
    environment: &SessionEnvironment,
    session_log_id: &CorrelationId,
    audio: &AgentAudioArguments,
) {
    let journal = PathBuf::from(format!("/run/user/{}", identity.uid))
        .join(crate::media::local_playback::JOURNAL_NAME);
    if !journal.exists() {
        return;
    }
    let mut command = crate::command_for_helper(agent_binary, "session-agent");
    command
        .arg(crate::media::local_playback::RECOVER_FLAG)
        .arg("--session-log-id")
        .arg(session_log_id.as_str())
        .arg(crate::media::local_playback::PACTL_FLAG)
        .arg(&audio.pactl)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if super::identity::configure_user_command(&mut command, identity, environment).is_err() {
        return;
    }
    // The user's runtime directory, never a private one: the journal of a
    // lease on the user's server is there.
    command.env("XDG_RUNTIME_DIR", format!("/run/user/{}", identity.uid));
    let status = match command.spawn() {
        Ok(mut child) => tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .ok()
            .and_then(Result::ok),
        Err(_) => None,
    };
    tracing::info!(
        target: target::SESSION,
        recovered = status.is_some_and(|status| status.success()),
        "a local playback lease left by the session agent was restored"
    );
}

async fn shutdown_desktop_resources<AF, AFut, DF, DFut, XF, XFut>(
    agent: AF,
    deskside: DF,
    xorg: XF,
) -> Result<(), String>
where
    AF: FnOnce() -> AFut,
    AFut: std::future::Future<Output = ()>,
    DF: FnOnce() -> DFut,
    DFut: std::future::Future<Output = Result<(), String>>,
    XF: FnOnce() -> XFut,
    XFut: std::future::Future<Output = ()>,
{
    agent().await;
    let restore = deskside().await;
    xorg().await;
    restore
}

#[cfg(target_os = "linux")]
async fn wait_for_deskside_failure(deskside: &mut Option<crate::deskside::LinuxDesksideGuard>) {
    match deskside {
        Some(deskside) => deskside.wait_for_failure().await,
        None => std::future::pending::<()>().await,
    }
}

/// Which seat position an Xorg the launcher starts takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum XorgSeat {
    /// An Arcen desktop on VT 1, confined to its own session's devices.
    Dedicated,
    /// A real login session on `seat0` at `vt` (ADR 0025 §10): the
    /// console's devices are its own, as with any display manager's X
    /// server, and the desk is made private while a Deck is attached.
    Console { vt: u32 },
}

impl XorgSeat {
    fn vt(self) -> u32 {
        match self {
            Self::Dedicated => 1,
            Self::Console { vt } => vt,
        }
    }
}

/// The template's configuration for a console seat's X server: devices are
/// added and enabled as on any console X server (the template turns that
/// off for a confined desktop).
#[cfg(any(target_os = "linux", test))]
fn with_console_input(config: &str) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(config.lines().count());
    for line in config.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        let off = words.first() == Some(&"Option")
            && matches!(
                words.get(1),
                Some(&"\"AutoAddDevices\"" | &"\"AutoEnableDevices\"")
            )
            && words.get(2) == Some(&"\"false\"");
        if !off {
            out.push(line);
        }
    }
    let mut rendered = out.join("\n");
    if config.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

#[cfg(target_os = "linux")]
pub(super) struct PreparedDedicatedXorg {
    display_number: u16,
    /// The virtual terminal the server runs on.
    vt: u32,
    config_text: String,
    /// The NVIDIA GPU `config_text` names, when the host has it.
    gpu_pci: Option<arcen_outputs::PciAddress>,
    /// The EDID the configuration's `CustomEDID` names, written beside it.
    edid: Option<Vec<u8>>,
    /// The topology this attempt applies, cloned out of the caller's plan so
    /// the binding -- not the provider -- owns it for `verify`.
    plan: super::output_provider::DedicatedXorgPlan,
    session_log_id: CorrelationId,
}

/// Owns every operating-system-visible resource one dedicated-Xorg attempt
/// creates.
///
/// The bound server used to live in the provider behind an `Option`, with a
/// separate `committed` flag. Both are gone: the shared transaction tracks the
/// state machine, and the binding is what `rollback` needs and nothing else.
#[cfg(target_os = "linux")]
pub(super) struct DedicatedXorgBinding {
    output: DedicatedXorg,
    plan: super::output_provider::DedicatedXorgPlan,
    session_log_id: CorrelationId,
    evidence: super::output_provider::DedicatedXorgEvidence,
    /// The private Xorg process tree and its runtime artifacts still have to
    /// be torn down. Commit does not clear this: the server keeps running for
    /// the whole session, and only the launcher taking ownership of it through
    /// [`Self::into_output`] moves that obligation.
    armed: bool,
}

#[cfg(target_os = "linux")]
impl DedicatedXorgBinding {
    pub(super) fn into_output(self) -> DedicatedXorg {
        self.output
    }
}

#[cfg(target_os = "linux")]
pub(super) struct DedicatedXorgProvider<'a> {
    binary: &'a Path,
    template: &'a Path,
    runtime_root: &'a Path,
    x_display: &'a str,
    gpu_head: &'a str,
    session_id: &'a str,
    identity: &'a UserIdentity,
    /// The single-head session's EDID, generated from the Deck display.
    edid: Option<&'a [u8; 128]>,
    /// The desktop's depth and the single-head screen size.
    screen: arcen_outputs::SessionScreen,
    seat: XorgSeat,
}

#[cfg(target_os = "linux")]
impl<'a> DedicatedXorgProvider<'a> {
    #[allow(clippy::too_many_arguments)] // Dedicated Xorg's existing launch context.
    pub(super) fn new(
        binary: &'a Path,
        template: &'a Path,
        runtime_root: &'a Path,
        x_display: &'a str,
        gpu_head: &'a str,
        session_id: &'a str,
        identity: &'a UserIdentity,
        edid: Option<&'a [u8; 128]>,
        screen: arcen_outputs::SessionScreen,
        seat: XorgSeat,
    ) -> Self {
        Self {
            binary,
            template,
            runtime_root,
            x_display,
            gpu_head,
            session_id,
            identity,
            edid,
            screen,
            seat,
        }
    }
}

#[cfg(target_os = "linux")]
impl arcen_outputs::OutputProvider for DedicatedXorgProvider<'_> {
    type Plan = super::output_provider::DedicatedXorgPlan;
    type Prepared = PreparedDedicatedXorg;
    type Binding = DedicatedXorgBinding;
    type Evidence = super::output_provider::DedicatedXorgEvidence;
    type Error = LauncherError;

    fn capabilities(&self) -> arcen_outputs::OutputCapabilities {
        super::output_provider::DEDICATED_XORG_CAPABILITIES
    }

    fn demand(&self, plan: &Self::Plan) -> arcen_outputs::OutputDemand {
        super::output_provider::dedicated_xorg_demand(plan)
    }

    /// Preflight touches nothing the operating system can see beyond the
    /// abandoned residue of a dead server: it parses the display, reclaims a
    /// provably stale lock/socket (refusing anything else), reads the
    /// root-owned template, and renders the configuration text. The
    /// region-count refusal it used to perform now happens in the shared
    /// admission gate, before this runs.
    fn preflight(
        &mut self,
        plan: &Self::Plan,
        context: &arcen_outputs::OutputContext,
    ) -> Result<Self::Prepared, Self::Error> {
        let display_number = parse_dedicated_display(self.x_display)?;
        if plan.is_none() {
            validate_gpu_head(self.gpu_head)?;
        }
        if !self.binary.is_file() || !self.template.is_file() || !safe_session_id(self.session_id) {
            return Err(LauncherError::XorgConfig);
        }
        if display_lock_path(display_number).exists()
            || display_socket_path(display_number).exists()
        {
            reclaim_stale_display(display_number)?;
        }

        let template_text = read_secure_xorg_template(self.template)?;
        let (template_text, gpu_pci) = crate::display::nvidia_discovery::resolve_xorg_template_gpu(
            self.template,
            &template_text,
        );
        let config_text = match plan.as_ref() {
            Some(plan) => {
                super::xorg_multihead::render_multi_head_xorg_config(&template_text, plan)
                    .map_err(|_| LauncherError::XorgConfig)?
            }
            None => render_xorg_config(&template_text, self.gpu_head)?,
        };
        let (config_text, edid) = match (plan.as_ref(), self.edid) {
            (None, Some(edid)) => {
                let path = self
                    .runtime_root
                    .join(self.session_id)
                    .join(SESSION_EDID_FILE);
                (
                    with_custom_edid(&config_text, self.gpu_head, &path)?,
                    Some(edid.to_vec()),
                )
            }
            _ => (config_text, None),
        };
        let config_text = match self.seat {
            XorgSeat::Dedicated => with_session_input_isolation(&config_text, self.x_display)?,
            XorgSeat::Console { .. } => with_console_input(&config_text),
        };
        let config_text = with_dont_zap(&config_text);
        // A multi-head plan already sized its own `Virtual`.
        // A single head's `Virtual` is raised to the served display if the
        // template's is smaller, never lowered: later resizes and larger
        // reattachments need the headroom. A multi-head plan already sized
        // its own `Virtual`, and an EDID-less head's mode is not the served
        // size.
        let screen = arcen_outputs::SessionScreen {
            served_size: if plan.is_none() && self.edid.is_some() {
                self.screen.served_size
            } else {
                None
            },
            ..self.screen
        };
        let rendered = arcen_outputs::with_session_screen(&config_text, screen);
        if rendered.kept_template_depth {
            tracing::warn!(
                target: target::SESSION,
                sid = %context.session_log_id(),
                requested_depth = ?screen.depth.map(arcen_media::video::DesktopDepth::x11_depth),
                "the Xorg template does not carry exactly one DefaultDepth and one Display Depth line; its own depth is kept and the live root depth decides what this desktop serves"
            );
        }
        let config_text = rendered.config;
        Ok(PreparedDedicatedXorg {
            display_number,
            vt: self.seat.vt(),
            config_text,
            gpu_pci,
            edid,
            plan: plan.clone(),
            session_log_id: context.session_log_id().clone(),
        })
    }

    /// The one genuinely asynchronous bind in the product.
    ///
    /// Its last-resort release is ownership, not a journal:
    /// `PendingOutputArtifacts` removes a half-built session directory on
    /// unwind, the child is spawned with `kill_on_drop`, and `DedicatedXorg`'s
    /// own `Drop` kills the process tree and removes the artifacts. A dropped
    /// bind future therefore cannot strand a server.
    async fn bind(
        &mut self,
        prepared: Self::Prepared,
    ) -> Result<Self::Binding, arcen_outputs::BindFailure<Self::Error>> {
        let output = DedicatedXorg::bind(
            self.binary,
            self.runtime_root,
            self.x_display,
            self.session_id,
            self.identity,
            &prepared,
        )
        .await
        .map_err(|source| arcen_outputs::BindFailure {
            source,
            // Nothing survives a failed bind to undo: every artifact is
            // released by the drop paths above before this returns.
            rollback: None,
        })?;
        let evidence = super::output_provider::DedicatedXorgEvidence::new(
            self.x_display,
            self.gpu_head,
            prepared.display_number,
            &output.xauthority,
            output.child.id(),
        );
        Ok(DedicatedXorgBinding {
            output,
            plan: prepared.plan,
            session_log_id: prepared.session_log_id,
            evidence,
            armed: true,
        })
    }

    async fn verify(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        let mut result = binding.output.wait_ready(self.x_display).await;
        if result.is_ok() {
            if let Some(plan) = binding.plan.as_ref() {
                result = binding
                    .output
                    .verify_applied_topology(self.x_display, plan)
                    .await;
            }
        }
        if let Err(error) = result {
            binding.output.keep_failed_logs(&error);
            return Err(error);
        }
        binding.evidence.set_applied_topology(binding.plan.clone());
        Ok(())
    }

    fn commit(
        &mut self,
        binding: &mut Self::Binding,
    ) -> impl core::future::Future<Output = Result<(), Self::Error>> + Send {
        tracing::info!(
            target: target::SESSION,
            sid = %binding.session_log_id,
            x_display = binding.evidence.x_display(),
            gpu_head = %binding.evidence.gpu_head(),
            pid = binding.evidence.pid(),
            xauthority = %binding.evidence.xauthority().display(),
            regions = binding.evidence.regions(),
            multi_monitor = binding.evidence.multi_monitor(),
            "dedicated session Xorg output committed"
        );
        core::future::ready(Ok(()))
    }

    /// Terminates the private Xorg process tree and removes this session's
    /// runtime artifacts, and refuses to claim more than it proved.
    ///
    /// Returning `Ok(())` is the [`arcen_outputs::OutputSurface::DedicatedPhysical`]
    /// claim of ADR 0010: every resource this provider created has been
    /// released, and the console topology was never touched, because a
    /// dedicated server on its own GPU head never mutates it.
    /// [`DedicatedXorg::release`] is what proves the first half; when it
    /// cannot, the obligation still stands, so the binding stays armed and a
    /// later [`arcen_outputs::OutputTransaction::rollback`] retries it.
    ///
    /// Idempotent: a no-op once the obligation has been released.
    async fn rollback(&mut self, binding: &mut Self::Binding) -> Result<(), Self::Error> {
        if !binding.armed {
            return Ok(());
        }
        binding.output.release().await?;
        binding.armed = false;
        Ok(())
    }

    fn evidence<'a>(&'a self, binding: &'a Self::Binding) -> &'a Self::Evidence {
        &binding.evidence
    }

    fn is_armed(&self, binding: &Self::Binding) -> bool {
        binding.armed
    }
}

#[cfg(target_os = "linux")]
fn output_provision_error(
    error: arcen_outputs::OutputTransactionError<LauncherError>,
) -> LauncherError {
    match error {
        arcen_outputs::OutputTransactionError::Admission(mismatch) => {
            tracing::warn!(
                target: target::SESSION,
                stage = %arcen_outputs::OutputStage::Admission,
                %mismatch,
                "dedicated Xorg output plan was refused before any provider code ran"
            );
            LauncherError::XorgConfig
        }
        arcen_outputs::OutputTransactionError::Operation { stage, source } => {
            tracing::warn!(
                target: target::SESSION,
                %stage,
                %source,
                "dedicated Xorg output provisioning failed closed"
            );
            source
        }
        arcen_outputs::OutputTransactionError::Rollback {
            stage,
            source,
            rollback,
        } => {
            tracing::error!(
                target: target::SESSION,
                %stage,
                %source,
                %rollback,
                "dedicated Xorg output provisioning and rollback both failed"
            );
            source
        }
    }
}

fn output_error_status(error: &LauncherError) -> &'static str {
    match error {
        LauncherError::XorgConfig => "xorg_config_error",
        LauncherError::XorgStart => "xorg_start_error",
        LauncherError::XorgVerify => "xorg_verify_error",
        LauncherError::XorgRelease => "xorg_release_error",
        LauncherError::WaylandStart => "wayland_start_error",
        _ => "output_error",
    }
}

/// Where libinput installs its quirks, on every distribution Arcen supports.
const SYSTEM_LIBINPUT_QUIRKS_DIR: &str = "/usr/share/libinput";
/// The administrator's libinput overrides.
const LOCAL_LIBINPUT_QUIRKS_OVERRIDE: &str = "/etc/libinput/local-overrides.quirks";
/// The override's name inside a session's quirks directory: last in
/// libinput's version sort, so it keeps the last word as it has system-wide.
const SESSION_LOCAL_OVERRIDE_NAME: &str = "zz-local-overrides.quirks";

/// The session X server's libinput quirks: the system's files, then Arcen's
/// virtual devices' ([`arcen_input::virtual_device::libinput_quirks`]), then
/// the administrator's overrides, in a directory private to the session.
/// libinput reads `LIBINPUT_QUIRKS_DIR` instead of its defaults, so the
/// session's server gets them without any system file being changed.
///
/// `None` (the server keeps libinput's defaults) when the system directory
/// cannot be read or holds no quirks: an empty directory would drop every
/// device quirk the distribution ships.
fn prepare_libinput_quirks(
    system_dir: &Path,
    local_override: &Path,
    session_dir: &Path,
) -> Option<PathBuf> {
    let attempt = || -> std::io::Result<Option<PathBuf>> {
        let mut system: Vec<PathBuf> = std::fs::read_dir(system_dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "quirks")
                    && path.is_file()
            })
            .collect();
        if system.is_empty() {
            return Ok(None);
        }
        system.sort();
        let dir = session_dir.join("libinput-quirks");
        std::fs::create_dir(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
        for path in &system {
            if let Some(name) = path.file_name() {
                std::fs::copy(path, dir.join(name))?;
            }
        }
        std::fs::write(
            dir.join(arcen_input::virtual_device::LIBINPUT_QUIRKS_FILE_NAME),
            arcen_input::virtual_device::libinput_quirks(),
        )?;
        if local_override.is_file() {
            std::fs::copy(local_override, dir.join(SESSION_LOCAL_OVERRIDE_NAME))?;
        }
        Ok(Some(dir))
    };
    match attempt() {
        Ok(Some(dir)) => Some(dir),
        Ok(None) => {
            tracing::warn!(
                target: target::SESSION,
                system_dir = %system_dir.display(),
                "no libinput quirks found; the session's virtual pen keeps libinput's smoothing"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                target: target::SESSION,
                %error,
                "session libinput quirks could not be prepared; the virtual pen keeps libinput's smoothing"
            );
            None
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) struct DedicatedXorg {
    child: Child,
    display_number: u16,
    session_dir: PathBuf,
    xauthority: PathBuf,
    gpu_pci: Option<arcen_outputs::PciAddress>,
    /// The root window depth the server reported once ready.
    root_depth: Option<u8>,
}

#[cfg(target_os = "linux")]
impl DedicatedXorg {
    async fn bind(
        binary: &Path,
        runtime_root: &Path,
        x_display: &str,
        session_id: &str,
        identity: &UserIdentity,
        prepared: &PreparedDedicatedXorg,
    ) -> Result<Self, LauncherError> {
        std::fs::create_dir_all(runtime_root)?;
        std::fs::set_permissions(runtime_root, std::fs::Permissions::from_mode(0o711))?;
        let session_dir = runtime_root.join(session_id);
        if session_dir.exists() {
            std::fs::remove_dir_all(&session_dir)?;
        }
        std::fs::create_dir(&session_dir)?;
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o711))?;
        let mut pending_artifacts = PendingOutputArtifacts::new(session_dir.clone());

        let config_path = session_dir.join("xorg.conf");
        std::fs::write(&config_path, &prepared.config_text)?;
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))?;
        if let Some(edid) = &prepared.edid {
            let edid_path = session_dir.join(SESSION_EDID_FILE);
            std::fs::write(&edid_path, edid)?;
            std::fs::set_permissions(&edid_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let xauthority = session_dir.join("Xauthority");
        create_xauthority(&xauthority, x_display, identity)?;
        let log_path = session_dir.join("Xorg.log");
        let quirks = prepare_libinput_quirks(
            Path::new(SYSTEM_LIBINPUT_QUIRKS_DIR),
            Path::new(LOCAL_LIBINPUT_QUIRKS_OVERRIDE),
            &session_dir,
        );
        let mut command = Command::new(binary);
        if let Some(quirks) = &quirks {
            command.env("LIBINPUT_QUIRKS_DIR", quirks);
        }
        command
            .arg(x_display)
            .arg("-config")
            .arg(&config_path)
            .arg("-auth")
            .arg(&xauthority)
            .arg("-logfile")
            .arg(&log_path)
            .arg("-noreset")
            .arg("-novtswitch")
            .arg(format!("vt{}", prepared.vt))
            .arg("-nolisten")
            .arg("tcp")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
        // SAFETY: this runs in the child immediately before exec and only arms
        // the existing race-safe kernel parent-death signal.
        unsafe {
            command
                .as_std_mut()
                .pre_exec(super::identity::arm_parent_death_signal);
        }
        let child = command.spawn()?;
        pending_artifacts.disarm();
        Ok(Self {
            child,
            display_number: prepared.display_number,
            session_dir,
            xauthority,
            gpu_pci: prepared.gpu_pci,
            root_depth: None,
        })
    }

    /// Keeps a failed desktop's Xorg log and configuration beside the
    /// session runtime root (`last-failed/`, replaced by the next failure)
    /// and records the log's tail at error level, so a host at the
    /// `critical` profile can say why its desktop did not start.
    fn keep_failed_logs(&self, error: &LauncherError) {
        use std::io::{Read as _, Seek as _};
        const TAIL_BYTES: u64 = 2048;
        let log = self.session_dir.join("Xorg.log");
        let mut tail = String::new();
        if let Ok(mut file) = std::fs::File::open(&log) {
            let length = file.metadata().map_or(0, |metadata| metadata.len());
            let _ = file.seek(std::io::SeekFrom::Start(length.saturating_sub(TAIL_BYTES)));
            let mut bytes = Vec::new();
            let _ = file.take(TAIL_BYTES).read_to_end(&mut bytes);
            tail = crate::eventlog::bounded_diagnostic_line(
                &String::from_utf8_lossy(&bytes).replace('\n', " | "),
            )
            .into_owned();
        }
        let kept = self
            .session_dir
            .parent()
            .map(|root| root.join("last-failed"));
        if let Some(kept) = kept.as_ref() {
            let _ = std::fs::remove_dir_all(kept);
            if std::fs::create_dir(kept).is_ok() {
                let _ = std::fs::set_permissions(kept, std::fs::Permissions::from_mode(0o700));
                for name in ["Xorg.log", "xorg.conf"] {
                    let _ = std::fs::copy(self.session_dir.join(name), kept.join(name));
                }
            }
        }
        tracing::error!(
            target: target::SESSION,
            %error,
            kept = ?kept,
            xorg_log_tail = %tail,
            "the dedicated desktop's X server failed; its log is kept"
        );
    }

    /// The NVIDIA GPU this server drives, for capture to open CUDA on.
    fn gpu_pci(&self) -> Option<arcen_outputs::PciAddress> {
        self.gpu_pci
    }

    pub(super) fn root_depth(&self) -> Option<u8> {
        self.root_depth
    }

    pub(super) fn xauthority(&self) -> &str {
        self.xauthority
            .to_str()
            .expect("session runtime paths must be UTF-8")
    }

    async fn wait_ready(&mut self, display: &str) -> Result<(), LauncherError> {
        // Bounded as a whole, and every probe on its own: an X server that
        // hangs (the field case beside another product's Xorg on the same
        // GPU) must fail here, with its log kept, before the broker's open
        // timeout kills the launcher and every trace of why.
        let deadline = tokio::time::Instant::now() + XORG_READY_BUDGET;
        while tokio::time::Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                return Err(LauncherError::XorgStart);
            }
            let mut probe = Command::new("/usr/bin/xdpyinfo");
            probe
                .args(["-display", display])
                .env("XAUTHORITY", &self.xauthority)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            let probe = tokio::time::timeout(Duration::from_secs(2), probe.output())
                .await
                .ok()
                .and_then(Result::ok);
            if let Some(output) = probe.filter(|output| output.status.success()) {
                // The answer must be this server's: alive, and holding
                // every socket of its display (a local user's server could
                // hold the display's abstract name, which clients try
                // first, without a file in /tmp).
                let display_number = parse_dedicated_display(display)?;
                if self.child.try_wait()?.is_some()
                    || !x_sockets_belong_to(display_number, self.child.id())
                {
                    tracing::error!(
                        target: target::SESSION,
                        display_number,
                        "the display answered, but not from this X server; it is not used"
                    );
                    return Err(LauncherError::XorgStart);
                }
                self.root_depth = parse_root_depth(&String::from_utf8_lossy(&output.stdout));
                // tracing's field macros shadow a local named `display`.
                let x_display = display;
                tracing::info!(
                    target: target::SESSION,
                    x_display,
                    root_depth = ?self.root_depth,
                    "dedicated session Xorg is ready"
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Err(LauncherError::XorgStart)
    }

    /// Verifies the exact applied RandR geometry/rotation/primary/overall
    /// screen bounds of every planned monitor against what this dedicated
    /// Xorg actually reports, once it is ready.
    ///
    /// Runs `/usr/bin/xrandr --query` against this session's private
    /// display/`XAUTHORITY` (hardcoded, matching the existing
    /// `/usr/bin/xdpyinfo`/`/usr/bin/xauth` convention in this file rather
    /// than introducing a new configurable binary path) and delegates the
    /// actual comparison to the pure [`super::randr_verify::verify_applied_topology`].
    async fn verify_applied_topology(
        &self,
        x_display: &str,
        plan: &crate::display::topology::LinuxTopologyPlan,
    ) -> Result<(), LauncherError> {
        let output = Command::new("/usr/bin/xrandr")
            .args(["-display", x_display, "--query"])
            .env("XAUTHORITY", &self.xauthority)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            tracing::error!(
                target: target::SESSION,
                x_display,
                status = ?output.status.code(),
                "xrandr query failed while verifying dedicated Xorg topology"
            );
            return Err(LauncherError::XorgVerify);
        }
        let stdout = String::from_utf8(output.stdout).map_err(|_| LauncherError::XorgVerify)?;
        if let Err(error) = super::randr_verify::verify_applied_topology(&stdout, plan) {
            tracing::error!(
                target: target::SESSION,
                x_display,
                %error,
                expected_width = plan.virtual_width,
                expected_height = plan.virtual_height,
                expected_regions = plan.monitors.len(),
                "dedicated Xorg RandR topology did not match the committed plan"
            );
            return Err(LauncherError::XorgVerify);
        }
        Ok(())
    }

    pub(super) async fn wait_for_exit(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    /// Terminates the private Xorg process tree, removes this session's
    /// runtime artifacts, and proves both.
    ///
    /// This is the [`arcen_outputs::OutputSurface::DedicatedPhysical`]
    /// postcondition of ADR 0010 section 6 written as code. `Ok(())` is a
    /// claim, so it is only returned once the child has actually been reaped
    /// and no artifact this attempt created is still on disk; a teardown that
    /// cannot prove that reports [`LauncherError::XorgRelease`] instead of
    /// silently claiming success. The console topology needs no restore: a
    /// dedicated server on its own GPU head never mutated it.
    async fn release(&mut self) -> Result<(), LauncherError> {
        terminate_child_tree(&mut self.child).await;
        let reaped = matches!(self.child.try_wait(), Ok(Some(_)));
        self.cleanup_artifacts();
        let lock = display_lock_path(self.display_number);
        let socket = display_socket_path(self.display_number);
        if reaped
            && dedicated_xorg_artifacts_released(&[
                lock.as_path(),
                socket.as_path(),
                self.session_dir.as_path(),
            ])
        {
            return Ok(());
        }
        Err(LauncherError::XorgRelease)
    }

    /// Best-effort teardown for the session-end path, which tears the whole
    /// session down regardless and has nowhere to return a failure to.
    pub(super) async fn shutdown(&mut self) {
        if let Err(error) = self.release().await {
            tracing::warn!(
                target: target::SESSION,
                display_number = self.display_number,
                %error,
                "dedicated session Xorg teardown could not be proved complete"
            );
        }
    }

    fn cleanup_artifacts(&self) {
        let _ = std::fs::remove_file(display_lock_path(self.display_number));
        let _ = std::fs::remove_file(display_socket_path(self.display_number));
        let _ = std::fs::remove_dir_all(&self.session_dir);
    }
}

/// Whether every listening socket of X display `display` (the abstract
/// `@/tmp/.X11-unix/X<n>` and the file `/tmp/.X11-unix/X<n>`) is held by
/// process `pid`, and there is at least one. Accepted connections carry the
/// listener's name too and are left out (`__SO_ACCEPTCON` marks a
/// listener in `/proc/net/unix`).
#[cfg(target_os = "linux")]
fn x_sockets_belong_to(display: u16, pid: Option<u32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    let Ok(table) = std::fs::read_to_string("/proc/net/unix") else {
        return false;
    };
    let names = [
        format!("@/tmp/.X11-unix/X{display}"),
        format!("/tmp/.X11-unix/X{display}"),
    ];
    let inodes: Vec<String> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.get(3).copied() != Some("00010000") {
                return None;
            }
            let path = fields.get(7)?;
            names
                .iter()
                .any(|name| name == path)
                .then(|| fields.get(6).map(|inode| (*inode).to_owned()))
                .flatten()
        })
        .collect();
    if inodes.is_empty() {
        return false;
    }
    let owned: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                .filter_map(|target| {
                    target
                        .to_str()?
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    inodes.iter().all(|inode| owned.contains(inode))
}

/// Whether every artifact one dedicated-Xorg attempt created is gone.
///
/// Split out of [`DedicatedXorg::release`], and deliberately not gated on
/// Linux, so the proof behind the `DedicatedPhysical` rollback postcondition
/// of ADR 0010 is testable on any development machine: the release claim holds
/// only when nothing this attempt put on disk is still there.
fn dedicated_xorg_artifacts_released(artifacts: &[&Path]) -> bool {
    artifacts.iter().all(|path| !path.exists())
}

#[cfg(target_os = "linux")]
struct PendingOutputArtifacts {
    session_dir: PathBuf,
    armed: bool,
}

#[cfg(target_os = "linux")]
impl PendingOutputArtifacts {
    fn new(session_dir: PathBuf) -> Self {
        Self {
            session_dir,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(target_os = "linux")]
impl Drop for PendingOutputArtifacts {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.session_dir);
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for DedicatedXorg {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.cleanup_artifacts();
    }
}

#[cfg(target_os = "linux")]
fn create_xauthority(
    path: &Path,
    display: &str,
    identity: &UserIdentity,
) -> Result<(), LauncherError> {
    let mut cookie = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut cookie)?;
    let cookie = cookie
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let status = std::process::Command::new("/usr/bin/xauth")
        .args(["-f", path.to_str().ok_or(LauncherError::XorgConfig)?, "add"])
        .arg(display)
        .args([".", &cookie])
        .status()?;
    if !status.success() {
        return Err(LauncherError::XorgConfig);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(identity.uid)),
        Some(nix::unistd::Gid::from_raw(identity.gid)),
    )
    .map_err(std::io::Error::other)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_secure_xorg_template(path: &Path) -> Result<String, LauncherError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    let trust = crate::display::xorg_template_trust::template_trust(
        metadata.file_type().is_file(),
        metadata.uid(),
        metadata.mode(),
        0,
    );
    if trust != crate::display::xorg_template_trust::TemplateTrust::Trusted {
        tracing::warn!(
            target: target::SESSION,
            template = %path.display(),
            "refusing the Xorg template: {}",
            trust.describe()
        );
        return Err(LauncherError::XorgConfig);
    }
    let mut template = String::new();
    file.read_to_string(&mut template)?;
    Ok(template)
}

/// The session EDID's file name in the session directory.
const SESSION_EDID_FILE: &str = "edid.bin";

/// Names `edid` as `head`'s EDID, beside the `ConnectedMonitor` line that
/// forces the head on. The template keeps `AllowNonEdidModes`, so the
/// NV-CONTROL `ViewPortIn` resize still works on an EDID-backed head.
#[cfg(any(target_os = "linux", test))]
fn with_custom_edid(config: &str, head: &str, edid: &Path) -> Result<String, LauncherError> {
    let edid = edid.to_str().ok_or(LauncherError::XorgConfig)?;
    if edid.contains('"') || head.contains('"') || edid.contains(';') {
        return Err(LauncherError::XorgConfig);
    }
    let mut hits = 0usize;
    let mut out = Vec::with_capacity(config.lines().count() + 1);
    for line in config.lines() {
        out.push(line.to_owned());
        let trimmed = line.trim_start();
        if trimmed.starts_with("Option") && trimmed.contains("\"ConnectedMonitor\"") {
            hits += 1;
            let indent = &line[..line.len() - trimmed.len()];
            out.push(format!(
                "{indent}Option         \"CustomEDID\" \"{head}:{edid}\""
            ));
        }
    }
    if hits != 1 || config.contains("\"CustomEDID\"") {
        return Err(LauncherError::XorgConfig);
    }
    let mut rendered = out.join("\n");
    if config.ends_with('\n') {
        rendered.push('\n');
    }
    Ok(rendered)
}

/// Appends the input classes that confine the dedicated X server to its own
/// session's virtual devices.
///
/// The rendered server hot-adds input devices, and udev offers it every
/// device on the seat: the console's keyboard and mice and any other
/// session's virtual devices. The appended classes ignore all of them and
/// re-admit only devices named for this display
/// ([`arcen_input::virtual_device::SessionInputIdentity`]), then re-admit
/// this desktop's own bridged USB tablet (Native tablet, lab) by its Xorg
/// tag (`arcen_usb_bridge::bridge_seat::xorg_session_bridge_class`), so the
/// desktop hot-adds it on the first connection while every other X server
/// ignores it (ADR 0021). A template that already defines any of these
/// classes or names a bridge tag is refused rather than merged.
#[cfg(any(target_os = "linux", test))]
fn with_session_input_isolation(config: &str, x_display: &str) -> Result<String, LauncherError> {
    use arcen_input::virtual_device::{
        SessionInputIdentity, FOREIGN_INPUT_CLASS, SESSION_INPUT_CLASS,
    };
    let identity =
        SessionInputIdentity::for_x_display(x_display).map_err(|_| LauncherError::XorgConfig)?;
    use arcen_usb_bridge::bridge_seat::{
        xorg_session_bridge_class, XORG_BRIDGE_TAG, XORG_SESSION_BRIDGE_CLASS,
    };
    if config.contains(FOREIGN_INPUT_CLASS)
        || config.contains(SESSION_INPUT_CLASS)
        || config.contains(XORG_SESSION_BRIDGE_CLASS)
        || config.contains(XORG_BRIDGE_TAG)
    {
        return Err(LauncherError::XorgConfig);
    }
    let display = parse_dedicated_display(x_display)?;
    let mut rendered = config.to_owned();
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered.push('\n');
    rendered.push_str(&identity.xorg_input_classes());
    rendered.push('\n');
    rendered.push_str(&xorg_session_bridge_class(display));
    Ok(rendered)
}

/// The X server never terminates on its zap chord (`Terminate_Server`,
/// Ctrl+Alt+Backspace under the `terminate:ctrl_alt_bksp` XKB option): a
/// Deck's keys, or a layout a session loads, must not end the desktop.
/// `DontZap` is set inside the template's first `ServerFlags` section (one
/// is added when it has none), replacing any `DontZap` the template sets.
#[cfg(any(target_os = "linux", test))]
fn with_dont_zap(config: &str) -> String {
    const DONT_ZAP: &str = "    Option         \"DontZap\" \"true\"";
    let mut out: Vec<&str> = Vec::with_capacity(config.lines().count() + 4);
    let mut placed = false;
    for line in config.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.first() == Some(&"Option") && words.get(1) == Some(&"\"DontZap\"") {
            continue;
        }
        out.push(line);
        if !placed && words == ["Section", "\"ServerFlags\""] {
            out.push(DONT_ZAP);
            placed = true;
        }
    }
    if !placed {
        out.extend(["Section \"ServerFlags\"", DONT_ZAP, "EndSection"]);
    }
    let mut rendered = out.join("\n");
    rendered.push('\n');
    rendered
}

#[cfg(target_os = "linux")]
fn render_xorg_config(template: &str, gpu_head: &str) -> Result<String, LauncherError> {
    validate_gpu_head(gpu_head)?;
    let source_heads = ["DFP-0", "DFP-1", "DFP-2", "DFP-3"]
        .into_iter()
        .filter(|head| template.contains(head))
        .collect::<Vec<_>>();
    if source_heads.len() != 1 {
        return Err(LauncherError::XorgConfig);
    }
    Ok(template
        .replace(source_heads[0], gpu_head)
        .replace(
            "\"AutoAddDevices\" \"false\"",
            "\"AutoAddDevices\" \"true\"",
        )
        .replace(
            "\"AutoEnableDevices\" \"false\"",
            "\"AutoEnableDevices\" \"true\"",
        ))
}

/// The root window depth from `xdpyinfo`'s `depth of root window:    30 planes`.
fn parse_root_depth(xdpyinfo: &str) -> Option<u8> {
    xdpyinfo.lines().find_map(|line| {
        line.trim()
            .strip_prefix("depth of root window:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

#[cfg(any(target_os = "linux", test))]
fn parse_dedicated_display(display: &str) -> Result<u16, LauncherError> {
    display
        .strip_prefix(':')
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|number| (1..=99).contains(number))
        .ok_or(LauncherError::XorgConfig)
}

#[cfg(target_os = "linux")]
fn validate_gpu_head(gpu_head: &str) -> Result<(), LauncherError> {
    arcen_outputs::is_nvidia_dfp_head_token(gpu_head)
        .then_some(())
        .ok_or(LauncherError::XorgConfig)
}

#[cfg(target_os = "linux")]
fn safe_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(target_os = "linux")]
fn display_lock_path(display_number: u16) -> PathBuf {
    PathBuf::from(format!("/tmp/.X{display_number}-lock"))
}

#[cfg(target_os = "linux")]
fn display_socket_path(display_number: u16) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{display_number}"))
}

/// The dedicated Xorg is spawned by this root-run launcher, so only
/// root-owned residue in the world-writable `/tmp` rendezvous directories can
/// possibly be ours to reclaim.
#[cfg(target_os = "linux")]
const XORG_OWNER_UID: u32 = 0;

/// What the dedicated display's `/tmp` lock file proves about its owner.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayLockState {
    /// A root-owned regular lock naming a process that no longer exists.
    AbandonedByDeadOwner,
    /// A root-owned regular lock naming a process that is still running.
    HeldByLiveOwner,
    /// Anything this host refuses to reason about: unreadable, malformed,
    /// not a regular file (symlink, directory, device), or not root-owned.
    Unrecognized,
}

/// What the dedicated display's `/tmp` socket proves about its owner.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplaySocketState {
    /// A root-owned socket with nothing listening: `connect` was refused.
    Abandoned,
    /// A server accepted a connection on it.
    ServingClients,
    /// Anything this host refuses to reason about: not a socket, not
    /// root-owned, or a `connect` failure that is not a plain refusal.
    Unrecognized,
}

/// Whether the residue found at a dedicated display number may be removed so
/// this session can start its own server there.
///
/// `None` means the artifact is simply absent. Reclaim is allowed *only* when
/// every artifact that exists positively proves abandonment — a dead lock
/// owner and a socket nothing is listening on. Every other combination,
/// including anything unrecognized, keeps the historical fail-closed refusal:
/// a live server, a lock this host did not write, or an inspection that could
/// not complete must never lead to deleting another server's rendezvous
/// files.
#[cfg(target_os = "linux")]
const fn stale_display_residue_is_reclaimable(
    lock: Option<DisplayLockState>,
    socket: Option<DisplaySocketState>,
) -> bool {
    matches!(
        (lock, socket),
        (None | Some(DisplayLockState::AbandonedByDeadOwner), None)
            | (None, Some(DisplaySocketState::Abandoned))
            | (
                Some(DisplayLockState::AbandonedByDeadOwner),
                Some(DisplaySocketState::Abandoned)
            )
    )
}

/// Reads `/tmp/.X<n>-lock` and classifies its owner, or `None` when absent.
///
/// X writes the server PID right-aligned in a fixed-width field, so the parse
/// tolerates surrounding whitespace and nothing else. Liveness reuses
/// [`pid_is_live`], so a zombie reads as dead and a recycled PID reads as
/// live — the safe direction in both cases.
#[cfg(target_os = "linux")]
fn inspect_display_lock(path: &Path, trusted_uid: u32) -> Option<DisplayLockState> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.uid() != trusted_uid {
        return Some(DisplayLockState::Unrecognized);
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Some(DisplayLockState::Unrecognized);
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return Some(DisplayLockState::Unrecognized);
    };
    if pid == 0 {
        return Some(DisplayLockState::Unrecognized);
    }
    if pid_is_live(pid) {
        Some(DisplayLockState::HeldByLiveOwner)
    } else {
        Some(DisplayLockState::AbandonedByDeadOwner)
    }
}

/// Probes `/tmp/.X11-unix/X<n>` and classifies it, or `None` when absent.
///
/// A refused `connect` is the only positive proof that no server is bound to
/// the socket; a successful connect proves the opposite and every other error
/// is inconclusive.
#[cfg(target_os = "linux")]
fn inspect_display_socket(path: &Path, trusted_uid: u32) -> Option<DisplaySocketState> {
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::fs::MetadataExt as _;

    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_socket() || metadata.uid() != trusted_uid {
        return Some(DisplaySocketState::Unrecognized);
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Some(DisplaySocketState::ServingClients),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            Some(DisplaySocketState::Abandoned)
        }
        Err(_) => Some(DisplaySocketState::Unrecognized),
    }
}

/// Clears the abandoned lock/socket left at `display_number` by a dedicated
/// Xorg that died without cleaning up, so the operator-pinned display number
/// does not stay permanently unusable.
///
/// The pinned display (`pier.json`) is the same for every session on this
/// host, so a crashed, OOM-killed or administratively terminated Xorg
/// otherwise fails *every* later session in `preflight` until somebody
/// removes the files by hand.
///
/// # Errors
///
/// Returns [`LauncherError::XorgStart`] whenever the residue is not provably
/// abandoned — the same fail-closed refusal this preflight has always made.
#[cfg(target_os = "linux")]
fn reclaim_stale_display(display_number: u16) -> Result<(), LauncherError> {
    reclaim_stale_display_at(
        &display_lock_path(display_number),
        &display_socket_path(display_number),
        XORG_OWNER_UID,
        display_number,
    )
}

/// The reclaim decision itself, parameterised over the two paths and the uid
/// that must own them.
///
/// Recovery is deliberately narrow: it refuses unless
/// [`stale_display_residue_is_reclaimable`] proves both artifacts are
/// abandoned, removes only those two exact paths, and verifies afterwards
/// that both are gone. Deleting a live server's rendezvous files would be far
/// worse than refusing a session, so every doubt resolves to refusal.
///
/// # Errors
///
/// Returns [`LauncherError::XorgStart`] when the residue is not provably
/// abandoned, when removal fails, or when anything reappears.
#[cfg(target_os = "linux")]
fn reclaim_stale_display_at(
    lock_path: &Path,
    socket_path: &Path,
    trusted_uid: u32,
    display_number: u16,
) -> Result<(), LauncherError> {
    let lock = inspect_display_lock(lock_path, trusted_uid);
    let socket = inspect_display_socket(socket_path, trusted_uid);
    if lock.is_none() && socket.is_none() {
        return Ok(());
    }
    if !stale_display_residue_is_reclaimable(lock, socket) {
        tracing::warn!(
            target: target::SESSION,
            display_number,
            ?lock,
            ?socket,
            "dedicated display is not provably free; refusing to start a server on it"
        );
        return Err(LauncherError::XorgStart);
    }
    for path in [lock_path, socket_path] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(LauncherError::XorgStart),
        }
    }
    if lock_path.exists() || socket_path.exists() {
        return Err(LauncherError::XorgStart);
    }
    tracing::warn!(
        target: target::SESSION,
        display_number,
        "reclaimed the abandoned lock and socket of a dedicated Xorg that died without cleaning up"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
async fn command_output_with_timeout(
    mut command: Command,
    timeout_error: LauncherError,
) -> Result<std::process::Output, LauncherError> {
    command.kill_on_drop(true);
    match tokio::time::timeout(LOGIND_COMMAND_TIMEOUT, command.output()).await {
        Ok(result) => result.map_err(LauncherError::Io),
        Err(_) => Err(timeout_error),
    }
}

#[cfg(target_os = "linux")]
async fn command_status_with_timeout(
    mut command: Command,
    timeout_error: LauncherError,
) -> Result<std::process::ExitStatus, LauncherError> {
    command.kill_on_drop(true);
    match tokio::time::timeout(LOGIND_COMMAND_TIMEOUT, command.status()).await {
        Ok(result) => result.map_err(LauncherError::Io),
        Err(_) => Err(timeout_error),
    }
}

#[cfg(target_os = "linux")]
pub(super) async fn activate_logind_session(
    environment: &SessionEnvironment,
) -> Result<(), LauncherError> {
    let session_id = environment
        .session_id()
        .filter(|session_id| safe_session_id(session_id))
        .ok_or(LauncherError::LogindActivation)?;
    let mut activate = Command::new("/usr/bin/busctl");
    activate.args([
        "call",
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "ActivateSessionOnSeat",
        "ss",
        session_id,
        "seat0",
    ]);
    let status = command_status_with_timeout(activate, LauncherError::LogindActivation).await?;
    if !status.success() {
        return Err(LauncherError::LogindActivation);
    }

    for _ in 0..25 {
        let mut show = Command::new("/usr/bin/loginctl");
        show.args(["show-session", session_id, "-p", "Active", "-p", "Seat"]);
        let output = command_output_with_timeout(show, LauncherError::LogindActivation).await?;
        let properties = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            && properties.lines().any(|line| line == "Active=yes")
            && properties.lines().any(|line| line == "Seat=seat0")
        {
            tracing::info!(
                target: target::SESSION,
                session_id,
                "logind session activated on seat0"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(LauncherError::LogindActivation)
}

#[cfg(target_os = "linux")]
pub async fn unlock_logind_session_id(session_id: &str) -> Result<(), LauncherError> {
    if !safe_session_id(session_id) {
        return Err(LauncherError::LogindUnlock);
    }
    let mut unlock = Command::new("/usr/bin/loginctl");
    unlock.args(["unlock-session", session_id]);
    let status = command_status_with_timeout(unlock, LauncherError::LogindUnlock).await?;
    if !status.success() {
        return Err(LauncherError::LogindUnlock);
    }
    for _ in 0..25 {
        let mut show = Command::new("/usr/bin/loginctl");
        show.args(["show-session", session_id, "-p", "LockedHint", "--value"]);
        let output = command_output_with_timeout(show, LauncherError::LogindUnlock).await?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "no" {
            tracing::info!(
                target: target::SESSION,
                session_id,
                "authenticated logind session unlocked"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(LauncherError::LogindUnlock)
}

#[cfg(target_os = "linux")]
pub async fn validate_active_logind_session(
    session_id: &str,
    expected_uid: u32,
) -> Result<(), LauncherError> {
    if !safe_session_id(session_id) {
        return Err(LauncherError::LogindSession);
    }
    let mut show = Command::new("/usr/bin/loginctl");
    show.args(["show-session", session_id, "-p", "Active", "-p", "User"]);
    let output = command_output_with_timeout(show, LauncherError::LogindSession).await?;
    if !output.status.success()
        || !active_logind_properties_match(&String::from_utf8_lossy(&output.stdout), expected_uid)
    {
        return Err(LauncherError::LogindSession);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub async fn validate_active_logind_session(
    _session_id: &str,
    _expected_uid: u32,
) -> Result<(), LauncherError> {
    Err(LauncherError::LogindSession)
}

fn active_logind_properties_match(properties: &str, expected_uid: u32) -> bool {
    let expected_user = format!("User={expected_uid}");
    properties.lines().any(|line| line.trim() == "Active=yes")
        && properties.lines().any(|line| line.trim() == expected_user)
}

#[cfg(not(target_os = "linux"))]
pub async fn unlock_logind_session_id(_session_id: &str) -> Result<(), LauncherError> {
    Err(LauncherError::LogindUnlock)
}

#[cfg(not(target_os = "linux"))]
pub async fn run_launcher(_args: &[String]) -> Result<(), LauncherError> {
    Err(LauncherError::RootRequired)
}

#[cfg(target_os = "linux")]
async fn validate_logind_ownership(
    environment: &SessionEnvironment,
    identity: &UserIdentity,
) -> Result<(), LauncherError> {
    let session_id = environment
        .session_id()
        .filter(|session_id| !session_id.is_empty())
        .ok_or(LauncherError::LogindSession)?;
    let mut show = Command::new("/usr/bin/loginctl");
    show.args([
        "show-session",
        session_id,
        "-p",
        "User",
        "-p",
        "Leader",
        "--value",
    ]);
    let output = command_output_with_timeout(show, LauncherError::LogindSession).await?;
    if !output.status.success() {
        return Err(LauncherError::LogindSession);
    }
    let output_text = String::from_utf8_lossy(&output.stdout);
    let values = output_text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let uid = identity.uid.to_string();
    let pid = std::process::id().to_string();
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")?;
    if !values.contains(&uid.as_str())
        || !values.contains(&pid.as_str())
        || !cgroup.contains(&format!("session-{session_id}.scope"))
    {
        return Err(LauncherError::LogindSession);
    }
    Ok(())
}

/// The launcher-protocol status for a logind failure, so the broker reports
/// what happened instead of a protocol failure.
fn logind_error_status(error: &LauncherError) -> &'static str {
    match error {
        LauncherError::SeatOccupied => "seat_occupied",
        LauncherError::SeatOccupiedOtherProduct(_) => "seat_occupied_other_product",
        LauncherError::Console(notice) => notice.token(),
        LauncherError::LogindActivation => "logind_activation_error",
        _ => "logind_error",
    }
}

/// Whether another session holds the X11 desktop position on seat0, from
/// logind's own session properties. A probe that fails says "not occupied":
/// the caller then reports the plain logind failure it already had.
#[cfg(target_os = "linux")]
async fn x11_seat_is_occupied() -> bool {
    let mut seat = Command::new("/usr/bin/loginctl");
    seat.args(["show-seat", "seat0", "-p", "Sessions", "--value"]);
    let Ok(output) = command_output_with_timeout(seat, LauncherError::LogindSession).await else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let mut sessions = Vec::new();
    for session_id in String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter(|session_id| safe_session_id(session_id))
    {
        let mut show = Command::new("/usr/bin/loginctl");
        show.args([
            "show-session",
            session_id,
            "-p",
            "Id",
            "-p",
            "Seat",
            "-p",
            "VTNr",
            "-p",
            "Class",
            "-p",
            "State",
            "-p",
            "Type",
        ]);
        let Ok(output) = command_output_with_timeout(show, LauncherError::LogindSession).await
        else {
            continue;
        };
        if let Some(fact) = arcen_session::x11_seat::SeatSessionFact::from_properties(
            &String::from_utf8_lossy(&output.stdout),
        ) {
            sessions.push(fact);
        }
    }
    let Some(occupant) = arcen_session::x11_seat::x11_seat_occupant(&sessions) else {
        return false;
    };
    tracing::warn!(
        target: target::SESSION,
        occupant_session = %occupant.id,
        occupant_state = %occupant.state,
        occupant_type = %occupant.session_type,
        "another desktop holds seat0's X11 position; one X11 desktop per host, so this sign-in is refused"
    );
    true
}

fn write_response(response: &LauncherResponse) -> Result<(), LauncherError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, response).map_err(|_| LauncherError::Protocol)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn open_response_error(
    status: &str,
    service: Option<arcen_session::console_session::ForeignService>,
) -> LauncherError {
    match status {
        "logind_error" => LauncherError::LogindSession,
        "logind_activation_error" => LauncherError::LogindActivation,
        "seat_occupied" => LauncherError::SeatOccupied,
        "seat_occupied_other_product" => LauncherError::SeatOccupiedOtherProduct(service),
        "pam_error" => LauncherError::PamSession,
        "xorg_config_error" => LauncherError::XorgConfig,
        "xorg_start_error" => LauncherError::XorgStart,
        "xorg_verify_error" => LauncherError::XorgVerify,
        "xorg_release_error" => LauncherError::XorgRelease,
        "wayland_start_error" => LauncherError::WaylandStart,
        "selinux_error" => LauncherError::SelinuxContext,
        "local_playback_refused" => LauncherError::LocalPlaybackRefused,
        "agent_error" => LauncherError::Agent(AgentError::ExitedBeforeReady),
        status => arcen_session::console_session::ConsoleNotice::from_status(status, service)
            .map_or(LauncherError::Protocol, LauncherError::Console),
    }
}

/// The launcher's command stream after the `authenticate` line: whatever
/// followed that line in the same read, then the rest of standard input.
type LauncherCommands = BufReader<tokio::io::Chain<std::io::Cursor<Vec<u8>>, tokio::io::Stdin>>;

/// The buffer the broker's `authenticate` line is serialized into: room for
/// every byte escaped as `\u00XX` plus the JSON keys, so it never grows.
fn authenticate_request_capacity(username: &str, password: &str, remote_host: &str) -> usize {
    6 * (username.len() + password.len() + remote_host.len()) + 128
}

/// Reads one `\n`-terminated line from `fd` into a single pre-allocated
/// zeroizing buffer of `max_bytes + 1` bytes, bounded in size and time. The
/// buffer is never reallocated. Returns the line (without `\r\n`) and any
/// bytes that followed it, which belong to the next command. End of input
/// ends the line as `read_bounded_line` does; an empty input, an oversize
/// line, a read error or the timeout is an error.
#[cfg(target_os = "linux")]
fn read_auth_request_line(
    fd: std::os::fd::RawFd,
    max_bytes: usize,
    budget: Duration,
) -> Result<(Zeroizing<Vec<u8>>, Vec<u8>), LauncherError> {
    let deadline = std::time::Instant::now() + budget;
    let mut buffer = Zeroizing::new(vec![0_u8; max_bytes + 1]);
    let mut filled = 0;
    let line_end = loop {
        if let Some(newline) = buffer[..filled].iter().position(|&byte| byte == b'\n') {
            break newline;
        }
        if filled == buffer.len() {
            return Err(LauncherError::Protocol);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(LauncherError::Timeout);
        }
        let mut poll = nix::libc::pollfd {
            fd,
            events: nix::libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = i32::try_from(remaining.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        // SAFETY: one valid pollfd on the stack, count 1.
        let ready = unsafe { nix::libc::poll(&raw mut poll, 1, timeout_ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(LauncherError::Io(error));
        }
        if ready == 0 {
            return Err(LauncherError::Timeout);
        }
        match nix::unistd::read(fd, &mut buffer[filled..]) {
            Ok(0) if filled == 0 => return Err(LauncherError::Protocol),
            Ok(0) => break filled,
            Ok(read) => filled += read,
            Err(nix::errno::Errno::EINTR | nix::errno::Errno::EAGAIN) => {}
            Err(errno) => return Err(LauncherError::Io(errno.into())),
        }
    };
    if line_end > max_bytes {
        return Err(LauncherError::Protocol);
    }
    let pending = buffer[(line_end + 1).min(filled)..filled].to_vec();
    let mut line_len = line_end;
    if line_len > 0 && buffer[line_len - 1] == b'\r' {
        line_len -= 1;
    }
    // Shorten in place: the bytes past the line are zeroed with the buffer.
    let mut line = buffer;
    line[line_len..].iter_mut().for_each(|byte| *byte = 0);
    line.truncate(line_len);
    Ok((line, pending))
}

async fn read_bounded_line<R>(reader: &mut R, max_bytes: usize) -> Result<String, LauncherError>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let read = reader
        .take((max_bytes + 1) as u64)
        .read_line(&mut line)
        .await?;
    if read == 0 || read > max_bytes {
        return Err(LauncherError::Protocol);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

fn parse_open_request(line: &str) -> Result<OpenRequest, LauncherError> {
    if line.len() > MAX_OPEN_REQUEST_BYTES {
        return Err(LauncherError::Protocol);
    }
    let request: OpenRequest = serde_json::from_str(line).map_err(|_| LauncherError::Protocol)?;
    if request.command != "open" {
        return Err(LauncherError::Protocol);
    }
    Ok(request)
}

fn parse_close_request(line: &str) -> Result<bool, LauncherError> {
    if line.len() > MAX_OPEN_REQUEST_BYTES {
        return Err(LauncherError::Protocol);
    }
    Ok(serde_json::from_str::<CloseRequest>(line).is_ok_and(|request| request.command == "close"))
}

fn argument_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn required_argument(args: &[String], flag: &str) -> Result<String, LauncherError> {
    argument_value(args, flag).ok_or(LauncherError::Protocol)
}

/// The launcher's stop signals, registered at construction so a stop that
/// arrives before anyone awaits it is not lost — and not fatal: once
/// installed, SIGTERM, SIGINT and SIGHUP no longer terminate the launcher.
///
/// logind ends a session (logout, `loginctl terminate-session`) with SIGTERM
/// plus SIGHUP to every process in its scope; SIGHUP's default action would
/// kill the launcher before it released the desktop.
#[cfg(target_os = "linux")]
pub(super) struct StopSignals {
    term: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(target_os = "linux")]
impl StopSignals {
    pub(super) fn install() -> Result<Self, LauncherError> {
        use tokio::signal::unix::{signal, SignalKind};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            interrupt: signal(SignalKind::interrupt())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    pub(super) async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.interrupt.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

#[cfg(target_os = "linux")]
async fn launcher_shutdown_signal() {
    match StopSignals::install() {
        Ok(mut signals) => signals.recv().await,
        Err(_) => std::future::pending().await,
    }
}

pub fn launcher_main(args: &[String]) -> ExitCode {
    // A short-lived, privileged (root/PAM) child: like the session agent,
    // its stderr is captured and re-emitted by the broker, so this
    // process's own canonical writer targets stderr directly. No shared
    // canonical `session_launcher` component exists in
    // `arcen_telemetry::names`, so a locally-constructed value is used —
    // `TelemetryComponent::new` validates generic bounded snake_case, not
    // a fixed enum, so this is schema-valid.
    let observability = match arcen_observability::ObservabilityBuilder::new(
        arcen_telemetry::TelemetryRole::Host,
        arcen_telemetry::TelemetryComponent::new("session_launcher")
            .expect("session_launcher is a valid canonical component"),
        arcen_telemetry::TelemetryPlatform::Linux,
        arcen_telemetry::OperationalProfile::Critical,
    )
    .canonical_writer("stderr", std::io::stderr())
    .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("session-launcher: logging setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = tracing::dispatcher::set_global_default(observability.dispatch()) {
        eprintln!("session-launcher: failed to install tracing dispatch: {error}");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "failed to create session-launcher runtime");
            return ExitCode::FAILURE;
        }
    };
    let exit_code = match runtime.block_on(run_launcher(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "session-launcher failed");
            ExitCode::FAILURE
        }
    };
    // The desktop is already torn down and PAM closed. A blocking stdin read
    // the broker never answers must not keep this process — the logind
    // session's leader — alive, or logind holds the session in `closing`.
    runtime.shutdown_timeout(std::time::Duration::from_millis(500));
    let _ = observability
        .guard()
        .shutdown(std::time::Duration::from_secs(2));
    exit_code
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_session_x_server_gets_the_system_quirks_plus_the_virtual_pens() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!(".arcen-quirks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let system = root.join("system");
        let session = root.join("session");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::create_dir_all(&session).unwrap();
        let override_file = root.join("local-overrides.quirks");

        // No system quirks: keep libinput's defaults rather than none at all.
        assert_eq!(
            super::prepare_libinput_quirks(&system, &override_file, &session),
            None
        );

        std::fs::write(
            system.join("10-generic-mouse.quirks"),
            "[a]\nMatchUdevType=mouse\n",
        )
        .unwrap();
        std::fs::write(system.join("README"), "not a quirks file").unwrap();
        std::fs::write(&override_file, "[local]\nMatchName=x\n").unwrap();
        let dir = super::prepare_libinput_quirks(&system, &override_file, &session).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "10-generic-mouse.quirks",
                arcen_input::virtual_device::LIBINPUT_QUIRKS_FILE_NAME,
                super::SESSION_LOCAL_OVERRIDE_NAME,
            ]
        );
        assert_eq!(
            std::fs::read_to_string(
                dir.join(arcen_input::virtual_device::LIBINPUT_QUIRKS_FILE_NAME)
            )
            .unwrap(),
            arcen_input::virtual_device::libinput_quirks()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn launcher_errors_keep_their_severity_in_the_broker_log() {
        use super::launcher_line_severity;
        assert_eq!(
            launcher_line_severity(
                r#"{"severity":"error","message":"dedicated Xorg failed to start"}"#
            ),
            tracing::Level::ERROR
        );
        assert_eq!(
            launcher_line_severity(r#"{"severity":"warn","message":"x"}"#),
            tracing::Level::WARN
        );
        assert_eq!(
            launcher_line_severity("ERROR session-launcher failed"),
            tracing::Level::ERROR
        );
        assert_eq!(
            launcher_line_severity(r#"{"severity":"info","message":"error in name only"}"#),
            tracing::Level::INFO
        );
    }

    #[test]
    fn the_broker_outwaits_a_bounded_wayland_start() {
        use arcen_session::desktop_provider::DesktopProvider;
        assert_eq!(open_timeout(DesktopProvider::Xorg), OPEN_TIMEOUT);
        assert!(
            open_timeout(DesktopProvider::Wayland)
                > super::super::wayland::WAYLAND_AGENT_READY_TIMEOUT
        );
        assert!(
            super::super::wayland::WAYLAND_AGENT_READY_TIMEOUT
                > super::super::wayland::WAYLAND_START_BUDGET
        );
    }

    #[test]
    fn wayland_sessions_must_be_seatless_user_wayland_sessions() {
        assert!(wayland_properties_match(
            "Type=wayland\nClass=user\nSeat=\n",
            None
        ));
        assert!(!wayland_properties_match(
            "Type=wayland\nClass=user\nSeat=seat0\n",
            None
        ));
        assert!(!wayland_properties_match(
            "Type=x11\nClass=user\nSeat=\n",
            None
        ));
        assert!(!wayland_properties_match(
            "Type=wayland\nClass=background\nSeat=\n",
            None
        ));
    }

    #[test]
    fn a_native_seat_session_must_be_on_exactly_its_own_seat() {
        let seat = "seat-arcen-pen-0123456789abcdef";
        assert!(wayland_properties_match(
            "Type=wayland\nClass=user\nSeat=seat-arcen-pen-0123456789abcdef\n",
            Some(seat)
        ));
        for refused in [
            "Type=wayland\nClass=user\nSeat=\n",
            "Type=wayland\nClass=user\nSeat=seat0\n",
            "Type=wayland\nClass=user\nSeat=seat-arcen-pen-fedcba9876543210\n",
        ] {
            assert!(!wayland_properties_match(refused, Some(seat)), "{refused}");
        }
    }

    #[test]
    fn only_a_valid_desktop_seat_of_a_native_request_reaches_pam() {
        let request = || {
            OpenRequest::new(
                None,
                crate::deskside::LinuxDesksideConfig::default(),
                None,
                None,
                &AgentAudioArguments::default(),
                None,
            )
        };
        let seat = "seat-arcen-pen-0123456789abcdef";
        assert_eq!(request().native_seat(), None);
        assert_eq!(
            request().with_native_seat(true).native_seat(),
            None,
            "no seat, no native seat"
        );
        let native = request()
            .with_lab_input_seat(Some(seat.into()))
            .with_native_seat(true);
        assert_eq!(native.native_seat(), Some(seat));
        let json = serde_json::to_string(&native).unwrap();
        assert_eq!(parse_open_request(&json).unwrap().native_seat(), Some(seat));
        let seat0 = request()
            .with_lab_input_seat(Some("seat0".into()))
            .with_native_seat(true);
        assert_eq!(seat0.native_seat(), None);
        let headless = request()
            .with_lab_input_seat(Some(seat.into()))
            .with_native_seat(false);
        assert_eq!(headless.native_seat(), None);
        assert!(!serde_json::to_string(&headless).unwrap().contains("native"));
    }

    #[test]
    fn a_headless_wayland_session_opens_a_committed_virtual_topology_primary_first() {
        use crate::display::topology::{
            plan_virtual_topology, wayland_output_limits, LinuxTopologyBackend,
        };
        let monitor = |id: &str, x: i32, primary: bool| {
            arcen_media::RequestedMonitor::new(
                arcen_media::Monitor {
                    identity: arcen_media::MonitorIdentity {
                        id: id.to_owned(),
                        ..arcen_media::MonitorIdentity::default()
                    },
                    x,
                    y: 0,
                    width_px: 1920,
                    height_px: 1080,
                    scale: 1.0,
                    refresh_hz: 60,
                    rotation: arcen_media::Rotation::Degrees0,
                    primary,
                    width_mm: 0.0,
                    height_mm: 0.0,
                    color: None,
                },
                1920,
                1080,
            )
            .unwrap()
        };
        let plan = plan_virtual_topology(
            &arcen_media::RequestedMonitorTopology::new(vec![
                monitor("left", -1920, false),
                monitor("main", 0, true),
            ])
            .unwrap(),
            arcen_media::TopologyGeneration::new(1).unwrap(),
            wayland_output_limits(4),
            arcen_session::wayland_outputs::VirtualOutputMode::RemoteMonitors,
        )
        .unwrap();
        let mut request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            Some(MultiHeadPlanMsg::from_plan(&plan)),
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let outputs = wayland_session_outputs(&request).unwrap();
        assert_eq!(
            outputs.iter().map(|output| output.x).collect::<Vec<_>>(),
            vec![1920, 0],
            "the primary comes first, each output at its planned place"
        );
        // The DTO round-trips the backend and the refresh.
        let back = MultiHeadPlanMsg::from_plan(&plan).into_plan().unwrap();
        assert_eq!(back, plan);

        // A plan made for dedicated Xorg never opens a Wayland session.
        let mut xorg = plan.clone();
        xorg.backend = LinuxTopologyBackend::DedicatedXorg;
        request.multi_monitor = Some(MultiHeadPlanMsg::from_plan(&xorg));
        assert!(wayland_session_outputs(&request).is_err());
    }

    #[test]
    fn wayland_output_follows_the_served_display() {
        let default = wayland_output_request(None).unwrap();
        assert_eq!(
            (default.width(), default.height(), default.refresh_hz()),
            super::super::wayland::DEFAULT_OUTPUT
        );
        let served = SessionDisplayMsg {
            width: 2560,
            height: 1600,
            refresh_hz: 120,
            width_mm: 0.0,
            height_mm: 0.0,
            scale: 2.0,
            product_id: 1,
            serial: 0,
            color: None,
        };
        let output = wayland_output_request(Some(&served)).unwrap();
        assert_eq!(
            (output.width(), output.height(), output.refresh_hz()),
            (2560, 1600, 120)
        );
        let refused = SessionDisplayMsg { width: 0, ..served };
        assert!(wayland_output_request(Some(&refused)).is_err());
    }

    use super::*;

    #[cfg(target_os = "linux")]
    fn pipe_with(
        bytes: &[u8],
        close_writer: bool,
    ) -> (std::os::fd::OwnedFd, Option<std::fs::File>) {
        use std::io::Write as _;
        let (reader, writer) = nix::unistd::pipe().unwrap();
        let mut writer = std::fs::File::from(writer);
        writer.write_all(bytes).unwrap();
        (reader, (!close_writer).then_some(writer))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_authenticate_line_is_read_whole_and_what_follows_is_kept_for_the_next_command() {
        use std::os::fd::AsRawFd as _;
        let line = br#"{"command":"authenticate","username":"u","password":"p\"w!-","remote_host":"203.0.113.7"}"#;
        let mut input = line.to_vec();
        input.extend_from_slice(b"\r\n{\"command\":\"open\"}\n");
        let (reader, _writer) = pipe_with(&input, false);
        let (read, pending) =
            read_auth_request_line(reader.as_raw_fd(), MAX_AUTH_REQUEST_BYTES, AUTH_TIMEOUT)
                .unwrap();
        assert_eq!(&read[..], &line[..]);
        assert_eq!(
            read.capacity(),
            MAX_AUTH_REQUEST_BYTES + 1,
            "never reallocated"
        );
        assert_eq!(pending, b"{\"command\":\"open\"}\n");
        let parsed: OwnedAuthenticateRequest = serde_json::from_slice(&read).unwrap();
        assert_eq!(parsed.password, "p\"w!-");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_authenticate_line_that_is_oversize_empty_or_late_is_refused() {
        use std::os::fd::AsRawFd as _;
        let (reader, _writer) = pipe_with(&[b'x'; 65], false);
        assert!(matches!(
            read_auth_request_line(reader.as_raw_fd(), 64, AUTH_TIMEOUT),
            Err(LauncherError::Protocol)
        ));
        let (reader, _) = pipe_with(b"", true);
        assert!(matches!(
            read_auth_request_line(reader.as_raw_fd(), 64, AUTH_TIMEOUT),
            Err(LauncherError::Protocol)
        ));
        let (reader, _writer) = pipe_with(b"{\"command\"", false);
        assert!(matches!(
            read_auth_request_line(reader.as_raw_fd(), 64, Duration::from_millis(50)),
            Err(LauncherError::Timeout)
        ));
        let (reader, _) = pipe_with(b"{\"a\":1}", true);
        let (line, pending) = read_auth_request_line(reader.as_raw_fd(), 64, AUTH_TIMEOUT).unwrap();
        assert_eq!(&line[..], b"{\"a\":1}");
        assert!(pending.is_empty());
    }

    #[test]
    fn the_authenticate_request_buffer_never_grows_even_fully_escaped() {
        let password = "\u{1}\"\\".repeat(40);
        let username = "\u{2}".repeat(20);
        let remote_host = "\u{7f}\n".repeat(10);
        let request = AuthenticateRequest {
            command: "authenticate",
            username: &username,
            password: &password,
            remote_host: &remote_host,
        };
        let capacity = authenticate_request_capacity(&username, &password, &remote_host);
        let mut buffer = Vec::with_capacity(capacity);
        serde_json::to_writer(&mut buffer, &request).unwrap();
        buffer.push(b'\n');
        assert_eq!(buffer.capacity(), capacity);
    }

    #[test]
    fn dedicated_xorg_release_is_only_claimed_when_every_artifact_is_gone() {
        // `DedicatedXorg::release` may only return `Ok` once nothing the
        // attempt created is still on disk -- ADR 0010's `DedicatedPhysical`
        // rollback postcondition. A single leftover lock, socket, or session
        // directory keeps the obligation outstanding.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let present = manifest.join("Cargo.toml");
        let absent = manifest.join("dedicated-xorg-artifact-that-never-exists");
        let released: [&Path; 3] = [absent.as_path(), absent.as_path(), absent.as_path()];
        assert!(dedicated_xorg_artifacts_released(&released));
        for leftover in 0..3 {
            let mut artifacts = released;
            artifacts[leftover] = present.as_path();
            assert!(
                !dedicated_xorg_artifacts_released(&artifacts),
                "a leftover artifact at index {leftover} must refuse the release claim"
            );
        }
    }

    #[tokio::test]
    async fn launcher_stderr_bounds_an_enormous_unterminated_line_and_keeps_reading() {
        // Same integration proof as the session-agent path: an enormous,
        // never-terminated stderr line from the launcher helper must not
        // hang the reader or grow memory with the input.
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        let writer_task = tokio::spawn(async move {
            let huge = vec![b'y'; 1024 * 1024];
            tokio::io::AsyncWriteExt::write_all(&mut writer, &huge)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut writer, b"\nafter\n")
                .await
                .unwrap();
        });
        let session_log_id = Arc::new(Mutex::new(
            CorrelationId::parse_uuid("00000000-0000-4000-8000-000000000000").unwrap(),
        ));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_stderr(reader, session_log_id),
        )
        .await;
        assert!(
            result.is_ok(),
            "reading an enormous unterminated line must complete promptly, not hang"
        );
        writer_task.await.unwrap();
    }

    #[test]
    fn the_dedicated_servers_gpu_reaches_capture_as_a_canonical_address() {
        assert_eq!(
            capture_gpu_environment("0000:00:10.0"),
            Some((
                "ARCEN_CAPTURE_GPU_PCI".to_string(),
                "0000:00:10.0".to_string()
            ))
        );
        assert_eq!(
            capture_gpu_environment("0000:3B:00.0"),
            Some((
                "ARCEN_CAPTURE_GPU_PCI".to_string(),
                "0000:3b:00.0".to_string()
            ))
        );
        assert_eq!(capture_gpu_environment(""), None);
        assert_eq!(capture_gpu_environment("PCI:0:16:0"), None);
        assert_eq!(capture_gpu_environment("0000:00:10.0\nLD_PRELOAD=x"), None);
    }

    #[test]
    fn launcher_responses_carry_the_capture_gpu_and_accept_older_launchers() {
        let error = serde_json::to_value(LauncherResponse::error("pam_error")).unwrap();
        assert_eq!(error["capture_gpu_pci"], "");
        let older: LauncherResponse =
            serde_json::from_str(r#"{"status":"ready","username":"u","uid":1000}"#).unwrap();
        assert!(older.capture_gpu_pci.is_empty());
        let newer: LauncherResponse =
            serde_json::from_str(r#"{"status":"ready","capture_gpu_pci":"0000:00:10.0"}"#).unwrap();
        assert_eq!(newer.capture_gpu_pci, "0000:00:10.0");
    }

    #[test]
    fn unavailable_identity_fields_are_empty_not_guessed() {
        let response = LauncherResponse::error("logind_error");
        assert!(response.username.is_empty());
        assert!(response.session_id.is_empty());
        assert!(response.session_type.is_empty());
        assert!(response.desktop.is_empty());
    }

    #[test]
    fn output_error_status_survives_the_launcher_protocol_boundary() {
        let cases = [
            (LauncherError::XorgConfig, "xorg_config_error"),
            (LauncherError::XorgStart, "xorg_start_error"),
            (LauncherError::XorgVerify, "xorg_verify_error"),
            (LauncherError::XorgRelease, "xorg_release_error"),
        ];
        for (error, status) in cases {
            assert_eq!(output_error_status(&error), status);
            assert_eq!(
                std::mem::discriminant(&open_response_error(status, None)),
                std::mem::discriminant(&error)
            );
        }
    }

    #[test]
    fn an_occupied_x11_seat_reaches_the_broker_as_a_refusal_not_a_protocol_failure() {
        for error in [
            LauncherError::SeatOccupied,
            LauncherError::LogindActivation,
            LauncherError::LogindSession,
        ] {
            let status = logind_error_status(&error);
            let response: LauncherResponse = serde_json::from_str(
                &serde_json::to_string(&LauncherResponse::error(status)).unwrap(),
            )
            .unwrap();
            assert_eq!(
                std::mem::discriminant(&open_response_error(&response.status, None)),
                std::mem::discriminant(&error)
            );
        }
        assert!(LauncherError::SeatOccupied
            .to_string()
            .contains("one X11 desktop per host"));
    }

    #[test]
    fn a_named_service_and_a_reattach_answer_survive_the_launcher_protocol_boundary() {
        use arcen_session::console_session::{ConsoleNotice, ConsoleRefusal, ForeignService};
        let service = ForeignService::new("labrdp-session", Some("labrdp.service"), None);
        let notice = ConsoleNotice::naming(ConsoleRefusal::OtherUserOtherProduct, Some(service));
        let line = serde_json::to_string(&LauncherResponse::console_refusal(&notice)).unwrap();
        let response: LauncherResponse = serde_json::from_str(&line).unwrap();
        let LauncherError::Console(received) =
            open_response_error(&response.status, response.console_service)
        else {
            panic!("a console refusal");
        };
        assert_eq!(received, notice);
        assert!(received.deck_reason().contains("labrdp-session"));
        // The broker's reading of a returning Deck's attachment.
        assert_eq!(
            console_attach_outcome(Some(r#"{"status":"attached"}"#)),
            Ok(())
        );
        assert_eq!(console_attach_outcome(Some(&line)), Err(notice));
        for silent in [None, Some("garbage"), Some(r#"{"status":"agent_error"}"#)] {
            assert_eq!(
                console_attach_outcome(silent),
                Err(ConsoleRefusal::SessionUnreachable.into()),
                "{silent:?}"
            );
        }
        let seat = LauncherResponse::error_naming(
            "seat_occupied_other_product",
            Some(&ForeignService::new("labrdp-session", None, None)),
        );
        let seat: LauncherResponse =
            serde_json::from_str(&serde_json::to_string(&seat).unwrap()).unwrap();
        assert!(matches!(
            open_response_error(&seat.status, seat.console_service),
            LauncherError::SeatOccupiedOtherProduct(Some(service)) if service.pam_service() == Some("labrdp-session")
        ));
    }

    #[test]
    fn selinux_and_wayland_refusals_survive_the_launcher_protocol_boundary() {
        for (status, error) in [
            ("selinux_error", LauncherError::SelinuxContext),
            ("wayland_start_error", LauncherError::WaylandStart),
            ("logind_error", LauncherError::LogindSession),
            ("seat_occupied", LauncherError::SeatOccupied),
            ("logind_activation_error", LauncherError::LogindActivation),
            (
                "local_playback_refused",
                LauncherError::LocalPlaybackRefused,
            ),
        ] {
            assert_eq!(
                std::mem::discriminant(&open_response_error(status, None)),
                std::mem::discriminant(&error)
            );
        }
    }

    #[test]
    fn open_request_carries_local_playback_and_defaults_to_muted() {
        let audible = AgentAudioArguments {
            local_playback: arcen_session::pier_config::LocalPlayback::Audible,
            pactl: PathBuf::from("/usr/bin/pactl"),
        };
        let request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &audible,
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"local_playback\":\"audible\""));
        let parsed = parse_open_request(&json).unwrap();
        assert_eq!(
            parsed.audio().local_playback,
            arcen_session::pier_config::LocalPlayback::Audible
        );
        // An older Pier's request names no policy: the safe default.
        let old = parse_open_request(r#"{"command":"open"}"#).unwrap();
        assert_eq!(
            old.audio().local_playback,
            arcen_session::pier_config::LocalPlayback::Muted
        );
        // A relative pactl is never run.
        let relative = parse_open_request(r#"{"command":"open","pactl":"pactl"}"#).unwrap();
        assert_eq!(
            relative.audio().pactl,
            PathBuf::from(crate::media::local_playback::DEFAULT_PACTL)
        );
    }

    #[test]
    fn open_request_roundtrips_timezone_and_absence() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let request = OpenRequest::new(
            Some(timezone.clone()),
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(parse_open_request(&json).unwrap(), request);

        let request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(parse_open_request(&json).unwrap(), request);
        assert!(!json.contains("timezone"));
    }

    #[test]
    fn open_request_carries_the_stream_cursor_mode_only_when_set() {
        use super::super::wayland::StreamCursorMode;
        let plain = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("stream_cursor"));
        assert_eq!(parse_open_request(&json).unwrap().stream_cursor, None);
        let metadata = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        )
        .with_stream_cursor(Some(StreamCursorMode::Metadata));
        let json = serde_json::to_string(&metadata).unwrap();
        assert!(json.contains("\"stream_cursor\":\"metadata\""), "{json}");
        assert_eq!(parse_open_request(&json).unwrap(), metadata);
        let hidden = json.replace("metadata", "hidden");
        assert_eq!(
            parse_open_request(&hidden).unwrap().stream_cursor,
            Some(StreamCursorMode::Hidden)
        );
        assert!(parse_open_request(&json.replace("metadata", "none")).is_err());
    }

    fn sample_topology_plan() -> crate::display::topology::LinuxTopologyPlan {
        use crate::display::topology::{plan_topology, HeadInventory, VALID_HEAD_TOKENS};
        use arcen_media::{
            Monitor, MonitorIdentity, RequestedMonitor, RequestedMonitorTopology,
            TopologyGeneration,
        };

        let monitors = vec![
            RequestedMonitor::new(
                Monitor {
                    identity: MonitorIdentity {
                        id: "primary".to_owned(),
                        name: "Display primary".to_owned(),
                        ..MonitorIdentity::default()
                    },
                    x: 0,
                    y: 0,
                    width_px: 1920,
                    height_px: 1080,
                    scale: 1.0,
                    refresh_hz: 60,
                    rotation: arcen_media::Rotation::Degrees0,
                    primary: true,
                    width_mm: 0.0,
                    height_mm: 0.0,
                    color: None,
                },
                1920,
                1080,
            )
            .expect("requested monitor"),
            RequestedMonitor::new(
                Monitor {
                    identity: MonitorIdentity {
                        id: "second".to_owned(),
                        name: "Display second".to_owned(),
                        ..MonitorIdentity::default()
                    },
                    x: 1920,
                    y: 0,
                    width_px: 1280,
                    height_px: 720,
                    scale: 1.0,
                    refresh_hz: 60,
                    rotation: arcen_media::Rotation::Degrees90,
                    primary: false,
                    width_mm: 0.0,
                    height_mm: 0.0,
                    color: None,
                },
                1280,
                720,
            )
            .expect("requested monitor"),
        ];
        let requested = RequestedMonitorTopology::new(monitors).expect("requested topology");
        let generation = TopologyGeneration::new(1).expect("generation");
        let inventory =
            HeadInventory::uniform(VALID_HEAD_TOKENS.iter().take(2).copied()).expect("inventory");
        plan_topology(&requested, generation, &inventory).expect("plan")
    }

    #[test]
    fn multi_head_plan_msg_round_trips_a_two_monitor_plan_through_json() {
        let plan = sample_topology_plan();
        let wire = MultiHeadPlanMsg::from_plan(&plan);
        let json = serde_json::to_string(&wire).expect("serialize");
        let decoded: MultiHeadPlanMsg = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, wire);
        let rebuilt = decoded.into_plan().expect("rebuild plan");
        assert_eq!(rebuilt, plan);
    }

    #[test]
    fn open_request_carries_a_multi_monitor_plan_through_json() {
        let plan = sample_topology_plan();
        let wire = MultiHeadPlanMsg::from_plan(&plan);
        let request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            Some(wire),
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("multi_monitor"));
        assert_eq!(parse_open_request(&json).unwrap(), request);
    }

    fn xdr_monitor() -> arcen_protocol::messages::ClientMonitor {
        arcen_protocol::messages::ClientMonitor {
            width_px: 3600,
            height_px: 2338,
            scale: 2.0,
            refresh_hz: 120,
            is_primary: true,
            name: "Built-in Display".to_string(),
            width_mm: 344.0,
            height_mm: 223.0,
            model: 40_968,
            serial: 7,
            color: Some(arcen_protocol::messages::DisplayColorMsg {
                gamut: arcen_protocol::messages::DisplayGamutMsg::DisplayP3,
                hdr_headroom: 16.0,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn open_request_carries_the_served_deck_display_and_its_edid_is_that_display() {
        let served = SessionDisplayMsg::served(&xdr_monitor(), 1800, 1168);
        let request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            Some(served.clone()),
            &AgentAudioArguments::default(),
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(parse_open_request(&json).unwrap(), request);

        let edid = served.edid().expect("generated");
        arcen_outputs::edid::validate(&edid, 1800, 1168).expect("its preferred mode is the stream");
        assert_eq!(u16::from_le_bytes([edid[10], edid[11]]), 40_968_u16);
        let red_x = (u16::from(edid[27]) << 2) | u16::from(edid[25] >> 6);
        assert_eq!(red_x, 696, "Display P3 red, not sRGB");
        assert_eq!(
            (edid[21], edid[22]),
            (34, 22),
            "the Deck panel's centimetres"
        );
    }

    #[test]
    fn custom_edid_joins_the_forced_head_and_nothing_else() {
        let config = "Section \"Device\"\n    Option         \"ConnectedMonitor\" \"DFP-2\"\n    Option         \"MetaModes\" \"DFP-2: nvidia-auto-select +0+0\"\nEndSection\n";
        let path = Path::new("/run/arcen/sessions/7/edid.bin");
        let rendered = with_custom_edid(config, "DFP-2", path).expect("rendered");
        assert!(rendered.contains(
            "    Option         \"ConnectedMonitor\" \"DFP-2\"\n    Option         \"CustomEDID\" \"DFP-2:/run/arcen/sessions/7/edid.bin\"\n"
        ));
        assert!(rendered.ends_with("EndSection\n"));
        assert!(
            with_custom_edid(&rendered, "DFP-2", path).is_err(),
            "never twice"
        );
        assert!(with_custom_edid("EndSection\n", "DFP-2", path).is_err());
        assert!(with_custom_edid(config, "DFP-2", Path::new("/tmp/a\"b")).is_err());
    }

    #[test]
    fn open_request_omits_multi_monitor_when_absent() {
        let request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("multi_monitor"));
    }

    #[test]
    fn a_wayland_fidelity_route_round_trips_and_is_omitted_when_absent() {
        let request = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            Some("force-hdr".to_owned()),
        );
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"wayland_fidelity_route\":\"force-hdr\""));
        assert_eq!(parse_open_request(&json).unwrap(), request);
        let plain = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        );
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(!plain_json.contains("wayland_fidelity_route"));
        // The system compositor (no configured prefix) adds nothing.
        assert!(!plain_json.contains("wayland_compositor_prefix"));
        let mut prefixed = OpenRequest::new(
            None,
            crate::deskside::LinuxDesksideConfig::default(),
            None,
            None,
            &AgentAudioArguments::default(),
            None,
        );
        prefixed.wayland_compositor_prefix = Some("/opt/arcen-gnome/51".to_owned());
        let prefixed_json = serde_json::to_string(&prefixed).unwrap();
        assert!(prefixed_json.contains("\"wayland_compositor_prefix\":\"/opt/arcen-gnome/51\""));
        assert_eq!(parse_open_request(&prefixed_json).unwrap(), prefixed);
    }

    #[test]
    fn the_x11_desktop_depth_round_trips_and_only_24_or_30_is_accepted() {
        let plain = || {
            OpenRequest::new(
                None,
                crate::deskside::LinuxDesksideConfig::default(),
                None,
                None,
                &AgentAudioArguments::default(),
                None,
            )
        };
        let json = serde_json::to_string(&plain()).unwrap();
        assert!(!json.contains("x11_desktop_depth"));
        assert_eq!(
            parse_open_request(&json)
                .unwrap()
                .x11_desktop_depth()
                .unwrap(),
            None
        );
        let mut json = String::new();
        for depth in arcen_media::video::DesktopDepth::ALL {
            let request = plain().with_x11_desktop_depth(Some(*depth));
            json = serde_json::to_string(&request).unwrap();
            assert!(json.contains(&format!("\"x11_desktop_depth\":{}", depth.x11_depth())));
            let parsed = parse_open_request(&json).unwrap();
            assert_eq!(parsed, request);
            assert_eq!(parsed.x11_desktop_depth().unwrap(), Some(*depth));
        }
        let forged = json.replace("\"x11_desktop_depth\":30", "\"x11_desktop_depth\":16");
        let parsed = parse_open_request(&forged).unwrap();
        assert!(matches!(
            parsed.x11_desktop_depth(),
            Err(LauncherError::Protocol)
        ));
    }

    #[test]
    fn the_root_depth_is_read_from_xdpyinfo() {
        let xdpyinfo = "name of display:    :77\n  dimensions:    2560x1440 pixels (707x402 millimeters)\n  depth of root window:    30 planes\n  number of colormaps:    minimum 1, maximum 1\n";
        assert_eq!(parse_root_depth(xdpyinfo), Some(30));
        assert_eq!(
            parse_root_depth("  depth of root window:    24 planes\n"),
            Some(24)
        );
        assert_eq!(parse_root_depth("name of display:    :77\n"), None);
        assert_eq!(
            parse_root_depth("  depth of root window:    planes\n"),
            None
        );
    }

    #[test]
    fn a_ready_response_carries_the_root_depth_only_when_known() {
        let older: LauncherResponse = serde_json::from_str(r#"{"status":"ready"}"#).unwrap();
        assert_eq!(older.x11_root_depth, None);
        let newer: LauncherResponse =
            serde_json::from_str(r#"{"status":"ready","x11_root_depth":24}"#).unwrap();
        assert_eq!(newer.x11_root_depth, Some(24));
        assert!(!serde_json::to_string(&LauncherResponse::error("x"))
            .unwrap()
            .contains("x11_root_depth"));
    }

    #[test]
    fn multi_head_plan_msg_into_plan_rejects_a_zero_generation() {
        let wire = MultiHeadPlanMsg {
            backend: crate::display::topology::LinuxTopologyBackend::DedicatedXorg,
            generation: 0,
            virtual_width: 1920,
            virtual_height: 1080,
            monitors: vec![],
        };
        assert!(matches!(wire.into_plan(), Err(LauncherError::XorgConfig)));
    }

    #[test]
    fn multi_head_monitor_plan_msg_into_plan_rejects_a_zero_session_monitor_id() {
        let wire = MultiHeadMonitorPlanMsg {
            session_monitor_id: 0,
            client_display_id: "primary".to_owned(),
            head: "DFP-0".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            logical_x: 0,
            logical_y: 0,
            logical_width: 1920 * 120,
            logical_height: 1080 * 120,
            physical_width: 1920,
            physical_height: 1080,
            scale_120: 120,
            rotation: arcen_media::Rotation::Degrees0,
            primary: true,
            quality_intent: arcen_protocol::messages::MonitorQualityIntentMsg::BandwidthOptimized,
            mode_token: "1920x1080".to_owned(),
            refresh_hz: 60,
            width_mm: 0,
            height_mm: 0,
        };
        assert!(matches!(wire.into_plan(), Err(LauncherError::XorgConfig)));
    }

    #[test]
    fn open_request_rejects_invalid_unknown_and_oversized_input() {
        assert!(parse_open_request(r#"{"command":"open","timezone":"../Europe/Oslo"}"#).is_err());
        assert!(
            parse_open_request(r#"{"command":"open","timezone":"Europe/Oslo","extra":true}"#)
                .is_err()
        );
        assert!(parse_open_request(&"x".repeat(MAX_OPEN_REQUEST_BYTES + 1)).is_err());
    }

    #[tokio::test]
    async fn open_request_reader_rejects_oversized_input_before_deserialization() {
        let input = format!("{}\n", "x".repeat(MAX_OPEN_REQUEST_BYTES + 1));
        let mut reader = BufReader::new(input.as_bytes());
        assert!(read_bounded_line(&mut reader, MAX_OPEN_REQUEST_BYTES)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn launcher_eof_cleanup_restores_deskside_before_xorg() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let agent_calls = Arc::clone(&calls);
        let deskside_calls = Arc::clone(&calls);
        let xorg_calls = Arc::clone(&calls);
        shutdown_desktop_resources(
            move || async move {
                agent_calls.lock().expect("calls").push("agent");
            },
            move || async move {
                deskside_calls.lock().expect("calls").push("deskside");
                Ok(())
            },
            move || async move {
                xorg_calls.lock().expect("calls").push("xorg");
            },
        )
        .await
        .expect("cleanup");
        assert_eq!(*calls.lock().expect("calls"), ["agent", "deskside", "xorg"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dedicated_display_and_head_are_strictly_bounded() {
        assert_eq!(parse_dedicated_display(":10").unwrap(), 10);
        assert!(parse_dedicated_display(":0").is_err());
        assert!(parse_dedicated_display("10").is_err());
        assert!(parse_dedicated_display(":100").is_err());
        assert!(validate_gpu_head("DFP-0").is_ok());
        assert!(validate_gpu_head("DFP-3").is_ok());
        assert!(validate_gpu_head("DFP-4").is_ok());
        assert!(validate_gpu_head("HDMI-0").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn xorg_template_is_retargeted_to_exactly_one_gpu_head() {
        let template = r#"
Section "ServerFlags"
    Option         "AutoAddDevices" "false"
    Option         "AutoEnableDevices" "false"
EndSection
Section "Device"
    Option "ConnectedMonitor" "DFP-0"
    Option "MetaModes" "DFP-0: nvidia-auto-select +0+0"
EndSection
"#;
        let rendered = render_xorg_config(template, "DFP-2").unwrap();
        assert!(!rendered.contains("DFP-0"));
        assert_eq!(rendered.matches("DFP-2").count(), 2);
        assert!(rendered.contains("\"AutoAddDevices\" \"true\""));
        assert!(rendered.contains("\"AutoEnableDevices\" \"true\""));
        assert!(render_xorg_config("DFP-0,DFP-1", "DFP-2").is_err());
    }

    #[test]
    fn dedicated_xorg_admits_only_its_own_session_input() {
        const TEMPLATE: &str = include_str!("../../../../packaging/linux/arcen-xorg.conf");
        let rendered = with_session_input_isolation(TEMPLATE, ":58").unwrap();
        assert!(rendered.starts_with(TEMPLATE));
        let appended = &rendered[TEMPLATE.len()..];
        let foreign = appended.find("\"arcen-ignore-foreign-input\"").unwrap();
        let session = appended.find("\"arcen-session-input\"").unwrap();
        assert!(foreign < session);
        assert!(appended[..session].contains("Option         \"Ignore\" \"on\""));
        assert!(!appended[..session].contains("Match"));
        assert!(appended[session..].contains("MatchUSBID     \"a2ce:*\""));
        assert!(appended[session..].contains("MatchProduct   \"(session :58)\""));
        assert!(appended[session..].contains("Option         \"Ignore\" \"off\""));
        // The isolation classes, then the desktop's own bridge classes.
        assert_eq!(rendered.matches("Section \"InputClass\"").count(), 4);
        assert_eq!(appended.matches("EndSection").count(), 4);
    }

    /// Requirement (e): a bridged tablet reaches only the desktop it is
    /// tagged for. The appended bridge class comes after the isolation
    /// classes (a later class's `Ignore` wins), admits exactly this display's
    /// tag, and nothing else: console devices still meet "Ignore on".
    #[test]
    fn the_desktop_readmits_only_its_own_bridged_tablet_after_the_isolation_classes() {
        const TEMPLATE: &str = include_str!("../../../../packaging/linux/arcen-xorg.conf");
        let rendered = with_session_input_isolation(TEMPLATE, ":58").unwrap();
        let appended = &rendered[TEMPLATE.len()..];
        let foreign = appended.find("\"arcen-ignore-foreign-input\"").unwrap();
        let session = appended.find("\"arcen-session-input\"").unwrap();
        let ignore_bridges = appended
            .find("\"arcen-bridged-usb-session-ignore\"")
            .unwrap();
        let bridge = appended.find("\"arcen-bridged-usb-session\"").unwrap();
        // The session class admits by name and vendor id, which a bridged
        // device's string descriptors could imitate: every bridge is ignored
        // again after it, and only this display's own is re-admitted, last.
        assert!(foreign < session && session < ignore_bridges && ignore_bridges < bridge);
        assert!(appended[ignore_bridges..bridge].contains("MatchTag       \"arcen_bridge\"\n"));
        assert!(appended[ignore_bridges..bridge].contains("Option         \"Ignore\" \"on\""));
        assert!(appended[bridge..].contains("MatchTag       \"arcen_bridge_58\"\n"));
        assert_eq!(appended.matches("MatchTag").count(), 2);
        assert_eq!(appended[bridge..].matches("Section \"").count(), 0);
        assert_eq!(
            arcen_usb_bridge::bridge_seat::admitted_bridge_display(&rendered),
            Some(58)
        );
        let other = with_session_input_isolation(TEMPLATE, ":5").unwrap();
        assert_eq!(
            arcen_usb_bridge::bridge_seat::admitted_bridge_display(&other),
            Some(5)
        );
        assert!(!other.contains("arcen_bridge_58"));
        // A template that already carries a bridge class or tag is refused.
        for poisoned in [
            "Section \"InputClass\"\n    Identifier \"arcen-bridged-usb-session\"\nEndSection\n",
            "Section \"InputClass\"\n    MatchTag \"arcen_bridge\"\nEndSection\n",
        ] {
            assert!(with_session_input_isolation(poisoned, ":58").is_err());
        }
    }

    #[test]
    fn every_arcen_x_server_refuses_the_zap_chord() {
        const TEMPLATE: &str = include_str!("../../../../packaging/linux/arcen-xorg.conf");
        let rendered = with_dont_zap(&with_session_input_isolation(TEMPLATE, ":58").unwrap());
        assert_eq!(rendered.matches("\"DontZap\"").count(), 1);
        assert_eq!(rendered.matches("Section \"ServerFlags\"").count(), 1);
        let flags = rendered.find("Section \"ServerFlags\"").unwrap();
        let zap = rendered
            .find("Option         \"DontZap\" \"true\"")
            .unwrap();
        let end = flags + rendered[flags..].find("EndSection").unwrap();
        assert!(flags < zap && zap < end);
        // The template's own flags stay.
        assert!(rendered[flags..end].contains("\"DontVTSwitch\" \"true\""));
        // A template that allows zapping is overruled; one without flags
        // gets a section of its own.
        let allowing = "Section \"ServerFlags\"\n    Option \"DontZap\" \"false\"\nEndSection\n";
        let fixed = with_dont_zap(allowing);
        assert!(!fixed.contains("\"false\""));
        assert!(fixed.contains("\"DontZap\" \"true\""));
        let bare = with_dont_zap("Section \"Device\"\nEndSection\n");
        assert!(bare.ends_with(
            "Section \"ServerFlags\"\n    Option         \"DontZap\" \"true\"\nEndSection\n"
        ));
        assert_eq!(with_dont_zap(&rendered), rendered);
    }

    #[test]
    fn session_input_isolation_is_never_merged_or_unbound() {
        let once =
            with_session_input_isolation("Section \"ServerFlags\"\nEndSection", ":7").unwrap();
        assert!(once.contains("\"(session :7)\""));
        assert!(with_session_input_isolation(&once, ":7").is_err());
        assert!(with_session_input_isolation(&once, ":8").is_err());
        for display in [":0", ":100", "7", ":7.0", ""] {
            assert!(
                with_session_input_isolation("", display).is_err(),
                "{display}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rendered_dedicated_config_keeps_hotplug_and_takes_the_isolation() {
        const TEMPLATE: &str = include_str!("../../../../packaging/linux/arcen-xorg.conf");
        let single = render_xorg_config(TEMPLATE, "DFP-2").unwrap();
        let single = with_session_input_isolation(&single, ":61").unwrap();
        assert!(single.contains("\"AutoAddDevices\" \"true\""));
        assert!(single.contains("\"(session :61)\""));
        assert!(!single.contains("\"(session :6)\""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_eight_bit_desktop_changes_only_its_depth_and_keeps_virtual_headroom_and_input_isolation()
    {
        const TEMPLATE: &str = include_str!("../../../../packaging/linux/arcen-xorg.conf");
        let isolated =
            with_session_input_isolation(&render_xorg_config(TEMPLATE, "DFP-0").unwrap(), ":62")
                .unwrap();
        let rendered = arcen_outputs::with_session_screen(
            &isolated,
            arcen_outputs::SessionScreen {
                depth: Some(arcen_media::video::DesktopDepth::Eight),
                served_size: Some((2560, 1440)),
            },
        );
        assert!(!rendered.kept_template_depth);
        let changed: Vec<(&str, &str)> = isolated
            .lines()
            .zip(rendered.config.lines())
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(
            changed,
            vec![
                ("    DefaultDepth   30", "    DefaultDepth   24"),
                ("        Depth      30", "        Depth      24"),
            ],
            "a served display within the template's Virtual keeps its headroom for later resizes"
        );
        assert_eq!(isolated.lines().count(), rendered.config.lines().count());
        assert!(rendered.config.contains("\"arcen-ignore-foreign-input\""));
        assert!(rendered.config.contains("MatchProduct   \"(session :62)\""));
        // The bridge classes stay last, so the preflight still admits it.
        assert_eq!(
            arcen_usb_bridge::bridge_seat::admitted_bridge_display(&rendered.config),
            Some(62)
        );
        // Ten bits leaves the packaged template's screen exactly as shipped.
        let ten = arcen_outputs::with_session_screen(
            &isolated,
            arcen_outputs::SessionScreen {
                depth: Some(arcen_media::video::DesktopDepth::Ten),
                served_size: None,
            },
        );
        assert_eq!(ten.config, isolated);
        // A served display larger than the template raises the screen.
        let large = arcen_outputs::with_session_screen(
            &isolated,
            arcen_outputs::SessionScreen {
                depth: None,
                served_size: Some((6016, 3384)),
            },
        );
        assert!(large.config.contains("        Virtual    6016 3384\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_ids_cannot_escape_the_runtime_root() {
        assert!(safe_session_id("c42"));
        assert!(safe_session_id("artist-session_1"));
        assert!(!safe_session_id("../session"));
        assert!(!safe_session_id(""));
    }

    /// Scratch directory for the stale-display tests.
    ///
    /// It deliberately avoids the real `/tmp` rendezvous paths so a test can
    /// never disturb a live X server on the build machine.
    #[cfg(target_os = "linux")]
    fn stale_display_scratch(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("stale-display-tests")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    #[cfg(target_os = "linux")]
    fn current_uid() -> u32 {
        nix::unistd::geteuid().as_raw()
    }

    /// A PID that is certainly not running: the highest value the kernel will
    /// ever allocate is `/proc/sys/kernel/pid_max`, so one past it is free.
    #[cfg(target_os = "linux")]
    fn certainly_dead_pid() -> u32 {
        let pid_max = std::fs::read_to_string("/proc/sys/kernel/pid_max")
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .unwrap_or(4_194_304);
        assert!(!pid_is_live(pid_max + 1));
        pid_max + 1
    }

    /// Leaves a socket file nobody listens on, as a dead Xorg does.
    ///
    /// The socket is bound and never listened on. Binding a listener and
    /// dropping it raced every process another test forked meanwhile: the
    /// child holds a copy of the listening descriptor until it execs
    /// (close-on-exec acts only at exec), so `connect` could still succeed
    /// and the display read as serving clients.
    #[cfg(target_os = "linux")]
    fn abandoned_display_socket(path: &Path) {
        use nix::sys::socket::{bind, socket, AddressFamily, SockFlag, SockType, UnixAddr};
        let fd = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .expect("socket");
        bind(
            std::os::fd::AsRawFd::as_raw_fd(&fd),
            &UnixAddr::new(path).expect("socket path"),
        )
        .expect("bind");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_display_left_by_a_dead_xorg_is_reclaimed() {
        let root = stale_display_scratch("dead-owner");
        let lock = root.join(".X11-lock");
        let socket = root.join("X11");
        std::fs::write(&lock, format!("{:>10}\n", certainly_dead_pid())).expect("lock");
        abandoned_display_socket(&socket);

        assert_eq!(
            inspect_display_lock(&lock, current_uid()),
            Some(DisplayLockState::AbandonedByDeadOwner)
        );
        assert_eq!(
            inspect_display_socket(&socket, current_uid()),
            Some(DisplaySocketState::Abandoned)
        );
        assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_ok());
        assert!(!lock.exists());
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_display_whose_lock_names_a_live_process_is_never_reclaimed() {
        let root = stale_display_scratch("live-owner");
        let lock = root.join(".X11-lock");
        let socket = root.join("X11");
        std::fs::write(&lock, format!("{:>10}\n", std::process::id())).expect("lock");
        abandoned_display_socket(&socket);

        assert_eq!(
            inspect_display_lock(&lock, current_uid()),
            Some(DisplayLockState::HeldByLiveOwner)
        );
        assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_err());
        assert!(lock.exists(), "a live owner's lock must survive");
        assert!(socket.exists(), "a live owner's socket must survive");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_socket_a_server_still_answers_on_is_never_reclaimed() {
        let root = stale_display_scratch("serving-socket");
        let lock = root.join(".X11-lock");
        let socket = root.join("X11");
        std::fs::write(&lock, format!("{:>10}\n", certainly_dead_pid())).expect("lock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("socket");

        assert_eq!(
            inspect_display_socket(&socket, current_uid()),
            Some(DisplaySocketState::ServingClients)
        );
        assert!(
            reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_err(),
            "a dead lock beside a listening socket is still ambiguous"
        );
        assert!(socket.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ambiguous_display_residue_keeps_the_fail_closed_refusal() {
        let root = stale_display_scratch("ambiguous");
        let lock = root.join(".X11-lock");
        let socket = root.join("X11");

        for contents in ["", "not-a-pid", "0", "12 34"] {
            std::fs::write(&lock, contents).expect("lock");
            assert_eq!(
                inspect_display_lock(&lock, current_uid()),
                Some(DisplayLockState::Unrecognized),
                "{contents:?} must not be trusted as a PID"
            );
            assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_err());
            assert!(lock.exists());
        }

        std::fs::remove_file(&lock).expect("clear lock");
        std::fs::create_dir(&lock).expect("directory in the lock's place");
        assert_eq!(
            inspect_display_lock(&lock, current_uid()),
            Some(DisplayLockState::Unrecognized)
        );
        assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_err());
        std::fs::remove_dir(&lock).expect("clear lock");

        std::fs::write(&lock, format!("{:>10}\n", certainly_dead_pid())).expect("lock");
        assert_eq!(
            inspect_display_lock(&lock, current_uid().wrapping_add(1)),
            Some(DisplayLockState::Unrecognized),
            "a lock this host did not write must not be trusted"
        );
        assert!(
            reclaim_stale_display_at(&lock, &socket, current_uid().wrapping_add(1), 11).is_err()
        );
        assert!(lock.exists());

        std::fs::write(&socket, "regular file wearing the socket's name").expect("socket path");
        assert_eq!(
            inspect_display_socket(&socket, current_uid()),
            Some(DisplaySocketState::Unrecognized)
        );
        assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_err());
        assert!(socket.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn half_present_display_residue_is_handled_on_its_own_merits() {
        let root = stale_display_scratch("half-present");
        let lock = root.join(".X11-lock");
        let socket = root.join("X11");

        assert_eq!(inspect_display_lock(&lock, current_uid()), None);
        assert_eq!(inspect_display_socket(&socket, current_uid()), None);
        assert!(
            reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_ok(),
            "a clean display must not be refused"
        );

        std::fs::write(&lock, format!("{:>10}\n", certainly_dead_pid())).expect("lock");
        assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_ok());
        assert!(!lock.exists());

        abandoned_display_socket(&socket);
        assert!(reclaim_stale_display_at(&lock, &socket, current_uid(), 11).is_ok());
        assert!(!socket.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_provably_abandoned_residue_is_reclaimable() {
        use DisplayLockState::{AbandonedByDeadOwner, HeldByLiveOwner, Unrecognized as LockJunk};
        use DisplaySocketState::{Abandoned, ServingClients, Unrecognized as SocketJunk};

        assert!(stale_display_residue_is_reclaimable(None, None));
        assert!(stale_display_residue_is_reclaimable(
            Some(AbandonedByDeadOwner),
            None
        ));
        assert!(stale_display_residue_is_reclaimable(None, Some(Abandoned)));
        assert!(stale_display_residue_is_reclaimable(
            Some(AbandonedByDeadOwner),
            Some(Abandoned)
        ));

        for lock in [
            None,
            Some(AbandonedByDeadOwner),
            Some(HeldByLiveOwner),
            Some(LockJunk),
        ] {
            for socket in [Some(ServingClients), Some(SocketJunk)] {
                assert!(
                    !stale_display_residue_is_reclaimable(lock, socket),
                    "{lock:?}/{socket:?} must refuse"
                );
            }
        }
        for lock in [Some(HeldByLiveOwner), Some(LockJunk)] {
            for socket in [None, Some(Abandoned)] {
                assert!(
                    !stale_display_residue_is_reclaimable(lock, socket),
                    "{lock:?}/{socket:?} must refuse"
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn process_exists(pid: u32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_process_exit(pid: u32) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while pid_is_live(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("process must be reaped or killed");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bounded_command_output_times_out_without_real_loginctl() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 5"]);
        let started = std::time::Instant::now();
        let result = command_output_with_timeout(command, LauncherError::LogindSession).await;
        assert!(matches!(result, Err(LauncherError::LogindSession)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn close_request_gets_grace_before_launcher_signal_fallback() {
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "read command; test \"$command\" = '{\"command\":\"close\"}'",
            ])
            .stdin(Stdio::piped())
            .process_group(0);
        let mut launcher = command.spawn().unwrap();
        let mut stdin = launcher.stdin.take();

        close_and_stop_launcher(&mut stdin, &mut launcher).await;

        assert!(launcher.wait().await.unwrap().success());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn explicit_launcher_teardown_kills_complete_process_group() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 999 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0);
        let mut launcher = command.spawn().unwrap();
        let launcher_pid = launcher.id().unwrap();
        let stdout = launcher.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let descendant_pid = lines
            .next_line()
            .await
            .unwrap()
            .unwrap()
            .parse::<u32>()
            .unwrap();

        terminate_child_tree(&mut launcher).await;
        wait_for_process_exit(launcher_pid).await;
        wait_for_process_exit(descendant_pid).await;
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parent_death_helper_entry() {
        if std::env::var_os("ARCEN_PDEATH_HELPER").is_none() {
            return;
        }
        let current = nix::unistd::User::from_uid(nix::unistd::Uid::effective())
            .unwrap()
            .unwrap();
        let target = if nix::unistd::Uid::effective().is_root() {
            nix::unistd::User::from_name("nobody")
                .unwrap()
                .or_else(|| nix::unistd::User::from_name("nfsnobody").unwrap())
                .expect("root test host needs an unprivileged account")
        } else {
            current
        };
        let identity = UserIdentity {
            username: target.name,
            uid: target.uid.as_raw(),
            gid: target.gid.as_raw(),
            supplementary_groups: vec![target.gid.as_raw()],
            home: target.dir,
            shell: target.shell,
        };
        let environment =
            SessionEnvironment::build(&identity, ":99", None, "test", None, Vec::new());
        let mut command = tokio::process::Command::new("/bin/sleep");
        command.arg("999");
        if nix::unistd::Uid::effective().is_root() {
            super::super::identity::configure_user_command(&mut command, &identity, &environment)
                .unwrap();
        } else {
            use std::os::unix::process::CommandExt;
            // SAFETY: non-root CI cannot exercise setuid; still validate the
            // kernel parent-death path, while the root Rocky test covers the
            // production drop-then-arm ordering.
            unsafe {
                command
                    .as_std_mut()
                    .pre_exec(super::super::identity::arm_parent_death_signal);
            }
        }
        let command = command.as_std_mut();
        let mut agent = command.spawn().unwrap();
        println!("ARCEN_AGENT_PID={}", agent.id());
        std::io::stdout().flush().unwrap();
        let _ = agent.wait();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn forced_launcher_death_cascades_to_agent() {
        use std::process::Command as StdCommand;

        let mut launcher = StdCommand::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::launcher::tests::parent_death_helper_entry",
                "--nocapture",
            ])
            .env("ARCEN_PDEATH_HELPER", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = launcher.stdout.take().unwrap();
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
        let agent_pid = loop {
            let line = lines.next().unwrap().unwrap();
            if let Some(pid) = line.strip_prefix("ARCEN_AGENT_PID=") {
                break pid.parse::<u32>().unwrap();
            }
        };
        assert!(process_exists(agent_pid));
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(launcher.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let _ = launcher.wait();
        wait_for_process_exit(agent_pid).await;
    }
}
#[test]
fn active_logind_binding_requires_active_and_exact_uid() {
    assert!(active_logind_properties_match(
        "User=1001\nActive=yes\n",
        1001
    ));
    assert!(!active_logind_properties_match(
        "User=1002\nActive=yes\n",
        1001
    ));
    assert!(!active_logind_properties_match(
        "User=1001\nActive=no\n",
        1001
    ));
    assert!(!active_logind_properties_match("User=1001\n", 1001));
}
