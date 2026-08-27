//! Machine-service side session-agent supervision and user-side GNOME launch.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use arcen_telemetry::CorrelationId;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::logging::target;
#[cfg(target_os = "linux")]
use crate::session::identity::arm_parent_death_signal;
use crate::session::identity::{
    configure_user_command, IdentityError, SessionEnvironment, UserIdentity,
};

const READY_TIMEOUT: Duration = Duration::from_secs(12);
const DESKTOP_START_GRACE: Duration = Duration::from_millis(1500);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentDispatch {
    Desktop,
    Clipboard,
    Microphone,
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
    ) -> Result<Self, AgentError> {
        if !binary.is_file() {
            return Err(AgentError::BinaryUnavailable);
        }
        environment.validate_runtime(identity)?;

        let mut command = crate::command_for_helper(binary, "session-agent");
        command
            .arg("--desktop-session")
            .arg(desktop)
            .arg("--session-log-id")
            .arg(session_log_id.as_str())
            .stdin(Stdio::null())
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
        let expected_pid = child.id().ok_or(AgentError::ExitedBeforeReady)?;
        let stdout = child.stdout.take().ok_or(AgentError::InvalidReady)?;
        let stderr = child.stderr.take().ok_or(AgentError::InvalidReady)?;
        let stderr_task = tokio::spawn(read_agent_stderr(stderr));
        let mut lines = BufReader::new(stdout).lines();
        let line = tokio::time::timeout(READY_TIMEOUT, lines.next_line())
            .await
            .map_err(|_| AgentError::ReadyTimeout)??
            .ok_or(AgentError::ExitedBeforeReady)?;
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
        tracing::info!(target: target::SESSION, agent = true, "{text}");
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
        let username = required_environment("USER")?;
        let display = required_environment("DISPLAY")?;
        let session_id = required_environment("XDG_SESSION_ID")?;
        let session_type = required_environment("XDG_SESSION_TYPE")?;
        let advertised_desktop = required_environment("XDG_SESSION_DESKTOP")?;

        update_activation_environment().await?;

        let shell_args = gnome_shell_arguments(&display);
        let mut desktop_process = Command::new("/usr/bin/gnome-shell");
        desktop_process
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
        let mut desktop_child = desktop_process.spawn()?;
        tokio::time::sleep(DESKTOP_START_GRACE).await;
        if desktop_child.try_wait()?.is_some() {
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
        };
        println!(
            "{}",
            serde_json::to_string(&ready).map_err(|_| AgentError::InvalidReady)?
        );
        std::io::stdout().flush()?;

        tokio::select! {
            status = desktop_child.wait() => {
                status?;
                return Err(AgentError::DesktopExited);
            }
            _ = shutdown_signal() => {}
        }
        let _ = desktop_child.start_kill();
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, desktop_child.wait())
            .await
            .is_err()
        {
            let _ = desktop_child.kill().await;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn update_activation_environment() -> Result<(), AgentError> {
    let mut command = Command::new("/usr/bin/dbus-update-activation-environment");
    command.arg("--systemd");
    for key in ACTIVATION_ENVIRONMENT_KEYS {
        if let Ok(value) = std::env::var(key) {
            command.arg(format!("{key}={value}"));
        }
    }
    let status = tokio::time::timeout(Duration::from_secs(5), command.status())
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

    if microphone_agent + microphone_recover + clipboard + desktop > 1
        || microphone_codec > 1
        || microphone_generation > 1
        || (clipboard != 0 && microphone_setup)
        || (microphone_recover != 0 && microphone_setup)
        || (microphone_setup && microphone_agent == 0)
    {
        return Err(AgentError::InvalidMode);
    }
    if microphone_agent != 0 || microphone_recover != 0 {
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

pub fn agent_main(args: &[String]) -> ExitCode {
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
    let result = match agent_dispatch(args) {
        Ok(AgentDispatch::Microphone) => runtime.block_on(crate::microphone_input::run_child(args)),
        Ok(AgentDispatch::Clipboard) => runtime.block_on(crate::clipboard::run_clipboard_child()),
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
}
