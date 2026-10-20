//! The private compositor stack of one headless session: runtime directory,
//! D-Bus session bus, `PipeWire`, `WirePlumber`, the session's own
//! PulseAudio-protocol server when its audio is a private graph, the
//! compositor, and the GNOME login services around them
//! ([`super::session_services`]).
//!
//! Everything runs as the session user, inside the session's own logind
//! scope. [`HeadlessStack::stop`] proves its own release: no process of the
//! user is left in the scope except the agent, the runtime directory is
//! gone, and no X11 socket or lock file of the compositor's Xwayland is left
//! in `/tmp`. `Drop` is the synchronous last resort for a cancelled or
//! panicking owner.

use std::ffi::OsStr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use arcen_session::wayland_outputs::VirtualOutputMode;
use arcen_session::wayland_session::{HeadlessCompositor, VirtualOutputRequest};
use arcen_session::wayland_x11::SessionDisplays;
use nix::sys::signal::Signal;
use thiserror::Error;
use tokio::process::{Child, Command};

use super::audio::{self, StackAudio};
use super::session_services::{LogoutReceiver, SessionServices, SessionServicesConfig};
use super::{
    compositor_arguments, compositor_binary, BUS_SOCKET_NAME, PIPEWIRE_SOCKET_NAME,
    WAYLAND_DISPLAY_NAME,
};
use crate::logging::target;
use crate::session::identity::arm_parent_death_signal;

const DBUS_DAEMON: &str = "/usr/bin/dbus-daemon";
const PIPEWIRE: &str = "/usr/bin/pipewire";
const WIREPLUMBER: &str = "/usr/bin/wireplumber";
const FUSERMOUNT: [&str; 2] = ["/usr/bin/fusermount3", "/usr/bin/fusermount"];
const WIREPLUMBER_CONFIG_SOURCES: [&str; 2] = ["/usr/share/wireplumber", "/etc/wireplumber"];
const WIREPLUMBER_CONFIG_DIR: &str = "wireplumber";
/// Where distributions install the accessibility (AT-SPI) bus launcher and
/// registry daemon: `/usr/libexec` (Fedora, RHEL, Debian 12 and later) or
/// the older Debian package directory.
const AT_SPI_BUS_LAUNCHER: [&str; 2] = [
    "/usr/libexec/at-spi-bus-launcher",
    "/usr/lib/at-spi2-core/at-spi-bus-launcher",
];
const AT_SPI_REGISTRYD: [&str; 2] = [
    "/usr/libexec/at-spi2-registryd",
    "/usr/lib/at-spi2-core/at-spi2-registryd",
];
const A11Y_BUS_NAME: &str = "org.a11y.Bus";
const A11Y_BUS_TIMEOUT: Duration = Duration::from_secs(5);

const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const COMPOSITOR_TIMEOUT: Duration = Duration::from_secs(20);
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const SWEEP_TIMEOUT: Duration = Duration::from_secs(3);
/// Mutter locks its X11 displays before it creates its Wayland socket.
const X11_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);
/// Restarts of the session's PulseAudio-protocol server within
/// [`AUDIO_SERVER_RESTART_WINDOW`] before it is left down.
const AUDIO_SERVER_RESTARTS: usize = 3;
const AUDIO_SERVER_RESTART_WINDOW: Duration = Duration::from_secs(60);
const AUDIO_SERVER: &str = "pipewire-pulse";

/// `WirePlumber` 0.4 (Lua) override: no hardware. Ordered after the
/// `50-*-config.lua` defaults and before `90-enable-all.lua` reads them.
const WIREPLUMBER_04_MAIN: &str = "-- Arcen headless session: no hardware.\n\
-- Session audio is a private sink; never open ALSA, cameras or BlueZ.\n\
alsa_monitor.enabled = false\n\
v4l2_monitor.enabled = false\n\
libcamera_monitor.enabled = false\n";
const WIREPLUMBER_04_BLUETOOTH: &str = "-- Arcen headless session: no hardware.\n\
if bluez_monitor then bluez_monitor.enabled = false end\n\
if bluez_midi_monitor then bluez_midi_monitor.enabled = false end\n";
/// `WirePlumber` 0.5 (SPA-JSON) override with the same meaning.
const WIREPLUMBER_05: &str = "# Arcen headless session: no hardware.\n\
# Session audio is a private sink; never open ALSA, cameras or BlueZ.\n\
wireplumber.profiles = {\n  main = {\n    monitor.alsa = disabled\n    \
monitor.alsa-midi = disabled\n    monitor.bluez = disabled\n    \
monitor.bluez-midi = disabled\n    monitor.libcamera = disabled\n    \
monitor.v4l2 = disabled\n  }\n}\n";

/// A stack that could not be started or released.
#[derive(Debug, Error)]
pub enum StackError {
    #[error("the agent is not inside logind session {0}'s scope")]
    NotInLogindSession(String),
    #[error("private runtime directory {0} already exists")]
    RuntimeDirExists(PathBuf),
    #[error("private runtime directory {0} has unsafe ownership or permissions")]
    UnsafeRuntimeDir(PathBuf),
    #[error("{0} exited during startup")]
    Exited(&'static str),
    #[error("{0} did not become ready in time")]
    Timeout(&'static str),
    #[error("headless stack release not proven: {remaining} processes remain, runtime directory present: {runtime_dir_present}, X11 artefacts left: {x11_artifacts}")]
    Release {
        remaining: usize,
        runtime_dir_present: bool,
        x11_artifacts: usize,
    },
    #[error("stack I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// One running headless compositor stack.
pub struct HeadlessStack {
    runtime_dir: PathBuf,
    uid: u32,
    scope: PathBuf,
    compositor: HeadlessCompositor,
    children: Vec<(&'static str, Child)>,
    /// The X11 displays the compositor's Xwayland locked, when it runs one.
    x11_displays: Option<SessionDisplays>,
    audio: StackAudio,
    /// The session's PulseAudio-protocol server: optional, restarted
    /// within a budget, and never the end of the desktop.
    audio_server: Option<Child>,
    audio_server_restarts: std::collections::VecDeque<tokio::time::Instant>,
    /// Members the desktop works without (the accessibility bus): their
    /// exit is logged, never treated as the stack's death.
    optional: Vec<(&'static str, Child)>,
    /// The environment every member starts with.
    members: MemberEnvironment,
    /// The GNOME login services (session manager, keyring, settings
    /// daemons, autostart).
    services: SessionServices,
    released: bool,
}

/// The environment of every process the stack starts: the private runtime
/// directory and bus, plus variables the session's services add (the login
/// shell's, the keyring's, the settings overlay's). Arcen's own variables
/// always win over added ones.
#[derive(Debug, Clone)]
pub struct MemberEnvironment {
    runtime_dir: PathBuf,
    bus_address: String,
    extra: Vec<(String, String)>,
    /// The session's own PulseAudio-protocol server, when its audio is a
    /// private graph: wins over any imported `PULSE_*` variable.
    pulse_runtime: Option<PathBuf>,
}

impl MemberEnvironment {
    /// The environment of a stack in `runtime_dir`.
    #[must_use]
    pub fn new(runtime_dir: &Path) -> Self {
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
            bus_address: format!("unix:path={}", runtime_dir.join(BUS_SOCKET_NAME).display()),
            extra: Vec::new(),
            pulse_runtime: None,
        }
    }

    /// Every member, and everything the compositor and the session's
    /// services start, reaches the PulseAudio-protocol server in
    /// `pulse_runtime` and no other.
    pub fn use_pulse_runtime(&mut self, pulse_runtime: PathBuf) {
        self.pulse_runtime = Some(pulse_runtime);
    }

    /// The private runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// The private bus address.
    #[must_use]
    pub fn bus_address(&self) -> &str {
        &self.bus_address
    }

    /// Adds or replaces an added variable.
    pub fn set(&mut self, name: &str, value: &str) {
        self.extra.retain(|(existing, _)| existing != name);
        self.extra.push((name.to_owned(), value.to_owned()));
    }

    /// A variable as the members see it: an added one, else the agent's.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<String> {
        if let Some(pulse_runtime) = &self.pulse_runtime {
            match name {
                "PULSE_RUNTIME_PATH" => return Some(pulse_runtime.to_string_lossy().into_owned()),
                "PULSE_SERVER" => return None,
                _ => {}
            }
        }
        self.extra
            .iter()
            .rev()
            .find(|(existing, _)| existing == name)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(name).ok())
    }

    /// The members' `PATH`.
    #[must_use]
    pub fn path(&self) -> String {
        self.extra
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_owned())
    }

    /// A command for `binary` with this environment.
    pub fn command(&self, binary: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(binary);
        command
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("GDK_BACKEND")
            .envs(self.extra.iter().map(|(name, value)| (name, value)))
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("DBUS_SESSION_BUS_ADDRESS", &self.bus_address)
            .env("XDG_SESSION_TYPE", "wayland")
            // gvfs must not mount FUSE inside the private runtime directory:
            // a mount there would outlive a killed daemon and pin the
            // directory.
            .env("GVFS_DISABLE_FUSE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(pulse_runtime) = &self.pulse_runtime {
            command
                .env("PULSE_RUNTIME_PATH", pulse_runtime)
                .env_remove("PULSE_SERVER");
        }
        // SAFETY: runs in the child between fork and exec and only arms the
        // kernel parent-death signal, so a member that keeps the agent's
        // SELinux domain dies with the agent. A member whose `execve`
        // changes domain (the session bus enters `unconfined_dbusd_t`) loses
        // it; the agent's stop handling, armed before any member exists, and
        // the launcher's process-group kill cover those.
        unsafe {
            command.as_std_mut().pre_exec(arm_parent_death_signal);
        }
        command
    }
}

/// Mutter's environment switch that forces every colour device into an HDR
/// (BT.2100) colour state.
pub const FORCE_HDR_ENV: &str = "MUTTER_DEBUG_FORCE_HDR";

/// The colour state a session's compositor starts in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompositorColour {
    /// The compositor's own default (SDR on virtual monitors).
    #[default]
    Default,
    /// Every output forced into BT.2100 for a Grading or HDR session on a
    /// host whose qualification proved the forced-HDR route.
    ForcedHdr,
}

impl HeadlessStack {
    /// Creates the private runtime directory and starts the bus,
    /// `PipeWire`, `WirePlumber` and the compositor with exactly `outputs`,
    /// and with its Xwayland when `x11`.
    ///
    /// # Errors
    ///
    /// [`StackError`]; everything already started is released by `Drop`.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        runtime_dir: &Path,
        session_id: &str,
        compositor: HeadlessCompositor,
        outputs: &[VirtualOutputRequest],
        mode: VirtualOutputMode,
        audio: &StackAudio,
        colour: CompositorColour,
        x11: bool,
        services: Option<SessionServicesConfig>,
    ) -> Result<Self, StackError> {
        let uid = nix::unistd::Uid::effective().as_raw();
        let scope = own_session_scope(session_id)
            .ok_or_else(|| StackError::NotInLogindSession(session_id.to_owned()))?;
        create_private_runtime_dir(runtime_dir, uid)?;
        let mut stack = Self {
            runtime_dir: runtime_dir.to_path_buf(),
            uid,
            scope,
            compositor,
            children: Vec::new(),
            x11_displays: None,
            audio: audio.clone(),
            audio_server: None,
            audio_server_restarts: std::collections::VecDeque::new(),
            optional: Vec::new(),
            members: MemberEnvironment::new(runtime_dir),
            services: SessionServices::none(),
            released: false,
        };
        if audio.runs_pulse_server() {
            stack
                .members
                .use_pulse_runtime(runtime_dir.join(audio::PULSE_DIR));
        }
        write_wireplumber_config(&runtime_dir.join(WIREPLUMBER_CONFIG_DIR))?;
        audio::write_pipewire_config(&runtime_dir.join(audio::CONFIG_DIR), audio, link_tree)?;
        // Before the bus, so the bus and everything it activates share the
        // session environment (login shell, settings overlay).
        let mut password = None;
        if let Some(mut config) = services {
            password = config.keyring_password.take();
            stack.services = SessionServices::before_bus(&config, &mut stack.members).await;
        }

        // A side-by-side compositor prefix: its schemas, D-Bus services and
        // data come first for the bus and everything the session starts.
        if let Some(dirs) =
            super::compositor_prefix().xdg_data_dirs(stack.members.var("XDG_DATA_DIRS").as_deref())
        {
            stack.members.set("XDG_DATA_DIRS", &dirs);
        }

        let bus = runtime_dir.join(BUS_SOCKET_NAME);
        let mut dbus = stack.command(DBUS_DAEMON);
        dbus.arg("--session")
            .arg(format!("--address=unix:path={}", bus.display()))
            .args(["--nofork", "--nopidfile"]);
        stack.spawn("dbus-daemon", dbus)?;
        stack
            .wait_for_socket(&bus, "dbus-daemon", SOCKET_TIMEOUT)
            .await?;

        // The session manager before the compositor, so the shell's first
        // queries find it; the keyring before any application.
        let mut members = stack.members.clone();
        if let Some(keyring) = stack
            .services
            .after_bus(password.take(), compositor, &mut members)
            .await
        {
            stack.optional.push(("gnome-keyring-daemon", keyring));
        }
        stack.members = members;

        let pipewire = stack.pipewire_command(PIPEWIRE);
        stack.spawn("pipewire", pipewire)?;
        stack
            .wait_for_socket(
                &runtime_dir.join(PIPEWIRE_SOCKET_NAME),
                "pipewire",
                SOCKET_TIMEOUT,
            )
            .await?;

        let mut wireplumber = stack.command(WIREPLUMBER);
        wireplumber
            .env(
                "WIREPLUMBER_CONFIG_DIR",
                runtime_dir.join(WIREPLUMBER_CONFIG_DIR),
            )
            // The private session manager's state (default nodes, stream
            // targets) is its own, never the user's main session manager's.
            .env("XDG_STATE_HOME", runtime_dir.join(audio::STATE_DIR));
        stack.spawn("wireplumber", wireplumber)?;
        if stack.audio.runs_pulse_server() {
            stack.start_audio_server().await;
        }

        // Before the compositor, so the shell and the first applications
        // find the accessibility bus already owned and never ask the
        // private bus to activate it (see `start_accessibility_bus`).
        stack.start_accessibility_bus().await;

        let mut shell = stack.command(&compositor_binary(compositor));
        let arguments = compositor_arguments(outputs, mode, x11);
        if super::native_seat() {
            shell.args(super::native_seat_arguments(arguments));
        } else {
            shell.args(arguments);
        }
        if let Some(seat) = super::lab_input_seat().filter(|_| !super::native_seat()) {
            // Lab: a compositor with the headless-input patch runs libinput
            // on this seat only (the Pier's seat pen); others ignore it.
            shell.env(arcen_input::seat_pen::COMPOSITOR_SEAT_ENV, seat);
        }
        apply_colour(&mut shell, colour);
        stack.spawn("compositor", shell)?;
        stack
            .wait_for_socket(
                &runtime_dir.join(WAYLAND_DISPLAY_NAME),
                "compositor",
                COMPOSITOR_TIMEOUT,
            )
            .await?;
        stack.ensure_running("wireplumber")?;
        if x11 {
            if let Some(pid) = stack.compositor_pid() {
                stack.x11_displays = super::xwayland::discover(
                    &super::xwayland::X11Dirs::production(),
                    uid,
                    pid,
                    X11_DISCOVERY_TIMEOUT,
                )
                .await;
            }
            match stack.x11_displays {
                Some(displays) => tracing::info!(
                    target: target::SESSION,
                    public = displays.public,
                    private = displays.private,
                    "the compositor locked its Xwayland displays"
                ),
                None => tracing::warn!(
                    target: target::SESSION,
                    "the compositor runs with Xwayland but locked no pair of X11 displays"
                ),
            }
        }
        let mut session_members = stack.members.clone();
        session_members.set("WAYLAND_DISPLAY", WAYLAND_DISPLAY_NAME);
        // The session's services (gsd-xsettings among them) reach the
        // compositor's Xwayland as Mutter's own launches do.
        if let Some(displays) = stack.x11_displays {
            session_members.set(
                "DISPLAY",
                &arcen_session::wayland_x11::display_name(displays.public),
            );
            session_members.set(
                "GNOME_SETUP_DISPLAY",
                &arcen_session::wayland_x11::display_name(displays.private),
            );
            if let Some(auth) = super::xwayland::mutter_xauthority(runtime_dir, uid) {
                session_members.set("XAUTHORITY", &auth.to_string_lossy());
            }
        }
        stack.services.after_compositor(&session_members);
        tracing::info!(
            target: target::SESSION,
            compositor = compositor.token(),
            compositor_prefix = %super::compositor_prefix(),
            runtime_dir = %runtime_dir.display(),
            pid = stack.compositor_pid().unwrap_or_default(),
            x11 = x11,
            "headless compositor stack started"
        );
        Ok(stack)
    }

    /// The private bus address.
    #[must_use]
    pub fn bus_address(&self) -> String {
        format!(
            "unix:path={}",
            self.runtime_dir.join(BUS_SOCKET_NAME).display()
        )
    }

    /// The private runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// The compositor that runs.
    #[must_use]
    pub const fn compositor(&self) -> HeadlessCompositor {
        self.compositor
    }

    /// The compositor's process id.
    #[must_use]
    pub fn compositor_pid(&self) -> Option<u32> {
        self.child("compositor").and_then(Child::id)
    }

    /// The X11 displays the compositor's Xwayland locked, when it runs one.
    #[must_use]
    pub const fn x11_displays(&self) -> Option<SessionDisplays> {
        self.x11_displays
    }

    /// The session's logind scope.
    #[must_use]
    pub fn scope(&self) -> &Path {
        &self.scope
    }

    /// The session user.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// The render node the compositor holds open, when it holds exactly
    /// one, with that device's PCI address and vendor id from sysfs.
    /// Evidence from the process's own descriptors; never inferred from
    /// what is installed.
    #[must_use]
    pub fn compositor_gpu(&self) -> CompositorGpu {
        let Some(pid) = self.compositor_pid() else {
            return CompositorGpu::default();
        };
        let targets = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .collect::<Vec<_>>();
        let Some(render_node) = super::single_render_node(targets.iter().map(PathBuf::as_path))
        else {
            return CompositorGpu::default();
        };
        let (pci, vendor) = super::render_node_gpu(Path::new("/sys"), &render_node);
        CompositorGpu {
            render_node: Some(render_node),
            pci,
            vendor,
        }
    }

    fn child(&self, name: &str) -> Option<&Child> {
        self.children
            .iter()
            .find(|(child_name, _)| *child_name == name)
            .map(|(_, child)| child)
    }

    /// Whether any stack process has exited, and which.
    pub fn exited_member(&mut self) -> Option<&'static str> {
        self.children
            .iter_mut()
            .find_map(|(name, child)| (!matches!(child.try_wait(), Ok(None))).then_some(*name))
    }

    /// Resolves when a stack process exits. The session's
    /// PulseAudio-protocol server is not a member whose exit ends the
    /// desktop: it is restarted within its budget instead.
    pub async fn wait_for_member_exit(&mut self) -> &'static str {
        loop {
            if let Some(name) = self.exited_member() {
                return name;
            }
            self.keep_audio_server().await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// A `pipewire` process for this stack: the private configuration home
    /// when the session's audio needs Arcen's drop-in.
    fn pipewire_command(&self, binary: &str) -> Command {
        let mut command = self.command(binary);
        if self.audio.has_drop_in() {
            command.env("XDG_CONFIG_HOME", self.runtime_dir.join(audio::CONFIG_DIR));
        }
        command
    }

    async fn start_audio_server(&mut self) {
        let mut command = self.pipewire_command(PIPEWIRE);
        command
            .args(["-c", audio::PULSE_SERVER_CONFIG])
            .env("XDG_STATE_HOME", self.runtime_dir.join(audio::STATE_DIR));
        let socket = self.runtime_dir.join(audio::PULSE_DIR).join("native");
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(target: target::AUDIO, %error, "the session's audio server could not start");
                return;
            }
        };
        let deadline = tokio::time::Instant::now() + SOCKET_TIMEOUT;
        loop {
            if !matches!(child.try_wait(), Ok(None)) {
                tracing::warn!(target: target::AUDIO, "the session's audio server exited during startup");
                return;
            }
            if std::fs::symlink_metadata(&socket)
                .is_ok_and(|metadata| metadata.uid() == self.uid && is_socket(&metadata))
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(target: target::AUDIO, "the session's audio server did not open its socket in time");
                break;
            }
            tokio::time::sleep(POLL).await;
        }
        tracing::info!(
            target: target::AUDIO,
            pid = child.id().unwrap_or_default(),
            socket = %socket.display(),
            "the session's PulseAudio-protocol server is running on its private graph"
        );
        self.audio_server = Some(child);
    }

    /// Restarts the session's audio server if it exited, within budget.
    async fn keep_audio_server(&mut self) {
        let Some(child) = self.audio_server.as_mut() else {
            return;
        };
        if matches!(child.try_wait(), Ok(None)) {
            return;
        }
        self.audio_server = None;
        let now = tokio::time::Instant::now();
        while self
            .audio_server_restarts
            .front()
            .is_some_and(|at| now.duration_since(*at) > AUDIO_SERVER_RESTART_WINDOW)
        {
            self.audio_server_restarts.pop_front();
        }
        if self.audio_server_restarts.len() >= AUDIO_SERVER_RESTARTS {
            tracing::error!(
                target: target::AUDIO,
                member = AUDIO_SERVER,
                "the session's audio server keeps exiting; it stays down and the desktop continues without audio"
            );
            return;
        }
        self.audio_server_restarts.push_back(now);
        tracing::warn!(
            target: target::AUDIO,
            member = AUDIO_SERVER,
            restart = self.audio_server_restarts.len(),
            "the session's audio server exited; restarting it"
        );
        self.start_audio_server().await;
    }

    fn ensure_running(&mut self, name: &'static str) -> Result<(), StackError> {
        let running = self
            .children
            .iter_mut()
            .find(|(child_name, _)| *child_name == name)
            .is_some_and(|(_, child)| matches!(child.try_wait(), Ok(None)));
        if running {
            Ok(())
        } else {
            Err(StackError::Exited(name))
        }
    }

    fn command(&self, binary: &str) -> Command {
        self.members.command(binary)
    }

    /// The desktop announced itself ready: application-phase autostart
    /// programs start.
    pub fn desktop_ready(&self) {
        self.services.desktop_ready();
    }

    /// Turns true once the user confirmed a logout inside the session.
    #[must_use]
    pub fn logout_receiver(&self) -> Option<LogoutReceiver> {
        self.services.logout_receiver()
    }

    fn spawn(&mut self, name: &'static str, mut command: Command) -> Result<(), StackError> {
        let child = command.spawn()?;
        self.children.push((name, child));
        Ok(())
    }

    /// Starts the AT-SPI accessibility bus and its registry as optional
    /// members.
    ///
    /// The private bus runs as `unconfined_dbusd_t` under SELinux, and
    /// activating `org.a11y.Bus` from it computes the context
    /// `unconfined_r:gnome_atspi_t`, which the targeted policy refuses
    /// (`SELINUX_ERR invalid_context`): applications then log "AT-SPI: Error
    /// retrieving accessibility bus address" and accessibility is gone.
    /// Started by the agent instead, both daemons stay in the session's own
    /// domain, as on a systemd-activated login bus, without any policy
    /// change. A missing binary or a member that fails is logged and the
    /// desktop runs without accessibility; neither ends the session.
    async fn start_accessibility_bus(&mut self) {
        let Some(launcher) = first_existing(&AT_SPI_BUS_LAUNCHER, |path| path.is_file()) else {
            tracing::info!(
                target: target::SESSION,
                "AT-SPI bus launcher not installed; the desktop runs without an accessibility bus"
            );
            return;
        };
        let mut command = self.command(launcher);
        command.arg("--launch-immediately");
        if !self.spawn_optional("at-spi-bus-launcher", command) {
            return;
        }
        if let Err(reason) = self.wait_for_a11y_bus().await {
            tracing::warn!(
                target: target::SESSION,
                reason,
                "the accessibility bus did not come up; the desktop runs without it"
            );
            return;
        }
        let Some(registryd) = first_existing(&AT_SPI_REGISTRYD, |path| path.is_file()) else {
            tracing::info!(
                target: target::SESSION,
                "AT-SPI registry daemon not installed; the accessibility bus runs without it"
            );
            return;
        };
        let registry = self.command(registryd);
        if self.spawn_optional("at-spi2-registryd", registry) {
            tracing::info!(
                target: target::SESSION,
                "accessibility bus and registry started in the session's own context"
            );
        }
    }

    fn spawn_optional(&mut self, name: &'static str, mut command: Command) -> bool {
        match command.spawn() {
            Ok(child) => {
                self.optional.push((name, child));
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: target::SESSION,
                    member = name,
                    %error,
                    "optional session member could not start"
                );
                false
            }
        }
    }

    /// Waits, bounded, until the bus launcher owns `org.a11y.Bus`, so the
    /// registry's own address lookup never falls back to bus activation.
    async fn wait_for_a11y_bus(&mut self) -> Result<(), &'static str> {
        let connection = zbus::connection::Builder::address(self.bus_address().as_str())
            .map_err(|_| "private bus address")?
            .build()
            .await
            .map_err(|_| "private bus unreachable")?;
        let bus = zbus::fdo::DBusProxy::new(&connection)
            .await
            .map_err(|_| "private bus proxy")?;
        let name = zbus::names::BusName::try_from(A11Y_BUS_NAME).map_err(|_| "bus name")?;
        let deadline = tokio::time::Instant::now() + A11Y_BUS_TIMEOUT;
        loop {
            if bus.name_has_owner(name.clone()).await.unwrap_or(false) {
                return Ok(());
            }
            let launcher_exited = self.optional.iter_mut().any(|(member, child)| {
                *member == "at-spi-bus-launcher" && !matches!(child.try_wait(), Ok(None))
            });
            if launcher_exited {
                return Err("the bus launcher exited");
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("timed out");
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_for_socket(
        &mut self,
        path: &Path,
        name: &'static str,
        timeout: Duration,
    ) -> Result<(), StackError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.ensure_running(name)?;
            if std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.uid() == self.uid && is_socket(&metadata))
            {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(StackError::Timeout(name));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Stops every stack member and every other process of this user left
    /// in the session scope, unmounts anything mounted inside the runtime
    /// directory, removes it, and proves it. Idempotent.
    ///
    /// # Errors
    ///
    /// [`StackError::Release`] when a process or the directory remains; the
    /// obligation still stands and a later call retries it.
    pub async fn stop(&mut self) -> Result<(), StackError> {
        if self.released {
            return Ok(());
        }
        // Session programs first (told to stop, then stopped), then the
        // compositor, so clients see it go before their bus does.
        self.services.stop().await;
        for name in ["compositor", AUDIO_SERVER, "wireplumber", "pipewire"] {
            if name == AUDIO_SERVER {
                if let Some(child) = self.audio_server.as_mut() {
                    terminate_child(child).await;
                }
                continue;
            }
            if let Some((_, child)) = self
                .children
                .iter_mut()
                .find(|(child_name, _)| *child_name == name)
            {
                terminate_child(child).await;
            }
        }
        // Optional members (the accessibility bus) go before the session
        // bus they are clients of.
        for (name, child) in self.optional.iter_mut().rev() {
            if let Ok(Some(status)) = child.try_wait() {
                tracing::info!(
                    target: target::SESSION,
                    member = *name,
                    %status,
                    "optional session member had already exited"
                );
            }
            terminate_child(child).await;
        }
        self.services.release_bus();
        if let Some((_, child)) = self
            .children
            .iter_mut()
            .find(|(child_name, _)| *child_name == "dbus-daemon")
        {
            terminate_child(child).await;
        }
        self.sweep_scope().await;
        // Mutter removes its X11 sockets and lock files on a clean exit; a
        // compositor that died without its shutdown leaves them.
        let x11_artifacts = self.x11_displays.map_or(0, |displays| {
            super::xwayland::remove_session_artifacts(
                &super::xwayland::X11Dirs::production(),
                self.uid,
                &displays,
            )
            .remaining
            .len()
        });
        unmount_user_fuse_inside(&self.runtime_dir, self.uid, |_| true).await;
        match std::fs::remove_dir_all(&self.runtime_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                target: target::SESSION,
                %error,
                runtime_dir = %self.runtime_dir.display(),
                "private runtime directory could not be removed"
            ),
        }
        let remaining = self.remaining_processes().len();
        let runtime_dir_present = self.runtime_dir.exists();
        if remaining != 0 || runtime_dir_present || x11_artifacts != 0 {
            return Err(StackError::Release {
                remaining,
                runtime_dir_present,
                x11_artifacts,
            });
        }
        self.released = true;
        tracing::info!(
            target: target::SESSION,
            runtime_dir = %self.runtime_dir.display(),
            x11_displays = ?self.x11_displays.map(|displays| displays.numbers()),
            "headless compositor stack released: no session process, runtime directory or X11 socket remains"
        );
        Ok(())
    }

    /// Processes of this user in the session scope other than the agent.
    fn remaining_processes(&self) -> Vec<i32> {
        scope_processes(&self.scope, self.uid)
    }

    async fn sweep_scope(&self) {
        for signal in [ScopeSignal::Terminate, ScopeSignal::Kill] {
            let remaining = self.remaining_processes();
            if remaining.is_empty() {
                return;
            }
            if signal == ScopeSignal::Kill {
                tracing::warn!(
                    target: target::SESSION,
                    count = remaining.len(),
                    "session processes ignored SIGTERM; killing"
                );
            }
            for pid in remaining {
                signal_scope_process(&self.scope, self.uid, pid, signal);
            }
            let deadline = tokio::time::Instant::now() + SWEEP_TIMEOUT;
            while tokio::time::Instant::now() < deadline && !self.remaining_processes().is_empty() {
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

impl Drop for HeadlessStack {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        for (_, child) in self.children.iter_mut().chain(self.optional.iter_mut()) {
            let _ = child.start_kill();
        }
        if let Some(child) = self.audio_server.as_mut() {
            let _ = child.start_kill();
        }
        for pid in self.remaining_processes() {
            signal_scope_process(&self.scope, self.uid, pid, ScopeSignal::Kill);
        }
        if let Some(displays) = self.x11_displays {
            // Killed, the compositor cannot remove its own; a lock still
            // holding its pid is removed, any other is left alone.
            let _ = super::xwayland::remove_session_artifacts(
                &super::xwayland::X11Dirs::production(),
                self.uid,
                &displays,
            );
        }
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
    }
}

/// Sets or removes Mutter's forced-HDR switch on the compositor's command:
/// set only for a forced-HDR session, removed otherwise so an SDR session
/// never inherits it.
fn apply_colour(shell: &mut Command, colour: CompositorColour) {
    if colour == CompositorColour::ForcedHdr {
        // Mutter's own switch (GNOME 48 and later): every monitor of this
        // compositor, virtual ones included, composes in BT.2100 (PQ,
        // BT.2020), and from Mutter 50 its `RecordMonitor` streams offer
        // ten-bit and half-float formats. Set for this compositor only,
        // never exported on its bus (`--debug-control` is not used).
        shell.env(FORCE_HDR_ENV, "1");
    } else {
        // Never inherited: an SDR session composes in SDR whatever the
        // environment around the agent says.
        shell.env_remove(FORCE_HDR_ENV);
    }
}

async fn terminate_child(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), Signal::SIGTERM);
    }
    if tokio::time::timeout(CHILD_STOP_TIMEOUT, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

fn is_socket(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    metadata.file_type().is_socket()
}

/// The first candidate path that `exists` accepts, in preference order.
fn first_existing<'a>(candidates: &[&'a str], exists: impl Fn(&Path) -> bool) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| exists(Path::new(candidate)))
}

fn create_private_runtime_dir(path: &Path, uid: u32) -> Result<(), StackError> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(StackError::RuntimeDirExists(path.to_path_buf()));
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(StackError::UnsafeRuntimeDir(path.to_path_buf()));
    }
    Ok(())
}

/// The agent's own cgroup, when it is exactly `session-<id>.scope`.
pub(crate) fn own_session_scope(session_id: &str) -> Option<PathBuf> {
    let scope = super::unified_cgroup(&std::fs::read_to_string("/proc/self/cgroup").ok()?)?;
    let expected = format!("session-{session_id}.scope");
    (!session_id.is_empty() && scope.file_name()?.to_str()? == expected).then_some(scope)
}

/// What the compositor's own descriptors and sysfs prove about its GPU.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompositorGpu {
    /// The one render node it holds open.
    pub render_node: Option<PathBuf>,
    /// That node's PCI device, `0000:00:10.0`.
    pub pci: Option<String>,
    /// That device's PCI vendor id.
    pub vendor: Option<u16>,
}

/// A stop signal for a process of the session scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeSignal {
    /// SIGTERM.
    Terminate,
    /// SIGKILL.
    Kill,
}

/// Signals `pid` only if it is still a process of `uid` in `scope`, without
/// a pid-reuse race: the process is pinned with a pidfd first, its owner and
/// cgroup are checked after that, and the signal goes through the pidfd. A
/// process that exited, or a recycled pid that is no longer this session's,
/// is never signalled.
pub(crate) fn signal_scope_process(scope: &Path, uid: u32, pid: i32, signal: ScopeSignal) -> bool {
    use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};
    let Some(raw) = Pid::from_raw(pid) else {
        return false;
    };
    let Ok(pidfd) = pidfd_open(raw, PidfdFlags::empty()) else {
        return false;
    };
    let Ok(unsigned) = u32::try_from(pid) else {
        return false;
    };
    let still_ours = process_uid(unsigned) == Some(uid)
        && std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .ok()
            .and_then(|text| super::unified_cgroup(&text))
            .is_some_and(|cgroup| cgroup == scope);
    still_ours
        && pidfd_send_signal(
            &pidfd,
            match signal {
                ScopeSignal::Terminate => Signal::TERM,
                ScopeSignal::Kill => Signal::KILL,
            },
        )
        .is_ok()
}

/// Processes owned by `uid` in `scope`, except this one.
pub(crate) fn scope_processes(scope: &Path, uid: u32) -> Vec<i32> {
    let own = std::process::id();
    std::fs::read_to_string(scope.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .filter(|pid| *pid != own && process_uid(*pid) == Some(uid))
        .filter_map(|pid| i32::try_from(pid).ok())
        .collect()
}

fn process_uid(pid: u32) -> Option<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Mount points at or below `dir`, deepest first.
pub(crate) fn mounts_inside(dir: &Path) -> Vec<PathBuf> {
    let mut mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .map(PathBuf::from)
        .filter(|mount| mount.starts_with(dir))
        .collect::<Vec<_>>();
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.components().count()));
    mounts
}

/// Lazily unmounts the session user's FUSE mounts a session service left in
/// the runtime directory (the document portal mounts `doc/` there).
///
/// Only `fuse`/`fuse.*` mounts whose `user_id` is `uid` are considered, and
/// only `fusermount -u -z` running as the session user unmounts them:
/// `as_user` must make the command run with the session user's credentials
/// (the agent already has them; the root launcher drops to them). The
/// runtime directory is user-writable, so nothing here ever unmounts by path
/// with root privileges.
pub(crate) async fn unmount_user_fuse_inside<F>(dir: &Path, uid: u32, as_user: F)
where
    F: Fn(&mut Command) -> bool,
{
    let Some(fusermount) = FUSERMOUNT.iter().find(|path| Path::new(path).is_file()) else {
        return;
    };
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    for mount in super::user_fuse_mounts_inside(&mountinfo, dir, uid) {
        let mut command = Command::new(fusermount);
        command
            .args(["-u", "-z", "--"])
            .arg(&mount)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if !as_user(&mut command) {
            continue;
        }
        let unmounted = matches!(
            tokio::time::timeout(Duration::from_secs(2), command.status()).await,
            Ok(Ok(status)) if status.success()
        );
        tracing::info!(
            target: target::SESSION,
            mount = %mount.display(),
            unmounted,
            "unmounted a session FUSE mount inside the private runtime directory"
        );
    }
}

/// Builds a private `WirePlumber` configuration: the distribution's and the
/// administrator's configuration, linked file by file, plus Arcen's
/// video-only override. `WIREPLUMBER_CONFIG_DIR` replaces the search path
/// in `WirePlumber` 0.4, so the private directory has to be complete.
fn write_wireplumber_config(destination: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(destination)?;
    for source in WIREPLUMBER_CONFIG_SOURCES {
        link_tree(Path::new(source), destination, 0)?;
    }
    for (directory, file, contents) in [
        ("main.lua.d", "51-arcen-video-only.lua", WIREPLUMBER_04_MAIN),
        (
            "bluetooth.lua.d",
            "51-arcen-video-only.lua",
            WIREPLUMBER_04_BLUETOOTH,
        ),
        (
            "wireplumber.conf.d",
            "99-arcen-video-only.conf",
            WIREPLUMBER_05,
        ),
    ] {
        let directory = destination.join(directory);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&directory)?;
        let path = directory.join(file);
        let _ = std::fs::remove_file(&path);
        std::fs::write(path, contents)?;
    }
    Ok(())
}

fn link_tree(source: &Path, destination: &Path, depth: usize) -> std::io::Result<()> {
    const MAX_DEPTH: usize = 4;
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(source) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(&target)?;
            link_tree(&entry.path(), &target, depth + 1)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            let _ = std::fs::remove_file(&target);
            std::os::unix::fs::symlink(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;

    use super::*;

    #[test]
    fn only_a_forced_hdr_compositor_sees_the_force_hdr_switch() {
        let env_of = |command: &Command| {
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == FORCE_HDR_ENV)
                .map(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()))
        };
        let members = MemberEnvironment::new(Path::new("/run/user/1000/arcen-wayland-7"));
        let mut sdr = members.command("/usr/bin/gnome-shell");
        apply_colour(&mut sdr, CompositorColour::Default);
        assert_eq!(env_of(&sdr), Some(None));
        let mut hdr = members.command("/usr/bin/gnome-shell");
        apply_colour(&mut hdr, CompositorColour::ForcedHdr);
        assert_eq!(env_of(&hdr), Some(Some("1".to_owned())));
    }

    #[test]
    fn arcen_variables_win_over_added_ones_and_x11_is_never_inherited() {
        let mut members = MemberEnvironment::new(Path::new("/run/user/1000/arcen-wayland-7"));
        members.set("PATH", "/home/u/bin:/usr/bin");
        members.set("PATH", "/opt/x/bin:/usr/bin");
        members.set("XDG_RUNTIME_DIR", "/tmp/evil");
        assert_eq!(members.path(), "/opt/x/bin:/usr/bin");
        let command = members.command("/bin/true");
        let environment = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            environment.get("XDG_RUNTIME_DIR"),
            Some(&Some("/run/user/1000/arcen-wayland-7".to_owned()))
        );
        assert_eq!(
            environment.get("DBUS_SESSION_BUS_ADDRESS"),
            Some(&Some(
                "unix:path=/run/user/1000/arcen-wayland-7/bus".to_owned()
            ))
        );
        assert_eq!(
            environment.get("PATH"),
            Some(&Some("/opt/x/bin:/usr/bin".to_owned()))
        );
        assert_eq!(environment.get("DISPLAY"), Some(&None));
        assert_eq!(environment.get("WAYLAND_DISPLAY"), Some(&None));
    }

    #[test]
    fn a_private_audio_graph_wins_over_imported_pulse_variables() {
        let mut members = MemberEnvironment::new(Path::new("/run/user/1000/arcen-wayland-7"));
        members.set("PULSE_SERVER", "tcp:203.0.113.9");
        members.set("PULSE_RUNTIME_PATH", "/run/user/1000/pulse");
        members.use_pulse_runtime(PathBuf::from("/run/user/1000/arcen-wayland-7/pulse"));
        assert_eq!(members.var("PULSE_SERVER"), None);
        assert_eq!(
            members.var("PULSE_RUNTIME_PATH").as_deref(),
            Some("/run/user/1000/arcen-wayland-7/pulse")
        );
        let command = members.command("/bin/true");
        let environment = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(environment.get("PULSE_SERVER"), Some(&None));
        assert_eq!(
            environment.get("PULSE_RUNTIME_PATH"),
            Some(&Some("/run/user/1000/arcen-wayland-7/pulse".to_owned()))
        );
    }

    #[test]
    fn optional_members_are_found_in_preference_order() {
        let present = |only: &'static [&'static str]| {
            move |path: &Path| only.iter().any(|p| Path::new(p) == path)
        };
        assert_eq!(
            first_existing(
                &AT_SPI_BUS_LAUNCHER,
                present(&[
                    "/usr/libexec/at-spi-bus-launcher",
                    "/usr/lib/at-spi2-core/at-spi-bus-launcher"
                ])
            ),
            Some("/usr/libexec/at-spi-bus-launcher")
        );
        assert_eq!(
            first_existing(
                &AT_SPI_REGISTRYD,
                present(&["/usr/lib/at-spi2-core/at-spi2-registryd"])
            ),
            Some("/usr/lib/at-spi2-core/at-spi2-registryd")
        );
        assert_eq!(first_existing(&AT_SPI_BUS_LAUNCHER, present(&[])), None);
    }

    #[tokio::test]
    async fn an_optional_member_that_exits_never_counts_as_the_stack_dying() {
        let scope = super::super::unified_cgroup(
            &std::fs::read_to_string("/proc/self/cgroup").expect("own cgroup"),
        )
        .expect("unified cgroup");
        let required = Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .expect("sleep");
        let mut optional = Command::new("true").spawn().expect("true");
        optional.wait().await.expect("optional exits");
        let mut stack = HeadlessStack {
            runtime_dir: PathBuf::from("/nonexistent/arcen-stack-test"),
            uid: process_uid(std::process::id()).expect("own uid"),
            scope,
            compositor: HeadlessCompositor::Mutter,
            children: vec![("compositor", required)],
            optional: vec![("at-spi-bus-launcher", optional)],
            x11_displays: None,
            audio: StackAudio::None,
            audio_server: None,
            audio_server_restarts: std::collections::VecDeque::new(),
            members: MemberEnvironment::new(Path::new("/nonexistent/arcen-stack-test")),
            services: SessionServices::none(),
            // Nothing to release: the test owns no runtime directory.
            released: true,
        };
        assert_eq!(stack.exited_member(), None);
        let (_, required) = stack.children.first_mut().expect("required member");
        required.kill().await.expect("kill");
        assert_eq!(stack.exited_member(), Some("compositor"));
    }
    #[test]
    fn scope_signals_reach_only_a_live_process_of_the_scope_and_user() {
        let scope = super::super::unified_cgroup(
            &std::fs::read_to_string("/proc/self/cgroup").expect("own cgroup"),
        )
        .expect("unified cgroup");
        let uid = process_uid(std::process::id()).expect("own uid");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = i32::try_from(child.id()).expect("pid");

        assert!(!signal_scope_process(
            &scope,
            uid.wrapping_add(1),
            pid,
            ScopeSignal::Terminate
        ));
        assert!(!signal_scope_process(
            &scope.join("elsewhere.scope"),
            uid,
            pid,
            ScopeSignal::Terminate
        ));
        assert!(child.try_wait().expect("try_wait").is_none());

        assert!(signal_scope_process(
            &scope,
            uid,
            pid,
            ScopeSignal::Terminate
        ));
        let status = child.wait().expect("wait");
        assert_eq!(status.signal(), Some(nix::libc::SIGTERM));

        assert!(!signal_scope_process(&scope, uid, pid, ScopeSignal::Kill));
        assert!(!signal_scope_process(&scope, uid, 0, ScopeSignal::Kill));
    }
}
