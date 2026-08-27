//! Authenticated session-user virtual microphone adapter.
//!
//! The machine broker only forwards bounded microphone-v1 frames to this
//! helper. Source creation, decode, publication, default-source mutation, and
//! restoration all execute under the authenticated desktop identity.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use arcen_media::audio::ResolvedMicrophoneStream;
#[cfg(any(test, target_os = "linux"))]
use arcen_media::audio::MICROPHONE_STATS_INTERVAL;
#[cfg(target_os = "linux")]
use arcen_media::audio::{
    AudioBitrateTier, MicrophoneDecoder, MicrophoneIngestOutcome, MicrophoneStatsTracker,
    MICROPHONE_V1_FRAME_SAMPLES,
};
use arcen_media::audio::{
    MicrophoneStats, MICROPHONE_JITTER_MAX_FRAMES, MICROPHONE_JITTER_TARGET_FRAMES,
};
#[cfg(target_os = "linux")]
use arcen_protocol::decode_microphone_frame;
use arcen_protocol::AudioCodec;
use arcen_telemetry::CorrelationId;
use serde::{Deserialize, Serialize};
#[cfg(any(test, target_os = "linux"))]
use tokio::io::AsyncReadExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::JoinHandle;
use zeroize::Zeroize;

use crate::session::identity::UserExecution;

const FIFO_READY_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_SETUP_TIMEOUT: Duration = Duration::from_secs(12);
const PARENT_READY_TIMEOUT: Duration = Duration::from_secs(125);
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(25);
const CHILD_RECOVERY_TIMEOUT: Duration = Duration::from_secs(20);
const RECOVERY_REAP_TIMEOUT: Duration = Duration::from_secs(3);
const RECOVERY_HELPER_TIMEOUT: Duration = Duration::from_secs(25);
const PACTL_TIMEOUT: Duration = Duration::from_secs(2);
const RECOVERY_ATTEMPTS: usize = 3;
const RECOVERY_BACKOFF_TOTAL: Duration = Duration::from_millis(300);
const FORCED_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const WRITER_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const TELEMETRY_REAP_TIMEOUT: Duration = Duration::from_secs(1);
const RECOVERY_WORST_CASE: Duration = Duration::from_millis(84_300);
const CLEANUP_WORST_CASE: Duration = Duration::from_millis(114_300);
pub(crate) const MICROPHONE_CLEANUP_BOUND: Duration = Duration::from_secs(120);
const MAX_RECOVERY_MODULES: usize = 8;
const MAX_RECOVERY_PACTL_STAGES: usize = MAX_RECOVERY_MODULES + 4;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const MAX_PACTL_OUTPUT_BYTES: u64 = 1024 * 1024;
const MAX_HELPER_TELEMETRY_BYTES: usize = 4096;
const HELPER_TELEMETRY_QUEUE_DEPTH: usize = 8;
const HELPER_TELEMETRY_WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const FRAME_QUEUE_DEPTH: usize = 4;
const MAX_FRAME_BYTES: usize =
    arcen_protocol::MICROPHONE_HEADER_SIZE + arcen_protocol::MICROPHONE_PCM_BYTES;
const JOURNAL_NAME: &str = "arcen-microphone-input-lease.json";
const PIPE_MODULE_NAME: &str = "module-pipe-source";
const PIPE_SOURCE_PROPERTIES: &str = "device.description=Arcen_Microphone";

#[derive(Debug, Default)]
pub(crate) struct RateLimitedMicrophoneWarning {
    last_emitted: Option<Instant>,
    suppressed: u64,
}

impl RateLimitedMicrophoneWarning {
    pub(crate) fn observe(&mut self) -> Option<u64> {
        self.observe_at(Instant::now())
    }

    fn observe_at(&mut self, now: Instant) -> Option<u64> {
        if self.last_emitted.is_none_or(|last| {
            now.duration_since(last) >= arcen_media::audio::MICROPHONE_STATS_INTERVAL
        }) {
            self.last_emitted = Some(now);
            return Some(std::mem::take(&mut self.suppressed));
        }
        self.suppressed = self.suppressed.saturating_add(1);
        None
    }
}

struct FramedMicrophonePacket {
    bytes: [u8; MAX_FRAME_BYTES],
    len: usize,
}

impl FramedMicrophonePacket {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Drop for FramedMicrophonePacket {
    fn drop(&mut self) {
        self.bytes.zeroize();
        self.len = 0;
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Ready {
    ready: bool,
    pid: u32,
    uid: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HelperStopReason {
    Running,
    InputClosed,
    PlayoutFailure,
    RestoreFailure,
    PlayoutAndRestoreFailure,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HelperRejectionClass {
    Sequence,
    Decode,
    Protocol,
}

impl HelperStopReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::InputClosed => "input_closed",
            Self::PlayoutFailure => "playout_failure",
            Self::RestoreFailure => "restore_failure",
            Self::PlayoutAndRestoreFailure => "playout_and_restore_failure",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct HelperStats {
    received_frames: u64,
    received_bytes: u64,
    accepted_frames: u64,
    accepted_bytes: u64,
    duplicate_frames: u64,
    late_frames: u64,
    wrong_generation_frames: u64,
    discontinuities: u64,
    rejected_discontinuities: u64,
    silence_frames: u64,
    underflow_frames: u64,
    decoder_resets: u64,
    decoder_errors: u64,
    fifo_timeouts: u64,
    fifo_failures: u64,
    telemetry_drops: u64,
}

impl From<MicrophoneStats> for HelperStats {
    fn from(stats: MicrophoneStats) -> Self {
        Self {
            received_frames: stats.received_frames,
            received_bytes: stats.received_bytes,
            accepted_frames: stats.accepted_frames,
            accepted_bytes: stats.accepted_bytes,
            duplicate_frames: stats.duplicate_frames,
            late_frames: stats.late_frames,
            wrong_generation_frames: stats.wrong_generation_frames,
            discontinuities: stats.discontinuities,
            rejected_discontinuities: stats.rejected_discontinuities,
            silence_frames: stats.silence_frames,
            underflow_frames: stats.underflow_frames,
            decoder_resets: stats.decoder_resets,
            decoder_errors: stats.decoder_errors,
            fifo_timeouts: stats.backend_timeouts,
            fifo_failures: stats.backend_underruns,
            telemetry_drops: stats.telemetry_drops,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum HelperTelemetry {
    SourceReady {
        codec: AudioCodec,
        generation: u32,
    },
    SourceRestored,
    RestoreFailure,
    FrameRejected {
        class: HelperRejectionClass,
        generation: u32,
        suppressed_since_last: u64,
    },
    Stats {
        codec: AudioCodec,
        generation: u32,
        stats: HelperStats,
        jitter_depth: usize,
        final_snapshot: bool,
        duration_ms: u64,
        stop_reason: HelperStopReason,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct SourceJournal {
    version: u32,
    prior_default: String,
    source_name: String,
    module_id: Option<u32>,
    fifo_path: PathBuf,
    #[serde(default)]
    module_load_started: bool,
    #[serde(default)]
    default_mutation_started: bool,
    #[serde(default)]
    default_restored: bool,
    #[serde(default)]
    module_unloaded: bool,
    #[serde(default)]
    fifo_removed: bool,
}

pub struct MicrophoneAgent {
    frames: Option<MicrophoneFrameSender>,
    supervisor: Option<JoinHandle<MicrophoneCleanupOutcome>>,
    events: watch::Receiver<MicrophoneAgentEvent>,
}

#[derive(Default)]
struct MicrophoneCleanupState {
    active: AtomicUsize,
    unresolved: Mutex<HashSet<CleanupIdentity>>,
    idle: Notify,
}

struct MicrophoneCleanupGuard {
    state: Arc<MicrophoneCleanupState>,
    identity: CleanupIdentity,
    completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CleanupIdentity {
    uid: u32,
    journal_path: PathBuf,
}

#[derive(Clone)]
pub struct MicrophoneFrameSender {
    frames: mpsc::Sender<Vec<u8>>,
    active: Arc<AtomicBool>,
    stop: watch::Sender<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneSendError {
    Closed,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneAgentEvent {
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicrophoneCleanupOutcome {
    Graceful,
    Forced,
    Failure(String),
}

impl MicrophoneCleanupOutcome {
    fn restoration_verified(&self) -> bool {
        matches!(self, Self::Graceful | Self::Forced)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneCleanupDrain {
    Drained,
    Failed,
    TimedOut,
}

#[derive(Clone)]
struct RecoveryContext {
    binary: PathBuf,
    pactl: PathBuf,
    execution: UserExecution,
    session_log_id: CorrelationId,
}

impl MicrophoneAgent {
    pub fn try_send(&self, frame: Vec<u8>) -> Result<(), MicrophoneSendError> {
        self.frames
            .as_ref()
            .ok_or(MicrophoneSendError::Closed)?
            .try_send(frame)
    }

    pub fn frame_sender(&self) -> MicrophoneFrameSender {
        self.frames
            .as_ref()
            .expect("active microphone agent has a frame sender")
            .clone()
    }

    pub fn events(&self) -> watch::Receiver<MicrophoneAgentEvent> {
        self.events.clone()
    }

    pub async fn shutdown(mut self) -> MicrophoneCleanupOutcome {
        if let Some(frames) = self.frames.take() {
            frames.stop();
        }
        match self.supervisor.take() {
            Some(supervisor) => supervisor.await.unwrap_or_else(|error| {
                MicrophoneCleanupOutcome::Failure(format!("microphone supervisor failed: {error}"))
            }),
            None => MicrophoneCleanupOutcome::Failure(
                "microphone supervisor was unavailable".to_string(),
            ),
        }
    }
}

impl MicrophoneFrameSender {
    pub fn try_send(&self, frame: Vec<u8>) -> Result<(), MicrophoneSendError> {
        if !self.active.load(Ordering::Acquire) {
            let mut frame = frame;
            frame.zeroize();
            return Err(MicrophoneSendError::Closed);
        }
        self.frames.try_send(frame).map_err(|error| match error {
            mpsc::error::TrySendError::Full(mut frame) => {
                frame.zeroize();
                MicrophoneSendError::Full
            }
            mpsc::error::TrySendError::Closed(mut frame) => {
                frame.zeroize();
                MicrophoneSendError::Closed
            }
        })
    }

    pub fn stop(&self) {
        if self.active.swap(false, Ordering::AcqRel) {
            let _ = self.stop.send(true);
        }
    }
}

impl Drop for MicrophoneAgent {
    fn drop(&mut self) {
        if let Some(frames) = self.frames.take() {
            frames.stop();
        }
        // Dropping a JoinHandle detaches it; the supervisor keeps reaping.
        self.supervisor.take();
    }
}

impl Drop for MicrophoneCleanupGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.state.mark_unresolved(self.identity.clone());
        }
        if self.state.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.state.idle.notify_waiters();
        }
    }
}

fn cleanup_state() -> Arc<MicrophoneCleanupState> {
    static STATE: OnceLock<Arc<MicrophoneCleanupState>> = OnceLock::new();
    Arc::clone(STATE.get_or_init(|| Arc::new(MicrophoneCleanupState::default())))
}

impl MicrophoneCleanupState {
    fn mark_unresolved(&self, identity: CleanupIdentity) {
        self.unresolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(identity);
    }

    fn resolve(&self, identity: &CleanupIdentity) {
        self.unresolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(identity);
    }

    fn has_unresolved(&self) -> bool {
        !self
            .unresolved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
}

fn cleanup_identity(execution: &UserExecution) -> Result<CleanupIdentity, String> {
    let runtime = execution
        .environment
        .get("XDG_RUNTIME_DIR")
        .ok_or_else(|| "microphone cleanup identity omitted XDG_RUNTIME_DIR".to_string())?;
    let canonical_runtime = std::fs::canonicalize(runtime)
        .map_err(|error| format!("canonicalize microphone runtime directory: {error}"))?;
    Ok(CleanupIdentity {
        uid: execution.identity.uid,
        journal_path: canonical_runtime.join(JOURNAL_NAME),
    })
}

fn register_cleanup(identity: CleanupIdentity) -> MicrophoneCleanupGuard {
    let state = cleanup_state();
    state.active.fetch_add(1, Ordering::AcqRel);
    MicrophoneCleanupGuard {
        state,
        identity,
        completed: false,
    }
}

impl MicrophoneCleanupGuard {
    fn complete(mut self, verified: bool) {
        if verified {
            self.state.resolve(&self.identity);
        } else {
            self.state.mark_unresolved(self.identity.clone());
        }
        self.completed = true;
    }
}

pub async fn wait_for_cleanup(timeout: Duration) -> MicrophoneCleanupDrain {
    let state = cleanup_state();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let idle = state.idle.notified();
        tokio::pin!(idle);
        let _ = idle.as_mut().enable();
        if state.active.load(Ordering::Acquire) == 0 {
            return if state.has_unresolved() {
                MicrophoneCleanupDrain::Failed
            } else {
                MicrophoneCleanupDrain::Drained
            };
        }
        if tokio::time::timeout_at(deadline, idle).await.is_err() {
            return if state.active.load(Ordering::Acquire) == 0 {
                if state.has_unresolved() {
                    MicrophoneCleanupDrain::Failed
                } else {
                    MicrophoneCleanupDrain::Drained
                }
            } else {
                MicrophoneCleanupDrain::TimedOut
            };
        }
    }
}

pub async fn probe_backend(pactl: &Path, execution: &UserExecution) -> bool {
    if !pactl.is_file() || execution.identity.uid == 0 {
        return false;
    }
    let mut command = Command::new(pactl);
    command
        .arg("info")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if execution.configure(&mut command).is_err() {
        return false;
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return false,
    };
    match tokio::time::timeout(PACTL_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            false
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            false
        }
    }
}

pub async fn recover_for_user(
    binary: &Path,
    pactl: &Path,
    execution: &UserExecution,
    session_log_id: &CorrelationId,
) -> Result<(), String> {
    if execution.identity.uid == 0 {
        return Err("microphone recovery refuses a root session".to_string());
    }
    let cleanup = register_cleanup(cleanup_identity(execution)?);
    let context = RecoveryContext {
        binary: binary.to_path_buf(),
        pactl: pactl.to_path_buf(),
        execution: execution.clone(),
        session_log_id: session_log_id.clone(),
    };
    tracing::info!(
        target: crate::logging::target::AUDIO,
        event = "mic_linux_recovery_started",
        sid = %session_log_id,
        backend = "pulseaudio_pipe_source",
        "Linux microphone recovery started"
    );
    let result = retry_recovery(&context).await;
    if result.is_ok() {
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = "mic_linux_recovery_completed",
            sid = %session_log_id,
            backend = "pulseaudio_pipe_source",
            restore_verified = true,
            "Linux microphone recovery completed"
        );
    } else {
        tracing::warn!(
            target: crate::logging::target::AUDIO,
            event = "mic_linux_recovery_failure",
            sid = %session_log_id,
            backend = "pulseaudio_pipe_source",
            reason = "recovery_failed",
            "Linux microphone recovery failed"
        );
    }
    cleanup.complete(result.is_ok());
    result
}

pub async fn spawn(
    binary: &Path,
    pactl: &Path,
    execution: &UserExecution,
    stream: ResolvedMicrophoneStream,
    session_log_id: &CorrelationId,
) -> Result<MicrophoneAgent, String> {
    if !stream.is_enabled() || execution.identity.uid == 0 {
        return Err("microphone agent requires an enabled non-root session".to_string());
    }
    let cleanup = register_cleanup(cleanup_identity(execution)?);
    let binary = binary.to_path_buf();
    let pactl = pactl.to_path_buf();
    let execution = execution.clone();
    let session_log_id = session_log_id.clone();
    run_cancellation_safe(async move {
        spawn_inner(
            &binary,
            &pactl,
            &execution,
            stream,
            &session_log_id,
            cleanup,
        )
        .await
    })
    .await
}

async fn run_cancellation_safe<T, F>(future: F) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    tokio::spawn(future)
        .await
        .map_err(|error| format!("microphone lifecycle task failed: {error}"))?
}

async fn spawn_inner(
    binary: &Path,
    pactl: &Path,
    execution: &UserExecution,
    stream: ResolvedMicrophoneStream,
    session_log_id: &CorrelationId,
    cleanup: MicrophoneCleanupGuard,
) -> Result<MicrophoneAgent, String> {
    let codec = match stream.codec {
        Some(AudioCodec::Opus) => "opus",
        Some(AudioCodec::Pcm) => "pcm",
        None => return Err("microphone stream is disabled".to_string()),
    };
    let context = RecoveryContext {
        binary: binary.to_path_buf(),
        pactl: pactl.to_path_buf(),
        execution: execution.clone(),
        session_log_id: session_log_id.clone(),
    };
    tracing::info!(
        target: crate::logging::target::AUDIO,
        event = "mic_linux_helper_started",
        sid = %session_log_id,
        backend = "pulseaudio_pipe_source",
        codec,
        generation = stream.generation,
        sample_rate_hz = 48_000u32,
        channels = 1u8,
        frame_duration_ms = 20u16,
        "Linux microphone helper starting"
    );
    let mut command = crate::command_for_helper(binary, "session-agent");
    command
        .arg("--microphone-agent")
        .arg("--session-log-id")
        .arg(session_log_id.as_str())
        .arg("--pactl-bin")
        .arg(pactl)
        .arg("--microphone-codec")
        .arg(codec)
        .arg("--microphone-generation")
        .arg(stream.generation.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    if let Err(error) = execution.configure(&mut command) {
        cleanup.complete(true);
        return Err(format!("microphone agent identity: {error}"));
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            cleanup.complete(true);
            return Err(format!("spawn microphone agent: {error}"));
        }
    };
    let expected_pid = match child.id() {
        Some(pid) => pid,
        None => {
            let outcome = cleanup_startup_failure(child, None, &context, cleanup).await;
            return Err(format!(
                "microphone agent exited before readiness ({outcome:?})"
            ));
        }
    };
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let outcome = cleanup_startup_failure(child, None, &context, cleanup).await;
            return Err(format!("microphone agent stdin unavailable ({outcome:?})"));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            drop(stdin);
            let outcome = cleanup_startup_failure(child, None, &context, cleanup).await;
            return Err(format!("microphone agent stdout unavailable ({outcome:?})"));
        }
    };
    let stderr_pipe = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdin);
            drop(stdout);
            let outcome = cleanup_startup_failure(child, None, &context, cleanup).await;
            return Err(format!("microphone agent stderr unavailable ({outcome:?})"));
        }
    };
    let stderr = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr_pipe).lines();
        let mut line_count = 0u64;
        while let Ok(Some(line)) = lines.next_line().await {
            line_count = line_count.saturating_add(1);
            drop(line);
            if line_count == 1 || line_count.is_multiple_of(100) {
                tracing::debug!(
                    target: crate::logging::target::AUDIO,
                    event = "mic_linux_helper_diagnostics",
                    diagnostic_lines = line_count,
                    "Linux microphone helper emitted diagnostics"
                );
            }
        }
    });
    let mut stdout = tokio::io::BufReader::new(stdout);
    let mut line = Vec::new();
    let readiness = async {
        let read = tokio::time::timeout(PARENT_READY_TIMEOUT, stdout.read_until(b'\n', &mut line))
            .await
            .map_err(|_| "microphone agent readiness timed out".to_string())?
            .map_err(|error| format!("read microphone agent readiness: {error}"))?;
        if read == 0 {
            return Err("microphone agent exited before readiness".to_string());
        }
        let ready: Ready = serde_json::from_slice(&line)
            .map_err(|_| "microphone agent readiness is invalid".to_string())?;
        if !ready.ready || ready.pid != expected_pid || ready.uid != execution.identity.uid {
            return Err("microphone agent readiness identity mismatch".to_string());
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = readiness {
        drop(stdin);
        drop(stdout);
        let outcome = cleanup_startup_failure(child, Some(stderr), &context, cleanup).await;
        return Err(format!("{error}; cleanup: {outcome:?}"));
    }
    tracing::info!(
        target: crate::logging::target::AUDIO,
        event = "mic_linux_helper_ready",
        sid = %session_log_id,
        backend = "pulseaudio_pipe_source",
        codec,
        generation = stream.generation,
        "Linux microphone helper ready"
    );
    let telemetry_drops = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (telemetry_tx, telemetry_rx) = mpsc::channel(HELPER_TELEMETRY_QUEUE_DEPTH);
    let telemetry_reader = tokio::spawn(read_helper_telemetry(
        stdout,
        telemetry_tx,
        Arc::clone(&telemetry_drops),
    ));
    let telemetry_logger = tokio::spawn(log_helper_telemetry(
        telemetry_rx,
        Arc::clone(&telemetry_drops),
        session_log_id.clone(),
    ));
    let (frames, mut receiver) = mpsc::channel::<Vec<u8>>(FRAME_QUEUE_DEPTH);
    let active = Arc::new(AtomicBool::new(true));
    let (stop, mut writer_stop) = watch::channel(false);
    let supervisor_stop = stop.subscribe();
    let (writer_failed, writer_failure) = watch::channel(false);
    let writer_active = Arc::clone(&active);
    let writer = tokio::spawn(async move {
        let mut stdin = stdin;
        let mut failed = false;
        loop {
            if !writer_active.load(Ordering::Acquire) {
                break;
            }
            let frame = tokio::select! {
                biased;
                changed = writer_stop.changed() => {
                    let _ = changed;
                    break;
                },
                frame = receiver.recv() => frame,
            };
            let Some(mut frame) = frame else {
                break;
            };
            let Ok(length) = u32::try_from(frame.len()) else {
                frame.zeroize();
                continue;
            };
            if stdin.write_all(&length.to_le_bytes()).await.is_err()
                || stdin.write_all(&frame).await.is_err()
            {
                frame.zeroize();
                failed = true;
                break;
            }
            frame.zeroize();
        }
        writer_active.store(false, Ordering::Release);
        if failed {
            let _ = writer_failed.send(true);
        }
        while let Ok(mut frame) = receiver.try_recv() {
            frame.zeroize();
        }
        let _ = tokio::time::timeout(Duration::from_secs(1), stdin.shutdown()).await;
    });
    let frames = MicrophoneFrameSender {
        frames,
        active,
        stop,
    };
    let supervisor_frames = frames.clone();
    let (event_tx, events) = watch::channel(MicrophoneAgentEvent::Running);
    let supervisor = tokio::spawn(supervise_agent(
        child,
        writer,
        stderr,
        telemetry_reader,
        telemetry_logger,
        supervisor_frames,
        supervisor_stop,
        writer_failure,
        event_tx,
        context,
        cleanup,
    ));
    Ok(MicrophoneAgent {
        frames: Some(frames),
        supervisor: Some(supervisor),
        events,
    })
}

#[allow(clippy::too_many_arguments)]
async fn supervise_agent(
    mut child: Child,
    mut writer: JoinHandle<()>,
    stderr: JoinHandle<()>,
    mut telemetry_reader: JoinHandle<()>,
    mut telemetry_logger: JoinHandle<()>,
    frames: MicrophoneFrameSender,
    mut stop: watch::Receiver<bool>,
    mut writer_failure: watch::Receiver<bool>,
    event_tx: watch::Sender<MicrophoneAgentEvent>,
    context: RecoveryContext,
    cleanup: MicrophoneCleanupGuard,
) -> MicrophoneCleanupOutcome {
    enum Trigger {
        Exited(std::io::Result<std::process::ExitStatus>),
        Requested,
        WriterFailed,
    }

    let mut writer_failure_open = true;
    let trigger = loop {
        tokio::select! {
            status = child.wait() => break Trigger::Exited(status),
            changed = stop.changed() => {
                let _ = changed;
                break Trigger::Requested;
            }
            changed = writer_failure.changed(), if writer_failure_open => {
                match changed {
                    Ok(()) if *writer_failure.borrow() => break Trigger::WriterFailed,
                    Ok(()) => {}
                    Err(_) => writer_failure_open = false,
                }
            }
        }
    };
    let requested = matches!(trigger, Trigger::Requested);
    let writer_failed = matches!(trigger, Trigger::WriterFailed);
    frames.stop();
    let (status, forced) = match trigger {
        Trigger::Exited(status) => (status, false),
        Trigger::Requested | Trigger::WriterFailed => wait_or_terminate(&mut child).await,
    };
    if tokio::time::timeout(WRITER_REAP_TIMEOUT, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
    let _ = tokio::time::timeout(TELEMETRY_REAP_TIMEOUT, stderr).await;
    if tokio::time::timeout(TELEMETRY_REAP_TIMEOUT, &mut telemetry_reader)
        .await
        .is_err()
    {
        telemetry_reader.abort();
        let _ = telemetry_reader.await;
    }
    if tokio::time::timeout(TELEMETRY_REAP_TIMEOUT, &mut telemetry_logger)
        .await
        .is_err()
    {
        telemetry_logger.abort();
        let _ = telemetry_logger.await;
    }

    let clean_exit = matches!(&status, Ok(status) if status.success());
    let outcome = if clean_exit && !forced {
        MicrophoneCleanupOutcome::Graceful
    } else {
        match retry_recovery(&context).await {
            Ok(()) => MicrophoneCleanupOutcome::Forced,
            Err(error) => {
                let status = status
                    .map(|status| status.to_string())
                    .unwrap_or_else(|wait_error| wait_error.to_string());
                MicrophoneCleanupOutcome::Failure(format!(
                    "helper status {status}; recovery failed: {error}"
                ))
            }
        }
    };
    let duration_reason = if requested {
        "requested"
    } else if writer_failed {
        "writer_failed"
    } else {
        "helper_exit"
    };
    if outcome.restoration_verified() {
        tracing::info!(
            target: crate::logging::target::AUDIO,
            event = "mic_linux_helper_stopped",
            sid = %context.session_log_id,
            backend = "pulseaudio_pipe_source",
            stop_reason = duration_reason,
            forced,
            restore_verified = true,
            "Linux microphone helper stopped and source restored"
        );
    } else {
        tracing::warn!(
            target: crate::logging::target::AUDIO,
            event = "mic_linux_helper_failure",
            sid = %context.session_log_id,
            backend = "pulseaudio_pipe_source",
            stop_reason = duration_reason,
            forced,
            restore_verified = false,
            "Linux microphone helper cleanup failed"
        );
    }
    let event = if requested && !writer_failed && outcome.restoration_verified() {
        MicrophoneAgentEvent::Stopped
    } else {
        MicrophoneAgentEvent::Failed
    };
    let _ = event_tx.send(event);
    cleanup.complete(outcome.restoration_verified());
    outcome
}

async fn cleanup_startup_failure(
    mut child: Child,
    stderr: Option<JoinHandle<()>>,
    context: &RecoveryContext,
    cleanup: MicrophoneCleanupGuard,
) -> MicrophoneCleanupOutcome {
    let (status, forced) = wait_or_terminate(&mut child).await;
    if let Some(stderr) = stderr {
        let _ = tokio::time::timeout(TELEMETRY_REAP_TIMEOUT, stderr).await;
    }
    let clean_exit = matches!(&status, Ok(status) if status.success());
    let outcome = if clean_exit && !forced {
        MicrophoneCleanupOutcome::Graceful
    } else {
        match retry_recovery(context).await {
            Ok(()) => MicrophoneCleanupOutcome::Forced,
            Err(error) => {
                MicrophoneCleanupOutcome::Failure(format!("startup cleanup failed: {error}"))
            }
        }
    };
    cleanup.complete(outcome.restoration_verified());
    outcome
}

async fn wait_or_terminate(child: &mut Child) -> (std::io::Result<std::process::ExitStatus>, bool) {
    match tokio::time::timeout(GRACEFUL_EXIT_TIMEOUT, child.wait()).await {
        Ok(status) => (status, false),
        Err(_) => {
            let terminate_error = terminate_process_group(child).await.err();
            let status = match tokio::time::timeout(FORCED_REAP_TIMEOUT, child.wait()).await {
                Ok(status) => status,
                Err(_) => Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "microphone helper could not be reaped",
                )),
            };
            if let Some(error) = terminate_error {
                tracing::warn!(
                    target: crate::logging::target::AUDIO,
                    %error,
                    "failed to terminate microphone helper process group"
                );
            }
            (status, true)
        }
    }
}

async fn retry_recovery(context: &RecoveryContext) -> Result<(), String> {
    let mut last_error = "microphone recovery was not attempted".to_string();
    for attempt in 1..=RECOVERY_ATTEMPTS {
        match run_recovery_helper(context).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
        if attempt < RECOVERY_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
        }
    }
    Err(last_error)
}

async fn run_recovery_helper(context: &RecoveryContext) -> Result<(), String> {
    let mut command = crate::command_for_helper(&context.binary, "session-agent");
    command
        .arg("--microphone-recover")
        .arg("--session-log-id")
        .arg(context.session_log_id.as_str())
        .arg("--pactl-bin")
        .arg(&context.pactl)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    context
        .execution
        .configure(&mut command)
        .map_err(|error| format!("microphone recovery identity: {error}"))?;
    let mut child = command
        .spawn()
        .map_err(|error| format!("spawn microphone recovery: {error}"))?;
    match tokio::time::timeout(RECOVERY_HELPER_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(status)) => Err(format!("microphone recovery exited {status}")),
        Ok(Err(error)) => {
            let _ = terminate_process_group(&mut child).await;
            let _ = tokio::time::timeout(RECOVERY_REAP_TIMEOUT, child.wait()).await;
            Err(format!("wait for microphone recovery: {error}"))
        }
        Err(_) => {
            terminate_process_group(&mut child).await?;
            tokio::time::timeout(RECOVERY_REAP_TIMEOUT, child.wait())
                .await
                .map_err(|_| "microphone recovery could not be reaped".to_string())?
                .map_err(|error| format!("reap microphone recovery: {error}"))?;
            Err("microphone recovery timed out".to_string())
        }
    }
}

pub async fn run_child(args: &[String]) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        Err("microphone source is Linux-only".to_string())
    }
    #[cfg(target_os = "linux")]
    {
        if args
            .iter()
            .any(|argument| argument == "--microphone-recover")
        {
            run_linux_recovery(args).await
        } else {
            run_linux_child(args).await
        }
    }
}

#[cfg(target_os = "linux")]
async fn run_linux_recovery(args: &[String]) -> Result<(), String> {
    use nix::unistd::getuid;

    if getuid().is_root() {
        return Err("microphone recovery refuses to run as root".to_string());
    }
    let pactl = argument_value(args, "--pactl-bin")
        .map(PathBuf::from)
        .ok_or_else(|| "microphone recovery omitted pactl path".to_string())?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "microphone recovery requires XDG_RUNTIME_DIR".to_string())?;
    recover_previous(&pactl, &runtime.join(JOURNAL_NAME)).await
}

#[cfg(target_os = "linux")]
async fn run_linux_child(args: &[String]) -> Result<(), String> {
    use nix::unistd::getuid;
    let pactl = argument_value(args, "--pactl-bin")
        .map(PathBuf::from)
        .ok_or_else(|| "microphone agent omitted pactl path".to_string())?;
    let _session_log_id = argument_value(args, "--session-log-id")
        .and_then(|value| CorrelationId::parse_uuid(&value).ok())
        .ok_or_else(|| "microphone agent session log id is invalid".to_string())?;
    let generation = argument_value(args, "--microphone-generation")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| "microphone agent generation is invalid".to_string())?;
    let codec = match argument_value(args, "--microphone-codec").as_deref() {
        Some("opus") => AudioCodec::Opus,
        Some("pcm") => AudioCodec::Pcm,
        _ => return Err("microphone agent codec is invalid".to_string()),
    };
    if getuid().is_root() {
        return Err("microphone agent refuses to run as root".to_string());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "microphone agent requires XDG_RUNTIME_DIR".to_string())?;
    let journal_path = runtime.join(JOURNAL_NAME);
    recover_previous(&pactl, &journal_path).await?;
    let fifo_path = runtime.join(format!("arcen-microphone-{generation}.pcm"));
    let source_name = format!("arcen_microphone_{generation}");
    let prior_default = pactl_output(&pactl, &["get-default-source"]).await?;
    let journal = SourceJournal {
        version: 2,
        prior_default,
        source_name: source_name.clone(),
        module_id: None,
        fifo_path: fifo_path.clone(),
        module_load_started: false,
        default_mutation_started: false,
        default_restored: false,
        module_unloaded: false,
        fifo_removed: false,
    };
    write_journal(&journal_path, &journal)?;
    let mut lease = SourceLease {
        pactl,
        journal_path,
        journal: Some(journal),
    };
    let setup_result = tokio::time::timeout(CHILD_SETUP_TIMEOUT, async {
        match std::fs::remove_file(&fifo_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove stale microphone FIFO: {error}")),
        }
        let pactl = lease.pactl.clone();
        lease.update(|journal| journal.module_load_started = true)?;
        let module_id = pactl_output(
            &pactl,
            &[
                "load-module",
                PIPE_MODULE_NAME,
                &format!("source_name={source_name}"),
                &format!("file={}", fifo_path.display()),
                "format=s16le",
                "rate=48000",
                "channels=1",
                &format!("source_properties={PIPE_SOURCE_PROPERTIES}"),
            ],
        )
        .await?
        .parse::<u32>()
        .map_err(|_| "pactl returned an invalid module id".to_string())?;
        lease.update(|journal| journal.module_id = Some(module_id))?;
        secure_module_fifo(&fifo_path, getuid().as_raw()).await?;
        lease.update(|journal| journal.default_mutation_started = true)?;
        pactl_status(&pactl, &["set-default-source", &source_name]).await?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| "microphone child setup timed out".to_string())
    .and_then(|result| result);
    if let Err(error) = setup_result {
        let cleanup = lease.restore().await;
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
        });
    }
    let fifo = match open_fifo_writer(&fifo_path).await {
        Ok(fifo) => fifo,
        Err(error) => {
            let setup_error = format!("open microphone FIFO: {error}");
            let cleanup = lease.restore().await;
            return Err(match cleanup {
                Ok(()) => setup_error,
                Err(cleanup) => format!("{setup_error}; cleanup failed: {cleanup}"),
            });
        }
    };
    let stream = ResolvedMicrophoneStream {
        codec: Some(codec),
        bitrate: if matches!(codec, AudioCodec::Pcm) {
            AudioBitrateTier::Off
        } else {
            AudioBitrateTier::Kbps64
        },
        generation,
        reason: arcen_protocol::messages::MicrophoneStreamReason::Enabled,
    };
    let mut decoder = match MicrophoneDecoder::new(stream) {
        Ok(decoder) => decoder,
        Err(_) => {
            drop(fifo);
            let error = "initialize microphone decoder".to_string();
            let cleanup = lease.restore().await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
            });
        }
    };
    let input = match open_nonblocking_stdin() {
        Ok(input) => input,
        Err(error) => {
            decoder.clear();
            drop(fifo);
            let cleanup = lease.restore().await;
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
            });
        }
    };
    println!(
        "{}",
        serde_json::to_string(&Ready {
            ready: true,
            pid: std::process::id(),
            uid: getuid().as_raw(),
        })
        .expect("READY serializes")
    );
    use std::io::Write;
    if let Err(error) = std::io::stdout().flush() {
        decoder.clear();
        drop(fifo);
        let ready_error = format!("flush microphone readiness: {error}");
        let cleanup = lease.restore().await;
        return Err(match cleanup {
            Ok(()) => ready_error,
            Err(cleanup) => format!("{ready_error}; cleanup failed: {cleanup}"),
        });
    }
    let (helper_telemetry, helper_telemetry_rx) =
        mpsc::channel::<HelperTelemetry>(HELPER_TELEMETRY_QUEUE_DEPTH);
    let mut helper_telemetry_writer = tokio::spawn(write_helper_telemetry(helper_telemetry_rx));
    let mut stats = MicrophoneStatsTracker::default();
    if !try_emit_helper_telemetry(
        &helper_telemetry,
        HelperTelemetry::SourceReady { codec, generation },
    ) {
        stats.record_telemetry_drops(1);
    }

    let (frames, mut receiver) = mpsc::channel::<FramedMicrophonePacket>(FRAME_QUEUE_DEPTH);
    let reader = tokio::spawn(read_framed_nonblocking_input(input, frames));
    let mut fifo = fifo;
    let mut output = [0i16; MICROPHONE_V1_FRAME_SAMPLES];
    let mut bytes = [0u8; MICROPHONE_V1_FRAME_SAMPLES * 2];
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut stats_tick = tokio::time::interval(MICROPHONE_STATS_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;
    stats_tick.tick().await;
    let started_at = std::time::Instant::now();
    let mut sequence_rejection_log = RateLimitedMicrophoneWarning::default();
    let mut decode_rejection_log = RateLimitedMicrophoneWarning::default();
    let mut protocol_rejection_log = RateLimitedMicrophoneWarning::default();
    let playout_result = loop {
        tokio::select! {
            frame = receiver.recv() => {
                let Some(frame) = frame else { break Ok(()); };
                stats.record_received(frame.as_slice().len());
                match decode_microphone_frame(frame.as_slice()) {
                    Ok((header, payload)) => match decoder.ingest(header, payload) {
                        Ok(MicrophoneIngestOutcome::Reset) => {
                            stats.record_ingest(MicrophoneIngestOutcome::RejectedDiscontinuity, 0);
                            if let Some(suppressed) = sequence_rejection_log.observe() {
                                if !try_emit_helper_telemetry(
                                    &helper_telemetry,
                                    HelperTelemetry::FrameRejected {
                                        class: HelperRejectionClass::Sequence,
                                        generation,
                                        suppressed_since_last: suppressed,
                                    },
                                ) {
                                    stats.record_telemetry_drops(1);
                                }
                            }
                            break Err("microphone ordering discontinuity".to_string());
                        }
                        Ok(outcome) => {
                            stats.record_ingest(outcome, frame.as_slice().len());
                            if matches!(
                                outcome,
                                MicrophoneIngestOutcome::DroppedWrongGeneration
                            ) {
                                if let Some(suppressed) = sequence_rejection_log.observe() {
                                    if !try_emit_helper_telemetry(
                                        &helper_telemetry,
                                        HelperTelemetry::FrameRejected {
                                            class: HelperRejectionClass::Sequence,
                                            generation,
                                            suppressed_since_last: suppressed,
                                        },
                                    ) {
                                        stats.record_telemetry_drops(1);
                                    }
                                }
                            }
                        }
                        Err(_error) => {
                            stats.record_decoder_error();
                            if let Some(suppressed) = decode_rejection_log.observe() {
                                if !try_emit_helper_telemetry(
                                    &helper_telemetry,
                                    HelperTelemetry::FrameRejected {
                                        class: HelperRejectionClass::Decode,
                                        generation,
                                        suppressed_since_last: suppressed,
                                    },
                                ) {
                                    stats.record_telemetry_drops(1);
                                }
                            }
                        }
                    },
                    Err(_) => {
                        stats.record_decoder_error();
                        if let Some(suppressed) = protocol_rejection_log.observe() {
                            if !try_emit_helper_telemetry(
                                &helper_telemetry,
                                HelperTelemetry::FrameRejected {
                                    class: HelperRejectionClass::Protocol,
                                    generation,
                                    suppressed_since_last: suppressed,
                                },
                            ) {
                                stats.record_telemetry_drops(1);
                            }
                        }
                    }
                }
            }
            _ = tick.tick() => {
                let frame_output = match decoder.pop_into(&mut output) {
                    Ok(frame_output) => frame_output,
                    Err(_) => {
                        stats.record_decoder_error();
                        break Err("microphone playout failed".to_string());
                    }
                };
                stats.record_output(frame_output);
                for (target, sample) in bytes.chunks_exact_mut(2).zip(output) {
                    target.copy_from_slice(&sample.to_le_bytes());
                }
                match fifo.write_frame(&bytes) {
                    Ok(FifoWriteOutcome::Written) => {}
                    Ok(FifoWriteOutcome::RecoveredBackpressure) => {
                        stats.record_backend_timeout();
                    }
                    Err(error) => {
                        stats.record_backend_underrun();
                        break Err(format!("write microphone source: {error}"));
                    }
                }
                output.zeroize();
                bytes.zeroize();
            }
            _ = stats_tick.tick() => {
                let event = HelperTelemetry::Stats {
                    codec,
                    generation,
                    stats: stats.take_interval().into(),
                    jitter_depth: decoder.queued_frames(),
                    final_snapshot: false,
                    duration_ms: started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    stop_reason: HelperStopReason::Running,
                };
                if !try_emit_helper_telemetry(&helper_telemetry, event) {
                    stats.record_telemetry_drops(1);
                }
            }
        }
    };
    reader.abort();
    let _ = reader.await;
    decoder.clear();
    output.zeroize();
    bytes.zeroize();
    drop(fifo);
    let cleanup = lease.restore().await;
    if cleanup.is_ok() {
        if !try_emit_helper_telemetry(&helper_telemetry, HelperTelemetry::SourceRestored) {
            stats.record_telemetry_drops(1);
        }
    } else if !try_emit_helper_telemetry(&helper_telemetry, HelperTelemetry::RestoreFailure) {
        stats.record_telemetry_drops(1);
    }
    let stop_reason = match (&playout_result, &cleanup) {
        (Ok(()), Ok(())) => "input_closed",
        (Err(_), Ok(())) => "playout_failure",
        (Ok(()), Err(_)) => "restore_failure",
        (Err(_), Err(_)) => "playout_and_restore_failure",
    };
    let stop_reason = match stop_reason {
        "input_closed" => HelperStopReason::InputClosed,
        "playout_failure" => HelperStopReason::PlayoutFailure,
        "restore_failure" => HelperStopReason::RestoreFailure,
        _ => HelperStopReason::PlayoutAndRestoreFailure,
    };
    let final_event = HelperTelemetry::Stats {
        codec,
        generation,
        stats: stats.total().into(),
        jitter_depth: decoder.queued_frames(),
        final_snapshot: true,
        duration_ms: started_at.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        stop_reason,
    };
    let _ = try_emit_helper_telemetry(&helper_telemetry, final_event);
    drop(helper_telemetry);
    if tokio::time::timeout(HELPER_TELEMETRY_WRITE_TIMEOUT, &mut helper_telemetry_writer)
        .await
        .is_err()
    {
        helper_telemetry_writer.abort();
        let _ = helper_telemetry_writer.await;
    }
    return match (playout_result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup failed: {cleanup}")),
    };
}

fn try_emit_helper_telemetry(
    sender: &mpsc::Sender<HelperTelemetry>,
    event: HelperTelemetry,
) -> bool {
    sender.try_send(event).is_ok()
}

async fn write_helper_telemetry(mut receiver: mpsc::Receiver<HelperTelemetry>) {
    let mut stdout = tokio::io::stdout();
    while let Some(event) = receiver.recv().await {
        let Ok(mut line) = serde_json::to_vec(&event) else {
            continue;
        };
        if line.len() >= MAX_HELPER_TELEMETRY_BYTES {
            line.zeroize();
            continue;
        }
        line.push(b'\n');
        let written = tokio::time::timeout(HELPER_TELEMETRY_WRITE_TIMEOUT, async {
            stdout.write_all(&line).await?;
            stdout.flush().await
        })
        .await
        .is_ok_and(|result| result.is_ok());
        line.zeroize();
        if !written {
            return;
        }
    }
}

async fn read_helper_telemetry(
    mut stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    sender: mpsc::Sender<HelperTelemetry>,
    drops: Arc<std::sync::atomic::AtomicU64>,
) {
    let mut line = Vec::with_capacity(MAX_HELPER_TELEMETRY_BYTES);
    while let Ok(Some(valid_length)) = read_bounded_helper_line(&mut stdout, &mut line).await {
        let event = valid_length
            .then(|| serde_json::from_slice::<HelperTelemetry>(&line).ok())
            .flatten();
        line.zeroize();
        line.clear();
        if event.is_none_or(|event| sender.try_send(event).is_err()) {
            drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn read_bounded_helper_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> std::io::Result<Option<bool>> {
    let mut valid_length = true;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(valid_length))
            };
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let payload = &available[..consumed];
        let payload = payload.strip_suffix(b"\n").unwrap_or(payload);
        if valid_length && line.len().saturating_add(payload.len()) <= MAX_HELPER_TELEMETRY_BYTES {
            line.extend_from_slice(payload);
        } else {
            valid_length = false;
            if line.is_empty() {
                line.push(0);
            }
        }
        let complete = consumed != available.len() || available.last() == Some(&b'\n');
        reader.consume(consumed);
        if complete {
            return Ok(Some(valid_length));
        }
    }
}

async fn log_helper_telemetry(
    mut receiver: mpsc::Receiver<HelperTelemetry>,
    drops: Arc<std::sync::atomic::AtomicU64>,
    session_log_id: CorrelationId,
) {
    while let Some(event) = receiver.recv().await {
        let telemetry_drops = drops.swap(0, Ordering::Relaxed);
        match event {
            HelperTelemetry::SourceReady { codec, generation } => tracing::info!(
                target: crate::logging::target::AUDIO,
                event = "mic_linux_source_ready",
                sid = %session_log_id,
                backend = "pulseaudio_pipe_source",
                ?codec,
                generation,
                sample_rate_hz = 48_000u32,
                channels = 1u8,
                frame_duration_ms = 20u16,
                telemetry_drops,
                "Linux virtual microphone source ready"
            ),
            HelperTelemetry::SourceRestored => tracing::info!(
                target: crate::logging::target::AUDIO,
                event = "mic_linux_source_restored",
                sid = %session_log_id,
                backend = "pulseaudio_pipe_source",
                restore_verified = true,
                telemetry_drops,
                "Linux microphone source restored"
            ),
            HelperTelemetry::RestoreFailure => tracing::warn!(
                target: crate::logging::target::AUDIO,
                event = "mic_linux_restore_failure",
                sid = %session_log_id,
                backend = "pulseaudio_pipe_source",
                restore_verified = false,
                telemetry_drops,
                "Linux microphone source restoration failed"
            ),
            HelperTelemetry::FrameRejected {
                class,
                generation,
                suppressed_since_last,
            } => tracing::warn!(
                target: crate::logging::target::AUDIO,
                event = "mic_frame_rejected",
                sid = %session_log_id,
                backend = "pulseaudio_pipe_source",
                generation,
                reason = ?class,
                suppressed_since_last,
                telemetry_drops,
                "Linux microphone helper rejected a frame"
            ),
            HelperTelemetry::Stats {
                codec,
                generation,
                stats,
                jitter_depth,
                final_snapshot,
                duration_ms,
                stop_reason,
            } => tracing::info!(
                target: crate::logging::target::AUDIO,
                event = if final_snapshot { "mic_linux_teardown_summary" } else { "mic_linux_stats" },
                sid = %session_log_id,
                backend = "pulseaudio_pipe_source",
                ?codec,
                generation,
                final_snapshot,
                duration_ms,
                stop_reason = stop_reason.as_str(),
                received_frames = stats.received_frames,
                received_bytes = stats.received_bytes,
                accepted_frames = stats.accepted_frames,
                accepted_bytes = stats.accepted_bytes,
                duplicate_frames = stats.duplicate_frames,
                late_frames = stats.late_frames,
                wrong_generation_frames = stats.wrong_generation_frames,
                discontinuities = stats.discontinuities,
                rejected_discontinuities = stats.rejected_discontinuities,
                jitter_depth,
                jitter_target = MICROPHONE_JITTER_TARGET_FRAMES,
                jitter_max = MICROPHONE_JITTER_MAX_FRAMES,
                silence_frames = stats.silence_frames,
                underflow_frames = stats.underflow_frames,
                decoder_resets = stats.decoder_resets,
                decoder_errors = stats.decoder_errors,
                fifo_timeouts = stats.fifo_timeouts,
                fifo_failures = stats.fifo_failures,
                telemetry_drops = stats.telemetry_drops.saturating_add(telemetry_drops),
                "Linux microphone statistics"
            ),
        }
    }
}

#[cfg(target_os = "linux")]
struct NonblockingFifo {
    file: std::fs::File,
    path: PathBuf,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FifoWriteOutcome {
    Written,
    RecoveredBackpressure,
}

#[cfg(target_os = "linux")]
impl NonblockingFifo {
    fn write_frame(&mut self, bytes: &[u8]) -> Result<FifoWriteOutcome, String> {
        if write_fifo_frame(&mut self.file, bytes)? {
            return Ok(FifoWriteOutcome::Written);
        }
        drain_stale_fifo_audio(&self.path)?;
        let _ = write_fifo_frame(&mut self.file, bytes)?;
        Ok(FifoWriteOutcome::RecoveredBackpressure)
    }
}

#[cfg(target_os = "linux")]
async fn open_fifo_writer(path: &Path) -> Result<NonblockingFifo, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let deadline = tokio::time::Instant::now() + FIFO_READY_TIMEOUT;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => {
                return Ok(NonblockingFifo {
                    file,
                    path: path.to_path_buf(),
                });
            }
            Err(error)
                if error.raw_os_error() == Some(nix::libc::ENXIO)
                    || error.kind() == std::io::ErrorKind::Interrupted =>
            {
                if tokio::time::Instant::now() >= deadline {
                    return Err("timed out waiting for microphone FIFO reader".to_string());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(target_os = "linux")]
fn write_fifo_frame(file: &mut std::fs::File, bytes: &[u8]) -> Result<bool, String> {
    use std::io::Write;

    loop {
        match file.write(bytes) {
            Ok(written) if written == bytes.len() => return Ok(true),
            Ok(_) => return Err("microphone FIFO produced a partial frame write".to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(target_os = "linux")]
fn drain_stale_fifo_audio(path: &Path) -> Result<(), String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| format!("open microphone FIFO for resynchronization: {error}"))?;
    let mut discarded = [0u8; 4096];
    loop {
        match reader.read(&mut discarded) {
            Ok(0) => break,
            Ok(_) => discarded.zeroize(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                discarded.zeroize();
                return Err(format!("resynchronize microphone FIFO: {error}"));
            }
        }
    }
    discarded.zeroize();
    Ok(())
}

#[cfg(target_os = "linux")]
async fn secure_module_fifo(path: &Path, expected_uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    let deadline = tokio::time::Instant::now() + FIFO_READY_TIMEOUT;
    loop {
        match std::fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_fifo()
                    && metadata.uid() == expected_uid
                    && metadata.nlink() == 1 =>
            {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| format!("secure microphone FIFO permissions: {error}"))?;
                let secured = std::fs::symlink_metadata(path)
                    .map_err(|error| format!("verify microphone FIFO permissions: {error}"))?;
                if secured.file_type().is_fifo()
                    && secured.uid() == expected_uid
                    && secured.nlink() == 1
                    && secured.mode() & 0o777 == 0o600
                {
                    return Ok(());
                }
                return Err("microphone FIFO identity or permissions changed".to_string());
            }
            Ok(_) => return Err("microphone FIFO identity is invalid".to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if tokio::time::Instant::now() >= deadline {
                    return Err("timed out waiting for microphone FIFO creation".to_string());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(format!("inspect microphone FIFO: {error}")),
        }
    }
}

#[cfg(target_os = "linux")]
fn open_nonblocking_stdin() -> Result<tokio::io::unix::AsyncFd<std::fs::File>, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let input = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open("/dev/stdin")
        .map_err(|error| format!("open nonblocking microphone input: {error}"))?;
    tokio::io::unix::AsyncFd::new(input)
        .map_err(|error| format!("register nonblocking microphone input: {error}"))
}

#[cfg(target_os = "linux")]
async fn read_framed_nonblocking_input(
    input: tokio::io::unix::AsyncFd<std::fs::File>,
    frames: mpsc::Sender<FramedMicrophonePacket>,
) {
    let mut length_bytes = [0u8; 4];
    loop {
        if read_nonblocking_exact(&input, &mut length_bytes)
            .await
            .is_err()
        {
            break;
        }
        let length = u32::from_le_bytes(length_bytes) as usize;
        if !(arcen_protocol::MICROPHONE_HEADER_SIZE..=MAX_FRAME_BYTES).contains(&length) {
            break;
        }
        let mut frame = FramedMicrophonePacket {
            bytes: [0; MAX_FRAME_BYTES],
            len: length,
        };
        if read_nonblocking_exact(&input, &mut frame.bytes[..length])
            .await
            .is_err()
            || frames.send(frame).await.is_err()
        {
            break;
        }
    }
}

#[cfg(target_os = "linux")]
async fn read_nonblocking_exact(
    input: &tokio::io::unix::AsyncFd<std::fs::File>,
    output: &mut [u8],
) -> std::io::Result<()> {
    use std::io::Read;

    let mut offset = 0;
    while offset < output.len() {
        let mut ready = input.readable().await?;
        match ready.try_io(|inner| {
            let mut file = inner.get_ref();
            file.read(&mut output[offset..])
        }) {
            Ok(Ok(0)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "microphone input closed",
                ));
            }
            Ok(Ok(read)) => offset += read,
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
async fn read_framed_input<R: tokio::io::AsyncRead + Unpin>(
    mut input: R,
    frames: mpsc::Sender<FramedMicrophonePacket>,
) {
    while let Ok(length) = input.read_u32_le().await {
        let length = length as usize;
        if !(arcen_protocol::MICROPHONE_HEADER_SIZE..=MAX_FRAME_BYTES).contains(&length) {
            break;
        }
        let mut frame = FramedMicrophonePacket {
            bytes: [0; MAX_FRAME_BYTES],
            len: length,
        };
        if input.read_exact(&mut frame.bytes[..length]).await.is_err()
            || frames.send(frame).await.is_err()
        {
            break;
        }
    }
}

#[cfg(target_os = "linux")]
struct SourceLease {
    pactl: PathBuf,
    journal_path: PathBuf,
    journal: Option<SourceJournal>,
}

#[cfg(target_os = "linux")]
impl SourceLease {
    fn update(&mut self, mutate: impl FnOnce(&mut SourceJournal)) -> Result<(), String> {
        let journal = self
            .journal
            .as_mut()
            .ok_or_else(|| "microphone source lease is already restored".to_string())?;
        mutate(journal);
        write_journal(&self.journal_path, journal)
    }

    async fn restore(mut self) -> Result<(), String> {
        let Some(mut journal) = self.journal.take() else {
            return Ok(());
        };
        restore_journal(&self.pactl, &self.journal_path, &mut journal).await
    }
}

#[cfg(target_os = "linux")]
async fn recover_previous(pactl: &Path, journal_path: &Path) -> Result<(), String> {
    tokio::time::timeout(
        CHILD_RECOVERY_TIMEOUT,
        recover_previous_within_deadline(pactl, journal_path),
    )
    .await
    .map_err(|_| "microphone recovery exceeded its deadline".to_string())?
}

#[cfg(target_os = "linux")]
async fn recover_previous_within_deadline(pactl: &Path, journal_path: &Path) -> Result<(), String> {
    let bytes = match read_recovery_journal(journal_path)? {
        Some(bytes) => bytes,
        None => return Ok(()),
    };
    let mut journal: SourceJournal = serde_json::from_slice(&bytes)
        .map_err(|_| "microphone recovery journal is invalid".to_string())?;
    if !matches!(journal.version, 1 | 2) {
        return Err("microphone recovery journal version is unsupported".to_string());
    }
    restore_journal_within_deadline(pactl, journal_path, &mut journal).await
}

#[cfg(target_os = "linux")]
fn read_recovery_journal(path: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open microphone recovery journal: {error}")),
    };
    if !file
        .metadata()
        .map_err(|error| format!("inspect microphone recovery journal: {error}"))?
        .is_file()
    {
        return Err("microphone recovery journal is not a regular file".to_string());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read microphone recovery journal: {error}"))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err("microphone recovery journal exceeded the safety bound".to_string());
    }
    Ok(Some(bytes))
}

#[cfg(target_os = "linux")]
async fn restore_journal(
    pactl: &Path,
    journal_path: &Path,
    journal: &mut SourceJournal,
) -> Result<(), String> {
    tokio::time::timeout(
        CHILD_RECOVERY_TIMEOUT,
        restore_journal_within_deadline(pactl, journal_path, journal),
    )
    .await
    .map_err(|_| "microphone restoration exceeded its deadline".to_string())?
}

#[cfg(target_os = "linux")]
struct RecoveryStageBudget {
    remaining: usize,
}

#[cfg(target_os = "linux")]
impl RecoveryStageBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_RECOVERY_PACTL_STAGES,
        }
    }

    fn consume(&mut self) -> Result<(), String> {
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| "microphone recovery exceeded its stage bound".to_string())?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn restore_journal_within_deadline(
    pactl: &Path,
    journal_path: &Path,
    journal: &mut SourceJournal,
) -> Result<(), String> {
    let mut errors = Vec::new();
    let mut budget = RecoveryStageBudget::new();
    if journal.default_mutation_started && !journal.default_restored {
        budget.consume()?;
        let restored = match pactl_status(
            pactl,
            &["set-default-source", journal.prior_default.as_str()],
        )
        .await
        {
            Ok(()) => true,
            Err(restore_error) => {
                budget.consume()?;
                match pactl_output(pactl, &["list", "short", "sources"]).await {
                    Ok(sources) if !source_inventory_contains(&sources, &journal.prior_default) => {
                        true
                    }
                    Ok(_) => {
                        errors.push(format!("restore prior default source: {restore_error}"));
                        false
                    }
                    Err(probe_error) => {
                        errors.push(format!(
                            "restore prior default source: {restore_error}; verify source absence: {probe_error}"
                        ));
                        false
                    }
                }
            }
        };
        if restored {
            journal.default_restored = true;
            if let Err(error) = write_journal(journal_path, journal) {
                errors.push(error);
            }
        }
    }
    if !journal.module_unloaded {
        let mut unloaded = !journal.module_load_started && journal.module_id.is_none();
        if !unloaded {
            budget.consume()?;
            match pactl_output(pactl, &["list", "short", "modules"]).await {
                Ok(modules) => match matching_module_ids(&modules, journal) {
                    Ok(matching) => {
                        let mut unload_failed = None;
                        for module_id in matching {
                            budget.consume()?;
                            if let Err(error) =
                                pactl_status(pactl, &["unload-module", &module_id.to_string()])
                                    .await
                            {
                                unload_failed = Some(error);
                                break;
                            }
                        }
                        budget.consume()?;
                        match pactl_output(pactl, &["list", "short", "modules"]).await {
                            Ok(modules) => match matching_module_ids(&modules, journal) {
                                Ok(matching) if matching.is_empty() => unloaded = true,
                                Ok(_) => errors.push(format!(
                                    "unload microphone module: {}",
                                    unload_failed.unwrap_or_else(|| {
                                        "verified module remained loaded".to_string()
                                    })
                                )),
                                Err(error) => errors.push(error),
                            },
                            Err(error) => {
                                errors.push(format!("verify microphone module unload: {error}"));
                            }
                        }
                    }
                    Err(error) => {
                        errors.push(error);
                    }
                },
                Err(error) => errors.push(format!("inspect microphone modules: {error}")),
            }
        }
        if unloaded {
            journal.module_unloaded = true;
            if let Err(error) = write_journal(journal_path, journal) {
                errors.push(error);
            }
        }
    }
    if !journal.fifo_removed {
        let removed = match std::fs::remove_file(&journal.fifo_path) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                errors.push(format!("remove microphone FIFO: {error}"));
                false
            }
        };
        if removed {
            journal.fifo_removed = true;
            if let Err(error) = write_journal(journal_path, journal) {
                errors.push(error);
            }
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    std::fs::remove_file(journal_path)
        .map_err(|error| format!("remove microphone journal: {error}"))
}

fn source_inventory_contains(inventory: &str, source_name: &str) -> bool {
    inventory
        .lines()
        .any(|line| line.split_ascii_whitespace().nth(1) == Some(source_name))
}

fn module_inventory_contains(inventory: &str, module_id: &str) -> bool {
    inventory
        .lines()
        .any(|line| line.split_ascii_whitespace().next() == Some(module_id))
}

fn matching_module_ids(inventory: &str, journal: &SourceJournal) -> Result<Vec<u32>, String> {
    let matching = inventory
        .lines()
        .filter_map(|line| {
            let mut columns = line.splitn(3, '\t');
            let id = columns.next()?.trim().parse::<u32>().ok()?;
            let module = columns.next()?.trim();
            let arguments = columns.next().unwrap_or_default();
            (module == PIPE_MODULE_NAME && module_arguments_match(arguments, journal)).then_some(id)
        })
        .take(MAX_RECOVERY_MODULES + 1)
        .collect::<Vec<_>>();
    if matching.len() > MAX_RECOVERY_MODULES {
        Err("microphone module inventory exceeded the recovery bound".to_string())
    } else {
        Ok(matching)
    }
}

fn module_arguments_match(arguments: &str, journal: &SourceJournal) -> bool {
    let expected = [
        format!("source_name={}", journal.source_name),
        format!("file={}", journal.fifo_path.display()),
        "format=s16le".to_string(),
        "rate=48000".to_string(),
        "channels=1".to_string(),
    ];
    let property = format!("source_properties={PIPE_SOURCE_PROPERTIES}");
    let tokens = arguments.split_ascii_whitespace().collect::<Vec<_>>();
    expected
        .iter()
        .all(|expected| tokens.iter().any(|token| *token == expected))
        && (journal.version == 1 || tokens.contains(&property.as_str()))
}

#[cfg(target_os = "linux")]
fn write_journal(path: &Path, journal: &SourceJournal) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec(journal)
        .map_err(|error| format!("serialize microphone journal: {error}"))?;
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove stale microphone journal: {error}")),
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|error| format!("create microphone journal: {error}"))?;
    use std::io::Write;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("persist microphone journal: {error}"))?;
    std::fs::rename(&temporary, path).map_err(|error| format!("arm microphone journal: {error}"))
}

#[cfg(target_os = "linux")]
async fn pactl_output(pactl: &Path, args: &[&str]) -> Result<String, String> {
    let (status, stdout) = run_pactl(pactl, args, true).await?;
    if !status.success() {
        return Err("pactl operation failed".to_string());
    }
    String::from_utf8(stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "pactl returned non-UTF-8 output".to_string())
}

#[cfg(target_os = "linux")]
async fn pactl_status(pactl: &Path, args: &[&str]) -> Result<(), String> {
    let (status, _) = run_pactl(pactl, args, false).await?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "pactl operation failed".to_string())
}

#[cfg(target_os = "linux")]
async fn run_pactl(
    pactl: &Path,
    args: &[&str],
    capture_stdout: bool,
) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    let mut command = Command::new(pactl);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command.stdout(if capture_stdout {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = command
        .spawn()
        .map_err(|error| format!("run pactl: {error}"))?;
    let reader = child.stdout.take().map(|mut stdout| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            (&mut stdout)
                .take(MAX_PACTL_OUTPUT_BYTES + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|error| format!("read pactl output: {error}"))?;
            if bytes.len() as u64 > MAX_PACTL_OUTPUT_BYTES {
                return Err("pactl output exceeded the safety bound".to_string());
            }
            Ok(bytes)
        })
    });
    let status = match tokio::time::timeout(PACTL_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            return Err(format!("wait for pactl: {error}"));
        }
        Err(_) => {
            let _ = child.start_kill();
            match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => return Err(format!("reap timed-out pactl: {error}")),
                Err(_) => return Err("timed-out pactl could not be reaped".to_string()),
            }
            return Err("pactl operation timed out".to_string());
        }
    };
    let stdout = match reader {
        Some(reader) => tokio::time::timeout(Duration::from_secs(1), reader)
            .await
            .map_err(|_| "pactl output drain timed out".to_string())?
            .map_err(|error| format!("join pactl output reader: {error}"))??,
        None => Vec::new(),
    };
    Ok((status, stdout))
}

fn argument_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find_map(|pair| (pair[0] == name).then(|| pair[1].clone()))
}

async fn terminate_process_group(child: &mut Child) -> Result<(), String> {
    if !matches!(child.try_wait(), Ok(None)) {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        use nix::sys::signal::{killpg, Signal};
        use nix::unistd::Pid;

        let pid = child
            .id()
            .ok_or_else(|| "microphone helper has no process id".to_string())?;
        match killpg(Pid::from_raw(pid as i32), Signal::SIGKILL) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => {
                let _ = child.start_kill();
                Err(format!("kill microphone helper process group: {error}"))
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        child
            .start_kill()
            .map_err(|error| format!("kill microphone helper: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_warning_limiter_reports_suppressed_count_once_per_interval() {
        let start = Instant::now();
        let mut limiter = RateLimitedMicrophoneWarning::default();
        assert_eq!(limiter.observe_at(start), Some(0));
        assert_eq!(limiter.observe_at(start + Duration::from_secs(1)), None);
        assert_eq!(limiter.observe_at(start + Duration::from_secs(9)), None);
        assert_eq!(
            limiter.observe_at(start + arcen_media::audio::MICROPHONE_STATS_INTERVAL),
            Some(2)
        );
        assert_eq!(
            limiter.observe_at(
                start + arcen_media::audio::MICROPHONE_STATS_INTERVAL + Duration::from_secs(1)
            ),
            None
        );
    }

    #[test]
    fn microphone_warning_limiters_are_independent_by_rejection_class() {
        let start = Instant::now();
        let mut sequence = RateLimitedMicrophoneWarning::default();
        let mut decode = RateLimitedMicrophoneWarning::default();
        let mut protocol = RateLimitedMicrophoneWarning::default();
        assert_eq!(sequence.observe_at(start), Some(0));
        assert_eq!(sequence.observe_at(start + Duration::from_secs(1)), None);
        assert_eq!(decode.observe_at(start + Duration::from_secs(1)), Some(0));
        assert_eq!(protocol.observe_at(start + Duration::from_secs(1)), Some(0));
        assert_eq!(
            sequence.observe_at(start + MICROPHONE_STATS_INTERVAL),
            Some(1)
        );
        assert_eq!(decode.observe_at(start + MICROPHONE_STATS_INTERVAL), None);
    }

    #[test]
    fn child_arguments_are_not_shell_parsed() {
        let args = vec![
            "agent".to_string(),
            "--pactl-bin".to_string(),
            "/usr/bin/pactl;touch /tmp/no".to_string(),
        ];
        assert_eq!(
            argument_value(&args, "--pactl-bin").as_deref(),
            Some("/usr/bin/pactl;touch /tmp/no")
        );
    }

    #[test]
    fn parent_readiness_deadline_covers_all_child_setup_deadlines() {
        assert!(
            PARENT_READY_TIMEOUT
                > CHILD_RECOVERY_TIMEOUT + CHILD_SETUP_TIMEOUT + FIFO_READY_TIMEOUT
        );
        assert!(
            RECOVERY_HELPER_TIMEOUT > CHILD_RECOVERY_TIMEOUT + RECOVERY_REAP_TIMEOUT,
            "the parent must leave enough time for the child's full recovery and reaping overhead"
        );
        assert_eq!(
            RECOVERY_WORST_CASE,
            (RECOVERY_HELPER_TIMEOUT + RECOVERY_REAP_TIMEOUT) * RECOVERY_ATTEMPTS as u32
                + RECOVERY_BACKOFF_TOTAL
        );
        assert_eq!(
            CLEANUP_WORST_CASE,
            GRACEFUL_EXIT_TIMEOUT
                + FORCED_REAP_TIMEOUT
                + WRITER_REAP_TIMEOUT
                + TELEMETRY_REAP_TIMEOUT
                + RECOVERY_WORST_CASE
        );
        assert!(MICROPHONE_CLEANUP_BOUND > CLEANUP_WORST_CASE);
        assert!(PARENT_READY_TIMEOUT > MICROPHONE_CLEANUP_BOUND);
        assert_eq!(
            MAX_RECOVERY_PACTL_STAGES,
            MAX_RECOVERY_MODULES + 4,
            "default restoration, inventories, and unloads must have a fixed stage ceiling"
        );
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_cancel_the_lifecycle_task() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(run_cancellation_safe(async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            let _ = completed_tx.send(());
            Ok(())
        }));

        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), completed_rx)
            .await
            .expect("detached lifecycle must finish")
            .expect("completion signal");
    }

    #[test]
    fn helper_telemetry_schema_rejects_private_fields() {
        let event = HelperTelemetry::Stats {
            codec: AudioCodec::Pcm,
            generation: 7,
            stats: HelperStats::from(MicrophoneStats::default()),
            jitter_depth: 0,
            final_snapshot: false,
            duration_ms: 10,
            stop_reason: HelperStopReason::Running,
        };
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(!encoded.contains("path"));
        assert!(!encoded.contains("stderr"));
        assert!(!encoded.contains("sid"));
        assert!(encoded.len() < MAX_HELPER_TELEMETRY_BYTES);
        let rejection = serde_json::to_string(&HelperTelemetry::FrameRejected {
            class: HelperRejectionClass::Decode,
            generation: 7,
            suppressed_since_last: 3,
        })
        .unwrap();
        assert!(!rejection.contains("path"));
        assert!(!rejection.contains("stderr"));
        assert!(!rejection.contains("sid"));
        assert!(rejection.len() < MAX_HELPER_TELEMETRY_BYTES);
    }

    #[test]
    fn helper_telemetry_enqueue_is_bounded_and_nonblocking() {
        let (sender, mut receiver) = mpsc::channel(1);
        assert!(try_emit_helper_telemetry(
            &sender,
            HelperTelemetry::SourceReady {
                codec: AudioCodec::Pcm,
                generation: 7,
            }
        ));
        assert!(!try_emit_helper_telemetry(
            &sender,
            HelperTelemetry::SourceRestored
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(HelperTelemetry::SourceReady { generation: 7, .. })
        ));
    }

    #[tokio::test]
    async fn helper_telemetry_reader_bounds_and_drains_oversized_lines() {
        let mut input = vec![b'x'; MAX_HELPER_TELEMETRY_BYTES + 1];
        input.extend_from_slice(b"\n{\"event\":\"source_restored\"}\n");
        let mut reader = tokio::io::BufReader::new(input.as_slice());
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_helper_line(&mut reader, &mut line)
                .await
                .unwrap(),
            Some(false)
        );
        assert!(line.len() <= MAX_HELPER_TELEMETRY_BYTES);
        line.zeroize();
        line.clear();
        assert_eq!(
            read_bounded_helper_line(&mut reader, &mut line)
                .await
                .unwrap(),
            Some(true)
        );
        assert!(matches!(
            serde_json::from_slice::<HelperTelemetry>(&line),
            Ok(HelperTelemetry::SourceRestored)
        ));
    }

    #[test]
    fn failed_cleanup_outcome_never_verifies_restoration() {
        assert!(MicrophoneCleanupOutcome::Graceful.restoration_verified());
        assert!(MicrophoneCleanupOutcome::Forced.restoration_verified());
        assert!(
            !MicrophoneCleanupOutcome::Failure("fake restore failure".to_string())
                .restoration_verified()
        );
    }

    #[tokio::test]
    async fn framed_reader_rejects_oversized_input_without_forwarding() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(u32::try_from(MAX_FRAME_BYTES + 1).unwrap()).to_le_bytes());
        let (tx, mut rx) = mpsc::channel(1);
        read_framed_input(bytes.as_slice(), tx).await;
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn module_inventory_requires_an_exact_module_id() {
        let inventory =
            "9\tmodule-native-protocol-unix\t\n42\tmodule-pipe-source\tfile=/run/user/source\n";
        assert!(module_inventory_contains(inventory, "42"));
        assert!(!module_inventory_contains(inventory, "4"));
        assert!(!module_inventory_contains(inventory, "43"));
    }

    #[test]
    fn recovery_matches_exact_module_arguments_and_never_trusts_reused_id() {
        let journal = SourceJournal {
            version: 2,
            prior_default: "alsa_input.usb".to_string(),
            source_name: "arcen_microphone_77".to_string(),
            module_id: Some(42),
            fifo_path: PathBuf::from("/run/user/1000/arcen-microphone-77.pcm"),
            module_load_started: true,
            default_mutation_started: true,
            default_restored: false,
            module_unloaded: false,
            fifo_removed: false,
        };
        let inventory = concat!(
            "42\tmodule-pipe-source\tsource_name=someone_else file=/run/user/1000/other.pcm format=s16le rate=48000 channels=1 source_properties=device.description=Arcen_Microphone\n",
            "73\tmodule-pipe-source\tsource_name=arcen_microphone_77 file=/run/user/1000/arcen-microphone-77.pcm format=s16le rate=48000 channels=1 source_properties=device.description=Arcen_Microphone\n",
        );
        assert_eq!(matching_module_ids(inventory, &journal).unwrap(), vec![73]);
    }

    #[test]
    fn recovery_rejects_similar_module_without_exact_source_properties() {
        let journal = SourceJournal {
            version: 2,
            prior_default: "alsa_input.usb".to_string(),
            source_name: "arcen_microphone_8".to_string(),
            module_id: None,
            fifo_path: PathBuf::from("/run/user/1000/arcen-microphone-8.pcm"),
            module_load_started: true,
            default_mutation_started: false,
            default_restored: false,
            module_unloaded: false,
            fifo_removed: false,
        };
        let inventory = "8\tmodule-pipe-source\tsource_name=arcen_microphone_8 file=/run/user/1000/arcen-microphone-8.pcm format=s16le rate=48000 channels=1 source_properties=device.description=Other\n";
        assert!(matching_module_ids(inventory, &journal).unwrap().is_empty());
    }

    #[test]
    fn recovery_rejects_more_matching_modules_than_its_inventory_bound() {
        let journal = SourceJournal {
            version: 2,
            prior_default: "alsa_input.usb".to_string(),
            source_name: "arcen_microphone_8".to_string(),
            module_id: None,
            fifo_path: PathBuf::from("/run/user/1000/arcen-microphone-8.pcm"),
            module_load_started: true,
            default_mutation_started: false,
            default_restored: false,
            module_unloaded: false,
            fifo_removed: false,
        };
        let arguments = "source_name=arcen_microphone_8 file=/run/user/1000/arcen-microphone-8.pcm format=s16le rate=48000 channels=1 source_properties=device.description=Arcen_Microphone";
        let inventory = (0..=MAX_RECOVERY_MODULES)
            .map(|id| format!("{id}\t{PIPE_MODULE_NAME}\t{arguments}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(matching_module_ids(&inventory, &journal).is_err());
    }

    #[test]
    fn source_inventory_requires_an_exact_source_name() {
        let inventory = "3\talsa_input.usb-headset\tmodule-alsa-card.c\n7\tarcen_microphone_5\tmodule-pipe-source.c\n";
        assert!(source_inventory_contains(
            inventory,
            "alsa_input.usb-headset"
        ));
        assert!(!source_inventory_contains(inventory, "alsa_input.usb"));
        assert!(!source_inventory_contains(inventory, "missing"));
    }

    #[test]
    fn stopped_publisher_rejects_frames_without_queueing_them() {
        let (frames, mut receiver) = mpsc::channel(1);
        let (stop, _stopped) = watch::channel(false);
        let sender = MicrophoneFrameSender {
            frames,
            active: Arc::new(AtomicBool::new(true)),
            stop,
        };
        sender.stop();
        assert_eq!(
            sender.try_send(vec![0x5a; 32]),
            Err(MicrophoneSendError::Closed)
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn cleanup_tracker_waits_for_detached_reapers() {
        let cleanup = register_cleanup(CleanupIdentity {
            uid: 1000,
            journal_path: PathBuf::from("/run/user/1000").join(JOURNAL_NAME),
        });
        let waiter = tokio::spawn(wait_for_cleanup(Duration::from_secs(1)));
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        cleanup.complete(true);
        assert_eq!(waiter.await.unwrap(), MicrophoneCleanupDrain::Drained);
    }

    #[test]
    fn cleanup_waiter_enrolls_before_observing_active_reapers() {
        let source = include_str!("microphone_input.rs");
        let start = source.find("pub async fn wait_for_cleanup").unwrap();
        let rest = &source[start..];
        let body = &rest[..rest.find("\npub async fn probe_backend").unwrap()];
        let enrolled = body.find("idle.as_mut().enable()").unwrap();
        let observed = body.find("state.active.load(Ordering::Acquire)").unwrap();
        assert!(enrolled < observed);
    }

    #[test]
    fn failed_cleanup_registration_is_not_drained() {
        let state = Arc::new(MicrophoneCleanupState::default());
        let identity = CleanupIdentity {
            uid: 1000,
            journal_path: PathBuf::from("/run/user/1000").join(JOURNAL_NAME),
        };
        state.active.store(1, Ordering::Release);
        drop(MicrophoneCleanupGuard {
            state: Arc::clone(&state),
            identity: identity.clone(),
            completed: false,
        });
        assert_eq!(state.active.load(Ordering::Acquire), 0);
        assert!(state.has_unresolved());
        assert!(state.unresolved.lock().unwrap().contains(&identity));
    }

    #[test]
    fn verified_cleanup_resolves_only_the_matching_user_and_journal() {
        fn complete(
            state: &Arc<MicrophoneCleanupState>,
            identity: CleanupIdentity,
            verified: bool,
        ) {
            state.active.fetch_add(1, Ordering::AcqRel);
            MicrophoneCleanupGuard {
                state: Arc::clone(state),
                identity,
                completed: false,
            }
            .complete(verified);
        }

        let state = Arc::new(MicrophoneCleanupState::default());
        let user_a = CleanupIdentity {
            uid: 1000,
            journal_path: PathBuf::from("/run/user/1000").join(JOURNAL_NAME),
        };
        let user_b = CleanupIdentity {
            uid: 1001,
            journal_path: PathBuf::from("/run/user/1001").join(JOURNAL_NAME),
        };

        complete(&state, user_a.clone(), false);
        complete(&state, user_b, true);
        assert!(state.unresolved.lock().unwrap().contains(&user_a));

        complete(&state, user_a.clone(), true);
        complete(&state, user_a, true);
        assert!(!state.has_unresolved());
    }

    #[test]
    fn helper_diagnostics_are_counted_without_forwarding_private_text() {
        let source = include_str!("microphone_input.rs");
        let forbidden_forward = ["message = %", "line"].concat();
        assert!(!source.contains(&forbidden_forward));
        assert!(source.contains("diagnostic_lines = line_count"));
    }
}
