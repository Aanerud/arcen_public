//! Capturing more than one display at once.
//!
//! `ScreenCaptureKit` captures a display, not a desktop: there is no single
//! stream that yields every screen. Multi-monitor therefore means running one
//! session per display and keeping their frames apart, which is what this does.
//!
//! It is deliberately separate from the single-display path rather than a
//! generalisation of it. The overwhelming majority of sessions are one screen,
//! that path is the one whose cost matters, and making it carry a vector and a
//! per-frame monitor identity to serve the minority is how a fast path stops
//! being fast. The streaming contract in the repository root says the same
//! thing about bit depth, for the same reason.
//!
//! Topology negotiation and region frame headers live beside the session
//! wire code. This module stays at the native boundary: it starts several
//! `ScreenCaptureKit` streams and keeps their frames apart.

use std::time::Duration;

use crate::capture::{CaptureConfig, CaptureError, CaptureSession, CapturedFrame};
use crate::displays::DisplaySnapshot;

/// How many displays one session will drive.
///
/// The wire contract has its own ceiling and an operator may set a lower one;
/// this is the point past which running more `ScreenCaptureKit` streams stops
/// being something this host will attempt at all, regardless of what anyone
/// asked for.
pub const MAX_CAPTURED_DISPLAYS: usize = 4;

/// One display's capture, with the identity its frames belong to.
#[derive(Debug)]
pub struct MonitorCapture {
    /// The display this stream captures.
    pub display_id: u32,
    /// Zero-based capture order only. Wire monitor ids are nonzero.
    pub monitor_index: u8,
    session: CaptureSession,
}

impl MonitorCapture {
    /// Returns the next frame from this display.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureError`] when the stream fails or the timeout expires.
    pub fn next_frame(&self, timeout: Duration) -> Result<CapturedFrame, CaptureError> {
        self.session.next_frame(timeout)
    }

    /// Stops this display's stream.
    pub fn stop(&self) {
        self.session.stop();
    }
}

/// Concurrent capture of several displays.
#[derive(Debug)]
pub struct MultiDisplayCapture {
    monitors: Vec<MonitorCapture>,
}

impl MultiDisplayCapture {
    /// Frames shed across every display because the encoder was behind.
    ///
    /// Summed, because the aggregate frame counters are summed too: a session
    /// is judged as a whole, and a single screen shedding shows up here.
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.monitors
            .iter()
            .map(|monitor| monitor.session.dropped_frames())
            .sum()
    }
}

/// Why several displays could not be captured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiCaptureError {
    /// Fewer than two displays were offered, so this is the wrong path.
    NotEnoughDisplays(usize),
    /// More displays were offered than this host will drive.
    TooManyDisplays(usize),
    /// One display's stream failed to start, named so the operator knows which.
    Display {
        /// The display that failed.
        display_id: u32,
        /// What it failed with.
        detail: String,
    },
}

impl std::fmt::Display for MultiCaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEnoughDisplays(count) => write!(
                formatter,
                "multi-display capture needs at least two displays, found {count}"
            ),
            Self::TooManyDisplays(count) => write!(
                formatter,
                "{count} displays exceeds the {MAX_CAPTURED_DISPLAYS} this host will drive"
            ),
            Self::Display { display_id, detail } => {
                write!(formatter, "display {display_id}: {detail}")
            }
        }
    }
}

impl std::error::Error for MultiCaptureError {}

impl MultiDisplayCapture {
    /// Starts one stream per display.
    ///
    /// Either every display captures or none does. A partial start would leave
    /// a session advertising a topology it cannot fill, and a client drawing a
    /// second monitor that never receives a frame is worse than a refusal it
    /// can read — so a failure stops the streams already started and names the
    /// display that caused it.
    ///
    /// # Errors
    ///
    /// Returns [`MultiCaptureError`] when the display count is outside what
    /// this host drives, or when any display's stream fails to start.
    pub fn start(displays: &[DisplaySnapshot], fps: u32) -> Result<Self, MultiCaptureError> {
        let configs = displays
            .iter()
            .map(|display| {
                CaptureConfig::sdr(
                    display.display_id,
                    display.pixel_width,
                    display.pixel_height,
                    fps,
                )
            })
            .collect();
        Self::start_configured(configs)
    }

    /// Starts one stream per capture configuration.
    ///
    /// # Errors
    ///
    /// Returns [`MultiCaptureError`] when the capture count is outside what
    /// this host drives, or when any display's stream fails to start.
    pub fn start_configured(configs: Vec<CaptureConfig>) -> Result<Self, MultiCaptureError> {
        if configs.len() < 2 {
            return Err(MultiCaptureError::NotEnoughDisplays(configs.len()));
        }
        if configs.len() > MAX_CAPTURED_DISPLAYS {
            return Err(MultiCaptureError::TooManyDisplays(configs.len()));
        }

        let mut monitors: Vec<MonitorCapture> = Vec::with_capacity(configs.len());
        for (index, config) in configs.into_iter().enumerate() {
            match CaptureSession::start(config) {
                Ok(session) => monitors.push(MonitorCapture {
                    display_id: config.display_id,
                    // The index is bounded by MAX_CAPTURED_DISPLAYS above, so
                    // this cannot truncate.
                    monitor_index: u8::try_from(index).unwrap_or(u8::MAX),
                    session,
                }),
                Err(error) => {
                    for started in &monitors {
                        started.stop();
                    }
                    return Err(MultiCaptureError::Display {
                        display_id: config.display_id,
                        detail: error.to_string(),
                    });
                }
            }
        }
        Ok(Self { monitors })
    }

    /// Returns the running captures.
    #[must_use]
    pub fn monitors(&self) -> &[MonitorCapture] {
        &self.monitors
    }

    /// Returns how many displays are being captured.
    #[must_use]
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    /// Returns whether nothing is being captured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    /// Stops every stream.
    pub fn stop(&self) {
        for monitor in &self.monitors {
            monitor.stop();
        }
    }
}

impl Drop for MultiDisplayCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::expect_used)]
mod tests {
    use super::*;

    fn display(id: u32) -> DisplaySnapshot {
        DisplaySnapshot {
            display_id: id,
            pixel_width: 1920,
            pixel_height: 1080,
            origin_x: 0.0,
            origin_y: 0.0,
        }
    }

    #[test]
    fn one_display_is_refused_because_it_is_the_other_path() {
        // Not an error the operator caused: it means the caller should be
        // using the single-display capture, and saying so beats starting one
        // stream through the expensive path.
        let error = MultiDisplayCapture::start(&[display(1)], 60).expect_err("refused");
        assert_eq!(error, MultiCaptureError::NotEnoughDisplays(1));
    }

    #[test]
    fn more_displays_than_this_host_drives_are_refused_by_count() {
        let many: Vec<_> = (1..=(MAX_CAPTURED_DISPLAYS as u32 + 1))
            .map(display)
            .collect();
        let error = MultiDisplayCapture::start(&many, 60).expect_err("refused");
        assert_eq!(
            error,
            MultiCaptureError::TooManyDisplays(MAX_CAPTURED_DISPLAYS + 1)
        );
    }

    #[test]
    fn a_refusal_names_the_display_that_failed() {
        // Display 0 is not a display. Whatever the window server says about
        // it, the message has to identify which one went wrong, because an
        // operator with four screens cannot act on "capture failed".
        let error = MultiDisplayCapture::start(&[display(0), display(0)], 60);
        if let Err(MultiCaptureError::Display { display_id, .. }) = error {
            assert_eq!(display_id, 0, "the failing display must be named");
        }
        // A machine with no window server session refuses earlier, which is a
        // different and equally explicit outcome.
    }
}
