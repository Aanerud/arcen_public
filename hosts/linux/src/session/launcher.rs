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
use super::agent::{AgentError, AgentReady};
use super::identity::{IdentityError, SessionEnvironment, UserExecution, UserIdentity};
use crate::logging::target;

const AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);
const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(12);
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
    #[error("authenticated logind session could not be unlocked")]
    LogindUnlock,
    #[error("deskside protection failed")]
    Deskside,
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
    pub deskside: crate::deskside::LinuxDesksideConfig,
}

#[derive(Serialize)]
struct AuthenticateRequest<'a> {
    command: &'static str,
    username: &'a str,
    password: &'a str,
    remote_host: &'a str,
}

#[derive(Debug, Deserialize)]
struct OwnedAuthenticateRequest {
    command: String,
    username: String,
    password: String,
    remote_host: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
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
}

impl MultiHeadPlanMsg {
    fn from_plan(plan: &crate::display::topology::LinuxTopologyPlan) -> Self {
        Self {
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
    ) -> Self {
        Self {
            command: "open".to_string(),
            timezone,
            deskside,
            multi_monitor,
        }
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
        }
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
    session_log_id: Arc<Mutex<CorrelationId>>,
    timezone: Option<IanaTimeZone>,
    deskside: crate::deskside::LinuxDesksideConfig,
    multi_monitor_plan: Option<MultiHeadPlanMsg>,
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

    pub async fn authenticate(
        config: &LauncherConfig,
        username: &str,
        password: String,
        remote_host: &str,
        pam_permit: OwnedSemaphorePermit,
        session_log_id: CorrelationId,
    ) -> Result<Self, LauncherError> {
        let password = Zeroizing::new(password);
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
        let mut request_json =
            Zeroizing::new(serde_json::to_string(&request).map_err(|_| LauncherError::Protocol)?);
        request_json.push('\n');
        stdin.write_all(request_json.as_bytes()).await?;
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
                session_log_id,
                timezone: None,
                deskside: config.deskside.clone(),
                multi_monitor_plan: None,
            }),
            "rejected" => Err(LauncherError::Rejected),
            _ => Err(LauncherError::Protocol),
        }
    }

    pub async fn open(mut self) -> Result<SessionLauncher, LauncherError> {
        let result = self.open_inner().await;
        if result.is_err() {
            self.shutdown().await;
        }
        result
    }

    async fn open_inner(&mut self) -> Result<SessionLauncher, LauncherError> {
        let stdin = self.stdin.as_mut().ok_or(LauncherError::Protocol)?;
        let request = OpenRequest::new(
            self.timezone.clone(),
            self.deskside.clone(),
            self.multi_monitor_plan.clone(),
        );
        let mut request_json = serde_json::to_vec(&request).map_err(|_| LauncherError::Protocol)?;
        if request_json.len() > MAX_OPEN_REQUEST_BYTES {
            return Err(LauncherError::Protocol);
        }
        request_json.push(b'\n');
        stdin.write_all(&request_json).await?;
        stdin.flush().await?;
        let response = read_response(
            self.stdout.as_mut().ok_or(LauncherError::Protocol)?,
            OPEN_TIMEOUT,
        )
        .await?;
        if response.status != "ready"
            || response.username != self.identity.username
            || response.uid != self.identity.uid
        {
            return Err(open_response_error(response.status.as_str()));
        }
        let environment = SessionEnvironment::build(
            &self.identity,
            &response.display,
            (!response.xauthority.is_empty()).then_some(response.xauthority.as_str()),
            &response.desktop,
            self.timezone.as_ref(),
            [
                ("XDG_SESSION_ID".into(), response.session_id.clone()),
                (
                    "ARCEN_SESSION_LOG_ID".into(),
                    self.session_log_id
                        .lock()
                        .expect("session log id lock")
                        .to_string(),
                ),
            ],
        );
        if self.child.is_none()
            || self.stdin.is_none()
            || self.stderr_task.is_none()
            || self.pam_permit.is_none()
        {
            return Err(LauncherError::Protocol);
        }
        Ok(SessionLauncher {
            identity: self.identity.clone(),
            environment,
            agent_pid: response.agent_pid,
            child: self.child.take().expect("launcher child checked"),
            stdin: self.stdin.take(),
            stderr_task: self.stderr_task.take().expect("stderr task checked"),
            _pam_permit: self.pam_permit.take().expect("PAM permit checked"),
            session_log_id: Arc::clone(&self.session_log_id),
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
    pub agent_pid: u32,
    child: Child,
    stdin: Option<ChildStdin>,
    stderr_task: JoinHandle<()>,
    _pam_permit: OwnedSemaphorePermit,
    session_log_id: Arc<Mutex<CorrelationId>>,
}

impl SessionLauncher {
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

    pub fn agent_is_running(&self) -> bool {
        agent_pid_matches(
            self.agent_pid,
            self.identity.uid,
            self.environment.session_id().unwrap_or_default(),
        )
    }

    pub async fn shutdown(mut self) -> bool {
        close_and_stop_launcher(&mut self.stdin, &mut self.child).await;
        finish_stderr(&mut self.stderr_task).await;
        ensure_agent_exited(self.agent_pid).await
    }
}

impl Drop for SessionLauncher {
    fn drop(&mut self) {
        self.stdin.take();
        let _ = self.child.start_kill();
        self.stderr_task.abort();
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
        tracing::info!(target: target::AUTH, launcher = true, %sid, "{text}");
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
    if !pid_is_live(pid) || !safe_session_id(expected_session_id) {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let expected_uid = expected_uid.to_string();
    let uid_matches = status.lines().find_map(|line| {
        let fields = line
            .strip_prefix("Uid:")?
            .split_whitespace()
            .collect::<Vec<_>>();
        Some(fields.len() == 4 && fields.iter().all(|uid| *uid == expected_uid))
    }) == Some(true);
    let cgroup_matches = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .is_ok_and(|cgroup| cgroup.contains(&format!("session-{expected_session_id}.scope")));
    uid_matches && cgroup_matches
}

#[cfg(not(target_os = "linux"))]
fn agent_pid_matches(_pid: u32, _expected_uid: u32, _expected_session_id: &str) -> bool {
    true
}

#[cfg(target_os = "linux")]
fn signal_process_group(pid: u32, signal: nix::sys::signal::Signal) {
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid as i32), signal);
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
    let session_log_id = CorrelationId::parse_uuid(required_argument(args, "--session-log-id")?)
        .map_err(|_| LauncherError::Protocol)?;

    let stdin = tokio::io::stdin();
    let mut commands = BufReader::new(stdin);
    let auth_line = read_bounded_line(&mut commands, MAX_AUTH_REQUEST_BYTES).await?;
    let auth_line = Zeroizing::new(auth_line);
    let mut auth: OwnedAuthenticateRequest =
        serde_json::from_str(&auth_line).map_err(|_| LauncherError::Protocol)?;
    if auth.command != "authenticate" {
        return Err(LauncherError::Protocol);
    }

    struct SecretConversation {
        username: String,
        password: Zeroizing<String>,
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
        fn text_info(&mut self, _message: &CStr) {}
        fn error_msg(&mut self, _message: &CStr) {}
    }

    let conversation = SecretConversation {
        username: auth.username.clone(),
        password: Zeroizing::new(std::mem::take(&mut auth.password)),
    };
    let mut context = Context::new(&pam_service, Some(&auth.username), conversation)
        .map_err(|_| LauncherError::PamInitialization)?;
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
    if context.authenticate(Flag::NONE).is_err() || context.acct_mgmt(Flag::NONE).is_err() {
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
    let mut commands = commands.lines();

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
        .map_err(|_| LauncherError::PamSession)?;
    let pam_session = match context.open_session(Flag::NONE) {
        Ok(session) => session,
        Err(_) => {
            write_response(&LauncherResponse::error("pam_error"))?;
            return Err(LauncherError::PamSession);
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
    let session_id = pam_environment
        .iter()
        .find_map(|(key, value)| (key == "XDG_SESSION_ID").then_some(value.clone()))
        .ok_or(LauncherError::LogindSession)?;
    let multi_monitor_plan = open
        .multi_monitor
        .clone()
        .map(MultiHeadPlanMsg::into_plan)
        .transpose()?;
    let provider = DedicatedXorgProvider::new(
        &xorg_binary,
        &xorg_config_template,
        &runtime_root,
        &display,
        &gpu_head,
        &session_id,
        &identity,
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
    );
    if environment.validate_runtime(&identity).is_err()
        || validate_logind_ownership(&environment, &identity)
            .await
            .is_err()
    {
        write_response(&LauncherResponse::error("logind_error"))?;
        drop(pam_session);
        return Err(LauncherError::LogindSession);
    }
    activate_logind_session(&environment).await?;
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
    let mut agent = match SessionAgent::spawn(
        &agent_binary,
        &identity,
        &environment,
        &desktop,
        &session_log_id,
    )
    .await
    {
        Ok(agent) => agent,
        Err(error) => {
            if let Some(deskside) = deskside.as_mut() {
                let _ = deskside.restore().await;
            }
            xorg.shutdown().await;
            drop(pam_session);
            return Err(error.into());
        }
    };
    if let Err(error) = write_response(&LauncherResponse::ready(
        &identity,
        &environment,
        &agent.ready,
    )) {
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
        || async { agent.shutdown().await },
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

#[cfg(target_os = "linux")]
struct PreparedDedicatedXorg {
    display_number: u16,
    config_text: String,
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
struct DedicatedXorgBinding {
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
    fn into_output(self) -> DedicatedXorg {
        self.output
    }
}

#[cfg(target_os = "linux")]
struct DedicatedXorgProvider<'a> {
    binary: &'a Path,
    template: &'a Path,
    runtime_root: &'a Path,
    x_display: &'a str,
    gpu_head: &'a str,
    session_id: &'a str,
    identity: &'a UserIdentity,
}

#[cfg(target_os = "linux")]
impl<'a> DedicatedXorgProvider<'a> {
    #[allow(clippy::too_many_arguments)] // Dedicated Xorg's existing launch context.
    fn new(
        binary: &'a Path,
        template: &'a Path,
        runtime_root: &'a Path,
        x_display: &'a str,
        gpu_head: &'a str,
        session_id: &'a str,
        identity: &'a UserIdentity,
    ) -> Self {
        Self {
            binary,
            template,
            runtime_root,
            x_display,
            gpu_head,
            session_id,
            identity,
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
        let config_text = match plan.as_ref() {
            Some(plan) => {
                super::xorg_multihead::render_multi_head_xorg_config(&template_text, plan)
                    .map_err(|_| LauncherError::XorgConfig)?
            }
            None => render_xorg_config(&template_text, self.gpu_head)?,
        };
        Ok(PreparedDedicatedXorg {
            display_number,
            config_text,
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
        binding.output.wait_ready(self.x_display).await?;
        if let Some(plan) = binding.plan.as_ref() {
            binding
                .output
                .verify_applied_topology(self.x_display, plan)
                .await?;
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
        _ => "output_error",
    }
}

#[cfg(target_os = "linux")]
struct DedicatedXorg {
    child: Child,
    display_number: u16,
    session_dir: PathBuf,
    xauthority: PathBuf,
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

        let xauthority = session_dir.join("Xauthority");
        create_xauthority(&xauthority, x_display, identity)?;
        let log_path = session_dir.join("Xorg.log");
        let mut command = Command::new(binary);
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
            .arg("vt1")
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
        })
    }

    fn xauthority(&self) -> &str {
        self.xauthority
            .to_str()
            .expect("session runtime paths must be UTF-8")
    }

    async fn wait_ready(&mut self, display: &str) -> Result<(), LauncherError> {
        for _ in 0..50 {
            if self.child.try_wait()?.is_some() {
                return Err(LauncherError::XorgStart);
            }
            let ready = Command::new("/usr/bin/xdpyinfo")
                .args(["-display", display])
                .env("XAUTHORITY", &self.xauthority)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|status| status.success());
            if ready {
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
            tracing::warn!(
                target: target::SESSION,
                x_display,
                status = ?output.status.code(),
                "xrandr query failed while verifying dedicated Xorg topology"
            );
            return Err(LauncherError::XorgVerify);
        }
        let stdout = String::from_utf8(output.stdout).map_err(|_| LauncherError::XorgVerify)?;
        if let Err(error) = super::randr_verify::verify_applied_topology(&stdout, plan) {
            tracing::warn!(
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

    async fn wait_for_exit(&mut self) -> std::io::Result<std::process::ExitStatus> {
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
    async fn shutdown(&mut self) {
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
    if !metadata.file_type().is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(LauncherError::XorgConfig);
    }
    let mut template = String::new();
    file.read_to_string(&mut template)?;
    Ok(template)
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

#[cfg(target_os = "linux")]
fn parse_dedicated_display(display: &str) -> Result<u16, LauncherError> {
    display
        .strip_prefix(':')
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|number| (1..=99).contains(number))
        .ok_or(LauncherError::XorgConfig)
}

#[cfg(target_os = "linux")]
fn validate_gpu_head(gpu_head: &str) -> Result<(), LauncherError> {
    matches!(gpu_head, "DFP-0" | "DFP-1" | "DFP-2" | "DFP-3")
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
async fn activate_logind_session(environment: &SessionEnvironment) -> Result<(), LauncherError> {
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

fn write_response(response: &LauncherResponse) -> Result<(), LauncherError> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, response).map_err(|_| LauncherError::Protocol)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn open_response_error(status: &str) -> LauncherError {
    match status {
        "logind_error" => LauncherError::LogindSession,
        "pam_error" => LauncherError::PamSession,
        "xorg_config_error" => LauncherError::XorgConfig,
        "xorg_start_error" => LauncherError::XorgStart,
        "xorg_verify_error" => LauncherError::XorgVerify,
        "xorg_release_error" => LauncherError::XorgRelease,
        _ => LauncherError::Protocol,
    }
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

#[cfg(target_os = "linux")]
async fn launcher_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = interrupt.recv() => {}
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
    let _ = observability
        .guard()
        .shutdown(std::time::Duration::from_secs(2));
    exit_code
}

#[cfg(test)]
mod tests {
    use super::*;

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
                std::mem::discriminant(&open_response_error(status)),
                std::mem::discriminant(&error)
            );
        }
    }

    #[test]
    fn open_request_roundtrips_timezone_and_absence() {
        let timezone = IanaTimeZone::parse("Europe/Oslo").unwrap();
        let request = OpenRequest::new(
            Some(timezone.clone()),
            crate::deskside::LinuxDesksideConfig::default(),
            None,
        );
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(parse_open_request(&json).unwrap(), request);

        let request = OpenRequest::new(None, crate::deskside::LinuxDesksideConfig::default(), None);
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(parse_open_request(&json).unwrap(), request);
        assert!(!json.contains("timezone"));
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
        );
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("multi_monitor"));
        assert_eq!(parse_open_request(&json).unwrap(), request);
    }

    #[test]
    fn open_request_omits_multi_monitor_when_absent() {
        let request = OpenRequest::new(None, crate::deskside::LinuxDesksideConfig::default(), None);
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("multi_monitor"));
    }

    #[test]
    fn multi_head_plan_msg_into_plan_rejects_a_zero_generation() {
        let wire = MultiHeadPlanMsg {
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
        assert!(validate_gpu_head("DFP-4").is_err());
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

    #[cfg(target_os = "linux")]
    #[test]
    fn a_display_left_by_a_dead_xorg_is_reclaimed() {
        let root = stale_display_scratch("dead-owner");
        let lock = root.join(".X11-lock");
        let socket = root.join("X11");
        std::fs::write(&lock, format!("{:>10}\n", certainly_dead_pid())).expect("lock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("socket");
        drop(listener);

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
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("socket");
        drop(listener);

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

        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("socket");
        drop(listener);
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
