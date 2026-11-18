// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How a video present reaches the glass: the fullscreen arrangement DWM
//! needs to stop composing the video.
//!
//! DWM composes every flip-model present into its own frame unless the
//! swapchain alone covers the output: then it can flip the swapchain's
//! buffers straight to the display ("independent flip"), or scan it out
//! from a hardware overlay plane (MPO). The conditions DWM
//! checks that the Deck controls: the video covers the monitor exactly
//! (no scaling, no letterbox), it is opaque (`DXGI_ALPHA_MODE_IGNORE`), and
//! nothing the Deck draws is above it. Whether the GPU, its driver and the
//! display allow it at all is DWM's decision; a virtual GPU usually does
//! not (DirectFlip needs the driver's support).
//!
//! Composed or not, a present at sync interval 1 waits for a vsync before
//! DWM may use it: one refresh later than at interval 0 (Windows lab,
//! phase-swept, composed: 25.9 → 12.2 ms present to glass). When each frame
//! is presented is the served pipeline's shared
//! [`arcen_media::video::PresentTiming`] ([`super::swapchain::VideoSwapchain::present`]).
//!
//! Fullscreen draws nothing over the video for every preset: the connection
//! bar opens only at the top edge or with Ctrl+Alt+Home.

use super::composition::VideoFit;

/// What stops DWM from flipping the video swapchain straight to the
/// display, as far as the Deck's own arrangement goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlipBlocker {
    /// The window does not cover its monitor.
    Windowed,
    /// No presenter shows a picture yet.
    NoVideo,
    /// The picture is scaled or letterboxed to fit the monitor.
    Scaled,
    /// The Deck draws over the video (connection bar, panels, notices).
    Overlay,
}

impl FlipBlocker {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Windowed => "the window is not fullscreen",
            Self::NoVideo => "no video shown yet",
            Self::Scaled => {
                "the picture is scaled or letterboxed (stream size is not the monitor's)"
            }
            Self::Overlay => "the Deck draws over the video",
        }
    }
}

/// The arrangement of one window's composition tree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Arrangement {
    pub fullscreen: bool,
    /// The picture's fit, when a presenter shows one.
    pub fit: Option<VideoFit>,
    /// The overlay visual is in the tree (the Deck draws something).
    pub overlay_attached: bool,
}

impl Arrangement {
    /// The first reason DWM must compose the video, or `None` when the
    /// arrangement is eligible for independent flip.
    #[must_use]
    pub fn blocker(&self) -> Option<FlipBlocker> {
        if !self.fullscreen {
            return Some(FlipBlocker::Windowed);
        }
        let fit = self.fit?;
        if !covers_exactly(fit) {
            return Some(FlipBlocker::Scaled);
        }
        self.overlay_attached.then_some(FlipBlocker::Overlay)
    }

    /// Like [`Self::blocker`], with "no video" when nothing is shown.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match (self.fullscreen, self.fit) {
            (true, None) => FlipBlocker::NoVideo.reason(),
            _ => self
                .blocker()
                .map_or("eligible for independent flip", FlipBlocker::reason),
        }
    }
}

/// The picture is placed 1:1 at the client's origin, so the swapchain's
/// buffers are the client area pixel for pixel.
#[must_use]
pub fn covers_exactly(fit: VideoFit) -> bool {
    (fit.scale - 1.0).abs() < f32::EPSILON
        && fit.offset_x.abs() < f32::EPSILON
        && fit.offset_y.abs() < f32::EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fullscreen_one_to_one_picture_with_nothing_over_it_is_eligible() {
        let exact = VideoFit::new((1800, 1130), (1800, 1130));
        let mut arrangement = Arrangement {
            fullscreen: true,
            fit: Some(exact),
            overlay_attached: false,
        };
        assert_eq!(arrangement.blocker(), None);
        assert_eq!(arrangement.describe(), "eligible for independent flip");
        arrangement.overlay_attached = true;
        assert_eq!(arrangement.blocker(), Some(FlipBlocker::Overlay));
        arrangement.fit = Some(VideoFit::new((1920, 1080), (1800, 1130)));
        assert_eq!(arrangement.blocker(), Some(FlipBlocker::Scaled));
        arrangement.fit = None;
        assert_eq!(arrangement.describe(), "no video shown yet");
        arrangement.fullscreen = false;
        assert_eq!(arrangement.blocker(), Some(FlipBlocker::Windowed));
    }

    #[test]
    fn a_same_aspect_upscale_is_not_an_exact_cover() {
        assert!(!covers_exactly(VideoFit::new((900, 565), (1800, 1130))));
        assert!(covers_exactly(VideoFit::new((3600, 2260), (3600, 2260))));
    }
}
