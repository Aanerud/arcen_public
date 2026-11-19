// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `media-smoke --canvas` (`ARCEN_MEDIA_SMOKE_CANVAS=1`): a Deck that asks
//! a Pier for the canvas (ADR 0028) and applies every record with the
//! shared CPU executor (`arcen_media::canvas::CanvasMirror`, the oracle
//! every Deck executor is tested against), acknowledges each frame as a
//! Deck does, and logs the canvas digest at the fences the Pier logs its
//! mirror's, so the two compare line for line. Lab evidence only: nothing
//! is presented.

use std::collections::BTreeMap;

use arcen_media::canvas::pixels::ZstdTileDecoder;
use arcen_media::canvas::{
    digest_fields, CanvasMirror, NoSurfacePixels, CANVAS_DIGEST_EVERY_FENCES,
};
use arcen_protocol::canvas::{CanvasCapability, CanvasControlMsg, CanvasRecord, CanvasTerms};

/// The canvas offer of a CPU executor (16,384 a side, two surfaces, the
/// default cache).
#[must_use]
pub fn request() -> arcen_deck_core::canvas::CanvasRequest {
    arcen_deck_core::canvas::CanvasRequest::for_executor(
        Some(arcen_deck_core::present::CanvasExecutorFacts {
            max_canvas_px: arcen_protocol::canvas::MAX_CANVAS_SIDE,
        }),
        true,
        true,
        arcen_deck_core::canvas::DEFAULT_CACHE_MIB,
    )
}

/// Per-second and total counts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CanvasCounts {
    /// Canvas record bytes.
    pub bytes: u64,
    /// Frames applied.
    pub frames: u64,
    /// Resets applied.
    pub resets: u64,
    /// Records the state owner refused.
    pub refused: u64,
    /// Digests logged.
    pub digests: u64,
}

/// The smoke's canvases, one per monitor.
pub struct CanvasSmoke {
    terms: CanvasTerms,
    mirrors: BTreeMap<u16, (CanvasMirror, u32)>,
    decoder: ZstdTileDecoder,
    /// This second.
    pub window: CanvasCounts,
    /// The whole run.
    pub total: CanvasCounts,
}

impl CanvasSmoke {
    /// A smoke under the terms it offered.
    #[must_use]
    pub fn new(offer: &CanvasCapability) -> Option<Self> {
        Some(Self {
            terms: offer.terms()?,
            mirrors: BTreeMap::new(),
            decoder: ZstdTileDecoder,
            window: CanvasCounts::default(),
            total: CanvasCounts::default(),
        })
    }

    /// Applies one record and returns the control message a Deck sends for
    /// it: an acknowledgement, or a reset request after a refusal.
    pub fn apply(&mut self, monitor_id: u16, bytes: &[u8]) -> Option<CanvasControlMsg> {
        self.window.bytes += bytes.len() as u64;
        let terms = self.terms;
        let (mirror, generation) = self
            .mirrors
            .entry(monitor_id)
            .or_insert_with(|| (CanvasMirror::new(monitor_id, terms), 0));
        let Ok(record) = CanvasRecord::decode(bytes) else {
            self.window.refused += 1;
            return Some(CanvasControlMsg::CanvasResetRequest {
                monitor_id,
                generation: *generation,
                reason: "decode".to_owned(),
            });
        };
        match &record {
            CanvasRecord::Reset(reset) => {
                self.window.resets += 1;
                *generation = reset.scope.generation.get();
            }
            CanvasRecord::Frame(_) => self.window.frames += 1,
        }
        if let Err(error) = mirror.apply(&record, &mut self.decoder, &mut NoSurfacePixels) {
            self.window.refused += 1;
            eprintln!("canvas record refused monitor={monitor_id}: {error:?}");
            return Some(CanvasControlMsg::CanvasResetRequest {
                monitor_id,
                generation: *generation,
                reason: "refused".to_owned(),
            });
        }
        let digest = mirror.digest()?;
        if digest.fence != 0 && digest.fence % CANVAS_DIGEST_EVERY_FENCES == 0 {
            self.window.digests += 1;
            println!(
                "canvas digest monitor={monitor_id} {}",
                digest_fields(*generation, &digest)
            );
        }
        Some(CanvasControlMsg::CanvasAck {
            monitor_id,
            generation: *generation,
            fence: digest.fence,
            unprocessed_bytes: 0,
            decode_ms_p95: 1,
        })
    }

    /// The per-second line; starts the next second.
    pub fn take_line(&mut self) -> String {
        let window = std::mem::take(&mut self.window);
        self.total.bytes += window.bytes;
        self.total.frames += window.frames;
        self.total.resets += window.resets;
        self.total.refused += window.refused;
        self.total.digests += window.digests;
        format!(
            "canvas-smoke wire_bytes={} frames={} resets={} refused={} digests={}",
            window.bytes, window.frames, window.resets, window.refused, window.digests
        )
    }
}
