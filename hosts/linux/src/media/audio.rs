//! Supervision of the native PulseAudio/PipeWire `arcen-audiocap` helper.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arcen_telemetry::CorrelationId;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::logging::target;
use crate::session::audio::{AudioFrameEncoder, AudioQueue, EncodeOutcome};
use crate::session::identity::UserExecution;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u8 = 2;
const CHUNK_MS: u32 = 20;
const BYTES_PER_SAMPLE: usize = 2;
const CHUNK_BYTES: usize =
    SAMPLE_RATE as usize * CHANNELS as usize * BYTES_PER_SAMPLE * CHUNK_MS as usize / 1000;
const CAPTURE_GAP_THRESHOLD: Duration = Duration::from_millis(100);
const IDLE_NOTICE_AFTER: Duration = Duration::from_millis(500);
const CHILD_LIVENESS_INTERVAL: Duration = Duration::from_secs(30);
const RESTART_DELAY_MIN: Duration = Duration::from_millis(500);
const RESTART_DELAY_MAX: Duration = Duration::from_secs(4);
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(10);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const STDERR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Copy)]
struct SupervisorPolicy {
    idle_notice_after: Duration,
    child_liveness_interval: Duration,
    restart_delay_min: Duration,
    restart_delay_max: Duration,
    backoff_reset_after: Duration,
    child_reap_timeout: Duration,
}

const SUPERVISOR_POLICY: SupervisorPolicy = SupervisorPolicy {
    idle_notice_after: IDLE_NOTICE_AFTER,
    child_liveness_interval: CHILD_LIVENESS_INTERVAL,
    restart_delay_min: RESTART_DELAY_MIN,
    restart_delay_max: RESTART_DELAY_MAX,
    backoff_reset_after: BACKOFF_RESET_AFTER,
    child_reap_timeout: CHILD_REAP_TIMEOUT,
};

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub binary: PathBuf,
    pub execution: Option<UserExecution>,
    pub session_log_id: CorrelationId,
}

#[derive(Debug, Default)]
pub struct AudioStats {
    captured_frames: AtomicU64,
    restarts: AtomicU64,
    restart_failures: AtomicU64,
    capture_gaps: AtomicU64,
    idle_periods: AtomicU64,
    termination_failures: AtomicU64,
    #[cfg(test)]
    helper_pids: Mutex<Vec<u32>>,
}

impl AudioStats {
    pub fn captured_frames(&self) -> u64 {
        self.captured_frames.load(Ordering::Relaxed)
    }
    pub fn restarts(&self) -> u64 {
        self.restarts.load(Ordering::Relaxed)
    }
    pub fn restart_failures(&self) -> u64 {
        self.restart_failures.load(Ordering::Relaxed)
    }
    /// Capture gaps over 100ms are an honest host-side underrun-risk proxy. The
    /// actual CoreAudio playback underrun counter belongs to the client.
    pub fn capture_gaps(&self) -> u64 {
        self.capture_gaps.load(Ordering::Relaxed)
    }
    pub fn idle_periods(&self) -> u64 {
        self.idle_periods.load(Ordering::Relaxed)
    }
    pub fn termination_failures(&self) -> u64 {
        self.termination_failures.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn record_helper(&self, pid: Option<u32>) {
        if let Some(pid) = pid {
            self.helper_pids.lock().unwrap().push(pid);
        }
    }

    #[cfg(test)]
    fn helper_pids(&self) -> Vec<u32> {
        self.helper_pids.lock().unwrap().clone()
    }

    #[cfg(not(test))]
    fn record_helper(&self, _pid: Option<u32>) {}
}

pub struct AudioSession {
    shutdown_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
    stats: Arc<AudioStats>,
    close_queue_on_shutdown: Arc<AtomicBool>,
}

impl AudioSession {
    pub fn stats(&self) -> Arc<AudioStats> {
        self.stats.clone()
    }

    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        if tokio::time::timeout(SESSION_SHUTDOWN_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }

    pub async fn shutdown_preserving_queue(mut self) {
        self.close_queue_on_shutdown.store(false, Ordering::Release);
        let _ = self.shutdown_tx.send(true);
        if tokio::time::timeout(SESSION_SHUTDOWN_TIMEOUT, &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
            let _ = (&mut self.task).await;
        }
    }
}

impl Drop for AudioSession {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
    }
}

pub fn spawn(
    config: AudioConfig,
    queue: Arc<AudioQueue>,
    encoder: Arc<Mutex<AudioFrameEncoder>>,
    failure_tx: mpsc::Sender<arcen_protocol::messages::AudioStreamReason>,
) -> std::io::Result<AudioSession> {
    spawn_with_policy(config, queue, encoder, failure_tx, SUPERVISOR_POLICY)
}

fn spawn_with_policy(
    config: AudioConfig,
    queue: Arc<AudioQueue>,
    encoder: Arc<Mutex<AudioFrameEncoder>>,
    failure_tx: mpsc::Sender<arcen_protocol::messages::AudioStreamReason>,
    policy: SupervisorPolicy,
) -> std::io::Result<AudioSession> {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let stats = Arc::new(AudioStats::default());
    let child = spawn_child(&config)?;
    stats.record_helper(child.id());
    let close_queue_on_shutdown = Arc::new(AtomicBool::new(true));
    let span = tracing::info_span!(
        target: target::AUDIO,
        "audiocap_helper",
        sid = %config.session_log_id
    );
    let task = tokio::spawn(
        supervise(
            child,
            config,
            queue,
            encoder,
            failure_tx,
            shutdown_rx,
            stats.clone(),
            Arc::clone(&close_queue_on_shutdown),
            policy,
        )
        .instrument(span),
    );
    Ok(AudioSession {
        shutdown_tx,
        task,
        stats,
        close_queue_on_shutdown,
    })
}

fn spawn_child(config: &AudioConfig) -> std::io::Result<Child> {
    let mut command = crate::command_for_helper(&config.binary, "audiocap");
    command
        .args([
            "--sample-rate",
            "48000",
            "--channels",
            "2",
            "--chunk-ms",
            "20",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(execution) = &config.execution {
        execution
            .configure(&mut command)
            .map_err(std::io::Error::other)?;
    }
    command.env("ARCEN_SESSION_LOG_ID", config.session_log_id.as_str());

    let child = command.spawn()?;
    tracing::info!(
        target: target::AUDIO,
        binary = %config.binary.display(),
        user = config
            .execution
            .as_ref()
            .map_or("<current>", |execution| execution.identity.username.as_str()),
        sample_rate = SAMPLE_RATE,
        channels = CHANNELS,
        chunk_ms = CHUNK_MS,
        "native audiocap started"
    );
    Ok(child)
}

async fn supervise(
    mut child: Child,
    config: AudioConfig,
    queue: Arc<AudioQueue>,
    encoder: Arc<Mutex<AudioFrameEncoder>>,
    failure_tx: mpsc::Sender<arcen_protocol::messages::AudioStreamReason>,
    mut shutdown: watch::Receiver<bool>,
    stats: Arc<AudioStats>,
    close_queue_on_shutdown: Arc<AtomicBool>,
    policy: SupervisorPolicy,
) {
    let mut restart_delay = policy.restart_delay_min;
    let mut child_started = Instant::now();
    let mut child_liveness = tokio::time::interval(policy.child_liveness_interval);
    child_liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let mut stdout = child.stdout.take().expect("audiocap stdout piped");
        let stderr = child.stderr.take().expect("audiocap stderr piped");
        let stderr_task = tokio::spawn(read_stderr(stderr));
        let mut pcm = vec![0u8; CHUNK_BYTES];
        let mut last_chunk = None;
        let mut timestamp_ms: Option<u32> = None;
        let mut captured_since_spawn = false;
        let restart = 'capture: loop {
            let result = {
                let read = stdout.read_exact(&mut pcm);
                tokio::pin!(read);
                let idle_notice = tokio::time::sleep(policy.idle_notice_after);
                tokio::pin!(idle_notice);
                let mut idle_reported = false;
                loop {
                    tokio::select! {
                        biased;
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                terminate_child(
                                    &mut child,
                                    policy.child_reap_timeout,
                                    stats.as_ref(),
                                )
                                .await;
                                finish_stderr_task(stderr_task).await;
                                close_queue_if_requested(&queue, &close_queue_on_shutdown);
                                return;
                            }
                        }
                        result = &mut read => break result,
                        _ = &mut idle_notice, if !idle_reported => {
                            idle_reported = true;
                            let idle_periods =
                                stats.idle_periods.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::info!(
                                target: target::AUDIO,
                                idle_after_ms = policy.idle_notice_after.as_millis() as u64,
                                idle_periods,
                                "audiocap monitor idle; waiting without restarting"
                            );
                        }
                        _ = child_liveness.tick(), if idle_reported => {
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    tracing::warn!(
                                        target: target::AUDIO,
                                        %status,
                                        "audiocap exited while its stdout remained open; restarting"
                                    );
                                    break 'capture true;
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    tracing::warn!(
                                        target: target::AUDIO,
                                        %error,
                                        "audiocap liveness check failed; restarting"
                                    );
                                    break 'capture true;
                                }
                            }
                        }
                    }
                }
            };
            match result {
                Ok(_) => {
                    let now = Instant::now();
                    if let Some(previous) = last_chunk {
                        let elapsed = now.duration_since(previous);
                        if is_capture_gap(elapsed) {
                            let capture_gaps =
                                stats.capture_gaps.fetch_add(1, Ordering::Relaxed) + 1;
                            timestamp_ms = None;
                            tracing::warn!(
                                target: target::AUDIO,
                                gap_ms = elapsed.as_millis() as u64,
                                capture_gaps,
                                "audiocap resumed after a capture gap"
                            );
                        }
                    }
                    last_chunk = Some(now);
                    captured_since_spawn = true;
                    stats.captured_frames.fetch_add(1, Ordering::Relaxed);
                    let frame_timestamp_ms = match timestamp_ms {
                        Some(previous) => previous.wrapping_add(CHUNK_MS as u32),
                        None => super::now_ms_u32(),
                    };
                    timestamp_ms = Some(frame_timestamp_ms);
                    let outcome = encoder.lock().expect("audio encoder poisoned").encode(
                        &pcm,
                        frame_timestamp_ms,
                        &queue,
                    );
                    if let EncodeOutcome::Disabled(reason) = outcome {
                        let _ = failure_tx.try_send(reason);
                    }
                }
                Err(error) => {
                    tracing::warn!(target: target::AUDIO, %error, "audiocap stdout ended; restarting");
                    break true;
                }
            }
        };
        if !restart {
            break;
        }
        if !terminate_child(&mut child, policy.child_reap_timeout, stats.as_ref()).await {
            finish_stderr_task(stderr_task).await;
            queue.close();
            return;
        }
        finish_stderr_task(stderr_task).await;
        let restarts = stats.restarts.fetch_add(1, Ordering::Relaxed) + 1;
        if captured_since_spawn && child_started.elapsed() >= policy.backoff_reset_after {
            restart_delay = policy.restart_delay_min;
        }
        let delay = restart_delay;
        restart_delay = next_restart_delay(restart_delay, policy.restart_delay_max);
        tracing::info!(
            target: target::AUDIO,
            restarts,
            retry_delay_ms = delay.as_millis() as u64,
            "audiocap helper reaped; scheduling restart"
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    close_queue_if_requested(&queue, &close_queue_on_shutdown);
                    return;
                }
            }
        }
        loop {
            match spawn_child(&config) {
                Ok(new_child) => {
                    if let Err(reason) = encoder
                        .lock()
                        .expect("audio encoder poisoned")
                        .reset_after_capture_restart()
                    {
                        let _ = failure_tx.try_send(reason);
                        queue.close();
                        return;
                    }
                    stats.record_helper(new_child.id());
                    child = new_child;
                    child_started = Instant::now();
                    break;
                }
                Err(error) => {
                    stats.restart_failures.fetch_add(1, Ordering::Relaxed);
                    let delay = restart_delay;
                    restart_delay = next_restart_delay(restart_delay, policy.restart_delay_max);
                    tracing::warn!(
                        target: target::AUDIO,
                        %error,
                        retry_delay_ms = delay.as_millis() as u64,
                        "audiocap restart failed"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() {
                                close_queue_if_requested(&queue, &close_queue_on_shutdown);
                                return;
                            }
                        }
                    }
                }
            }
        }
    }
    close_queue_if_requested(&queue, &close_queue_on_shutdown);
}

fn close_queue_if_requested(queue: &AudioQueue, close_queue_on_shutdown: &AtomicBool) {
    if close_queue_on_shutdown.load(Ordering::Acquire) {
        queue.close();
    } else {
        queue.clear();
    }
}

fn is_capture_gap(elapsed: Duration) -> bool {
    elapsed > CAPTURE_GAP_THRESHOLD
}

fn next_restart_delay(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}

async fn terminate_child(child: &mut Child, timeout: Duration, stats: &AudioStats) -> bool {
    let pid = child.id();
    if let Err(error) = child.start_kill() {
        tracing::debug!(
            target: target::AUDIO,
            ?pid,
            %error,
            "audiocap kill raced with helper exit"
        );
    }
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            tracing::debug!(
                target: target::AUDIO,
                ?pid,
                %status,
                "audiocap helper reaped"
            );
            true
        }
        Ok(Err(error)) => {
            let failures = stats.termination_failures.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                target: target::AUDIO,
                ?pid,
                %error,
                failures,
                "failed to reap audiocap helper"
            );
            false
        }
        Err(_) => {
            let failures = stats.termination_failures.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                target: target::AUDIO,
                ?pid,
                timeout_ms = timeout.as_millis() as u64,
                failures,
                "timed out reaping audiocap helper; refusing overlapping restart"
            );
            false
        }
    }
}

async fn finish_stderr_task(mut task: JoinHandle<()>) {
    if tokio::time::timeout(STDERR_SHUTDOWN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

async fn read_stderr<R>(stderr: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = crate::bounded_io::BoundedLineReader::new(BufReader::new(stderr));
    while let Ok(Some(line)) = reader.next_bounded_line().await {
        if line.truncated {
            tracing::warn!(
                target: target::AUDIO,
                "audiocap stderr line exceeded the bounded read limit; excess bytes discarded"
            );
        }
        let text = crate::eventlog::bounded_diagnostic_line(&line.text);
        tracing::info!(target: target::AUDIO, "{text}");
    }
}

pub fn find_audiocap_binary() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(path) = std::env::var("ARCEN_AUDIOCAP") {
        candidates.push(PathBuf::from(path));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("server/audiocap/target/release/arcen-audiocap"));
    }
    if let Ok(mut executable) = std::env::current_exe() {
        executable.pop();
        candidates.push(executable.join("arcen-audiocap"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static ARTIFACT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn audiocap_stderr_bounds_an_enormous_unterminated_line_and_keeps_reading() {
        // Same integration proof as the other helper stderr paths: an
        // enormous, never-terminated stderr line from audiocap must not
        // hang the reader or grow memory with the input.
        let (reader, mut writer) = tokio::io::duplex(64 * 1024);
        let writer_task = tokio::spawn(async move {
            let huge = vec![b'z'; 1024 * 1024];
            tokio::io::AsyncWriteExt::write_all(&mut writer, &huge)
                .await
                .unwrap();
            tokio::io::AsyncWriteExt::write_all(&mut writer, b"\nafter\n")
                .await
                .unwrap();
        });
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), read_stderr(reader)).await;
        assert!(
            result.is_ok(),
            "reading an enormous unterminated line must complete promptly, not hang"
        );
        writer_task.await.unwrap();
    }

    #[cfg(unix)]
    fn test_directory(name: &str) -> PathBuf {
        let sequence = ARTIFACT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/arcen-test-artifacts")
            .join(format!("audiocap-{name}-{}-{sequence}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[cfg(unix)]
    fn write_test_script(directory: &std::path::Path, body: &str) -> PathBuf {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("arcen-audiocap");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

        // Wait until the kernel will actually exec it. Writing a file and
        // immediately running it races with fork in a multi-threaded process:
        // a thread that forks while the write descriptor is open leaves the
        // child holding a copy, and the exec fails with ETXTBSY. Here the
        // supervisor would see the helper fail to start, count a restart, and
        // the test would fail an assertion about restarts or helper pids —
        // roughly one run in six, with a message pointing nowhere near the
        // cause.
        //
        // Probing with spawn rather than output matters: these stubs
        // `exec sleep 999`, so waiting for one to finish would hang the suite.
        const ETXTBSY: i32 = 26;
        for _ in 0..50 {
            match std::process::Command::new(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Err(error) if error.raw_os_error() == Some(ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        path
    }

    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn test_policy() -> SupervisorPolicy {
        SupervisorPolicy {
            idle_notice_after: Duration::from_millis(100),
            child_liveness_interval: Duration::from_millis(50),
            restart_delay_min: Duration::from_millis(20),
            restart_delay_max: Duration::from_millis(80),
            backoff_reset_after: Duration::from_millis(250),
            child_reap_timeout: Duration::from_secs(1),
        }
    }

    fn test_encoder() -> (
        Arc<Mutex<AudioFrameEncoder>>,
        mpsc::Sender<arcen_protocol::messages::AudioStreamReason>,
    ) {
        let policy = arcen_media::audio::AudioPolicy {
            opus_available: false,
            pcm_available: true,
        };
        let encoder =
            AudioFrameEncoder::new(policy.resolve(None, true, 128)).expect("test PCM encoder");
        let (failure_tx, _failure_rx) = mpsc::channel(1);
        (Arc::new(Mutex::new(encoder)), failure_tx)
    }

    async fn wait_for_counter(counter: impl Fn() -> u64, minimum: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while counter() < minimum {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn pcm_chunk_contract_is_20ms_48k_stereo_s16le() {
        assert_eq!(CHUNK_BYTES, 3840);
    }

    #[test]
    fn capture_gap_telemetry_uses_honest_100ms_threshold() {
        assert!(!is_capture_gap(Duration::from_millis(100)));
        assert!(is_capture_gap(Duration::from_millis(101)));
    }

    #[test]
    fn repeated_stall_backoff_is_bounded() {
        let mut delay = Duration::from_millis(20);
        let maximum = Duration::from_millis(80);
        for expected in [40, 80, 80, 80] {
            delay = next_restart_delay(delay, maximum);
            assert_eq!(delay, Duration::from_millis(expected));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_helper_is_not_restarted_and_reaps_on_shutdown() {
        let directory = test_directory("stall");
        let path = write_test_script(&directory, "exec sleep 999");
        let queue = Arc::new(AudioQueue::new());
        let (encoder, failure_tx) = test_encoder();
        let session = spawn_with_policy(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue,
            encoder,
            failure_tx,
            test_policy(),
        )
        .unwrap();
        let stats = session.stats();

        wait_for_counter(|| stats.idle_periods(), 1).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(stats.restarts(), 0);
        assert_eq!(stats.capture_gaps(), 0);
        assert_eq!(stats.helper_pids().len(), 1);

        session.shutdown().await;
        let captured_pids = stats.helper_pids();
        assert!(captured_pids.into_iter().all(|pid| !process_exists(pid)));
        assert_eq!(stats.termination_failures(), 0);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn normal_helper_startup_is_not_counted_as_capture_gap() {
        let directory = test_directory("startup");
        let path = write_test_script(
            &directory,
            "sleep 0.03\nhead -c 3840 /dev/zero\nexec sleep 999",
        );
        let queue = Arc::new(AudioQueue::new());
        let policy = SupervisorPolicy {
            idle_notice_after: Duration::from_secs(2),
            ..test_policy()
        };
        let (encoder, failure_tx) = test_encoder();
        let session = spawn_with_policy(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue.clone(),
            encoder,
            failure_tx,
            policy,
        )
        .unwrap();
        let stats = session.stats();

        let frame = tokio::time::timeout(Duration::from_secs(3), queue.dequeue())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame.payload.len(), CHUNK_BYTES);
        assert_eq!(stats.capture_gaps(), 0);
        assert_eq!(stats.idle_periods(), 0);

        session.shutdown().await;
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn helper_resumes_after_idle_without_restart_or_frame_misalignment() {
        let directory = test_directory("resume");
        let path = write_test_script(
            &directory,
            "sleep 0.15\nhead -c 3840 /dev/zero\nsleep 0.15\nhead -c 3840 /dev/zero\nexec sleep 999",
        );
        let queue = Arc::new(AudioQueue::new());
        let (encoder, failure_tx) = test_encoder();
        let session = spawn_with_policy(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue.clone(),
            encoder,
            failure_tx,
            test_policy(),
        )
        .unwrap();
        let stats = session.stats();

        for _ in 0..2 {
            let frame = tokio::time::timeout(Duration::from_secs(2), queue.dequeue())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(frame.payload.len(), CHUNK_BYTES);
        }
        assert_eq!(stats.restarts(), 0);
        assert!(stats.idle_periods() >= 2);
        assert_eq!(stats.capture_gaps(), 1);

        session.shutdown().await;
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reaps_silent_helper_and_closes_queue() {
        let directory = test_directory("shutdown");
        let path = write_test_script(&directory, "exec sleep 999");
        let queue = Arc::new(AudioQueue::new());
        let policy = SupervisorPolicy {
            idle_notice_after: Duration::from_secs(5),
            ..test_policy()
        };
        let (encoder, failure_tx) = test_encoder();
        let session = spawn_with_policy(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue.clone(),
            encoder,
            failure_tx,
            policy,
        )
        .unwrap();

        tokio::time::timeout(Duration::from_secs(1), session.shutdown())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), queue.dequeue())
                .await
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drop_requests_reaped_shutdown_without_aborting_supervisor() {
        let directory = test_directory("drop");
        let path = write_test_script(&directory, "exec sleep 999");
        let queue = Arc::new(AudioQueue::new());
        let (encoder, failure_tx) = test_encoder();
        let session = spawn_with_policy(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue.clone(),
            encoder,
            failure_tx,
            test_policy(),
        )
        .unwrap();
        let stats = session.stats();
        let pid = stats.helper_pids()[0];

        drop(session);
        tokio::time::timeout(Duration::from_secs(2), async {
            while process_exists(pid) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), queue.dequeue())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(stats.termination_failures(), 0);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn supervisor_restarts_failed_helper_and_shuts_down_cleanly() {
        let directory = test_directory("restart");
        let path = write_test_script(&directory, "head -c 3840 /dev/zero");

        let queue = Arc::new(AudioQueue::new());
        let (encoder, failure_tx) = test_encoder();
        let session = spawn(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue.clone(),
            encoder,
            failure_tx,
        )
        .unwrap();
        let stats = session.stats();

        let first = tokio::time::timeout(Duration::from_secs(2), queue.dequeue())
            .await
            .unwrap()
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(3), queue.dequeue())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.payload.len(), CHUNK_BYTES);
        assert_eq!(second.payload.len(), CHUNK_BYTES);
        assert!(stats.restarts() >= 1);

        session.shutdown().await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), queue.dequeue())
                .await
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn partial_frame_eof_restarts_without_emitting_corrupt_audio() {
        let directory = test_directory("partial-eof");
        let path = write_test_script(&directory, "head -c 1920 /dev/zero");
        let queue = Arc::new(AudioQueue::new());
        let (encoder, failure_tx) = test_encoder();
        let session = spawn_with_policy(
            AudioConfig {
                binary: path.clone(),
                execution: None,
                session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
            },
            queue.clone(),
            encoder,
            failure_tx,
            test_policy(),
        )
        .unwrap();
        let stats = session.stats();

        wait_for_counter(|| stats.restarts(), 1).await;
        assert_eq!(stats.captured_frames(), 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), queue.dequeue())
                .await
                .is_err()
        );

        session.shutdown().await;
        let _ = std::fs::remove_dir_all(directory);
    }
}
