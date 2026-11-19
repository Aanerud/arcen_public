// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas (ADR 0028): damage-first software streaming. A Deck keeps one
//! persistent eight-bit canvas per monitor and applies ordered commands to
//! it; the Pier keeps an exact mirror of the same canvas. This module is
//! the **one** interpreter of those commands, for the Pier and every Deck.
//!
//! Everything here is OS-free and codec-free:
//!
//! - [`state::CanvasState`] validates each `arcen_protocol::canvas` record
//!   against the canvas (the checks the wire decoder cannot make) and turns
//!   it into [`state::CanvasWork`], the ordered operations an executor
//!   performs — a Deck's GPU executor, or the CPU one here.
//! - [`pixels::CanvasPixels`] is the CPU executor: the Pier's mirror pixels
//!   and the oracle every GPU executor is tested against.
//! - [`mirror::CanvasMirror`] is the two together, with the canvas digest
//!   (refine's canonical digest, ADR 0016 R4, over the exact blocks).
//! - [`tile`] packs lossless tiles at depth 8 from an eight-bit source
//!   ([`crate::refine::source::Bgra8Source`]) with `delta-zstd1`.
//!
//! - [`planner::CanvasPlanner`] turns one monitor's Keel facts into canvas
//!   records (design rules 1–7: copies, fills, cache, surfaces, tiles,
//!   one-shot pictures, refinement), with the surface lifecycle of
//!   [`surface::SurfaceSet`] and the [`surface::SurfaceEncoder`] seam the
//!   Pier implements with its software H.264 session.
//! - [`governor::CanvasGovernor`] spends one CPU budget
//!   (`video.software.cpu_share`) across every monitor in a fixed order,
//!   degrades surfaces in a fixed order (rate, quality, scale, then tile
//!   coalescing) and keeps `canvas_ack` flow control; its estimates come
//!   from [`cost::CostModel`] over Keel's `WorkMeter`.
//!
//! - [`corpus`] (tests, and the `canvas-corpus` feature): the planner's
//!   records over Keel's scenarios, which every executor is tested
//!   against.
//!
//! The Zstandard adapters are refine's, behind the same features:
//! `refine-zstd-encode` packs ([`tile::ZstdTilePacker`]),
//! `refine-zstd-decode` unpacks ([`pixels::ZstdTileDecoder`]). Neither is in
//! the default graph.

#[cfg(any(test, feature = "canvas-corpus"))]
pub mod corpus;
pub mod cost;
pub mod encode_loop;
pub mod governor;
pub mod mirror;
pub mod pixels;
pub mod planner;
pub mod serve;
pub mod state;
pub mod surface;
pub mod tile;

#[cfg(test)]
mod encode_loop_tests;
#[cfg(test)]
mod planner_tests;
#[cfg(test)]
mod tests;

pub use cost::{CostModel, DEFAULT_PRIORS_MS_PER_MPX};
pub use encode_loop::{
    CANVAS_DIGEST_EVERY_FENCES, CanvasEncodeLoop, CanvasLoopTerms, EncodeLoopConfig,
    EncodeLoopStats, FullFrameUnit, LoopEncoder, LoopError, LoopMode, LoopRecord, LoopTick,
    digest_fields,
};
pub use governor::{
    CanvasFlow, CanvasGovernor, CpuShare, DEFAULT_CPU_SHARE_PERCENT, DegradationStep,
    GovernorConfig, GovernorEvent, GovernorState, MAX_UNACKED_OPTIONAL_FRAMES, MonitorDemand,
    MonitorGrant, SurfaceDemand, TickOutcome,
};
pub use mirror::{CanvasDigest, CanvasMirror, canvas_digest};
pub use pixels::{
    CanvasExecError, CanvasPixels, NoSurfacePixels, PackedTileCodes, SurfaceDecoder,
    SurfacePicture, TileDecoder,
};
pub use planner::{
    CanvasPlanner, CommandMix, PlannedTick, PlannerConfig, PlannerError, TickEstimate,
};
pub use state::{CanvasOp, CanvasState, CanvasStateError, CanvasWork};
pub use surface::{
    SurfaceEncodeError, SurfaceEncodeRequest, SurfaceEncoder, SurfaceQuality, SurfaceScale,
};

use crate::refine::RefineContract;
use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
};
use arcen_protocol::refine::{RefineContractBytes, RefineDomain};

/// Code depth of every canvas v1 tile.
pub const CANVAS_TILE_DEPTH: u8 = arcen_protocol::canvas::CANVAS_TILE_DEPTH;

/// The colour contract of a canvas v1 canvas: full-range eight-bit R′G′B′,
/// the desktop's own sRGB-coded values in BT.709 primaries.
#[must_use]
pub fn canvas_contract() -> RefineContract {
    RefineContract {
        domain: RefineDomain::Rgb,
        depth: BitDepth::Eight,
        transfer: TransferCharacteristics::Srgb,
        primaries: ColorPrimaries::Bt709,
        range: ColorRange::Full,
        chroma: ChromaSubsampling::Yuv444,
        matrix: ColorMatrix::Identity,
    }
}

/// The coded format of every canvas surface: H.264 Constrained Baseline
/// 4:2:0 eight-bit, limited-range BT.709, which every Deck decodes into its
/// canvas.
#[must_use]
pub const fn canvas_surface_video() -> crate::VideoConfiguration {
    crate::VideoConfiguration {
        codec: crate::VideoCodec::H264,
        chroma: ChromaSubsampling::Yuv420,
        bit_depth: BitDepth::Eight,
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
    }
}

/// [`canvas_contract`] as the digest's contract bytes.
#[must_use]
pub fn canvas_contract_bytes() -> RefineContractBytes {
    canvas_contract().to_bytes()
}
