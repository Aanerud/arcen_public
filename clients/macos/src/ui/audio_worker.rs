// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The audio worker moved to [`arcen_deck_core::runtime::audio_worker`]
//! (ADR 0024 extraction step 5); the macOS Deck plays through cpal on
//! CoreAudio. The media worker starts it with [`macos_player`].

pub use arcen_deck_core::runtime::audio_worker::{
    audio_playback_snapshot, AudioFeedStatus, AudioPlaybackCell, AudioPlaybackStatus,
};

/// The macOS player, created on the audio worker's thread.
pub fn macos_player() -> Box<dyn arcen_deck_core::runtime::audio_worker::AudioPlayback> {
    Box::new(crate::pipeline::audio::PcmAudioPlayer::awaiting_stream_result())
}
