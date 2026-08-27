//! Reversible NVIDIA NV-CONTROL ViewPortIn resize via `nvidia-settings`.

use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::Instant as TokioInstant;

use crate::logging::target;
use crate::LifecycleEmitter;
use arcen_telemetry::{CorrelationId, FieldValue, LifecycleEventKind, StructuredFields};

const MIN_WIDTH: u32 = 320;
const MIN_HEIGHT: u32 = 240;
const MAX_WIDTH: u32 = 16_384;
const MAX_HEIGHT: u32 = 8_640;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);

struct ReapRequest {
    child: std::process::Child,
    operation: &'static str,
    poll_error_reported: bool,
}

static BLOCKING_REAPER: LazyLock<Result<Sender<ReapRequest>, String>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel::<ReapRequest>();
    std::thread::Builder::new()
        .name("arcen-nvctrl-reaper".to_string())
        .spawn(move || {
            let mut pending = Vec::<ReapRequest>::new();
            let mut disconnected = false;
            loop {
                if pending.is_empty() {
                    if disconnected {
                        return;
                    }
                    match receiver.recv() {
                        Ok(request) => pending.push(request),
                        Err(_) => return,
                    }
                } else if !disconnected {
                    match receiver.recv_timeout(Duration::from_millis(5)) {
                        Ok(request) => pending.push(request),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => disconnected = true,
                    }
                }
                pending.extend(receiver.try_iter());

                let mut index = 0;
                while index < pending.len() {
                    match pending[index].child.try_wait() {
                        Ok(Some(status)) => {
                            let request = pending.swap_remove(index);
                            tracing::debug!(
                                target: target::DISPLAY,
                                operation = request.operation,
                                %status,
                                "reaped timed-out NV-CONTROL helper"
                            );
                        }
                        Ok(None) => index += 1,
                        Err(error) => {
                            let operation = pending[index].operation;
                            let should_report = !pending[index].poll_error_reported;
                            pending[index].poll_error_reported = true;
                            index += 1;
                            if should_report {
                                tracing::warn!(
                                    target: target::DISPLAY,
                                    operation,
                                    %error,
                                    "NV-CONTROL helper reaper poll failed; retaining child"
                                );
                            }
                        }
                    }
                }
                if !pending.is_empty() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        })
        .map(|_| sender)
        .map_err(|error| error.to_string())
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub fn new(width: u32, height: u32) -> Result<Self, DisplayError> {
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&width) || !(MIN_HEIGHT..=MAX_HEIGHT).contains(&height)
        {
            return Err(DisplayError::InvalidResolution(width, height));
        }
        if width % 2 != 0 || height % 2 != 0 {
            return Err(DisplayError::InvalidResolution(width, height));
        }
        Ok(Self { width, height })
    }
}

#[derive(Debug, Error)]
pub enum DisplayError {
    #[error("invalid client resolution {0}x{1}")]
    InvalidResolution(u32, u32),
    #[error("nvidia-settings query returned no MetaMode")]
    MissingMetaMode,
    #[error("cannot parse active output/base mode from MetaMode")]
    InvalidMetaMode,
    #[error("nvidia-settings failed: {0}")]
    Command(String),
    #[error("nvidia-settings {operation} timed out after {timeout_ms}ms")]
    Timeout {
        operation: &'static str,
        timeout_ms: u64,
    },
    #[error("nvidia-settings {operation} could not be reaped after termination")]
    ReapTimeout { operation: &'static str },
    #[error("NV-CONTROL child reaper is unavailable: {0}")]
    ReaperUnavailable(String),
    #[error("nvidia-settings I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct NvControl {
    display: String,
    xauthority: Option<String>,
    binary: PathBuf,
    command_timeout: Duration,
    reap_timeout: Duration,
    #[cfg(test)]
    pid_observer: Option<Arc<Mutex<Vec<u32>>>>,
}

impl NvControl {
    pub fn new(display: String, xauthority: Option<String>) -> Self {
        Self {
            display,
            xauthority,
            binary: PathBuf::from("nvidia-settings"),
            command_timeout: COMMAND_TIMEOUT,
            reap_timeout: REAP_TIMEOUT,
            #[cfg(test)]
            pid_observer: None,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .env("DISPLAY", &self.display)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(xauthority) = &self.xauthority {
            command.env("XAUTHORITY", xauthority);
        }
        command
    }

    fn command_blocking(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.binary);
        command
            .env("DISPLAY", &self.display)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(xauthority) = &self.xauthority {
            command.env("XAUTHORITY", xauthority);
        }
        command
    }

    pub async fn snapshot(&self) -> Result<String, DisplayError> {
        let output = self
            .run_async("snapshot", &["--query", "CurrentMetaMode", "--terse"])
            .await?;
        if !output.status.success() {
            return Err(DisplayError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        parse_metamode_query(&String::from_utf8_lossy(&output.stdout))
    }

    pub async fn assign(&self, metamode: &str) -> Result<(), DisplayError> {
        let assignment = format!("CurrentMetaMode={metamode}");
        let output = self.run_async("assign", &["--assign", &assignment]).await?;
        if !output.status.success() {
            return Err(DisplayError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    fn assign_blocking(&self, metamode: &str) -> Result<(), DisplayError> {
        let assignment = format!("CurrentMetaMode={metamode}");
        let output = self.run_blocking("drop restore", &["--assign", &assignment])?;
        if !output.status.success() {
            return Err(DisplayError::Command(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    async fn run_async(
        &self,
        operation: &'static str,
        args: &[&str],
    ) -> Result<Output, DisplayError> {
        let deadline = TokioInstant::now() + self.command_timeout;
        let mut command = self.command();
        command.args(args);
        let mut child = command.spawn()?;
        self.observe_pid(child.id());
        let stdout = child.stdout.take().expect("nvidia-settings stdout piped");
        let stderr = child.stderr.take().expect("nvidia-settings stderr piped");
        let mut stdout_task = tokio::spawn(read_pipe(stdout));
        let mut stderr_task = tokio::spawn(read_pipe(stderr));

        let status = match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(error)) => {
                let cleanup = kill_and_reap_async(&mut child, self.reap_timeout, operation).await;
                abort_pipe_tasks(&mut stdout_task, &mut stderr_task).await;
                cleanup?;
                return Err(DisplayError::Io(error));
            }
            Err(_) => {
                let cleanup = kill_and_reap_async(&mut child, self.reap_timeout, operation).await;
                abort_pipe_tasks(&mut stdout_task, &mut stderr_task).await;
                cleanup?;
                return Err(DisplayError::Timeout {
                    operation,
                    timeout_ms: duration_ms(self.command_timeout),
                });
            }
        };
        let stdout = match finish_pipe_task(&mut stdout_task, deadline).await {
            Ok(stdout) => stdout,
            Err(PipeTaskError::Deadline) => {
                abort_pipe_tasks(&mut stdout_task, &mut stderr_task).await;
                return Err(PipeTaskError::Deadline.with_operation(operation, self.command_timeout));
            }
            Err(error) => {
                stderr_task.abort();
                let _ = stderr_task.await;
                return Err(error.with_operation(operation, self.command_timeout));
            }
        };
        let stderr = match finish_pipe_task(&mut stderr_task, deadline).await {
            Ok(stderr) => stderr,
            Err(PipeTaskError::Deadline) => {
                stderr_task.abort();
                let _ = stderr_task.await;
                return Err(PipeTaskError::Deadline.with_operation(operation, self.command_timeout));
            }
            Err(error) => return Err(error.with_operation(operation, self.command_timeout)),
        };
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    fn run_blocking(&self, operation: &'static str, args: &[&str]) -> Result<Output, DisplayError> {
        let reaper = blocking_reaper()?.clone();
        let mut command = self.command_blocking();
        command.args(args);
        let mut child = command.spawn()?;
        self.observe_pid(Some(child.id()));
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    kill_and_reap_blocking(child, self.reap_timeout, operation, &reaper)?;
                    return Err(DisplayError::Io(error));
                }
            }
            if started.elapsed() >= self.command_timeout {
                kill_and_reap_blocking(child, self.reap_timeout, operation, &reaper)?;
                return Err(DisplayError::Timeout {
                    operation,
                    timeout_ms: duration_ms(self.command_timeout),
                });
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        Ok(Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    #[cfg(test)]
    fn observe_pid(&self, pid: Option<u32>) {
        if let (Some(observer), Some(pid)) = (&self.pid_observer, pid) {
            observer.lock().unwrap().push(pid);
        }
    }

    #[cfg(not(test))]
    fn observe_pid(&self, _pid: Option<u32>) {}
}

async fn kill_and_reap_async(
    child: &mut tokio::process::Child,
    timeout: Duration,
    operation: &'static str,
) -> Result<(), DisplayError> {
    let _ = child.start_kill();
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(DisplayError::Io(error)),
        Err(_) => Err(DisplayError::ReapTimeout { operation }),
    }
}

fn kill_and_reap_blocking(
    mut child: std::process::Child,
    timeout: Duration,
    operation: &'static str,
    reaper: &Sender<ReapRequest>,
) -> Result<(), DisplayError> {
    if let Err(error) = child.kill() {
        handoff_child_to_reaper(child, operation, reaper);
        return Err(DisplayError::Io(error));
    }
    if timeout.is_zero() {
        handoff_child_to_reaper(child, operation, reaper);
        return Err(DisplayError::ReapTimeout { operation });
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => {
                handoff_child_to_reaper(child, operation, reaper);
                return Err(DisplayError::Io(error));
            }
        }
        if started.elapsed() >= timeout {
            handoff_child_to_reaper(child, operation, reaper);
            return Err(DisplayError::ReapTimeout { operation });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn blocking_reaper() -> Result<&'static Sender<ReapRequest>, DisplayError> {
    BLOCKING_REAPER
        .as_ref()
        .map_err(|error| DisplayError::ReaperUnavailable(error.clone()))
}

fn handoff_child_to_reaper(
    child: std::process::Child,
    operation: &'static str,
    reaper: &Sender<ReapRequest>,
) {
    reaper
        .send(ReapRequest {
            child,
            operation,
            poll_error_reported: false,
        })
        .expect("process-wide NV-CONTROL reaper remains connected");
}

impl Drop for MetaModeGuard {
    fn drop(&mut self) {
        self.dispatch_restore(|state| {
            std::thread::Builder::new()
                .name("arcen-display-restore".to_string())
                .spawn(move || MetaModeGuard::run_restore_job(state))
                .map(|_| ())
        });
    }
}

pub struct MetaModeGuard {
    controller: NvControl,
    prior: String,
    resolution: Resolution,
    changed: bool,
    restore_hold: Option<Box<dyn Send>>,
    emitter: LifecycleEmitter,
    correlation_id: CorrelationId,
}

struct RestoreJob {
    controller: NvControl,
    prior: String,
    restore_hold: Option<Box<dyn Send>>,
    emitter: LifecycleEmitter,
    correlation_id: CorrelationId,
}

/// Emits `DISPLAY_ARMED` (1200) only when the transaction actually mutated
/// the active MetaMode; an already-satisfied resize arms no restore
/// obligation and emits nothing.
fn emit_display_armed(
    emitter: &LifecycleEmitter,
    correlation_id: CorrelationId,
    resolution: Resolution,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("display_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert(
        "policy",
        FieldValue::String("hold_session_permit".to_string()),
    );
    let _ = fields.insert("changed", FieldValue::Boolean(true));
    let _ = fields.insert("width", FieldValue::Integer(i64::from(resolution.width)));
    let _ = fields.insert("height", FieldValue::Integer(i64::from(resolution.height)));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayArmed,
        correlation_id,
        fields,
    );
}

/// Emits `DISPLAY_RESTORED` (1201) after a verified explicit restore.
fn emit_display_restored(emitter: &LifecycleEmitter, correlation_id: CorrelationId) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("restore_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert("changed", FieldValue::Boolean(true));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayRestored,
        correlation_id,
        fields,
    );
}

/// Emits `DISPLAY_RESTORE_FAILED` (1203) for an explicit restore failure or a
/// Drop-triggered fallback failure. `stage` distinguishes the two; Linux has
/// no separate recovery journal, so `journal_pending` reflects only whether
/// this guard will still retry on `Drop` (`stage == "explicit_restore"`) or
/// has already exhausted its only fallback (`stage == "drop_fallback"`).
fn emit_display_restore_failed(
    emitter: &LifecycleEmitter,
    correlation_id: CorrelationId,
    stage: &'static str,
    journal_pending: bool,
) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("restore_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert("stage", FieldValue::String(stage.to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("restore_verification_failed".to_string()),
    );
    let _ = fields.insert("journal_pending", FieldValue::Boolean(journal_pending));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::DisplayRestoreFailed,
        correlation_id,
        fields,
    );
}

/// Emits `WATCHDOG_RESTORE` (1204) once the `Drop`-triggered fallback path
/// (this crate's substitute for a separate watchdog process) has restored
/// display state.
fn emit_watchdog_restore(emitter: &LifecycleEmitter, correlation_id: CorrelationId) {
    let mut fields = StructuredFields::default();
    let _ = fields.insert("restore_backend", FieldValue::String("nvctrl".to_string()));
    let _ = fields.insert(
        "reason_class",
        FieldValue::String("drop_fallback_restore".to_string()),
    );
    let _ = fields.insert("journal_pending", FieldValue::Boolean(false));
    crate::emit_lifecycle_event(
        emitter,
        LifecycleEventKind::WatchdogRestore,
        correlation_id,
        fields,
    );
}

impl MetaModeGuard {
    fn dispatch_restore(
        &mut self,
        spawn_worker: impl FnOnce(Arc<Mutex<Option<RestoreJob>>>) -> std::io::Result<()>,
    ) {
        if !self.changed {
            return;
        }

        let state = Arc::new(Mutex::new(Some(RestoreJob {
            controller: self.controller.clone(),
            prior: self.prior.clone(),
            restore_hold: self.restore_hold.take(),
            emitter: self.emitter.clone(),
            correlation_id: self.correlation_id.clone(),
        })));
        match spawn_worker(state.clone()) {
            Ok(()) => self.changed = false,
            Err(error) => {
                tracing::error!(
                    target: target::DISPLAY,
                    %error,
                    prior = %self.prior,
                    "DROP GUARD restore worker spawn failed; using bounded synchronous fallback"
                );
                Self::run_restore_job(state);
                self.changed = false;
            }
        }
    }

    pub(crate) async fn apply(
        controller: NvControl,
        resolution: Resolution,
        emitter: LifecycleEmitter,
        correlation_id: CorrelationId,
    ) -> Result<Self, DisplayError> {
        Self::apply_inner(controller, resolution, None, emitter, correlation_id).await
    }

    fn run_restore_job(state: Arc<Mutex<Option<RestoreJob>>>) {
        let job = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(job) = job else {
            return;
        };
        let RestoreJob {
            controller,
            prior,
            restore_hold: _restore_hold,
            emitter,
            correlation_id,
        } = job;
        match controller.assign_blocking(&prior) {
            Ok(()) => {
                tracing::info!(
                    target: target::DISPLAY,
                    restored = %prior,
                    "NV-CONTROL MetaMode restored by drop guard"
                );
                emit_watchdog_restore(&emitter, correlation_id);
            }
            Err(error) => {
                tracing::error!(
                    target: target::DISPLAY,
                    %error,
                    prior = %prior,
                    "DROP GUARD FAILED to restore prior NV-CONTROL MetaMode"
                );
                emit_display_restore_failed(&emitter, correlation_id, "drop_fallback", false);
            }
        }
    }

    pub(crate) async fn apply_with_hold<T: Send + 'static>(
        controller: NvControl,
        resolution: Resolution,
        hold: T,
        emitter: LifecycleEmitter,
        correlation_id: CorrelationId,
    ) -> Result<Self, DisplayError> {
        Self::apply_inner(
            controller,
            resolution,
            Some(Box::new(hold)),
            emitter,
            correlation_id,
        )
        .await
    }

    async fn apply_inner(
        controller: NvControl,
        resolution: Resolution,
        restore_hold: Option<Box<dyn Send>>,
        emitter: LifecycleEmitter,
        correlation_id: CorrelationId,
    ) -> Result<Self, DisplayError> {
        blocking_reaper()?;
        let prior = controller.snapshot().await?;
        let requested = build_viewport_metamode(&prior, resolution)?;
        let changed = requested != prior;
        let guard = Self {
            controller,
            prior,
            resolution,
            changed,
            restore_hold,
            emitter,
            correlation_id,
        };
        if changed {
            guard.controller.assign(&requested).await?;
            tracing::info!(
                target: target::DISPLAY,
                width = resolution.width,
                height = resolution.height,
                prior = %guard.prior,
                active = %requested,
                "NV-CONTROL ViewPortIn applied"
            );
            emit_display_armed(&guard.emitter, guard.correlation_id.clone(), resolution);
        }
        Ok(guard)
    }

    /// Re-target the active MetaMode to a new resolution mid-session.
    ///
    /// Builds from the original pre-session `prior` snapshot, so the restore
    /// target (explicit or `Drop`-fallback) never moves: disconnect always
    /// returns the desktop to its pre-session MetaMode no matter how many
    /// resizes happened in between. The restore obligation stays armed —
    /// the canonical rebuilt MetaMode string never byte-matches the raw
    /// query snapshot, and a redundant restore to the same raster is
    /// harmless.
    pub async fn reassign(&mut self, resolution: Resolution) -> Result<(), DisplayError> {
        let requested = build_viewport_metamode(&self.prior, resolution)?;
        self.controller.assign(&requested).await?;
        self.resolution = resolution;
        self.changed = true;
        tracing::info!(
            target: target::DISPLAY,
            width = resolution.width,
            height = resolution.height,
            prior = %self.prior,
            active = %requested,
            "NV-CONTROL ViewPortIn reassigned mid-session"
        );
        emit_display_armed(&self.emitter, self.correlation_id.clone(), resolution);
        Ok(())
    }

    /// Returns the physical raster (X11 virtual screen) dimensions by parsing
    /// `ViewPortOut=` from the pre-session MetaMode snapshot.  Returns `None`
    /// only when the prior MetaMode is an unusual format that contains neither
    /// `ViewPortOut=` nor an `@WxH` token.
    pub fn raster_size(&self) -> Option<(u32, u32)> {
        self.prior
            .split_once("ViewPortOut=")
            .and_then(|(_, v)| parse_size_prefix(v))
            .or_else(|| {
                self.prior
                    .split_once('@')
                    .and_then(|(_, v)| parse_size_prefix(v))
            })
    }

    pub fn resolution(&self) -> Resolution {
        self.resolution
    }

    pub async fn restore(&mut self) -> Result<(), DisplayError> {
        if !self.changed {
            return Ok(());
        }
        match self.controller.assign(&self.prior).await {
            Ok(()) => {
                tracing::info!(
                    target: target::DISPLAY,
                    restored = %self.prior,
                    "NV-CONTROL MetaMode restored"
                );
                self.changed = false;
                emit_display_restored(&self.emitter, self.correlation_id.clone());
                Ok(())
            }
            Err(error) => {
                // `self.changed` stays `true`: the `Drop` fallback will
                // still attempt to restore this guard's prior MetaMode.
                emit_display_restore_failed(
                    &self.emitter,
                    self.correlation_id.clone(),
                    "explicit_restore",
                    true,
                );
                Err(error)
            }
        }
    }
}

async fn read_pipe<R: AsyncRead + Unpin>(mut pipe: R) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

enum PipeTaskError {
    Io(std::io::Error),
    Deadline,
}

impl PipeTaskError {
    fn with_operation(self, operation: &'static str, timeout: Duration) -> DisplayError {
        match self {
            Self::Io(error) => DisplayError::Io(error),
            Self::Deadline => DisplayError::Timeout {
                operation,
                timeout_ms: duration_ms(timeout),
            },
        }
    }
}

async fn finish_pipe_task(
    task: &mut JoinHandle<std::io::Result<Vec<u8>>>,
    deadline: TokioInstant,
) -> Result<Vec<u8>, PipeTaskError> {
    match tokio::time::timeout_at(deadline, &mut *task).await {
        Ok(Ok(Ok(bytes))) => Ok(bytes),
        Ok(Ok(Err(error))) => Err(PipeTaskError::Io(error)),
        Ok(Err(error)) => Err(PipeTaskError::Io(std::io::Error::other(error))),
        Err(_) => Err(PipeTaskError::Deadline),
    }
}

async fn abort_pipe_tasks(
    stdout: &mut JoinHandle<std::io::Result<Vec<u8>>>,
    stderr: &mut JoinHandle<std::io::Result<Vec<u8>>>,
) {
    stdout.abort();
    stderr.abort();
    let _ = stdout.await;
    let _ = stderr.await;
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

pub fn requested_resolution(width: u32, height: u32) -> Result<Option<Resolution>, DisplayError> {
    match (width, height) {
        (0, 0) => Ok(None),
        (width, height) => {
            let mut resolution = Resolution::new(width, height)?;
            resolution.width &= !3;
            Ok(Some(resolution))
        }
    }
}

fn parse_metamode_query(output: &str) -> Result<String, DisplayError> {
    output
        .split_once("::")
        .map(|(_, metamode)| metamode.trim().to_string())
        .filter(|metamode| !metamode.is_empty())
        .ok_or(DisplayError::MissingMetaMode)
}

fn build_viewport_metamode(prior: &str, resolution: Resolution) -> Result<String, DisplayError> {
    let (output, mode) = prior.split_once(':').ok_or(DisplayError::InvalidMetaMode)?;
    let base_mode = mode
        .split_whitespace()
        .next()
        .filter(|mode| {
            *mode == "nvidia-auto-select"
                || mode.split_once('x').is_some_and(|(width, height)| {
                    width.parse::<u32>().is_ok() && height.parse::<u32>().is_ok()
                })
        })
        .ok_or(DisplayError::InvalidMetaMode)?;
    let (raster_width, raster_height) =
        metamode_raster_size(prior, base_mode).ok_or(DisplayError::InvalidMetaMode)?;
    Ok(format!(
        "{}: {} {{ ViewPortIn={}x{}, ViewPortOut={}x{}+0+0 }}",
        output.trim(),
        base_mode,
        resolution.width,
        resolution.height,
        raster_width,
        raster_height
    ))
}

fn metamode_raster_size(prior: &str, base_mode: &str) -> Option<(u32, u32)> {
    prior
        .split_once("ViewPortOut=")
        .and_then(|(_, value)| parse_size_prefix(value))
        .or_else(|| {
            prior
                .split_once('@')
                .and_then(|(_, value)| parse_size_prefix(value))
        })
        .or_else(|| parse_size_prefix(base_mode))
}

fn parse_size_prefix(value: &str) -> Option<(u32, u32)> {
    let token = value
        .trim_start()
        .split(|character: char| !character.is_ascii_digit() && character != 'x')
        .next()?;
    let (width, height) = token.split_once('x')?;
    let width = width.parse::<u32>().ok()?;
    let height = height.parse::<u32>().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    #[cfg(unix)]
    use std::sync::Arc;

    #[cfg(unix)]
    static ARTIFACT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_correlation_id() -> CorrelationId {
        CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
            .expect("canonical correlation id")
    }

    const QUERY: &str = "id=50, switchable=no, source=nv-control :: DPY-0: 3840x2160 \
                         @3840x2160 +0+0 {ViewPortIn=3840x2160, \
                         ViewPortOut=3840x2160+0+0}\n";
    const AUTO_QUERY: &str = "id=50, switchable=yes, source=xconfig :: \
                             DPY-1: nvidia-auto-select @2560x1600 +0+0 \
                             {ViewPortIn=2560x1600, ViewPortOut=2560x1600+0+0}\n";

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    fn wait_for_file(path: &std::path::Path) {
        let started = Instant::now();
        while !path.is_file() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(path.is_file(), "timed out waiting for {}", path.display());
    }

    #[cfg(unix)]
    fn wait_for_pids(observer: &Arc<Mutex<Vec<u32>>>, minimum: usize) {
        let started = Instant::now();
        while observer.lock().unwrap().len() < minimum && started.elapsed() < Duration::from_secs(2)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(observer.lock().unwrap().len() >= minimum);
    }

    #[cfg(unix)]
    fn test_directory(name: &str) -> PathBuf {
        let sequence = ARTIFACT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/arcen-test-artifacts")
            .join(format!("metamode-{name}-{}-{sequence}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    /// Write an executable stub and wait until the kernel will actually run it.
    ///
    /// Writing a file and immediately exec'ing it races with `fork` in a
    /// multi-threaded process: any thread that forks while the write descriptor
    /// is still open leaves the child holding a copy, and the kernel answers
    /// the exec with ETXTBSY (26). The whole suite runs in one such process, so
    /// this surfaced as `explicit_restore_emits_display_restored_on_success`
    /// failing under load while passing every time in isolation — the most
    /// expensive kind of test failure, because it looks like the change under
    /// review.
    ///
    /// Probing with `spawn` rather than `output` matters: several stubs
    /// `exec sleep 999`, so waiting for one to finish would hang the suite.
    /// A successful `spawn` already proves the exec went through.
    #[cfg(unix)]
    fn write_executable_stub(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
        settle_executable(path);
    }

    /// Make `path` executable and wait until the kernel will actually run it.
    ///
    /// Probing with `spawn` rather than `output` matters: several stubs
    /// `exec sleep 999`, so waiting for one to finish would hang the suite.
    /// A successful `spawn` already proves the exec went through. The stubs
    /// that redirect to a file all truncate rather than append, so an extra
    /// no-argument invocation cannot corrupt what a test later asserts on.
    #[cfg(unix)]
    fn settle_executable(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();

        const ETXTBSY: i32 = 26;
        for _ in 0..50 {
            match std::process::Command::new(path).spawn() {
                Ok(mut child) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Err(error) if error.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // Anything else is not this race; let the test itself report it.
                Err(_) => return,
            }
        }
    }

    #[test]
    fn validates_even_client_resolution_bounds() {
        assert_eq!(
            Resolution::new(3600, 2338).unwrap(),
            Resolution {
                width: 3600,
                height: 2338
            }
        );
        for (width, height) in [
            (0, 0),
            (319, 240),
            (1921, 1080),
            (1920, 1081),
            (20_000, 1080),
        ] {
            assert!(Resolution::new(width, height).is_err());
        }
    }

    #[test]
    fn zero_pair_means_no_resize_but_partial_zero_is_invalid() {
        assert_eq!(requested_resolution(0, 0).unwrap(), None);
        assert!(requested_resolution(1920, 0).is_err());
    }

    #[test]
    fn requested_resolution_aligns_width_for_yuv444_capture() {
        assert_eq!(
            requested_resolution(1366, 768).unwrap(),
            Some(Resolution {
                width: 1364,
                height: 768,
            })
        );
        assert_eq!(
            requested_resolution(1398, 760).unwrap(),
            Some(Resolution {
                width: 1396,
                height: 760,
            })
        );
    }

    #[test]
    fn parses_exact_prior_metamode_from_terse_query() {
        assert_eq!(
            parse_metamode_query(QUERY).unwrap(),
            "DPY-0: 3840x2160 @3840x2160 +0+0 {ViewPortIn=3840x2160, \
             ViewPortOut=3840x2160+0+0}"
        );
    }

    #[test]
    fn builds_viewportin_from_active_output_and_base_mode() {
        let prior = parse_metamode_query(QUERY).unwrap();
        assert_eq!(
            build_viewport_metamode(&prior, Resolution::new(3600, 2338).unwrap()).unwrap(),
            "DPY-0: 3840x2160 { ViewPortIn=3600x2338, ViewPortOut=3840x2160+0+0 }"
        );
    }

    #[test]
    fn fresh_xorg_auto_select_metamode_can_be_resized() {
        let prior = parse_metamode_query(AUTO_QUERY).unwrap();
        assert_eq!(
            build_viewport_metamode(&prior, Resolution::new(3600, 2338).unwrap()).unwrap(),
            "DPY-1: nvidia-auto-select { ViewPortIn=3600x2338, ViewPortOut=2560x1600+0+0 }"
        );
    }

    #[test]
    fn regression_a_three_head_metamode_string_would_collapse_to_one_head_if_ever_passed_through() {
        // This documents exactly why `run_ws` must skip the legacy
        // single-viewport resize entirely for a committed multi-monitor
        // session (`skip_metamode_resize_for_multi_monitor` in
        // `net::server`), rather than ever calling
        // `MetaModeGuard::apply_with_hold`/`build_viewport_metamode` with a
        // dedicated multi-head Xorg display's live MetaModes snapshot:
        // `build_viewport_metamode` only ever understands and preserves a
        // *single* output clause. Fed the full three-head MetaModes string
        // a real `DedicatedXorg` session's own NV-CONTROL query would
        // return, it silently drops the second and third heads instead of
        // rejecting the extra clauses outright.
        let three_head_prior = "DFP-0: 1920x1080 +0+0 {ViewPortIn=1920x1080, \
                                 ViewPortOut=1920x1080+0+0}, \
                                 DFP-1: 1920x1080 +1920+0 {ViewPortIn=1920x1080, \
                                 ViewPortOut=1920x1080+0+0}, \
                                 DFP-2: 1920x1080 +3840+0 {ViewPortIn=1920x1080, \
                                 ViewPortOut=1920x1080+0+0}";
        let corrupted =
            build_viewport_metamode(three_head_prior, Resolution::new(1920, 1080).unwrap())
                .unwrap();
        assert_eq!(
            corrupted,
            "DFP-0: 1920x1080 { ViewPortIn=1920x1080, ViewPortOut=1920x1080+0+0 }"
        );
        assert!(
            !corrupted.contains("DFP-1") && !corrupted.contains("DFP-2"),
            "a real assign() with this string would silently drop 2 of the 3 applied heads"
        );
    }

    #[test]
    fn raster_size_prefers_explicit_viewportout_then_at_mode_then_base_mode() {
        assert_eq!(
            metamode_raster_size("DPY-1: auto @1920x1080 {ViewPortOut=2560x1600+0+0}", "auto"),
            Some((2560, 1600))
        );
        assert_eq!(
            metamode_raster_size("DPY-1: auto @1920x1080 +0+0", "auto"),
            Some((1920, 1080))
        );
        assert_eq!(
            metamode_raster_size("DPY-1: 3840x2160 +0+0", "3840x2160"),
            Some((3840, 2160))
        );
    }

    #[cfg(unix)]
    #[test]
    fn armed_drop_guard_restores_prior_metamode() {
        use std::fs;

        let directory = test_directory("drop");
        let script = directory.join("nvidia-settings");
        let output = directory.join("args");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", output.display()),
        )
        .unwrap();
        settle_executable(&script);
        {
            let controller = NvControl {
                display: ":0".to_string(),
                xauthority: None,
                binary: script,
                command_timeout: Duration::from_secs(3),
                reap_timeout: Duration::from_secs(1),
                pid_observer: None,
            };
            let _guard = MetaModeGuard {
                controller,
                prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
                resolution: Resolution {
                    width: 3840,
                    height: 2160,
                },
                changed: true,
                restore_hold: None,
                emitter: LifecycleEmitter::disabled(),
                correlation_id: test_correlation_id(),
            };
        }
        wait_for_file(&output);
        let args = fs::read_to_string(output).unwrap();
        assert!(args.contains("--assign CurrentMetaMode=DPY-0: 3840x2160 @3840x2160 +0+0"));
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn apply_with_hold_emits_display_armed_only_when_the_metamode_changes() {
        use std::fs;

        let directory = test_directory("apply-armed");
        let script = directory.join("nvidia-settings");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *--query*) printf '%s' \"{}\" ;;\n  *) exit 0 ;;\nesac\n",
                QUERY
            ),
        )
        .unwrap();
        settle_executable(&script);
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_secs(3),
            reap_timeout: Duration::from_secs(1),
            pid_observer: None,
        };
        let (emitter, recorded) = LifecycleEmitter::recording();
        let resolution = Resolution::new(3600, 2338).unwrap();

        let guard = MetaModeGuard::apply(controller, resolution, emitter, test_correlation_id())
            .await
            .expect("apply succeeds against the fake nvidia-settings script");
        assert!(guard.changed);

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::DisplayArmed
        );
        assert_eq!(
            recorded[0].fields().as_map().get("changed"),
            Some(&FieldValue::Boolean(true))
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reassign_preserves_the_original_restore_target() {
        use std::fs;

        let directory = test_directory("reassign");
        let script = directory.join("nvidia-settings");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *--query*) printf '%s' \"{}\" ;;\n  *) exit 0 ;;\nesac\n",
                QUERY
            ),
        )
        .unwrap();
        settle_executable(&script);
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_secs(3),
            reap_timeout: Duration::from_secs(1),
            pid_observer: None,
        };
        let (emitter, _recorded) = LifecycleEmitter::recording();

        let mut guard = MetaModeGuard::apply(
            controller,
            Resolution::new(1512, 982).unwrap(),
            emitter,
            test_correlation_id(),
        )
        .await
        .expect("apply succeeds against the fake nvidia-settings script");
        let original_prior = guard.prior.clone();
        assert!(guard.changed);
        assert_eq!(guard.resolution(), Resolution::new(1512, 982).unwrap());

        // A second resize must keep the pre-session restore target.
        guard
            .reassign(Resolution::new(1512, 944).unwrap())
            .await
            .expect("reassign succeeds");
        assert_eq!(guard.prior, original_prior, "restore target must not move");
        assert!(guard.changed, "restore obligation persists after reassign");
        assert_eq!(guard.resolution(), Resolution::new(1512, 944).unwrap());

        // Even resizing back to the pre-session raster keeps the obligation
        // armed: the canonical rebuilt string never matches the raw query
        // snapshot, and a redundant same-raster restore is harmless.
        guard
            .reassign(Resolution::new(3840, 2160).unwrap())
            .await
            .expect("reassign back to prior succeeds");
        assert_eq!(guard.prior, original_prior);
        assert!(guard.changed, "restore obligation stays armed");
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_restore_emits_display_restored_on_success() {
        use std::fs;

        let directory = test_directory("restore-success");
        let script = directory.join("nvidia-settings");
        write_executable_stub(&script, "#!/bin/sh\nexit 0\n");
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_secs(3),
            reap_timeout: Duration::from_secs(1),
            pid_observer: None,
        };
        let (recording_emitter, recorded) = LifecycleEmitter::recording();
        let mut guard = MetaModeGuard {
            controller,
            prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
            changed: true,
            restore_hold: None,
            emitter: recording_emitter,
            correlation_id: test_correlation_id(),
        };

        guard.restore().await.expect("restore succeeds");
        assert!(!guard.changed);

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::DisplayRestored
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_restore_emits_display_restore_failed_and_leaves_the_guard_armed() {
        use std::fs;

        let directory = test_directory("restore-failure");
        let script = directory.join("nvidia-settings");
        write_executable_stub(&script, "#!/bin/sh\nexit 1\n");
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_secs(3),
            reap_timeout: Duration::from_secs(1),
            pid_observer: None,
        };
        let (recording_emitter, recorded) = LifecycleEmitter::recording();
        let mut guard = MetaModeGuard {
            controller,
            prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
            changed: true,
            restore_hold: None,
            emitter: recording_emitter,
            correlation_id: test_correlation_id(),
        };

        assert!(guard.restore().await.is_err());
        // The guard stays armed so `Drop`'s fallback still retries.
        assert!(guard.changed);

        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::DisplayRestoreFailed
        );
        assert_eq!(
            recorded[0].fields().as_map().get("journal_pending"),
            Some(&FieldValue::Boolean(true))
        );
        guard.changed = false; // avoid a real Drop-triggered background retry in this test
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn drop_fallback_restore_success_emits_watchdog_restore() {
        use std::fs;

        let directory = test_directory("drop-watchdog");
        let script = directory.join("nvidia-settings");
        let output = directory.join("args");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", output.display()),
        )
        .unwrap();
        settle_executable(&script);
        let (recording_emitter, recorded) = LifecycleEmitter::recording();
        {
            let controller = NvControl {
                display: ":0".to_string(),
                xauthority: None,
                binary: script,
                command_timeout: Duration::from_secs(3),
                reap_timeout: Duration::from_secs(1),
                pid_observer: None,
            };
            let _guard = MetaModeGuard {
                controller,
                prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
                resolution: Resolution {
                    width: 3840,
                    height: 2160,
                },
                changed: true,
                restore_hold: None,
                emitter: recording_emitter,
                correlation_id: test_correlation_id(),
            };
        }
        wait_for_file(&output);
        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::eventlog::test_support::recorded_events(&recorded).is_empty()
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        let recorded = crate::eventlog::test_support::recorded_events(&recorded);
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].kind(),
            arcen_telemetry::LifecycleEventKind::WatchdogRestore
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_and_assign_commands_have_hard_deadlines_and_are_reaped() {
        use std::fs;

        let directory = test_directory("timeout");
        let script = directory.join("nvidia-settings");
        write_executable_stub(&script, "#!/bin/sh\nexec sleep 999\n");
        let observer = Arc::new(Mutex::new(Vec::new()));
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_millis(100),
            reap_timeout: Duration::from_secs(1),
            pid_observer: Some(observer.clone()),
        };

        assert!(matches!(
            controller.snapshot().await,
            Err(DisplayError::Timeout {
                operation: "snapshot",
                ..
            })
        ));
        assert!(matches!(
            controller.assign("DPY-0: 3840x2160").await,
            Err(DisplayError::Timeout {
                operation: "assign",
                ..
            })
        ));
        let captured_pids = observer.lock().unwrap().clone();
        assert_eq!(captured_pids.len(), 2);
        assert!(captured_pids.into_iter().all(|pid| !process_exists(pid)));
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_restore_has_a_hard_deadline_and_reaps() {
        use std::fs;

        let directory = test_directory("restore-timeout");
        let script = directory.join("nvidia-settings");
        write_executable_stub(&script, "#!/bin/sh\nexec sleep 999\n");
        let observer = Arc::new(Mutex::new(Vec::new()));
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_millis(100),
            reap_timeout: Duration::from_secs(1),
            pid_observer: Some(observer.clone()),
        };
        let mut guard = MetaModeGuard {
            controller,
            prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
            changed: true,
            restore_hold: None,
            emitter: LifecycleEmitter::disabled(),
            correlation_id: test_correlation_id(),
        };

        assert!(matches!(
            guard.restore().await,
            Err(DisplayError::Timeout {
                operation: "assign",
                ..
            })
        ));
        guard.changed = false;
        let pid = observer.lock().unwrap()[0];
        assert!(!process_exists(pid));
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn drop_restore_worker_is_nonblocking_and_bounded() {
        use std::fs;

        let directory = test_directory("drop-timeout");
        let script = directory.join("nvidia-settings");
        write_executable_stub(&script, "#!/bin/sh\nexec sleep 999\n");
        let observer = Arc::new(Mutex::new(Vec::new()));
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            command_timeout: Duration::from_millis(100),
            reap_timeout: Duration::from_secs(1),
            pid_observer: Some(observer.clone()),
        };
        struct RestoreHold(Arc<AtomicBool>);
        impl Drop for RestoreHold {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let hold_released = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let mut guard = MetaModeGuard {
            controller,
            prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
            changed: true,
            restore_hold: None,
            emitter: LifecycleEmitter::disabled(),
            correlation_id: test_correlation_id(),
        };
        guard.restore_hold = Some(Box::new(RestoreHold(hold_released.clone())));
        drop(guard);
        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(!hold_released.load(Ordering::SeqCst));
        tokio::time::timeout(
            Duration::from_millis(100),
            tokio::time::sleep(Duration::from_millis(1)),
        )
        .await
        .unwrap();
        wait_for_pids(&observer, 1);
        let pid = observer.lock().unwrap()[0];
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!process_exists(pid));
        let release_deadline = Instant::now() + Duration::from_secs(2);
        while !hold_released.load(Ordering::SeqCst) && Instant::now() < release_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(hold_released.load(Ordering::SeqCst));
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn restore_spawn_failure_uses_bounded_synchronous_fallback() {
        use std::fs;

        let directory = test_directory("drop-spawn-failure");
        let script = directory.join("nvidia-settings");
        let output = directory.join("args");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", output.display()),
        )
        .unwrap();
        settle_executable(&script);
        struct RestoreHold(Arc<AtomicBool>);
        impl Drop for RestoreHold {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let hold_released = Arc::new(AtomicBool::new(false));
        let controller = NvControl {
            display: ":0".to_string(),
            xauthority: None,
            binary: script,
            // Generous timeout: under full-suite parallel load the fake
            // script can take well over a second to be scheduled, and a
            // timeout here surfaces as a missing args file, not a clean
            // assertion failure.
            command_timeout: Duration::from_secs(10),
            reap_timeout: Duration::from_secs(1),
            pid_observer: None,
        };
        let mut guard = MetaModeGuard {
            controller,
            prior: "DPY-0: 3840x2160 @3840x2160 +0+0".to_string(),
            resolution: Resolution {
                width: 3840,
                height: 2160,
            },
            changed: true,
            restore_hold: Some(Box::new(RestoreHold(hold_released.clone()))),
            emitter: LifecycleEmitter::disabled(),
            correlation_id: test_correlation_id(),
        };

        guard.dispatch_restore(|_| Err(std::io::Error::other("forced spawn failure")));

        assert!(!guard.changed);
        assert!(hold_released.load(Ordering::SeqCst));
        let args = fs::read_to_string(output).unwrap();
        assert!(args.contains("--assign CurrentMetaMode=DPY-0: 3840x2160 @3840x2160 +0+0"));
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[test]
    fn blocking_reap_timeout_transfers_child_to_waiter() {
        let child = std::process::Command::new("sh")
            .args(["-c", "exec sleep 999"])
            .spawn()
            .unwrap();
        let pid = child.id();

        assert!(matches!(
            kill_and_reap_blocking(
                child,
                Duration::ZERO,
                "test reap handoff",
                blocking_reaper().unwrap()
            ),
            Err(DisplayError::ReapTimeout {
                operation: "test reap handoff"
            })
        ));
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!process_exists(pid));
    }

    #[cfg(unix)]
    #[test]
    fn stuck_reap_request_does_not_block_later_children() {
        let reaper = blocking_reaper().unwrap();
        let blocker = std::process::Command::new("sh")
            .args(["-c", "exec sleep 1"])
            .spawn()
            .unwrap();
        let blocker_pid = blocker.id();
        handoff_child_to_reaper(blocker, "test stuck child", reaper);

        let child = std::process::Command::new("sh")
            .args(["-c", "exec sleep 999"])
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(matches!(
            kill_and_reap_blocking(child, Duration::ZERO, "test later child", reaper),
            Err(DisplayError::ReapTimeout {
                operation: "test later child"
            })
        ));

        let deadline = Instant::now() + Duration::from_millis(500);
        while process_exists(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!process_exists(pid));
        assert!(process_exists(blocker_pid));

        let cleanup_deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(blocker_pid) && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!process_exists(blocker_pid));
    }
}
