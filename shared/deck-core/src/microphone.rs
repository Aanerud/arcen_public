// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The microphone seam.
//!
//! The session engine asks [`crate::DeckServices::start_microphone`] for
//! capture when the Pier opens the microphone lane. The platform (macOS
//! `AVAudioEngine`, Windows WASAPI) captures, normalises to the v1 frame
//! (48 kHz mono, [`MICROPHONE_V1_FRAME_SAMPLES`] signed 16-bit samples per
//! frame) and pushes frames into a [`MicrophoneFrameSink`]; the engine
//! encodes and sends them. Samples may be speech: every buffer is zeroised
//! when it is dropped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use arcen_media::audio::MICROPHONE_V1_FRAME_SAMPLES;
use zeroize::Zeroize;

/// One normalised v1 microphone frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedMicrophoneFrame {
    /// Mono 48 kHz samples.
    pub samples: [i16; MICROPHONE_V1_FRAME_SAMPLES],
    /// Capture order, starting at zero, wrapping.
    pub sequence: u32,
    /// Capture time in milliseconds since capture started.
    pub timestamp_ms: u32,
}

impl CapturedMicrophoneFrame {
    /// Wipes the samples.
    pub fn zeroize(&mut self) {
        self.samples.zeroize();
    }
}

impl Drop for CapturedMicrophoneFrame {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// Why microphone capture did not start or stopped. The engine reports
/// `PermissionDenied` to the Pier as such; everything else as a capture
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneCaptureError {
    /// This Deck has no microphone capture.
    UnsupportedPlatform,
    /// The user or policy refused microphone access.
    PermissionDenied,
    /// Access was withdrawn while capturing.
    PermissionLost,
    /// No input device.
    InputUnavailable,
    /// The input device went away while capturing.
    InputRemoved,
    /// The device delivered a format the normaliser cannot read.
    InvalidCallbackFormat,
    /// The audio engine or client did not start.
    EngineStartFailed,
    /// The audio engine stopped by itself.
    EngineStopped,
    /// Callbacks stopped arriving.
    ProgressStarved,
    /// Start-up (including a permission prompt) took too long.
    StartupTimedOut,
    /// The session asked to stop before capture started.
    Cancelled,
}

/// The two flags that end a capture: this stream's stop and the session's
/// close.
#[derive(Debug, Clone)]
pub struct MicrophoneCancel {
    /// Set when this capture should stop.
    pub stop: Arc<AtomicBool>,
    /// Set when the whole session closed.
    pub session_closed: Arc<AtomicBool>,
}

impl MicrophoneCancel {
    /// Whether either flag is set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.session_closed.load(Ordering::Acquire)
    }
}

/// Where a platform capture delivers frames. Implemented by the engine
/// (`runtime::MicrophoneQueue`); called from the platform's capture thread.
pub trait MicrophoneFrameSink: Send + Sync {
    /// Queues a frame. The sink keeps only the newest few and wipes what it
    /// drops; this never blocks the capture thread.
    fn push(&self, frame: CapturedMicrophoneFrame);

    /// Ends the stream: `Ok(())` after a requested stop, otherwise why
    /// capture ended. Later pushes are wiped and ignored.
    fn finish(&self, result: Result<(), MicrophoneCaptureError>);
}

/// A running platform capture.
///
/// Dropping it must request a stop without blocking the caller (the engine
/// may drop it on an async task); [`Self::stop`] stops and waits.
pub trait MicrophoneStream: Send {
    /// Stops capture and waits until the platform delivers no more frames.
    fn stop(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_follows_either_flag() {
        let cancel = MicrophoneCancel {
            stop: Arc::new(AtomicBool::new(false)),
            session_closed: Arc::new(AtomicBool::new(false)),
        };
        assert!(!cancel.is_cancelled());
        cancel.session_closed.store(true, Ordering::Release);
        assert!(cancel.is_cancelled());
    }
}
