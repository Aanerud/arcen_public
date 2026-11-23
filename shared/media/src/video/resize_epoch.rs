// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A capture source resized inside a running attachment (a Deck's
//! `display_update`: a window resized, a connect-time fit).
//!
//! The capture epoch at the old size is retired, the source is given its new
//! size, and a new epoch must take over the attachment's contract unchanged
//! except for geometry. Two rules keep that from ending anything:
//!
//! - The new epoch is given the host's capture-rate evidence for the new
//!   size whatever it replaces: Auto above
//!   [`UNQUALIFIED_COPY_FPS`](super::UNQUALIFIED_COPY_FPS) on a copying route
//!   rests on the same evidence as Speed, so withholding it clamps Auto and
//!   changes the contract. An epoch served as Auto is asked for Auto at no
//!   more than the rate it was served at, so the evidence keeps Auto's rate
//!   but never upgrades it to Speed or raises its rate
//!   ([`resized_epoch_request`]): a Speed attachment served as Auto after a
//!   resize (or a runtime shortfall) still asks for Speed's rate, and a size
//!   with evidence for that rate would otherwise serve Auto faster than the
//!   attachment streams and change the contract.
//! - An epoch that cannot take over puts the source back at the size the
//!   attachment streamed at and streams on there, the Deck told why
//!   ([`resize_recovery`]); a size that failed is refused at once for the
//!   rest of the attachment ([`RefusedResizeSizes`]). A resize never ends the
//!   user's session.

use super::PipelineId;

/// What a resized capture epoch asks the capture for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizedEpochRequest {
    /// The attachment's own request: Speed, Grading and HDR keep it (Speed
    /// may come back served as Auto at a size it has no evidence for).
    AsAttached,
    /// Auto at no more than `max_fps`: the epoch it replaces was served as
    /// Auto at that rate, and a resize never upgrades a served contract or
    /// raises its rate.
    Auto {
        /// The rate the replaced epoch was served at.
        max_fps: u32,
    },
}

impl ResizedEpochRequest {
    /// The frame rate to ask the capture for, given the attachment's
    /// current request.
    #[must_use]
    pub const fn requested_fps(self, attached_fps: u32) -> u32 {
        match self {
            Self::Auto { max_fps } if max_fps < attached_fps => max_fps,
            Self::AsAttached | Self::Auto { .. } => attached_fps,
        }
    }
}

/// The request for an epoch that replaces one served as `served_before`
/// (`None` when the capture stated no admission) at `served_fps`.
#[must_use]
pub const fn resized_epoch_request(
    served_before: Option<PipelineId>,
    served_fps: u32,
) -> ResizedEpochRequest {
    match served_before {
        Some(PipelineId::Auto) => ResizedEpochRequest::Auto {
            max_fps: served_fps,
        },
        _ => ResizedEpochRequest::AsAttached,
    }
}

/// Why a resized epoch could not take over the attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeFailure {
    /// The source reported no stream at the new size.
    NoStream,
    /// The capture process did not start.
    CaptureFailedToStart,
    /// The capture started but would change more than geometry (frame rate,
    /// codec, colour, cursor authority).
    ContractChanged,
    /// The capture delivered no keyframe in time.
    NoKeyframe,
}

impl ResizeFailure {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NoStream => "no_stream",
            Self::CaptureFailedToStart => "capture_failed_to_start",
            Self::ContractChanged => "contract_changed",
            Self::NoKeyframe => "no_keyframe",
        }
    }

    /// What the Deck is told (`display_update_result.message`).
    #[must_use]
    pub const fn deck_reason(self) -> &'static str {
        match self {
            Self::NoStream => "the host kept the current size: the resized display had no stream",
            Self::CaptureFailedToStart => {
                "the host kept the current size: capture at the new size did not start"
            }
            Self::ContractChanged => {
                "the host kept the current size: the new size cannot be served with this \
                 session's picture settings"
            }
            Self::NoKeyframe => {
                "the host kept the current size: capture at the new size delivered no picture"
            }
        }
    }
}

/// What the attachment does after a resized epoch failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeRecovery {
    /// Put the source back at the size the attachment streamed at, start an
    /// epoch there and refuse the `display_update` at that size.
    RestorePrevious,
    /// The restored size failed too: end the attachment (never the desktop)
    /// with the failure's reason.
    EndAttachment,
}

/// The recovery after a resized epoch failed, whatever the
/// [`ResizeFailure`]; `restoring` is whether the failed epoch was already
/// the restore of the previous size.
#[must_use]
pub const fn resize_recovery(restoring: bool) -> ResizeRecovery {
    if restoring {
        ResizeRecovery::EndAttachment
    } else {
        ResizeRecovery::RestorePrevious
    }
}

/// What the Deck is told when it asks again for a size in
/// [`RefusedResizeSizes`].
pub const RESIZE_SIZE_REFUSED_REASON: &str =
    "the host kept the current size: this size could not be served earlier in this session";

/// Sizes remembered per attachment.
pub const REFUSED_RESIZE_SIZES: usize = 8;

/// Sizes whose resized epoch failed in this attachment: a Deck that asks for
/// one again (a retried connect fit, a window dragged back) is refused at
/// once, at the current size, instead of the stream stopping again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefusedResizeSizes {
    sizes: Vec<(u32, u32)>,
}

impl RefusedResizeSizes {
    /// No size refused.
    #[must_use]
    pub const fn new() -> Self {
        Self { sizes: Vec::new() }
    }

    /// Remembers a size whose epoch failed (the oldest is forgotten past
    /// [`REFUSED_RESIZE_SIZES`]).
    pub fn remember(&mut self, width: u32, height: u32) {
        if self.refuses(width, height) {
            return;
        }
        if self.sizes.len() == REFUSED_RESIZE_SIZES {
            self.sizes.remove(0);
        }
        self.sizes.push((width, height));
    }

    /// Whether a request for this size is refused without a new epoch.
    #[must_use]
    pub fn refuses(&self, width: u32, height: u32) -> bool {
        self.sizes.contains(&(width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_auto_epoch_is_resized_as_auto_and_others_keep_their_request() {
        assert_eq!(
            resized_epoch_request(Some(PipelineId::Auto), 30),
            ResizedEpochRequest::Auto { max_fps: 30 }
        );
        for served in [
            Some(PipelineId::Speed),
            Some(PipelineId::Grading),
            Some(PipelineId::Hdr),
            None,
        ] {
            assert_eq!(
                resized_epoch_request(served, 30),
                ResizedEpochRequest::AsAttached
            );
            assert_eq!(resized_epoch_request(served, 30).requested_fps(60), 60);
        }
    }

    #[test]
    fn a_resized_auto_epoch_never_asks_for_more_than_auto_was_served_at() {
        // Speed at 60 served as Auto at 30 after a resize: the attachment
        // still asks for 60, the next epoch for 30.
        let auto_at_30 = resized_epoch_request(Some(PipelineId::Auto), 30);
        assert_eq!(auto_at_30.requested_fps(60), 30);
        // Never raised above what the attachment asks for.
        assert_eq!(
            resized_epoch_request(Some(PipelineId::Auto), 60).requested_fps(30),
            30
        );
        assert_eq!(
            resized_epoch_request(Some(PipelineId::Auto), 60).requested_fps(60),
            60
        );
    }

    #[test]
    fn a_failed_resize_restores_once_then_ends_only_the_attachment() {
        assert_eq!(resize_recovery(false), ResizeRecovery::RestorePrevious);
        assert_eq!(resize_recovery(true), ResizeRecovery::EndAttachment);
        for failure in [
            ResizeFailure::NoStream,
            ResizeFailure::CaptureFailedToStart,
            ResizeFailure::ContractChanged,
            ResizeFailure::NoKeyframe,
        ] {
            assert!(
                failure
                    .deck_reason()
                    .starts_with("the host kept the current size")
            );
        }
    }

    #[test]
    fn a_failed_size_is_refused_again_and_the_memory_is_bounded() {
        let mut refused = RefusedResizeSizes::new();
        assert!(!refused.refuses(1120, 732));
        refused.remember(1120, 732);
        refused.remember(1120, 732);
        assert!(refused.refuses(1120, 732));
        assert!(!refused.refuses(1120, 734));
        for width in 0..u32::try_from(REFUSED_RESIZE_SIZES).unwrap() {
            refused.remember(2000 + width, 1000);
        }
        assert!(!refused.refuses(1120, 732), "the oldest size is forgotten");
        assert!(refused.refuses(2000, 1000));
    }
}
