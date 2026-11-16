// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Audio (owner: `platform` stream).
//!
//! WASAPI shared-mode render through cpal (the macOS Deck's audio library)
//! with the shared playout policy (`arcen_media::audio::playback_queue`),
//! following the default device; and microphone capture behind
//! `arcen_deck_core::DeckServices::start_microphone`. Both device threads
//! join MMCSS "Pro Audio" on their first callback.

#![allow(
    unsafe_code,
    reason = "Win32 is a C API; every unsafe block states its contract"
)]

pub mod microphone;
pub mod output;

use std::cell::Cell;

use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
use windows::core::w;

thread_local! {
    static MMCSS_JOINED: Cell<bool> = const { Cell::new(false) };
}

/// Registers the calling device thread with MMCSS "Pro Audio" once, as the
/// macOS audio threads run at real-time priority. A refusal leaves the
/// thread at cpal's time-critical priority.
pub(crate) fn join_pro_audio_once() {
    MMCSS_JOINED.with(|joined| {
        if joined.replace(true) {
            return;
        }
        let mut task_index = 0_u32;
        // SAFETY: a static task name and a live out-parameter; the handle is
        // released when the thread exits.
        let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &raw mut task_index) };
    });
}
