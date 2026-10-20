//! Machine-service side session-agent supervision and user-side GNOME launch.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use arcen_telemetry::CorrelationId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

use crate::logging::target;
#[cfg(target_os = "linux")]
use crate::session::identity::arm_parent_death_signal;
use crate::session::identity::{
    configure_user_command, IdentityError, SessionEnvironment, UserIdentity,
};

const READY_TIMEOUT: Duration = Duration::from_secs(12);
/// A headless compositor, its stack and the first `ScreenCast` stream take
/// longer to prove than an X client's start; the agent bounds its own start
/// below this.
const WAYLAND_READY_TIMEOUT: Duration = super::wayland::WAYLAND_AGENT_READY_TIMEOUT;
const DESKTOP_START_GRACE: Duration = Duration::from_millis(1500);
/// The longest `dbus-update-activation-environment` may take.
const ACTIVATION_ENVIRONMENT_TIMEOUT: Duration = Duration::from_secs(5);
/// The desktop agent's launcher, which it must not outlive.
const PARENT_PID_FLAG: &str = "--parent-pid";
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const GRACEFUL_AGENT_STOP: Duration = Duration::from_secs(9);
/// The X11 agent's login-shell bound: its whole start before READY
/// (login shell, activation environment, start grace) must stay inside
/// [`READY_TIMEOUT`].
const X11_LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(3);
const ACTIVATION_ENVIRONMENT_KEYS: &[&str] = &[
    "DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_DESKTOP",
    "XDG_CURRENT_DESKTOP",
    "GDK_BACKEND",
    "TZ",
];

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("session-agent binary is unavailable")]
    BinaryUnavailable,
    #[error("session-agent process failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("session-agent identity setup failed: {0}")]
    Identity(#[from] IdentityError),
    #[error("session-agent readiness timed out")]
    ReadyTimeout,
    #[error("session-agent readiness message is invalid")]
    InvalidReady,
    #[error("the configured compositor prefix may not be used: {0}")]
    CompositorPrefix(String),
    #[error("session-agent exited before readiness")]
    ExitedBeforeReady,
    #[error("desktop session is unsupported")]
    UnsupportedDesktop,
    #[error("session-agent must not run as root")]
    RootAgent,
    #[error("D-Bus activation environment update failed")]
    DbusEnvironment,
    #[error("GNOME session exited during startup")]
    DesktopExited,
    #[error("host-owned display already has graphical clients")]
    ForeignXClients,
    #[error("session log id is missing or invalid")]
    InvalidSessionLogId,
    #[error("session-agent mode arguments are ambiguous or conflicting")]
    InvalidMode,
    #[error("session-agent launcher is gone or was never named")]
    LauncherGone,
    #[error("headless Wayland desktop failed: {0}")]
    Wayland(String),
    /// The operator requires the host's speakers to be silent and the
    /// agent could not prove they are.
    #[error("{0}")]
    LocalPlaybackRefused(arcen_session::local_playback::LocalPlaybackRefusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentDispatch {
    Desktop,
    Clipboard,
    Microphone,
    LocalPlaybackRecover,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentReady {
    pub pid: u32,
    pub uid: u32,
    pub username: String,
    pub display: String,
    pub session_id: String,
    pub session_type: String,
    pub desktop: String,
    /// The headless Wayland session's outputs, streams and input socket.
    /// Absent for the dedicated-Xorg desktop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wayland: Option<super::wayland::WaylandAgentReady>,
    /// How the session's audio is served and the local-playback evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<SessionAudioReady>,
}

/// A session's audio, as its agent established it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionAudioReady {
    pub mode: arcen_session::local_playback::SessionAudioMode,
    /// `PULSE_RUNTIME_PATH` for every helper of the session; `None` keeps
    /// the user's own server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulse_runtime_path: Option<PathBuf>,
    /// The monitor source audiocap records; `None` records the default
    /// sink's monitor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_source: Option<String>,
    pub local_playback: arcen_session::local_playback::LocalPlaybackReport,
    /// Whether native `PipeWire` clients are heard.
    pub native_pipewire: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<String>,
}

/// The line an agent prints instead of [`AgentReady`] when the
/// local-playback policy refuses the session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRefusal {
    pub local_playback_refused: arcen_session::local_playback::LocalPlaybackReport,
}

/// What the agent needs to serve the session's audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAudioArguments {
    pub local_playback: arcen_session::pier_config::LocalPlayback,
    pub pactl: PathBuf,
}

impl Default for AgentAudioArguments {
    fn default() -> Self {
        Self {
            local_playback: arcen_session::pier_config::LocalPlayback::default(),
            pactl: PathBuf::from(crate::media::local_playback::DEFAULT_PACTL),
        }
    }
}

impl AgentAudioArguments {
    fn to_arguments(&self) -> [String; 4] {
        [
            crate::media::local_playback::POLICY_FLAG.to_owned(),
            self.local_playback.token().to_owned(),
            crate::media::local_playback::PACTL_FLAG.to_owned(),
            self.pactl.to_string_lossy().into_owned(),
        ]
    }

    /// Reads the agent's own arguments. A missing policy is the safe
    /// default, `muted`; an unknown one is refused.
    ///
    /// # Errors
    ///
    /// [`AgentError::InvalidMode`] for an unknown policy token.
    pub fn from_arguments(args: &[String]) -> Result<Self, AgentError> {
        let local_playback = match argument_value(args, crate::media::local_playback::POLICY_FLAG) {
            Some(token) => arcen_session::pier_config::LocalPlayback::from_token(&token)
                .ok_or(AgentError::InvalidMode)?,
            None => arcen_session::pier_config::LocalPlayback::default(),
        };
        let pactl = argument_value(args, crate::media::local_playback::PACTL_FLAG).map_or_else(
            || PathBuf::from(crate::media::local_playback::DEFAULT_PACTL),
            PathBuf::from,
        );
        Ok(Self {
            local_playback,
            pactl,
        })
    }
}

/// Prints the refusal line for the launcher.
pub(crate) fn emit_local_playback_refusal(
    report: arcen_session::local_playback::LocalPlaybackReport,
) {
    use std::io::Write;
    if let Ok(line) = serde_json::to_string(&AgentRefusal {
        local_playback_refused: report,
    }) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
}

fn refusal_error(report: arcen_session::local_playback::LocalPlaybackReport) -> AgentError {
    AgentError::LocalPlaybackRefused(
        arcen_session::local_playback::admit(
            arcen_session::pier_config::LocalPlayback::Muted,
            Some(report.evidence),
        )
        .err()
        .unwrap_or(arcen_session::local_playback::LocalPlaybackRefusal::NoEvidence),
    )
}

/// What the launcher hands a headless Wayland agent for the session's
/// GNOME login services.
#[derive(Default)]
pub struct WaylandSessionServices {
    /// The login password, for the session's keyring.
    pub keyring_password: Option<Zeroizing<Vec<u8>>>,
    /// `wayland.first_login_setup`.
    pub first_login: arcen_session::desktop_session::first_login::FirstLoginSetup,
}

impl std::fmt::Debug for WaylandSessionServices {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WaylandSessionServices")
            .field(
                "keyring_password",
                &self.keyring_password.as_ref().map(|_| "<redacted>"),
            )
            .field("first_login", &self.first_login)
            .finish()
    }
}

pub struct SessionAgent {
    child: Child,
    stderr_task: JoinHandle<()>,
    pub ready: AgentReady,
}

impl SessionAgent {
    pub async fn spawn(
        binary: &Path,
        identity: &UserIdentity,
        environment: &SessionEnvironment,
        desktop: &str,
        session_log_id: &CorrelationId,
        audio: &AgentAudioArguments,
    ) -> Result<Self, AgentError> {
        let agent = Self::spawn_with(
            binary,
            identity,
            environment,
            desktop,
            session_log_id,
            &audio.to_arguments(),
            READY_TIMEOUT,
            None,
        )
        .await?;
        if agent.ready.wayland.is_some() {
            return Err(AgentError::InvalidReady);
        }
        Ok(agent)
    }

    /// Spawns the agent for a headless Wayland desktop that must create
    /// `runtime_dir` and exactly `outputs` (the first is primary), placed as
    /// given, with the compositor's Xwayland when `x11`, and waits for its
    /// verified ready message: every output at its requested size, refresh
    /// and, for several outputs, position.
    #[allow(clippy::too_many_arguments)] // The launcher's open context.
    pub async fn spawn_wayland(
        binary: &Path,
        identity: &UserIdentity,
        environment: &SessionEnvironment,
        desktop: &str,
        session_log_id: &CorrelationId,
        runtime_dir: &Path,
        outputs: &[arcen_session::wayland_outputs::PlacedOutput],
        audio: &AgentAudioArguments,
        fidelity_route: Option<&str>,
        compositor_prefix: Option<&str>,
        lab_input_seat: Option<&str>,
        native_seat: bool,
        cursor_mode: super::wayland::StreamCursorMode,
        services: WaylandSessionServices,
        x11: bool,
    ) -> Result<Self, AgentError> {
        let mut arguments = vec![
            super::wayland::AGENT_WAYLAND_FLAG.to_owned(),
            super::wayland::AGENT_RUNTIME_DIR_FLAG.to_owned(),
            runtime_dir.to_string_lossy().into_owned(),
            super::wayland::AGENT_CURSOR_MODE_FLAG.to_owned(),
            cursor_mode.token().to_owned(),
            super::wayland::FIRST_LOGIN_SETUP_FLAG.to_owned(),
            services.first_login.token().to_owned(),
        ];
        arguments.extend(audio.to_arguments());
        if services.keyring_password.is_some() {
            arguments.push(super::wayland::AGENT_KEYRING_STDIN_FLAG.to_owned());
        }
        for output in outputs {
            arguments.push(super::wayland::AGENT_OUTPUT_FLAG.to_owned());
            arguments.push(super::wayland::format_placed_output(output));
        }
        if let Some(route) = fidelity_route {
            arguments.push(super::wayland::AGENT_FIDELITY_ROUTE_FLAG.to_owned());
            arguments.push(route.to_owned());
        }
        if x11 {
            arguments.push(super::wayland::AGENT_X11_FLAG.to_owned());
        }
        if let Some(prefix) = compositor_prefix {
            let prefix = arcen_session::compositor_prefix::CompositorPrefix::parse(prefix)
                .map_err(|error| AgentError::CompositorPrefix(error.to_string()))?;
            super::wayland::verify_compositor_prefix(&prefix)
                .map_err(|error| AgentError::CompositorPrefix(error.to_string()))?;
            arguments.push(super::wayland::AGENT_COMPOSITOR_PREFIX_FLAG.to_owned());
            arguments.push(prefix.as_str().to_owned());
        }
        if let Some(seat) = lab_input_seat {
            arguments.push(super::wayland::AGENT_LAB_INPUT_SEAT_FLAG.to_owned());
            arguments.push(seat.to_owned());
            if native_seat {
                arguments.push(super::wayland::AGENT_NATIVE_SEAT_FLAG.to_owned());
            }
        }
        let agent = Self::spawn_with(
            binary,
            identity,
            environment,
            desktop,
            session_log_id,
            &arguments,
            WAYLAND_READY_TIMEOUT,
            services.keyring_password,
        )
        .await?;
        let consistent = agent.ready.wayland.as_ref().is_some_and(|wayland| {
            wayland.is_consistent()
                && wayland.x11.is_some() == x11
                && wayland.runtime_dir == runtime_dir
                && wayland.cursor_mode == cursor_mode
                && wayland.outputs.len() == outputs.len()
                && wayland
                    .outputs
                    .iter()
                    .zip(outputs)
                    .all(|(ready, requested)| {
                        ready.width == requested.request.width()
                            && ready.height == requested.request.height()
                            && ready.refresh_hz == requested.request.refresh_hz()
                    })
                && (outputs.len() == 1
                    || wayland
                        .streams
                        .iter()
                        .zip(outputs)
                        .all(|(stream, requested)| {
                            (stream.x, stream.y) == (requested.x, requested.y)
                        }))
        });
        if !consistent {
            return Err(AgentError::InvalidReady);
        }
        Ok(agent)
    }

    #[allow(clippy::too_many_arguments)] // The launcher's open context.
    async fn spawn_with(
        binary: &Path,
        identity: &UserIdentity,
        environment: &SessionEnvironment,
        desktop: &str,
        session_log_id: &CorrelationId,
        extra_arguments: &[String],
        ready_timeout: Duration,
        stdin_secret: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<Self, AgentError> {
        if !binary.is_file() {
            return Err(AgentError::BinaryUnavailable);
        }
        environment.validate_runtime(identity)?;

        let mut command = crate::command_for_helper(binary, "session-agent");
        command
            .arg(PARENT_PID_FLAG)
            .arg(std::process::id().to_string())
            .arg("--desktop-session")
            .arg(desktop)
            .arg("--session-log-id")
            .arg(session_log_id.as_str())
            .args(extra_arguments)
            .stdin(if stdin_secret.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        configure_user_command(&mut command, identity, environment)?;
        let mut child = command.spawn()?;
        if let Some(secret) = stdin_secret {
            // The agent reads it to end of file before anything else; the
            // pipe holds it, and it is never in argv, env or a file.
            if let Some(mut stdin) = child.stdin.take() {
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    stdin.write_all(&secret).await?;
                    stdin.shutdown().await
                })
                .await;
            }
            drop(secret);
        }
        let expected_pid = child.id().ok_or(AgentError::ExitedBeforeReady)?;
        let stdout = child.stdout.take().ok_or(AgentError::InvalidReady)?;
        let stderr = child.stderr.take().ok_or(AgentError::InvalidReady)?;
        let stderr_task = tokio::spawn(read_agent_stderr(stderr));
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(ready_timeout, lines.next_line())
            .await
            .map_err(|_| AgentError::ReadyTimeout)??
            .ok_or(AgentError::ExitedBeforeReady)?;
        if let Ok(refusal) = serde_json::from_str::<AgentRefusal>(&line) {
            tracing::warn!(
                target: target::SESSION,
                basis = refusal.local_playback_refused.basis.token(),
                observed_muted = ?refusal.local_playback_refused.evidence.observed_muted,
                "session-agent refused the session: the host's speakers could not be proven silent"
            );
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await;
            stderr_task.abort();
            return Err(refusal_error(refusal.local_playback_refused));
        }
        let ready: AgentReady =
            serde_json::from_str(&line).map_err(|_| AgentError::InvalidReady)?;
        if ready.pid != expected_pid
            || ready.uid != identity.uid
            || ready.username != identity.username
            || ready.display != environment.display()
            || ready.session_id != environment.session_id().unwrap_or_default()
            || ready.session_type != environment.session_type()
            || ready.desktop != environment.desktop()
        {
            return Err(AgentError::InvalidReady);
        }
        tracing::info!(
            target: target::SESSION,
            pid = ready.pid,
            uid = ready.uid,
            username = %ready.username,
            display = %ready.display,
            session_id = %ready.session_id,
            session_type = %ready.session_type,
            desktop = %ready.desktop,
            "authenticated user session-agent ready"
        );
        Ok(Self {
            child,
            stderr_task,
            ready,
        })
    }

    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub async fn wait_for_exit(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub async fn shutdown(mut self) {
        terminate_process(&mut self.child).await;
        finish_log_task(&mut self.stderr_task).await;
    }

    /// Asks only the agent to stop first, so it can end its D-Bus sessions
    /// and release its compositor stack in order; the process group follows
    /// only if it does not finish in time.
    pub async fn shutdown_gracefully(mut self) {
        #[cfg(target_os = "linux")]
        if let Some(pid) = self.child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        if tokio::time::timeout(GRACEFUL_AGENT_STOP, self.child.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                target: target::SESSION,
                "session-agent did not finish its own teardown in time; stopping its process group"
            );
            terminate_process(&mut self.child).await;
        }
        finish_log_task(&mut self.stderr_task).await;
    }
}

impl Drop for SessionAgent {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.stderr_task.abort();
    }
}

async fn read_agent_stderr<R>(stderr: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = crate::bounded_io::BoundedLineReader::new(BufReader::new(stderr));
    while let Ok(Some(line)) = reader.next_bounded_line().await {
        let text = crate::eventlog::bounded_diagnostic_line(&line.text);
        if line.truncated {
            tracing::warn!(
                target: target::SESSION,
                agent = true,
                "session-agent stderr line exceeded the bounded read limit; excess bytes discarded"
            );
        }
        // The agent's own warnings and errors stay visible at the
        // launcher's (production) level; its progress lines do not.
        if text.contains("\"severity\":\"error\"") {
            tracing::error!(target: target::SESSION, agent = true, "{text}");
        } else if text.contains("\"severity\":\"warn\"") {
            tracing::warn!(target: target::SESSION, agent = true, "{text}");
        } else {
            tracing::info!(target: target::SESSION, agent = true, "{text}");
        }
    }
}

async fn finish_log_task(task: &mut JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(1), &mut *task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn terminate_process(child: &mut Child) {
    #[cfg(target_os = "linux")]
    if let Some(pid) = child.id() {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait())
        .await
        .is_err()
    {
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

pub fn find_session_agent_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = explicit {
        candidates.push(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("ARCEN_SESSION_AGENT") {
        candidates.push(path.into());
    }
    candidates.push("/opt/arcen/bin/arcen-session-agent".into());
    candidates.push("/usr/local/libexec/arcen/arcen-session-agent".into());
    if let Ok(mut executable) = std::env::current_exe() {
        executable.pop();
        candidates.push(executable.join("arcen-session-agent"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

pub async fn verify_display_available(
    display: &str,
    xauthority: Option<&str>,
    allow_existing_clients: bool,
) -> Result<(), AgentError> {
    if allow_existing_clients {
        let requested_display = display.to_string();
        tracing::warn!(
            target: target::SESSION,
            x_display = requested_display,
            "UNSAFE: allowing a new user desktop on a display with unchecked existing X clients"
        );
        return Ok(());
    }
    let mut command = Command::new("/usr/bin/xlsclients");
    command.args(["-display", display, "-l"]);
    if let Some(xauthority) = xauthority {
        command.env("XAUTHORITY", xauthority);
    }
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .map_err(|_| AgentError::ForeignXClients)??;
    if !display_probe_succeeded(output.status.success(), &output.stdout) {
        return Err(AgentError::ForeignXClients);
    }

    Ok(())
}

fn display_probe_succeeded(status_success: bool, stdout: &[u8]) -> bool {
    status_success && stdout.is_empty()
}

pub async fn run_user_agent(args: &[String]) -> Result<(), AgentError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        Err(AgentError::Identity(IdentityError::Unsupported))
    }

    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        use std::os::unix::process::CommandExt;

        let uid = nix::unistd::Uid::effective();
        if uid.is_root() {
            return Err(AgentError::RootAgent);
        }
        let desktop =
            argument_value(args, "--desktop-session").unwrap_or_else(|| "gnome".to_string());
        if desktop != "gnome-classic" && desktop != "gnome" {
            return Err(AgentError::UnsupportedDesktop);
        }
        if args
            .iter()
            .any(|argument| argument == super::wayland::AGENT_WAYLAND_FLAG)
        {
            return run_wayland_agent(args).await;
        }
        let username = required_environment("USER")?;
        let display = required_environment("DISPLAY")?;
        let session_id = required_environment("XDG_SESSION_ID")?;
        let session_type = required_environment("XDG_SESSION_TYPE")?;
        let advertised_desktop = required_environment("XDG_SESSION_DESKTOP")?;
        let audio_arguments = AgentAudioArguments::from_arguments(args)?;
        let session_log_id = argument_value(args, "--session-log-id").unwrap_or_default();

        // The lease, before the desktop exists, so nothing it plays reaches
        // the host's speakers; beside the login-shell import and the
        // activation-environment update it does not depend on (the login
        // shell can never name a PULSE_* variable), so the two share one
        // slice of the ready budget. The import comes before the update, so
        // applications the user's bus or systemd user manager start
        // (D-Bus-activatable GNOME applications, user services) get it too,
        // as on a GDM login where gnome-session uploads its environment.
        let (mut audio, (login_environment, activation)) = tokio::join!(
            X11SessionAudio::establish(&audio_arguments, &session_log_id),
            async {
                let login_environment =
                    super::login_env::import_login_environment(X11_LOGIN_SHELL_TIMEOUT).await;
                let activation = update_activation_environment(&login_environment.imported).await;
                (login_environment, activation)
            }
        );
        if let Err(report) = audio.admission(&audio_arguments) {
            audio.release().await;
            emit_local_playback_refusal(report);
            return Err(refusal_error(report));
        }
        if let Err(error) = activation {
            audio.release().await;
            return Err(error);
        }

        let shell_args = gnome_shell_arguments(&display);
        let mut desktop_process = Command::new("/usr/bin/gnome-shell");
        desktop_process
            .envs(login_environment.imported)
            .args(&shell_args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        // SAFETY: this runs in the child immediately before exec and only arms
        // a kernel parent-death signal. GNOME remains in the agent's process
        // group, so the launcher can terminate the complete tree with killpg.
        unsafe {
            desktop_process
                .as_std_mut()
                .pre_exec(arm_parent_death_signal);
        }
        let mut desktop_child = match desktop_process.spawn() {
            Ok(child) => child,
            Err(error) => {
                audio.release().await;
                return Err(error.into());
            }
        };
        tokio::time::sleep(DESKTOP_START_GRACE).await;
        if !matches!(desktop_child.try_wait(), Ok(None)) {
            audio.release().await;
            return Err(AgentError::DesktopExited);
        }

        let ready = AgentReady {
            pid: std::process::id(),
            uid: uid.as_raw(),
            username,
            display,
            session_id,
            session_type,
            desktop: advertised_desktop,
            wayland: None,
            audio: Some(audio.ready()),
        };
        println!(
            "{}",
            serde_json::to_string(&ready).map_err(|_| AgentError::InvalidReady)?
        );
        std::io::stdout().flush()?;

        let desktop_ended = tokio::select! {
            status = desktop_child.wait() => Some(status),
            () = shutdown_signal() => None,
            () = audio.watch() => None,
        };
        if desktop_ended.is_none() {
            let _ = desktop_child.start_kill();
            if tokio::time::timeout(SHUTDOWN_TIMEOUT, desktop_child.wait())
                .await
                .is_err()
            {
                let _ = desktop_child.kill().await;
            }
        }
        audio.release().await;
        match desktop_ended {
            Some(status) => {
                status?;
                Err(AgentError::DesktopExited)
            }
            None => Ok(()),
        }
    }
}

/// The local-playback lease of a dedicated-Xorg desktop, on the user's own
/// audio server.
#[cfg(target_os = "linux")]
struct X11SessionAudio {
    lease: Option<crate::media::local_playback::PlaybackLease>,
    report: arcen_session::local_playback::LocalPlaybackReport,
    /// Native `PipeWire` clients bridged into a PulseAudio server; started
    /// after the agent is ready (see [`Self::watch`]).
    bridge: Option<crate::media::pipewire_bridge::PulseBridge>,
    bridge_pending: bool,
    server: crate::media::local_playback::PulseServer,
    runtime: PathBuf,
    native_pipewire: bool,
}

/// The longest the session's audio may delay the desktop agent's ready
/// message: the lease on the user's audio server is bounded by it, and the
/// native `PipeWire` bridge waits until after the ready message.
#[cfg(target_os = "linux")]
const PRE_READY_AUDIO_BUDGET: Duration = Duration::from_secs(4);
/// The longest a native `PipeWire` bridge may take to start, in the
/// background once the desktop is ready.
#[cfg(target_os = "linux")]
const BRIDGE_START_BUDGET: Duration = Duration::from_secs(20);

/// What the launcher's ready timeout always keeps unspent.
#[cfg(target_os = "linux")]
const READY_MARGIN: Duration = Duration::from_millis(1500);

#[cfg(target_os = "linux")]
const fn max_millis(left: u128, right: u128) -> u128 {
    if left > right {
        left
    } else {
        right
    }
}

// The one budget of a dedicated-Xorg agent's start before its ready message,
// every step at its worst: the lease runs beside the login-shell import and
// the activation-environment update (which follow each other), then the
// desktop's start grace; the launcher's ready timeout keeps a margin over it.
#[cfg(target_os = "linux")]
const _: () = assert!(
    max_millis(
        PRE_READY_AUDIO_BUDGET.as_millis(),
        X11_LOGIN_SHELL_TIMEOUT.as_millis() + ACTIVATION_ENVIRONMENT_TIMEOUT.as_millis()
    ) + DESKTOP_START_GRACE.as_millis()
        + READY_MARGIN.as_millis()
        < READY_TIMEOUT.as_millis()
);

#[cfg(target_os = "linux")]
impl X11SessionAudio {
    async fn establish(arguments: &AgentAudioArguments, session_log_id: &str) -> Self {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(
            || {
                PathBuf::from(format!(
                    "/run/user/{}",
                    nix::unistd::Uid::effective().as_raw()
                ))
            },
            PathBuf::from,
        );
        Self::establish_within(arguments, session_log_id, runtime, PRE_READY_AUDIO_BUDGET).await
    }

    /// The lease, bounded by `budget`: a server that does not answer in
    /// time leaves the mute unproven (a required mute then refuses the
    /// session, and the launcher restores whatever the lease had begun).
    async fn establish_within(
        arguments: &AgentAudioArguments,
        session_log_id: &str,
        runtime: PathBuf,
        budget: Duration,
    ) -> Self {
        use crate::media::local_playback::{establish_shared_lease, PulseServer, JOURNAL_NAME};
        use crate::media::pipewire_bridge::{pipewire_installed, pulse_tunnel_installed};
        use arcen_session::local_playback::{
            shared_server_session_sink, LocalPlaybackReport, MuteBasis, MuteEvidence,
        };
        let server = PulseServer::inherited(&arguments.pactl);
        let (lease, report) = if arguments.local_playback.requires_mute() {
            let tag: String = session_log_id
                .chars()
                .filter(char::is_ascii_hexdigit)
                .take(8)
                .collect();
            match tokio::time::timeout(
                budget,
                establish_shared_lease(
                    server.clone(),
                    runtime.join(JOURNAL_NAME),
                    shared_server_session_sink(&tag),
                ),
            )
            .await
            {
                Ok(outcome) => (outcome.lease, outcome.report),
                Err(_) => {
                    tracing::warn!(
                        target: target::AUDIO,
                        event = "local_playback_lease",
                        action = "timed_out",
                        budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                        "the user's audio server did not answer in time; the mute is unproven"
                    );
                    (
                        None,
                        LocalPlaybackReport {
                            evidence: MuteEvidence::observed(true, None),
                            basis: MuteBasis::SessionSinkLease,
                        },
                    )
                }
            }
        } else {
            (None, LocalPlaybackReport::not_requested())
        };
        // Only file checks before the ready message; the server is asked
        // whether it is PulseAudio itself (and so needs the bridge) after
        // it, and a bridge that cannot start is logged then.
        let pipewire = pipewire_installed();
        Self {
            lease,
            report,
            bridge: None,
            bridge_pending: pipewire && pulse_tunnel_installed(),
            server,
            runtime,
            native_pipewire: pipewire,
        }
    }

    fn admission(
        &self,
        arguments: &AgentAudioArguments,
    ) -> Result<(), arcen_session::local_playback::LocalPlaybackReport> {
        arcen_session::local_playback::admit(arguments.local_playback, Some(self.report.evidence))
            .map_err(|_| self.report)
    }

    fn ready(&self) -> SessionAudioReady {
        SessionAudioReady {
            mode: arcen_session::local_playback::SessionAudioMode::UserServer,
            pulse_runtime_path: None,
            capture_source: self
                .lease
                .as_ref()
                .map(|lease| format!("{}.monitor", lease.session_sink())),
            local_playback: self.report,
            native_pipewire: self.native_pipewire,
            degradation: (!self.native_pipewire).then(|| "native_pipewire_unserved".to_owned()),
        }
    }

    /// Keeps the lease enforced from the moment the desktop is ready and,
    /// beside it, starts the native `PipeWire` bridge (bounded) and keeps it
    /// on its defaults: a slow bridge never delays enforcement.
    async fn watch(&mut self) {
        self.watch_within(BRIDGE_START_BUDGET).await;
    }

    async fn watch_within(&mut self, bridge_budget: Duration) {
        let Self {
            lease,
            bridge,
            bridge_pending,
            server,
            runtime,
            ..
        } = self;
        let pending = std::mem::take(bridge_pending);
        let session_sink = lease.as_ref().map(|lease| lease.session_sink().to_owned());
        let enforce = async {
            match lease.as_mut() {
                Some(lease) => lease.watch().await,
                None => std::future::pending().await,
            }
        };
        let tunnels = async {
            if pending {
                *bridge = start_bridge_within(server, runtime, session_sink, bridge_budget).await;
            }
            match bridge.as_mut() {
                Some(bridge) => bridge.follow().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::join!(enforce, tunnels);
    }

    async fn release(&mut self) {
        if let Some(mut bridge) = self.bridge.take() {
            bridge.stop().await;
        }
        if let Some(lease) = self.lease.take() {
            let _ = lease.release().await;
        }
    }
}

/// On a host whose audio server is PulseAudio itself, the user's `PipeWire`
/// has no audio sink: native clients would be silent and would not see the
/// Deck microphone. A bridge client tunnels both ways. Bounded by `budget`;
/// a bridge that cannot start is logged and the session continues without
/// it.
#[cfg(target_os = "linux")]
async fn start_bridge_within(
    server: &crate::media::local_playback::PulseServer,
    runtime: &Path,
    session_sink: Option<String>,
    budget: Duration,
) -> Option<crate::media::pipewire_bridge::PulseBridge> {
    match tokio::time::timeout(budget, start_bridge(server, runtime, session_sink)).await {
        Ok(bridge) => bridge,
        Err(_) => {
            tracing::warn!(
                target: target::AUDIO,
                event = "session_audio_bridge",
                budget_ms = u64::try_from(budget.as_millis()).unwrap_or(u64::MAX),
                "the native PipeWire bridge did not start in time; native PipeWire clients are not heard"
            );
            None
        }
    }
}

#[cfg(target_os = "linux")]
async fn start_bridge(
    server: &crate::media::local_playback::PulseServer,
    runtime: &Path,
    session_sink: Option<String>,
) -> Option<crate::media::pipewire_bridge::PulseBridge> {
    use crate::media::pipewire_bridge::{
        pipewire_version, server_is_pulseaudio_itself, PulseBridge,
    };
    let info = server.info().await.ok()?;
    if !server_is_pulseaudio_itself(&info) {
        return None;
    }
    // The user's PipeWire is socket-activated by the user manager that this
    // session's login started; give its socket a moment to appear.
    let socket = runtime.join("pipewire-0");
    for _ in 0..40 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !socket.exists() {
        tracing::warn!(
            target: target::AUDIO,
            event = "session_audio_mode",
            degradation = "native_pipewire_unserved",
            "the audio server is PulseAudio and the user's PipeWire is not running: native PipeWire clients are not heard"
        );
        return None;
    }
    let user_server = std::env::var_os("PULSE_RUNTIME_PATH")
        .filter(|value| !value.is_empty())
        .map_or_else(|| runtime.join("pulse"), PathBuf::from)
        .join("native");
    let reconnect = pipewire_version().await.is_some_and(|(major, minor)| {
        arcen_session::local_playback::pulse_tunnel_reconnects(major, minor)
    });
    // The sink tunnel plays into a muted session's lease sink, or follows the
    // default sink of an audible one.
    Some(PulseBridge::start(server, &user_server, runtime, None, reconnect, session_sink).await)
}

#[cfg(not(target_os = "linux"))]
async fn run_local_playback_recovery(_args: &[String]) -> Result<(), String> {
    Err("local playback recovery runs on Linux only".to_owned())
}

/// Restores a local-playback journal a session left behind, as the
/// session user (the launcher runs it after an agent that ended without
/// releasing its lease).
#[cfg(target_os = "linux")]
async fn run_local_playback_recovery(args: &[String]) -> Result<(), String> {
    use crate::media::local_playback::{recover, PulseServer, JOURNAL_NAME};
    if nix::unistd::Uid::effective().is_root() {
        return Err("local playback recovery refuses to run as root".to_owned());
    }
    let arguments = AgentAudioArguments::from_arguments(args).map_err(|error| error.to_string())?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "local playback recovery requires XDG_RUNTIME_DIR".to_owned())?;
    recover(
        &PulseServer::inherited(&arguments.pactl),
        &runtime.join(JOURNAL_NAME),
    )
    .await
    .map(drop)
    .map_err(|error| error.to_string())
}

#[cfg(all(target_os = "linux", feature = "wayland-provider"))]
async fn run_wayland_agent(args: &[String]) -> Result<(), AgentError> {
    let session_log_id = argument_value(args, "--session-log-id")
        .and_then(|value| CorrelationId::parse_uuid(value).ok())
        .ok_or(AgentError::InvalidSessionLogId)?;
    super::wayland::agent::run(args, &session_log_id)
        .await
        .map_err(|error| AgentError::Wayland(error.to_string()))
}

#[cfg(all(target_os = "linux", not(feature = "wayland-provider")))]
async fn run_wayland_agent(_args: &[String]) -> Result<(), AgentError> {
    Err(AgentError::UnsupportedDesktop)
}

/// What the X11 desktop passes to its bus and systemd user manager, so
/// services they start get the session's display, time zone and locale.
fn activation_environment_keys() -> impl Iterator<Item = &'static str> {
    ACTIVATION_ENVIRONMENT_KEYS
        .iter()
        .chain(arcen_session::session_locale::LOCALE_VARIABLES.iter())
        .copied()
}

/// `dbus-update-activation-environment` arguments: the session's own
/// display, time-zone and locale variables, then the login shell's
/// (already filtered: none of them can name a variable Arcen owns, and
/// together they are bounded by the login-environment dump limit).
fn activation_arguments(
    current: impl Fn(&str) -> Option<String>,
    imported: &[(String, String)],
) -> Vec<String> {
    let mut arguments = vec!["--systemd".to_owned()];
    for key in activation_environment_keys() {
        if let Some(value) = current(key) {
            arguments.push(format!("{key}={value}"));
        }
    }
    for (name, value) in imported {
        if !activation_environment_keys().any(|key| key == name) {
            arguments.push(format!("{name}={value}"));
        }
    }
    arguments
}

#[cfg(target_os = "linux")]
async fn update_activation_environment(imported: &[(String, String)]) -> Result<(), AgentError> {
    let mut command = Command::new("/usr/bin/dbus-update-activation-environment");
    command.args(activation_arguments(
        |key| std::env::var(key).ok(),
        imported,
    ));
    let status = tokio::time::timeout(ACTIVATION_ENVIRONMENT_TIMEOUT, command.status())
        .await
        .map_err(|_| AgentError::DbusEnvironment)??;
    if status.success() {
        Ok(())
    } else {
        Err(AgentError::DbusEnvironment)
    }
}

#[cfg(target_os = "linux")]
fn required_environment(name: &str) -> Result<String, AgentError> {
    std::env::var(name).map_err(|_| AgentError::InvalidReady)
}

#[cfg(target_os = "linux")]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = interrupt.recv() => {}
    }
}

fn argument_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn agent_dispatch(args: &[String]) -> Result<AgentDispatch, AgentError> {
    let count = |flag: &str| args.iter().filter(|argument| *argument == flag).count();
    let microphone_agent = count("--microphone-agent");
    let microphone_recover = count("--microphone-recover");
    let clipboard = count("--clipboard-agent");
    let desktop = count("--desktop-session");
    let microphone_codec = count("--microphone-codec");
    let microphone_generation = count("--microphone-generation");
    let microphone_setup = microphone_codec != 0 || microphone_generation != 0;
    let parent_pid = count(PARENT_PID_FLAG);
    let local_playback_recover = count(crate::media::local_playback::RECOVER_FLAG);

    if microphone_agent + microphone_recover + clipboard + desktop + local_playback_recover > 1
        || (local_playback_recover != 0 && (parent_pid != 0 || microphone_setup))
        || microphone_codec > 1
        || microphone_generation > 1
        || parent_pid > 1
        // Only the launcher's desktop agent is tied to a parent; helpers the
        // Pier spawns keep the Pier's own lifetime rules.
        || (parent_pid != 0 && microphone_agent + microphone_recover + clipboard != 0)
        || (clipboard != 0 && microphone_setup)
        || (microphone_recover != 0 && microphone_setup)
        || (microphone_setup && microphone_agent == 0)
    {
        return Err(AgentError::InvalidMode);
    }
    if local_playback_recover != 0 {
        Ok(AgentDispatch::LocalPlaybackRecover)
    } else if microphone_agent != 0 || microphone_recover != 0 {
        Ok(AgentDispatch::Microphone)
    } else if clipboard != 0 {
        Ok(AgentDispatch::Clipboard)
    } else {
        Ok(AgentDispatch::Desktop)
    }
}

fn gnome_shell_arguments(display: &str) -> [String; 4] {
    [
        "--x11".to_string(),
        "--sm-disable".to_string(),
        format!("--display={display}"),
        "--replace".to_string(),
    ]
}

/// The launcher pid a desktop agent was started with: `None` when the flag
/// is absent, an error when it is present but not a plausible pid.
fn parent_pid_argument(args: &[String]) -> Result<Option<i32>, AgentError> {
    if !args.iter().any(|argument| argument == PARENT_PID_FLAG) {
        return Ok(None);
    }
    argument_value(args, PARENT_PID_FLAG)
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|pid| *pid > 1)
        .map(Some)
        .ok_or(AgentError::LauncherGone)
}

/// Ties a desktop agent to its launcher before anything else runs.
///
/// The `execve` that starts the agent enters the session user's SELinux
/// context, and the kernel clears the parent-death signal the launcher
/// armed on every such transition (as it does on a credential change). So
/// the agent arms it again itself — `SIGTERM`, which its normal stop
/// handling turns into a complete release of the desktop and its stack —
/// and then proves its parent is still the launcher that named itself: a
/// launcher that died before the signal was armed left the agent with
/// another parent (init or a subreaper), and it exits.
#[cfg(target_os = "linux")]
fn tie_to_launcher(launcher: i32) -> Result<(), AgentError> {
    // SAFETY: prctl/getppid take no pointers here.
    unsafe {
        if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGTERM) != 0 {
            return Err(AgentError::Io(std::io::Error::last_os_error()));
        }
        if nix::libc::getppid() != launcher {
            return Err(AgentError::LauncherGone);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn tie_to_launcher(_launcher: i32) -> Result<(), AgentError> {
    Err(AgentError::Identity(IdentityError::Unsupported))
}

pub fn agent_main(args: &[String]) -> ExitCode {
    let dispatch = agent_dispatch(args);
    match (&dispatch, parent_pid_argument(args)) {
        (Ok(AgentDispatch::Desktop), Ok(Some(launcher))) => {
            if let Err(error) = tie_to_launcher(launcher) {
                eprintln!("session-agent: {error}");
                return ExitCode::FAILURE;
            }
        }
        (Ok(AgentDispatch::Desktop), _) => {
            eprintln!("session-agent: {}", AgentError::LauncherGone);
            return ExitCode::FAILURE;
        }
        (_, Err(error)) => {
            eprintln!("session-agent: {error}");
            return ExitCode::FAILURE;
        }
        _ => {}
    }
    let session_log_id = match argument_value(args, "--session-log-id")
        .ok_or(AgentError::InvalidSessionLogId)
        .and_then(|value| {
            CorrelationId::parse_uuid(value).map_err(|_| AgentError::InvalidSessionLogId)
        }) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("session-agent: {error}");
            return ExitCode::FAILURE;
        }
    };
    // A short-lived, unprivileged desktop-session child: its stderr is
    // captured and re-emitted by the privileged session launcher (which is
    // in turn captured by the broker), so this process's own canonical
    // writer targets stderr directly rather than the root-owned managed
    // log file. `ARCEN_LOG` still refines the resolved production-default
    // profile when the environment carries it (inherited from the broker).
    let observability = match arcen_observability::ObservabilityBuilder::new(
        arcen_telemetry::TelemetryRole::Host,
        arcen_telemetry::TelemetryComponent::new(arcen_telemetry::names::component::SESSION_AGENT)
            .expect("session_agent is a valid canonical component"),
        arcen_telemetry::TelemetryPlatform::Linux,
        arcen_telemetry::OperationalProfile::Critical,
    )
    .canonical_writer("stderr", std::io::stderr())
    .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("session-agent: logging setup failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = tracing::dispatcher::set_global_default(observability.dispatch()) {
        eprintln!("session-agent: failed to install tracing dispatch: {error}");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "failed to create session-agent runtime");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(sid = %session_log_id, "session-agent correlation initialized");
    let result = match dispatch {
        Ok(AgentDispatch::Microphone) => runtime.block_on(crate::microphone_input::run_child(args)),
        Ok(AgentDispatch::Clipboard) => runtime.block_on(crate::clipboard::run_clipboard_child()),
        Ok(AgentDispatch::LocalPlaybackRecover) => {
            runtime.block_on(run_local_playback_recovery(args))
        }
        Ok(AgentDispatch::Desktop) => runtime
            .block_on(run_user_agent(args))
            .map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let exit_code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "session-agent failed");
            ExitCode::FAILURE
        }
    };
    let _ = observability
        .guard()
        .shutdown(std::time::Duration::from_secs(2));
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_stderr_bounds_an_enormous_unterminated_line_and_keeps_reading() {
        // Proves the integration wiring (not just the shared bounded_io
        // module in isolation): a helper stderr pipe that emits far more
        // than the bounded-read cap with no newline must not hang or grow
        // the reading task's memory with the input, and normal lines after
        // it must still be forwarded.
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        let writer_task = tokio::spawn(async move {
            let huge = vec![b'x'; 1024 * 1024]; // 1 MiB, no newline
            tokio::io::AsyncWriteExt::write_all(&mut writer, &huge)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut writer, b"\nafter\n")
                .await
                .unwrap();
        });
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), read_agent_stderr(reader))
                .await;
        assert!(
            result.is_ok(),
            "reading an enormous unterminated line must complete promptly, not hang"
        );
        writer_task.await.unwrap();
    }

    #[test]
    fn only_known_desktop_tokens_are_accepted_by_argument_parser() {
        let args = vec![
            "agent".to_string(),
            "--desktop-session".to_string(),
            "gnome-classic".to_string(),
        ];
        assert_eq!(
            argument_value(&args, "--desktop-session").as_deref(),
            Some("gnome-classic")
        );
    }

    #[test]
    fn helper_dispatch_is_pure_and_recovery_never_selects_desktop() {
        let arguments = |mode: &str| {
            vec![
                "agent".to_string(),
                mode.to_string(),
                "--session-log-id".to_string(),
                "00000000-0000-0000-0000-000000000001".to_string(),
                "--pactl-bin".to_string(),
                "/usr/bin/pactl".to_string(),
            ]
        };
        assert_eq!(
            agent_dispatch(&arguments("--microphone-agent")).unwrap(),
            AgentDispatch::Microphone
        );
        assert_eq!(
            agent_dispatch(&arguments("--microphone-recover")).unwrap(),
            AgentDispatch::Microphone
        );
    }

    #[cfg(target_os = "linux")]
    fn slow_pactl(name: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/arcen-test-artifacts")
            .join(format!("slow-pactl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let script = directory.join("pactl");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, directory)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_silent_audio_server_cannot_hold_up_the_ready_message() {
        let (pactl, runtime) = slow_pactl("lease");
        let arguments = AgentAudioArguments {
            local_playback: arcen_session::pier_config::LocalPlayback::Muted,
            pactl,
        };
        let started = std::time::Instant::now();
        let audio = X11SessionAudio::establish_within(
            &arguments,
            "0123abcd-0000",
            runtime.clone(),
            Duration::from_millis(300),
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the lease is bounded by its budget, not by the server"
        );
        assert_eq!(audio.report.evidence.observed_muted, None);
        assert!(
            audio.admission(&arguments).is_err(),
            "an unproven mute refuses the session"
        );
        assert!(audio.bridge.is_none(), "no bridge starts before ready");
        let _ = std::fs::remove_dir_all(runtime);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_slow_bridge_start_is_bounded() {
        let (pactl, runtime) = slow_pactl("bridge");
        let started = std::time::Instant::now();
        let bridge = start_bridge_within(
            &crate::media::local_playback::PulseServer::inherited(&pactl),
            &runtime,
            None,
            Duration::from_millis(300),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(bridge.is_none());
        let _ = std::fs::remove_dir_all(runtime);
    }

    /// A pactl whose `subscribe` (the lease's watcher) leaves a mark and
    /// whose every other command (the bridge's `info` among them) hangs.
    #[cfg(target_os = "linux")]
    fn subscribing_pactl(name: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let (script, directory) = slow_pactl(name);
        let mark = directory.join("subscribed");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = subscribe ]; then touch {}; fi\nexec sleep 30\n",
                mark.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, directory)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_lease_is_enforced_while_the_bridge_is_still_starting() {
        let (pactl, runtime) = subscribing_pactl("enforce");
        let server = crate::media::local_playback::PulseServer::inherited(&pactl);
        let mut audio = X11SessionAudio {
            lease: Some(crate::media::local_playback::PlaybackLease::detached(
                server.clone(),
                runtime.join("journal.json"),
                "arcen_session_0123abcd",
            )),
            report: arcen_session::local_playback::LocalPlaybackReport::not_requested(),
            bridge: None,
            bridge_pending: true,
            server,
            runtime: runtime.clone(),
            native_pipewire: true,
        };
        // The bridge's start is held up for the whole window (its `pactl
        // info` never answers); the lease's watcher must subscribe anyway.
        let _ = tokio::time::timeout(
            Duration::from_millis(800),
            audio.watch_within(Duration::from_secs(30)),
        )
        .await;
        assert!(
            runtime.join("subscribed").exists(),
            "the lease began enforcing before the bridge had started"
        );
        assert!(audio.bridge.is_none(), "the bridge was still starting");
        let _ = std::fs::remove_dir_all(runtime);
    }

    #[test]
    fn local_playback_recovery_is_its_own_helper_mode() {
        let recover = [
            "agent",
            crate::media::local_playback::RECOVER_FLAG,
            "--session-log-id",
            "00000000-0000-0000-0000-000000000001",
        ]
        .map(str::to_owned);
        assert_eq!(
            agent_dispatch(&recover).unwrap(),
            AgentDispatch::LocalPlaybackRecover
        );
        for conflicting in [
            vec![
                "agent",
                crate::media::local_playback::RECOVER_FLAG,
                "--microphone-agent",
            ],
            vec![
                "agent",
                crate::media::local_playback::RECOVER_FLAG,
                "--desktop-session",
                "gnome",
            ],
            vec![
                "agent",
                crate::media::local_playback::RECOVER_FLAG,
                PARENT_PID_FLAG,
                "7",
            ],
        ] {
            let conflicting = conflicting
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert!(agent_dispatch(&conflicting).is_err(), "{conflicting:?}");
        }
    }

    #[test]
    fn audio_arguments_round_trip_and_default_to_muted() {
        let audio = AgentAudioArguments {
            local_playback: arcen_session::pier_config::LocalPlayback::Audible,
            pactl: PathBuf::from("/opt/pactl"),
        };
        let mut args = vec!["agent".to_owned()];
        args.extend(audio.to_arguments());
        assert_eq!(AgentAudioArguments::from_arguments(&args).unwrap(), audio);
        assert_eq!(
            AgentAudioArguments::from_arguments(&["agent".to_owned()]).unwrap(),
            AgentAudioArguments::default()
        );
        assert!(AgentAudioArguments::from_arguments(
            &["agent", crate::media::local_playback::POLICY_FLAG, "quiet"].map(str::to_owned)
        )
        .is_err());
    }

    #[test]
    fn a_refusal_line_is_not_a_ready_line_and_names_its_evidence() {
        let report = arcen_session::local_playback::LocalPlaybackReport {
            evidence: arcen_session::local_playback::MuteEvidence::observed(true, Some(false)),
            basis: arcen_session::local_playback::MuteBasis::SessionSinkLease,
        };
        let line = serde_json::to_string(&AgentRefusal {
            local_playback_refused: report,
        })
        .unwrap();
        assert!(serde_json::from_str::<AgentReady>(&line).is_err());
        let parsed: AgentRefusal = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            refusal_error(parsed.local_playback_refused),
            AgentError::LocalPlaybackRefused(
                arcen_session::local_playback::LocalPlaybackRefusal::NotHonoured { .. }
            )
        ));
        let ready = r#"{"pid":1,"uid":2,"username":"u","display":":1","session_id":"3","session_type":"x11","desktop":"gnome"}"#;
        assert!(serde_json::from_str::<AgentRefusal>(ready).is_err());
        assert!(serde_json::from_str::<AgentReady>(ready)
            .unwrap()
            .audio
            .is_none());
    }

    #[test]
    fn helper_dispatch_rejects_ambiguous_or_conflicting_modes() {
        for args in [
            vec!["agent", "--microphone-agent", "--microphone-recover"],
            vec!["agent", "--microphone-recover", "--clipboard-agent"],
            vec!["agent", "--microphone-agent", "--desktop-session", "gnome"],
            vec![
                "agent",
                "--microphone-recover",
                "--microphone-codec",
                "opus",
            ],
            vec!["agent", "--microphone-generation", "7"],
            vec!["agent", "--microphone-agent", "--microphone-agent"],
        ] {
            let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
            assert!(matches!(
                agent_dispatch(&args),
                Err(AgentError::InvalidMode)
            ));
        }
    }

    #[test]
    fn only_the_desktop_agent_names_one_launcher() {
        let args = |list: &[&str]| list.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            agent_dispatch(&args(&[
                "agent",
                "--parent-pid",
                "42",
                "--desktop-session",
                "gnome"
            ]))
            .unwrap(),
            AgentDispatch::Desktop
        );
        for invalid in [
            &["agent", "--parent-pid", "42", "--parent-pid", "43"][..],
            &["agent", "--parent-pid", "42", "--clipboard-agent"],
            &["agent", "--parent-pid", "42", "--microphone-agent"],
        ] {
            assert!(matches!(
                agent_dispatch(&args(invalid)),
                Err(AgentError::InvalidMode)
            ));
        }
        assert_eq!(parent_pid_argument(&args(&["agent"])).unwrap(), None);
        assert_eq!(
            parent_pid_argument(&args(&["agent", "--parent-pid", "4242"])).unwrap(),
            Some(4242)
        );
        for bad in ["1", "0", "-5", "x", ""] {
            assert!(parent_pid_argument(&args(&["agent", "--parent-pid", bad])).is_err());
        }
        assert!(parent_pid_argument(&args(&["agent", "--parent-pid"])).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_whose_launcher_is_not_its_parent_refuses_to_run() {
        // SAFETY: getppid takes no arguments.
        let parent = unsafe { nix::libc::getppid() };
        assert!(tie_to_launcher(parent).is_ok());
        assert!(matches!(
            tie_to_launcher(parent.wrapping_add(1).max(2)),
            Err(AgentError::LauncherGone)
        ));
        // SAFETY: disarm what the test armed on this test thread.
        unsafe {
            nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, 0);
        }
    }

    #[test]
    fn direct_shell_launch_disables_the_locking_session_manager() {
        assert_eq!(
            gnome_shell_arguments(":10"),
            ["--x11", "--sm-disable", "--display=:10", "--replace",].map(str::to_string)
        );
    }

    #[test]
    fn display_probe_fails_closed_on_tool_error_or_any_existing_client() {
        assert!(display_probe_succeeded(true, b""));
        assert!(!display_probe_succeeded(false, b""));
        assert!(!display_probe_succeeded(
            true,
            b"Window 0x40001e: root xterm"
        ));
    }

    #[test]
    fn activation_environment_includes_process_timezone() {
        assert!(ACTIVATION_ENVIRONMENT_KEYS.contains(&"TZ"));
    }

    #[test]
    fn the_login_environment_reaches_the_activation_environment_after_the_sessions_own() {
        let current = |key: &str| match key {
            "DISPLAY" => Some(":10".to_owned()),
            "TZ" => Some("Europe/Oslo".to_owned()),
            _ => None,
        };
        let imported = vec![
            ("PATH".to_owned(), "/home/u/bin:/usr/bin".to_owned()),
            (
                "LM_LICENSE_FILE".to_owned(),
                "27000@licence.example.internal".to_owned(),
            ),
            ("DISPLAY".to_owned(), ":99".to_owned()),
        ];
        assert_eq!(
            activation_arguments(current, &imported),
            [
                "--systemd",
                "DISPLAY=:10",
                "TZ=Europe/Oslo",
                "PATH=/home/u/bin:/usr/bin",
                "LM_LICENSE_FILE=27000@licence.example.internal",
            ]
        );
    }

    #[test]
    fn activation_environment_includes_the_session_locale() {
        let keys = activation_environment_keys().collect::<Vec<_>>();
        assert!(keys.contains(&"LANG"));
        assert!(keys.contains(&"LC_TIME"));
        assert!(!keys.contains(&"LC_ALL"));
    }
}
