// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host audio playout on the default output device.
//!
//! The macOS Deck's `CpalOutput` on WASAPI: the same queue and playout
//! policy (`arcen_media::audio::playback_queue`, shared), the same API
//! (`push_s16le`, `push_samples`, `reset`, `snapshot`), event-driven shared
//! mode through cpal. Two things Windows needs that macOS does not:
//!
//! - **Format.** WASAPI shared mode plays only the device's mix format, so
//!   a 44.1 kHz or surround device is fed through the shared
//!   [`DeviceFormatAdapter`] instead of being refused.
//! - **Device changes.** A stream stays on the device it opened. A worker
//!   thread owns the stream and rebuilds it on the new default device when
//!   the default changes or the stream fails (headphones unplugged,
//!   Bluetooth disconnect, the audio service restarting), keeping the queue.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use arcen_media::audio::playback_queue::{DeviceFormatAdapter, PlaybackQueue, PlaybackSnapshot};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The audio backend label for Stream Statistics and logs.
pub const BACKEND: &str = "cpal-wasapi";
/// How often the worker checks the default device.
const DEVICE_POLL: Duration = Duration::from_millis(500);

/// What the worker shares with the device callback and the caller.
struct Shared {
    queue: Mutex<PlaybackQueue>,
    reset_generation: AtomicU64,
    stream_failed: AtomicBool,
    stop: AtomicBool,
    device_name: Mutex<String>,
    device_format: Mutex<String>,
    rebuilds: AtomicU64,
}

/// Host audio playout on the default output device.
pub struct WindowsAudioOutput {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WindowsAudioOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsAudioOutput")
            .field("device", &self.device_name())
            .finish_non_exhaustive()
    }
}

impl WindowsAudioOutput {
    /// Opens the default output device and starts the worker.
    ///
    /// # Errors
    ///
    /// Why no stream could be opened (no device, or the device refused).
    pub fn start() -> Result<Self, String> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(PlaybackQueue::new()),
            reset_generation: AtomicU64::new(0),
            stream_failed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            device_name: Mutex::new(String::new()),
            device_format: Mutex::new(String::new()),
            rebuilds: AtomicU64::new(0),
        });
        let (started, ready) = std::sync::mpsc::sync_channel(1);
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("audio-output".to_owned())
            .spawn(move || run_worker(&worker_shared, &started))
            .map_err(|error| format!("Could not start the audio output thread: {error}"))?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err("The audio output thread ended before opening a device".to_owned())
            }
        }
    }

    /// Queues little-endian PCM; returns the bytes accepted.
    #[must_use]
    pub fn push_s16le(&self, payload: &[u8]) -> usize {
        self.queue().push_s16le(payload)
    }

    /// Queues interleaved samples; returns the bytes accepted.
    #[must_use]
    pub fn push_samples(&self, samples: &[i16]) -> usize {
        self.queue().push_samples(samples)
    }

    /// Drops what is queued and prebuffers again.
    pub fn reset(&self) {
        self.queue().reset();
        self.shared.reset_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Queue statistics.
    #[must_use]
    pub fn snapshot(&self) -> PlaybackSnapshot {
        self.queue().snapshot()
    }

    /// The device playing now.
    #[must_use]
    pub fn device_name(&self) -> String {
        self.shared
            .device_name
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The device's mix format, for example `48000 Hz 2 ch F32`.
    #[must_use]
    pub fn device_format(&self) -> String {
        self.shared
            .device_format
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many times playout moved to another device.
    #[must_use]
    pub fn device_changes(&self) -> u64 {
        self.shared.rebuilds.load(Ordering::Acquire)
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, PlaybackQueue> {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for WindowsAudioOutput {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn run_worker(shared: &Arc<Shared>, started: &std::sync::mpsc::SyncSender<Result<(), String>>) {
    let host = cpal::default_host();
    let mut first = true;
    while !shared.stop.load(Ordering::Acquire) {
        let opened = open_default(&host, shared);
        let (stream, name) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                if first {
                    let _ = started.send(Err(error));
                    return;
                }
                tracing::warn!(
                    target: arcen_deck_core::log_target::AUDIO,
                    %error,
                    "audio output device unavailable; retrying"
                );
                std::thread::park_timeout(Duration::from_secs(1));
                continue;
            }
        };
        if first {
            let _ = started.send(Ok(()));
            first = false;
        } else {
            shared.rebuilds.fetch_add(1, Ordering::AcqRel);
            tracing::info!(
                target: arcen_deck_core::log_target::AUDIO,
                device = %name,
                "audio output moved to the default device"
            );
        }
        while !shared.stop.load(Ordering::Acquire) {
            std::thread::park_timeout(DEVICE_POLL);
            if shared.stream_failed.swap(false, Ordering::AcqRel) {
                // Back off so a device that fails at once cannot spin.
                std::thread::park_timeout(Duration::from_secs(1));
                break;
            }
            let default_name = host
                .default_output_device()
                .and_then(|device| device.name().ok());
            if default_name
                .as_deref()
                .is_some_and(|default| default != name)
            {
                break;
            }
        }
        // Dropping joins cpal's render thread, which may still report an
        // error on its way out (the unplugged device): that error belongs to
        // the stream just closed, not to the next one.
        drop(stream);
        shared.stream_failed.store(false, Ordering::Release);
    }
}

fn open_default(host: &cpal::Host, shared: &Arc<Shared>) -> Result<(cpal::Stream, String), String> {
    let device = host
        .default_output_device()
        .ok_or_else(|| "No default audio output device".to_owned())?;
    let name = device.name().unwrap_or_else(|_| "unknown".to_owned());
    let supported = device
        .default_output_config()
        .map_err(|error| format!("Could not read default output config: {error}"))?;
    let format = supported.sample_format();
    let config = cpal::StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    let adapter = DeviceFormatAdapter::new(config.sample_rate.0, config.channels)
        .ok_or_else(|| "The output device reports no channels".to_owned())?;
    let error_shared = Arc::clone(shared);
    // cpal's WASAPI render thread ends after reporting any error (device
    // gone, audio service restarted), so every error means: reopen.
    let on_error = move |error: cpal::StreamError| {
        error_shared.stream_failed.store(true, Ordering::Release);
        tracing::warn!(target: arcen_deck_core::log_target::AUDIO, %error, "audio stream error");
    };
    let stream = match format {
        cpal::SampleFormat::F32 => {
            let mut render = Renderer::new(Arc::clone(shared), adapter);
            device.build_output_stream(
                &config,
                move |data: &mut [f32], _| render.fill_f32(data),
                on_error,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let mut render = Renderer::new(Arc::clone(shared), adapter);
            device.build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    render.fill_with(data, |value| {
                        #[allow(clippy::cast_possible_truncation, reason = "clamped to i16")]
                        let sample = (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
                        sample
                    });
                },
                on_error,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let mut render = Renderer::new(Arc::clone(shared), adapter);
            device.build_output_stream(
                &config,
                move |data: &mut [u16], _| {
                    render.fill_with(data, |value| {
                        #[allow(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "clamped to the u16 range"
                        )]
                        let sample = ((value.clamp(-1.0, 1.0) + 1.0) * 32_767.5).round() as u16;
                        sample
                    });
                },
                on_error,
                None,
            )
        }
        other => return Err(format!("Unsupported output sample format: {other:?}")),
    }
    .map_err(|error| format!("Could not open the output stream on {name}: {error}"))?;
    stream
        .play()
        .map_err(|error| format!("Could not start audio output stream: {error}"))?;
    shared
        .device_name
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone_from(&name);
    *shared
        .device_format
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = format!(
        "{} Hz {} ch {format:?}",
        config.sample_rate.0, config.channels
    );
    Ok((stream, name))
}

/// The device callback's state: never allocates after the first callback.
struct Renderer {
    shared: Arc<Shared>,
    adapter: DeviceFormatAdapter,
    generation: u64,
    scratch: Vec<f32>,
}

impl Renderer {
    fn new(shared: Arc<Shared>, adapter: DeviceFormatAdapter) -> Self {
        let generation = shared.reset_generation.load(Ordering::Acquire);
        Self {
            shared,
            adapter,
            generation,
            scratch: Vec::new(),
        }
    }

    fn fill_f32(&mut self, data: &mut [f32]) {
        super::join_pro_audio_once();
        let generation = self.shared.reset_generation.load(Ordering::Acquire);
        if generation != self.generation {
            self.generation = generation;
            self.adapter.reset();
        }
        let mut queue = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.adapter.fill_f32(&mut queue, data);
    }

    fn fill_with<T>(&mut self, data: &mut [T], convert: impl Fn(f32) -> T) {
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.resize(data.len(), 0.0);
        self.fill_f32(&mut scratch);
        for (out, value) in data.iter_mut().zip(&scratch) {
            *out = convert(*value);
        }
        self.scratch = scratch;
    }
}

impl arcen_deck_core::runtime::audio_player::AudioOutputDevice for WindowsAudioOutput {
    fn open() -> Result<Self, String> {
        Self::start()
    }

    fn backend(&self) -> &'static str {
        BACKEND
    }

    fn push_s16le(&self, payload: &[u8]) -> usize {
        Self::push_s16le(self, payload)
    }

    fn push_samples(&self, samples: &[i16]) -> usize {
        Self::push_samples(self, samples)
    }

    fn reset(&self) {
        Self::reset(self);
    }

    fn snapshot(&self) -> arcen_deck_core::runtime::audio_player::PlaybackSnapshot {
        let snapshot = Self::snapshot(self);
        arcen_deck_core::runtime::audio_player::PlaybackSnapshot {
            queued_ms: snapshot.queued_ms,
            underruns: snapshot.underruns,
            trim_events: snapshot.trim_events,
            trimmed_samples: snapshot.trimmed_samples,
            prebuffering: snapshot.prebuffering,
            recovering: snapshot.recovering,
        }
    }
}

/// The Windows Deck's host-audio player (the Mac's logic, shared).
pub type Player = arcen_deck_core::runtime::audio_player::PcmAudioPlayer<WindowsAudioOutput>;
