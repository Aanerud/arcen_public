// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host audio playout on the default output device.
//!
//! The macOS Deck's output on WASAPI: the same queue and playout policy
//! (`arcen_media::audio::playback_queue`) and the same default-device
//! lifecycle ([`arcen_deck_core::runtime::audio_output`]: move to the new
//! default when it changes or the stream fails, back off, never two
//! streams, nothing after close), event-driven shared mode through cpal.
//! This adapter only finds the default device and opens a stream on it.
//! WASAPI shared mode plays only the device's mix format, so a 44.1 kHz or
//! surround device is fed through the shared `DeviceFormatAdapter` (the
//! stream ticket's renderer) instead of being refused.

use arcen_deck_core::runtime::audio_output::{
    FollowedOutput, OpenedStream, OutputBackend, StreamTicket,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// The audio backend label for Stream Statistics and logs.
pub const BACKEND: &str = "cpal-wasapi";

/// The system's default output device at one moment, compared by name (a
/// name that cannot be read matches any, so it never forces a move).
#[derive(Clone)]
pub struct DefaultOutput {
    device: cpal::Device,
    name: Option<String>,
}

impl PartialEq for DefaultOutput {
    fn eq(&self, other: &Self) -> bool {
        match (&self.name, &other.name) {
            (Some(name), Some(other)) => name == other,
            _ => true,
        }
    }
}

/// Opens WASAPI output streams for [`FollowedOutput`].
pub struct WasapiOutput {
    host: cpal::Host,
}

impl std::fmt::Debug for WasapiOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WasapiOutput")
            .finish_non_exhaustive()
    }
}

impl Default for WasapiOutput {
    fn default() -> Self {
        Self {
            host: cpal::default_host(),
        }
    }
}

/// Host audio playout on the default output device.
pub type WindowsAudioOutput = FollowedOutput<WasapiOutput>;

impl OutputBackend for WasapiOutput {
    type Device = DefaultOutput;
    type Stream = cpal::Stream;
    const BACKEND: &'static str = BACKEND;

    fn default_device(&mut self) -> Option<DefaultOutput> {
        let device = self.host.default_output_device()?;
        let name = device.name().ok();
        Some(DefaultOutput { device, name })
    }

    fn open(
        &mut self,
        output: &DefaultOutput,
        ticket: &StreamTicket,
    ) -> Result<OpenedStream<cpal::Stream>, String> {
        let device = &output.device;
        let name = output.name.clone().unwrap_or_else(|| "unknown".to_owned());
        let supported = device
            .default_output_config()
            .map_err(|error| format!("Could not read default output config: {error}"))?;
        let format = supported.sample_format();
        let config = cpal::StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let mut render = ticket.renderer(config.sample_rate.0, config.channels)?;
        let errors = ticket.errors();
        // cpal's WASAPI render thread ends after reporting any error (device
        // gone, audio service restarted), so every error means: reopen.
        let on_error = move |error: cpal::StreamError| {
            tracing::warn!(target: arcen_deck_core::log_target::AUDIO, %error, "audio stream error");
            errors.report(error.to_string());
        };
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    super::join_pro_audio_once();
                    render.fill_f32(data);
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |data: &mut [i16], _| {
                    super::join_pro_audio_once();
                    render.fill_i16(data);
                },
                on_error,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_output_stream(
                &config,
                move |data: &mut [u16], _| {
                    super::join_pro_audio_once();
                    render.fill_u16(data);
                },
                on_error,
                None,
            ),
            other => return Err(format!("Unsupported output sample format: {other:?}")),
        }
        .map_err(|error| format!("Could not open the output stream on {name}: {error}"))?;
        stream
            .play()
            .map_err(|error| format!("Could not start audio output stream: {error}"))?;
        Ok(OpenedStream {
            stream,
            device: name,
            format: format!(
                "{} Hz {} ch {format:?}",
                config.sample_rate.0, config.channels
            ),
        })
    }
}

/// The Windows Deck's host-audio player (the Mac's logic, shared).
pub type Player = arcen_deck_core::runtime::audio_player::PcmAudioPlayer<WindowsAudioOutput>;
