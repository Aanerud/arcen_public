//! macOS AVAudioEngine microphone capture.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use arcen_media::audio::MICROPHONE_V1_FRAME_SAMPLES;
use tokio::sync::Notify;
use zeroize::Zeroize;

const CAPTURE_QUEUE_FRAMES: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedMicrophoneFrame {
    pub samples: [i16; MICROPHONE_V1_FRAME_SAMPLES],
    pub sequence: u32,
    pub timestamp_ms: u32,
}

impl CapturedMicrophoneFrame {
    pub(crate) fn zeroize(&mut self) {
        self.samples.zeroize();
    }
}

impl Drop for CapturedMicrophoneFrame {
    fn drop(&mut self) {
        self.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneCaptureError {
    UnsupportedPlatform,
    PermissionDenied,
    PermissionLost,
    InputUnavailable,
    InputRemoved,
    InvalidCallbackFormat,
    EngineStartFailed,
    EngineStopped,
    ProgressStarved,
    StartupTimedOut,
    Cancelled,
}

struct CaptureQueueState {
    frames: VecDeque<CapturedMicrophoneFrame>,
    terminal: Option<Result<(), MicrophoneCaptureError>>,
}

struct CaptureQueue {
    state: Mutex<CaptureQueueState>,
    notify: Notify,
    captured_frames: AtomicU64,
    dropped_frames: AtomicU64,
}

impl CaptureQueue {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CaptureQueueState {
                frames: VecDeque::with_capacity(CAPTURE_QUEUE_FRAMES),
                terminal: None,
            }),
            notify: Notify::new(),
            captured_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
        })
    }

    #[cfg(any(target_os = "macos", test))]
    fn push_drop_oldest(&self, mut frame: CapturedMicrophoneFrame) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal.is_some() {
            frame.zeroize();
            return;
        }
        self.captured_frames.fetch_add(1, Ordering::Relaxed);
        if state.frames.len() == CAPTURE_QUEUE_FRAMES {
            if let Some(mut dropped) = state.frames.pop_front() {
                dropped.zeroize();
                self.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
        }
        state.frames.push_back(frame);
        drop(state);
        self.notify.notify_one();
    }

    fn finish(&self, result: Result<(), MicrophoneCaptureError>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if result.is_err() {
            for frame in &mut state.frames {
                frame.zeroize();
            }
            state.frames.clear();
        }
        if state.terminal.is_none() {
            state.terminal = Some(result);
        }
        drop(state);
        self.notify.notify_waiters();
    }
}

pub struct CapturedMicrophoneReceiver {
    queue: Arc<CaptureQueue>,
}

impl CapturedMicrophoneReceiver {
    pub async fn recv(
        &mut self,
    ) -> Result<Option<CapturedMicrophoneFrame>, MicrophoneCaptureError> {
        loop {
            let notified = self.queue.notify.notified();
            {
                let mut state = self
                    .queue
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(frame) = state.frames.pop_front() {
                    return Ok(Some(frame));
                }
                if let Some(terminal) = state.terminal {
                    return terminal.map(|()| None);
                }
            }
            notified.await;
        }
    }

    pub(crate) fn take_capture_counters(&self) -> (u64, u64) {
        (
            self.queue.captured_frames.swap(0, Ordering::Relaxed),
            self.queue.dropped_frames.swap(0, Ordering::Relaxed),
        )
    }
}

impl Drop for CapturedMicrophoneReceiver {
    fn drop(&mut self) {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for frame in &mut state.frames {
            frame.zeroize();
        }
        state.frames.clear();
    }
}

pub struct MicrophoneCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MicrophoneCapture {
    pub fn start_with_cancel(
        stop: Arc<AtomicBool>,
        session_closed: Arc<AtomicBool>,
    ) -> Result<(Self, CapturedMicrophoneReceiver), MicrophoneCaptureError> {
        let queue = CaptureQueue::new();
        let receiver = CapturedMicrophoneReceiver {
            queue: Arc::clone(&queue),
        };
        let (startup, ready) = std::sync::mpsc::sync_channel(1);
        let thread_stop = Arc::clone(&stop);
        let thread_session_closed = Arc::clone(&session_closed);
        let thread = std::thread::Builder::new()
            .name("arcen-av-audio-engine-input".to_string())
            .spawn(move || native::run(thread_stop, thread_session_closed, queue, startup))
            .map_err(|_| MicrophoneCaptureError::InputUnavailable)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        loop {
            if cancelled(&stop, &session_closed) {
                let _ = thread.join();
                return Err(MicrophoneCaptureError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                return Err(MicrophoneCaptureError::StartupTimedOut);
            }
            match ready.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(Ok(())) => {
                    return Ok((
                        Self {
                            stop,
                            thread: Some(thread),
                        },
                        receiver,
                    ));
                }
                Ok(Err(error)) => {
                    stop.store(true, Ordering::Release);
                    let _ = thread.join();
                    return Err(error);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    stop.store(true, Ordering::Release);
                    let _ = thread.join();
                    return Err(MicrophoneCaptureError::InputUnavailable);
                }
            }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        stop: Arc<AtomicBool>,
        thread: JoinHandle<()>,
    ) -> (Self, CapturedMicrophoneReceiver) {
        let queue = CaptureQueue::new();
        (
            Self {
                stop,
                thread: Some(thread),
            },
            CapturedMicrophoneReceiver { queue },
        )
    }
}

fn cancelled(stop: &AtomicBool, session_closed: &AtomicBool) -> bool {
    stop.load(Ordering::Acquire) || session_closed.load(Ordering::Acquire)
}

impl Drop for MicrophoneCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = std::thread::Builder::new()
                .name("arcen-microphone-reaper".to_owned())
                .spawn(move || {
                    let _ = thread.join();
                });
        }
    }
}

#[cfg(any(target_os = "macos", test))]
struct CaptureNormalizer {
    source_rate_hz: u32,
    phase: u64,
    frame: [i16; MICROPHONE_V1_FRAME_SAMPLES],
    frame_len: usize,
    sequence: u32,
    timestamp_ms: u32,
}

#[cfg(any(target_os = "macos", test))]
impl Drop for CaptureNormalizer {
    fn drop(&mut self) {
        self.frame.zeroize();
        self.frame_len = 0;
    }
}

#[cfg(any(target_os = "macos", test))]
impl CaptureNormalizer {
    fn new(source_rate_hz: u32) -> Option<Self> {
        (source_rate_hz > 0).then_some(Self {
            source_rate_hz,
            phase: 0,
            frame: [0; MICROPHONE_V1_FRAME_SAMPLES],
            frame_len: 0,
            sequence: 1,
            timestamp_ms: 0,
        })
    }

    fn push(
        &mut self,
        sample: f32,
        queue: &CaptureQueue,
        stop: &AtomicBool,
        session_closed: &AtomicBool,
    ) {
        self.phase = self.phase.saturating_add(48_000);
        while self.phase >= u64::from(self.source_rate_hz) {
            if cancelled(stop, session_closed) {
                return;
            }
            self.phase -= u64::from(self.source_rate_hz);
            let scaled = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round();
            self.frame[self.frame_len] = scaled as i16;
            self.frame_len += 1;
            if self.frame_len == MICROPHONE_V1_FRAME_SAMPLES {
                let samples = std::mem::replace(&mut self.frame, [0; MICROPHONE_V1_FRAME_SAMPLES]);
                let frame = CapturedMicrophoneFrame {
                    samples,
                    sequence: self.sequence,
                    timestamp_ms: self.timestamp_ms,
                };
                self.sequence = self.sequence.wrapping_add(1);
                if self.sequence == 0 {
                    self.sequence = 1;
                }
                self.timestamp_ms = self.timestamp_ms.wrapping_add(20);
                if cancelled(stop, session_closed) {
                    return;
                }
                queue.push_drop_oldest(frame);
                self.frame_len = 0;
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn clear(&mut self) {
        self.phase = 0;
        self.frame.zeroize();
        self.frame_len = 0;
    }
}

#[cfg(test)]
fn mix_float_frames(
    channel_data: &[&[f32]],
    frame_count: usize,
    stride: usize,
    interleaved: bool,
) -> Option<Vec<f32>> {
    let channels = if interleaved {
        (channel_data.len() == 1).then_some(stride)?
    } else {
        channel_data.len()
    };
    if channels == 0 || stride == 0 {
        return None;
    }
    let mut mixed = Vec::with_capacity(frame_count);
    for frame_index in 0..frame_count {
        let mut sum = 0.0;
        for channel in 0..channels {
            let (pointer_index, sample_index) =
                float_sample_location(frame_index, channel, stride, interleaved)?;
            sum += *channel_data.get(pointer_index)?.get(sample_index)?;
        }
        mixed.push(sum / channels as f32);
    }
    Some(mixed)
}

#[cfg(any(target_os = "macos", test))]
fn float_sample_location(
    frame_index: usize,
    channel: usize,
    stride: usize,
    interleaved: bool,
) -> Option<(usize, usize)> {
    let frame_offset = frame_index.checked_mul(stride)?;
    if interleaved {
        Some((0, frame_offset.checked_add(channel)?))
    } else {
        Some((channel, frame_offset))
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::ptr::NonNull;
    use std::sync::atomic::AtomicU64;

    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2::AnyThread;
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    use objc2_avf_audio::{AVAudioCommonFormat, AVAudioEngine, AVAudioPCMBuffer, AVAudioTime};

    const PROGRESS_TIMEOUT: Duration = Duration::from_secs(2);

    pub fn run(
        stop: Arc<AtomicBool>,
        session_closed: Arc<AtomicBool>,
        queue: Arc<CaptureQueue>,
        startup: std::sync::mpsc::SyncSender<Result<(), MicrophoneCaptureError>>,
    ) {
        if !request_permission(&stop, &session_closed) {
            let error = if cancelled(&stop, &session_closed) {
                MicrophoneCaptureError::Cancelled
            } else {
                MicrophoneCaptureError::PermissionDenied
            };
            queue.finish(Err(error));
            let _ = startup.send(Err(error));
            return;
        }
        // SAFETY: all Objective-C objects are created, used, and released on
        // this dedicated thread. The input node retains the copied tap block
        // until it is removed before engine destruction.
        unsafe {
            let engine = AVAudioEngine::init(AVAudioEngine::alloc());
            let input = engine.inputNode();
            let format = input.outputFormatForBus(0);
            let rate = format.sampleRate();
            let channels = format.channelCount();
            if !valid_float_format(format.commonFormat(), rate, channels) {
                queue.finish(Err(MicrophoneCaptureError::InputUnavailable));
                let _ = startup.send(Err(MicrophoneCaptureError::InputUnavailable));
                return;
            }
            let Some(normalizer) = CaptureNormalizer::new(rate.round() as u32) else {
                queue.finish(Err(MicrophoneCaptureError::InputUnavailable));
                let _ = startup.send(Err(MicrophoneCaptureError::InputUnavailable));
                return;
            };
            if cancelled(&stop, &session_closed) {
                queue.finish(Err(MicrophoneCaptureError::Cancelled));
                let _ = startup.send(Err(MicrophoneCaptureError::Cancelled));
                return;
            }

            let normalizer = Arc::new(Mutex::new(normalizer));
            let callback_normalizer = Arc::clone(&normalizer);
            let callback_queue = Arc::clone(&queue);
            let callback_stop = Arc::clone(&stop);
            let callback_session_closed = Arc::clone(&session_closed);
            let invalid_callback = Arc::new(AtomicBool::new(false));
            let callback_invalid = Arc::clone(&invalid_callback);
            let capture_clock = std::time::Instant::now();
            let progress_ms = Arc::new(AtomicU64::new(0));
            let callback_progress = Arc::clone(&progress_ms);
            let tap: RcBlock<dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>) + 'static> =
                RcBlock::new(
                    move |buffer: NonNull<AVAudioPCMBuffer>, _time: NonNull<AVAudioTime>| {
                        if cancelled(&callback_stop, &callback_session_closed) {
                            return;
                        }
                        // SAFETY: AVAudioEngine guarantees callback buffer storage
                        // through this invocation. Its format and stride are
                        // validated before pointer arithmetic.
                        let buffer = buffer.as_ref();
                        let actual = buffer.format();
                        let actual_rate = actual.sampleRate();
                        let actual_channels = actual.channelCount();
                        let length = buffer.frameLength() as usize;
                        let stride = buffer.stride() as usize;
                        let interleaved = actual.isInterleaved();
                        let expected_stride = if interleaved {
                            actual_channels as usize
                        } else {
                            1
                        };
                        let channel_data = buffer.floatChannelData();
                        if !valid_float_format(actual.commonFormat(), actual_rate, actual_channels)
                            || actual_channels != channels
                            || (actual_rate - rate).abs() > 0.5
                            || stride != expected_stride
                            || channel_data.is_null()
                            || length == 0
                        {
                            callback_invalid.store(true, Ordering::Release);
                            return;
                        }
                        let Ok(mut normalizer) = callback_normalizer.lock() else {
                            callback_invalid.store(true, Ordering::Release);
                            return;
                        };
                        for frame_index in 0..length {
                            if cancelled(&callback_stop, &callback_session_closed) {
                                return;
                            }
                            let mut mono = 0.0f32;
                            for channel in 0..actual_channels as usize {
                                let Some((pointer_index, sample_index)) = float_sample_location(
                                    frame_index,
                                    channel,
                                    stride,
                                    interleaved,
                                ) else {
                                    callback_invalid.store(true, Ordering::Release);
                                    return;
                                };
                                // SAFETY: non-interleaved buffers expose one pointer
                                // per channel; interleaved buffers expose one pointer
                                // whose frames contain `stride` channel samples.
                                let pointer = *channel_data.add(pointer_index);
                                mono += *pointer.as_ptr().add(sample_index);
                            }
                            normalizer.push(
                                mono / actual_channels as f32,
                                &callback_queue,
                                &callback_stop,
                                &callback_session_closed,
                            );
                        }
                        callback_progress.store(
                            capture_clock
                                .elapsed()
                                .as_millis()
                                .min(u128::from(u64::MAX)) as u64
                                + 1,
                            Ordering::Release,
                        );
                    },
                );
            if cancelled(&stop, &session_closed) {
                queue.finish(Err(MicrophoneCaptureError::Cancelled));
                let _ = startup.send(Err(MicrophoneCaptureError::Cancelled));
                return;
            }
            let tap_block: *mut block2::DynBlock<
                dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>),
            > = std::ptr::from_ref(&*tap).cast_mut();
            input.installTapOnBus_bufferSize_format_block(0, 960, Some(&format), tap_block);
            engine.prepare();
            if cancelled(&stop, &session_closed) {
                input.removeTapOnBus(0);
                queue.finish(Err(MicrophoneCaptureError::Cancelled));
                let _ = startup.send(Err(MicrophoneCaptureError::Cancelled));
                return;
            }
            if engine.startAndReturnError().is_err() {
                input.removeTapOnBus(0);
                queue.finish(Err(MicrophoneCaptureError::EngineStartFailed));
                let _ = startup.send(Err(MicrophoneCaptureError::EngineStartFailed));
                return;
            }
            if cancelled(&stop, &session_closed) {
                engine.stop();
                input.removeTapOnBus(0);
                queue.finish(Err(MicrophoneCaptureError::Cancelled));
                let _ = startup.send(Err(MicrophoneCaptureError::Cancelled));
                return;
            }
            let _ = startup.send(Ok(()));

            let started = std::time::Instant::now();
            let failure = loop {
                if cancelled(&stop, &session_closed) {
                    break None;
                }
                if authorization_status() != Some(AVAuthorizationStatus::Authorized) {
                    break Some(MicrophoneCaptureError::PermissionLost);
                }
                if !engine.isRunning() {
                    break Some(MicrophoneCaptureError::EngineStopped);
                }
                let current = input.outputFormatForBus(0);
                if !valid_float_format(
                    current.commonFormat(),
                    current.sampleRate(),
                    current.channelCount(),
                ) || current.channelCount() != channels
                    || (current.sampleRate() - rate).abs() > 0.5
                {
                    break Some(MicrophoneCaptureError::InputRemoved);
                }
                if invalid_callback.load(Ordering::Acquire) {
                    break Some(MicrophoneCaptureError::InvalidCallbackFormat);
                }
                let progress = progress_ms.load(Ordering::Acquire);
                if started.elapsed() > PROGRESS_TIMEOUT
                    && (progress == 0
                        || capture_clock
                            .elapsed()
                            .as_millis()
                            .saturating_sub(u128::from(progress.saturating_sub(1)))
                            > PROGRESS_TIMEOUT.as_millis())
                {
                    break Some(MicrophoneCaptureError::ProgressStarved);
                }
                std::thread::sleep(Duration::from_millis(50));
            };
            engine.stop();
            input.removeTapOnBus(0);
            if let Ok(mut normalizer) = normalizer.lock() {
                normalizer.clear();
            }
            queue.finish(failure.map_or(Ok(()), Err));
        }
    }

    fn valid_float_format(format: AVAudioCommonFormat, rate: f64, channels: u32) -> bool {
        format == AVAudioCommonFormat::PCMFormatFloat32
            && rate.is_finite()
            && rate > 0.0
            && channels > 0
    }

    fn authorization_status() -> Option<AVAuthorizationStatus> {
        // SAFETY: AVMediaTypeAudio is a framework-owned process-lifetime constant.
        unsafe {
            AVMediaTypeAudio
                .map(|media_type| AVCaptureDevice::authorizationStatusForMediaType(media_type))
        }
    }

    fn request_permission(stop: &AtomicBool, session_closed: &AtomicBool) -> bool {
        // SAFETY: AVMediaTypeAudio is framework-owned and the completion block
        // remains alive until this bounded result wait completes.
        unsafe {
            let Some(media_type) = AVMediaTypeAudio else {
                return false;
            };
            match AVCaptureDevice::authorizationStatusForMediaType(media_type) {
                AVAuthorizationStatus::Authorized => true,
                AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => false,
                AVAuthorizationStatus::NotDetermined => {
                    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
                    let completion = RcBlock::new(move |granted: Bool| {
                        let _ = sender.send(granted.as_bool());
                    });
                    AVCaptureDevice::requestAccessForMediaType_completionHandler(
                        media_type,
                        &completion,
                    );
                    let deadline = std::time::Instant::now() + Duration::from_secs(120);
                    loop {
                        if cancelled(stop, session_closed) {
                            break false;
                        }
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            break false;
                        }
                        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
                            Ok(granted) => break granted,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break false,
                        }
                    }
                }
                _ => false,
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::*;

    pub fn run(
        _stop: Arc<AtomicBool>,
        _session_closed: Arc<AtomicBool>,
        queue: Arc<CaptureQueue>,
        startup: std::sync::mpsc::SyncSender<Result<(), MicrophoneCaptureError>>,
    ) {
        queue.finish(Err(MicrophoneCaptureError::UnsupportedPlatform));
        let _ = startup.send(Err(MicrophoneCaptureError::UnsupportedPlatform));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit_frames(normalizer: &mut CaptureNormalizer, queue: &CaptureQueue, count: usize) {
        let stop = AtomicBool::new(false);
        let session_closed = AtomicBool::new(false);
        for _ in 0..MICROPHONE_V1_FRAME_SAMPLES * count {
            normalizer.push(0.5, queue, &stop, &session_closed);
        }
    }

    #[tokio::test]
    async fn normalizer_produces_exact_bounded_frames() {
        let queue = CaptureQueue::new();
        let mut receiver = CapturedMicrophoneReceiver {
            queue: Arc::clone(&queue),
        };
        let mut normalizer = CaptureNormalizer::new(48_000).unwrap();
        emit_frames(&mut normalizer, &queue, 1);
        let frame = receiver.recv().await.unwrap().unwrap();
        assert!(frame.samples.iter().all(|sample| *sample == 16_384));
        assert_eq!((frame.sequence, frame.timestamp_ms), (1, 0));
    }

    #[tokio::test]
    async fn backpressure_drops_oldest_and_preserves_capture_clock_gaps() {
        let queue = CaptureQueue::new();
        let mut receiver = CapturedMicrophoneReceiver {
            queue: Arc::clone(&queue),
        };
        let mut normalizer = CaptureNormalizer::new(48_000).unwrap();
        emit_frames(&mut normalizer, &queue, 5);
        let first = receiver.recv().await.unwrap().unwrap();
        let second = receiver.recv().await.unwrap().unwrap();
        assert_eq!((first.sequence, first.timestamp_ms), (4, 60));
        assert_eq!((second.sequence, second.timestamp_ms), (5, 80));
        assert_eq!(receiver.take_capture_counters(), (5, 3));
        assert_eq!(receiver.take_capture_counters(), (0, 0));
    }

    #[tokio::test]
    async fn typed_capture_failure_discards_audio_and_closes_receiver() {
        let queue = CaptureQueue::new();
        let mut receiver = CapturedMicrophoneReceiver {
            queue: Arc::clone(&queue),
        };
        let mut normalizer = CaptureNormalizer::new(48_000).unwrap();
        emit_frames(&mut normalizer, &queue, 1);
        queue.finish(Err(MicrophoneCaptureError::InputRemoved));
        assert_eq!(
            receiver.recv().await,
            Err(MicrophoneCaptureError::InputRemoved)
        );
    }

    #[test]
    fn owned_frame_zeroizes_original_sample_storage() {
        assert!(std::mem::needs_drop::<CapturedMicrophoneFrame>());
        let mut frame = CapturedMicrophoneFrame {
            samples: [123; MICROPHONE_V1_FRAME_SAMPLES],
            sequence: 1,
            timestamp_ms: 0,
        };
        frame.zeroize();
        assert!(frame.samples.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn non_interleaved_float32_addressing_uses_unit_stride() {
        let left = [0.1, 0.3, 0.5];
        let right = [0.3, 0.5, 0.7];
        let mixed = mix_float_frames(&[&left, &right], 3, 1, false).unwrap();
        assert!(mixed
            .iter()
            .zip([0.2, 0.4, 0.6])
            .all(|(actual, expected)| (actual - expected).abs() < 1e-6));
    }

    #[test]
    fn interleaved_float32_addressing_uses_buffer_stride() {
        let samples = [0.1, 0.3, 0.5, 0.7, 0.9, 1.0];
        let mixed = mix_float_frames(&[&samples], 3, 2, true).unwrap();
        assert!(mixed
            .iter()
            .zip([0.2, 0.6, 0.95])
            .all(|(actual, expected)| (actual - expected).abs() < 1e-6));
        assert!(mix_float_frames(&[&samples, &samples], 3, 2, true).is_none());
    }
}
