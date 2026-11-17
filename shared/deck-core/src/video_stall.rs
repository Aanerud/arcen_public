// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! When a session's picture counts as stalled.
//!
//! Capture is change-driven: a host monitor whose desktop does not change
//! sends no frames, for seconds at a time, and in a multi-monitor session one
//! monitor can be still while another moves. A picture that has not changed
//! is therefore not a stalled stream. It is stalled when the Deck is
//! waiting for a picture it cannot show (a keyframe after loss), when this
//! picture's own monitor keeps sending and nothing reaches the screen, or
//! when the host has gone silent altogether.

use std::time::Duration;

/// How long without a new picture before a stall may be shown.
pub const VIDEO_STALL_AFTER: Duration = Duration::from_secs(4);

/// How long without any word from the host before the link counts as
/// silent: the Deck pings every five seconds, so two missed answers.
pub const HOST_SILENT_AFTER: Duration = Duration::from_secs(12);

/// What the Deck knows about one presented picture and its session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoStallFacts {
    /// Since this picture last changed on screen; `None` before the first.
    pub since_presented: Option<Duration>,
    /// The decoder is waiting for a keyframe for this picture.
    pub waiting_for_keyframe: bool,
    /// Since this picture's own monitor first sent a packet that has not
    /// reached the screen; `None` while everything it sent was shown.
    pub since_unshown_packet: Option<Duration>,
    /// Since the last video packet of any monitor arrived.
    pub since_video_packet: Option<Duration>,
    /// Since the host last answered a health ping.
    pub since_host_answer: Option<Duration>,
}

/// Whether the picture should be shown as stalled.
#[must_use]
pub fn video_stalled(facts: VideoStallFacts) -> bool {
    if facts
        .since_presented
        .is_none_or(|since| since < VIDEO_STALL_AFTER)
    {
        return false;
    }
    if facts.waiting_for_keyframe
        || facts
            .since_unshown_packet
            .is_some_and(|since| since >= VIDEO_STALL_AFTER)
    {
        return true;
    }
    let video_silent = facts
        .since_video_packet
        .is_none_or(|since| since >= VIDEO_STALL_AFTER);
    let host_silent = facts
        .since_host_answer
        .is_none_or(|since| since >= HOST_SILENT_AFTER);
    video_silent && host_silent
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn a_still_host_monitor_on_a_live_session_is_not_a_stall() {
        // The other monitor still streams.
        assert!(!video_stalled(VideoStallFacts {
            since_presented: Some(secs(14)),
            waiting_for_keyframe: false,
            since_unshown_packet: None,
            since_video_packet: Some(secs(0)),
            since_host_answer: Some(secs(3)),
        }));
        // Every monitor is still; the host still answers its pings.
        assert!(!video_stalled(VideoStallFacts {
            since_presented: Some(secs(30)),
            waiting_for_keyframe: false,
            since_unshown_packet: None,
            since_video_packet: Some(secs(30)),
            since_host_answer: Some(secs(4)),
        }));
    }

    #[test]
    fn a_silent_host_or_a_keyframe_wait_is_a_stall() {
        assert!(video_stalled(VideoStallFacts {
            since_presented: Some(secs(13)),
            waiting_for_keyframe: false,
            since_unshown_packet: None,
            since_video_packet: Some(secs(13)),
            since_host_answer: Some(secs(12)),
        }));
        assert!(video_stalled(VideoStallFacts {
            since_presented: Some(secs(5)),
            waiting_for_keyframe: true,
            since_unshown_packet: None,
            since_video_packet: Some(secs(0)),
            since_host_answer: Some(secs(1)),
        }));
    }

    #[test]
    fn packets_of_this_picture_that_never_reach_the_screen_are_a_stall() {
        // Its own monitor keeps sending, the host answers, nothing shows.
        let facts = VideoStallFacts {
            since_presented: Some(secs(9)),
            waiting_for_keyframe: false,
            since_unshown_packet: Some(secs(5)),
            since_video_packet: Some(secs(0)),
            since_host_answer: Some(secs(1)),
        };
        assert!(video_stalled(facts));
        assert!(!video_stalled(VideoStallFacts {
            since_unshown_packet: Some(secs(1)),
            ..facts
        }));
    }

    #[test]
    fn nothing_is_stalled_before_the_first_picture_or_within_four_seconds() {
        assert!(!video_stalled(VideoStallFacts {
            waiting_for_keyframe: true,
            ..VideoStallFacts::default()
        }));
        assert!(!video_stalled(VideoStallFacts {
            since_presented: Some(secs(3)),
            waiting_for_keyframe: true,
            ..VideoStallFacts::default()
        }));
    }
}
