// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host audio output on the macOS Deck: cpal on Core Audio, on the system's
//! default output device.
//!
//! Everything but opening a stream is shared: the player (negotiation,
//! Opus, jitter, concealment) is [`arcen_deck_core::runtime::audio_player`],
//! the adaptive playback queue `arcen_media::audio::playback_queue`, and
//! following the default device (move on a default change or a stream
//! error, back off, never two streams, nothing after close) is
//! [`arcen_deck_core::runtime::audio_output`]. This adapter finds the
//! default device and opens a 48 kHz stereo stream on it; Core Audio's
//! output unit converts to the device's own format.

use arcen_deck_core::runtime::audio_output::{
    FollowedOutput, OpenedStream, OutputBackend, StreamTicket,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub use arcen_deck_core::runtime::audio_player::PlaybackSnapshot;
pub use arcen_deck_core::runtime::audio_worker::AudioFeedStatus;

/// The default Core Audio output, followed.
pub type CpalOutput = FollowedOutput<CoreAudioOutput>;

/// The macOS Deck's audio player: the shared player on cpal/CoreAudio.
pub type PcmAudioPlayer = arcen_deck_core::runtime::audio_player::PcmAudioPlayer<CpalOutput>;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;

// Core Audio's device (an `AudioDeviceID`) compares by identity, so two
// outputs with the same name (two identical monitors) are still told apart.
// The crate is type-checked for Windows too, where cpal's portable device
// is compared by name.
#[cfg(target_os = "macos")]
type Host = cpal::platform::CoreAudioHost;
#[cfg(target_os = "macos")]
type Device = cpal::platform::CoreAudioDevice;
#[cfg(target_os = "macos")]
fn host() -> Option<Host> {
    Host::new().ok()
}
#[cfg(not(target_os = "macos"))]
type Host = cpal::Host;
#[cfg(not(target_os = "macos"))]
type Device = cpal::Device;
#[cfg(not(target_os = "macos"))]
fn host() -> Option<Host> {
    Some(cpal::default_host())
}

/// The system's default output device at one moment.
#[derive(Clone)]
pub struct DefaultOutput {
    device: Device,
    name: String,
}

impl PartialEq for DefaultOutput {
    #[cfg(target_os = "macos")]
    fn eq(&self, other: &Self) -> bool {
        self.device == other.device
    }

    #[cfg(not(target_os = "macos"))]
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

/// Opens Core Audio output streams for [`FollowedOutput`].
pub struct CoreAudioOutput {
    host: Option<Host>,
}

impl std::fmt::Debug for CoreAudioOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CoreAudioOutput")
            .field("host", &self.host.is_some())
            .finish()
    }
}

impl Default for CoreAudioOutput {
    fn default() -> Self {
        Self { host: host() }
    }
}

impl OutputBackend for CoreAudioOutput {
    type Device = DefaultOutput;
    type Stream = <Device as DeviceTrait>::Stream;
    const BACKEND: &'static str = "cpal-coreaudio-48k-stereo";

    fn default_device(&mut self) -> Option<DefaultOutput> {
        let device = self.host.as_ref()?.default_output_device()?;
        let name = device.name().unwrap_or_else(|_| "unknown".to_owned());
        Some(DefaultOutput { device, name })
    }

    fn open(
        &mut self,
        output: &DefaultOutput,
        ticket: &StreamTicket,
    ) -> Result<OpenedStream<Self::Stream>, String> {
        let device = &output.device;
        let sample_format = device
            .default_output_config()
            .map_err(|error| format!("Could not read default output config: {error}"))?
            .sample_format();
        let config = cpal::StreamConfig {
            channels: CHANNELS,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut render = ticket.renderer(SAMPLE_RATE, CHANNELS)?;
        let errors = ticket.errors();
        // Every stream error ends this stream; the follower reopens.
        let on_error = move |error: cpal::StreamError| {
            tracing::warn!(target: crate::logging::target::AUDIO, %error, "audio stream error");
            errors.report(error.to_string());
        };
        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| render.fill_f32(data),
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |data: &mut [i16], _| render.fill_i16(data),
                on_error,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                &config,
                move |data: &mut [u16], _| render.fill_u16(data),
                on_error,
                None,
            ),
            other => return Err(format!("Unsupported output sample format: {other:?}")),
        }
        .map_err(|error| {
            format!(
                "Could not open 48 kHz stereo output stream on {}: {error}",
                output.name
            )
        })?;
        stream
            .play()
            .map_err(|error| format!("Could not start audio output stream: {error}"))?;
        Ok(OpenedStream {
            stream,
            device: output.name.clone(),
            format: format!("{SAMPLE_RATE} Hz {CHANNELS} ch {sample_format:?}"),
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::time::{Duration, Instant};

    use arcen_deck_core::runtime::audio_output::OutputPhase;

    use super::*;

    /// Native proof on this Mac's real default output; plays silence.
    /// With `ARCEN_E2E_AUDIO_FOLLOW_SECS=N` it keeps playing for N seconds
    /// and prints every device change, for an operator who switches or
    /// unplugs outputs meanwhile.
    #[test]
    #[ignore = "opens this Mac's default audio output (plays silence)"]
    fn the_default_output_plays_through_the_follower_and_closes() {
        let mut output = CpalOutput::start(CoreAudioOutput::default).expect("default output");
        let status = output.status();
        assert_eq!(status.phase, OutputPhase::Playing);
        println!("audio output: {} ({})", status.device, status.format);

        let frame = [0_i16; arcen_media::audio::playback_queue::INTERLEAVED_FRAME_SAMPLES];
        let follow = std::env::var("ARCEN_E2E_AUDIO_FOLLOW_SECS")
            .ok()
            .and_then(|secs| secs.parse().ok())
            .map_or(Duration::from_secs(1), Duration::from_secs);
        let started = Instant::now();
        let mut fed = 0_u32;
        let mut last = output.status();
        while started.elapsed() < follow {
            let _ = output.push_samples(&frame);
            fed += 1;
            let now = output.status();
            if now != last {
                println!(
                    "{:>6} ms phase={} device={} generation={} rebuilds={} errors={} note={:?}",
                    started.elapsed().as_millis(),
                    now.phase.as_str(),
                    now.device,
                    now.generation,
                    now.rebuilds,
                    now.stream_errors,
                    now.note()
                );
                last = now;
            }
            if let Some(wait) = (Duration::from_millis(20) * fed).checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        // The device pulls: with nothing more queued it runs dry.
        let deadline = Instant::now() + Duration::from_secs(2);
        while output.snapshot().queued_ms > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let drained = output.snapshot();
        println!("after the feed: {drained:?}");
        assert_eq!(
            drained.queued_ms, 0,
            "the device callback consumed the queue"
        );

        output.close();
        assert_eq!(output.status().phase, OutputPhase::Closed);
        let _ = output.push_samples(&frame);
        std::thread::sleep(Duration::from_millis(300));
        assert!(output.snapshot().queued_ms > 0, "nothing plays after close");
    }
}
