// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Linux Pier's carrier for one encoded frame.
//!
//! Access-unit boundaries come exclusively from capenc's explicit `framed-v1`
//! length prefix; nothing here infers a frame boundary from pipe read sizes.
//! Whether a unit is a keyframe or a recovery point is decided by the shared
//! rule every Pier uses, `arcen_media::annexb::{video_unit_is_keyframe,
//! video_unit_is_recovery_point}`.

/// One complete encoded frame, ready to be wire-framed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    /// When the Pier read this frame from the encoder. The wire timestamp is
    /// taken from it, so every host-side wait before the network counts in
    /// the frame's age.
    pub read_at: std::time::Instant,
    /// Lossless-refinement video-lane records (`RefineBarrier`/`RefineFence`)
    /// capenc wrote immediately before this access unit (ADR 0016); the
    /// video writer sends them first. Empty unless the session refines.
    pub refine: Vec<Vec<u8>>,
    /// A canvas record (ADR 0028), not an access unit: `data` is the whole
    /// wire frame (type `0x0E`) and `is_keyframe` marks a reset.
    pub canvas: bool,
}

impl AccessUnit {
    /// A frame read from the encoder now.
    #[must_use]
    pub fn new(data: Vec<u8>, is_keyframe: bool) -> Self {
        Self {
            data,
            is_keyframe,
            read_at: std::time::Instant::now(),
            refine: Vec::new(),
            canvas: false,
        }
    }

    /// A canvas record read from the encoder now. Its kind decides whether
    /// it is a recovery point (a reset supersedes what queued before it).
    ///
    /// # Errors
    ///
    /// [`std::io::ErrorKind::InvalidData`] for bytes that are not a canvas
    /// record's prefix.
    pub fn canvas(data: Vec<u8>) -> std::io::Result<Self> {
        let reset = arcen_media::canvas::serve::canvas_record_is_reset(&data).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "not a canvas record")
        })?;
        Ok(Self {
            canvas: true,
            ..Self::new(data, reset)
        })
    }
}
