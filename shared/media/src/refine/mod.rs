// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lossless refinement of still regions (ADR 0016), portable core.
//!
//! Everything here is codec-free and OS-free: the plan, the normative
//! limits, tile extraction and the residual transform, the canonical digest,
//! the Pier-side scheduler (generations, source fences, damage union, motion
//! gate, budget) and the Deck-side state owner (fences, generations,
//! invalidation union, revalidation before commit, verification levels).
//!
//! The Zstandard adapters are separate and feature-gated:
//! `refine-zstd-encode` (native `zstd`, for Piers) and `refine-zstd-decode`
//! (pure-Rust `ruzstd`, for Decks). Neither is in the default graph.

pub mod budget;
pub mod deck;
pub mod digest;
pub mod limits;
pub mod pier;
pub mod producer;
pub mod runtime;
pub mod tile;

#[cfg(feature = "refine-zstd-decode")]
pub mod zstd_decode;
#[cfg(feature = "refine-zstd-encode")]
pub mod zstd_encode;

use crate::{BitDepth, ChromaSubsampling, ColorPrimaries, ColorRange, TransferCharacteristics};
use arcen_protocol::refine::{LosslessRefineCapability, RefineContractBytes, RefineDomain};

/// Side of a refinement block: Keel's grid.
pub const BLOCK: usize = arcen_keel::BLOCK_SIZE;

/// Default per-block stillness before a block is refined.
pub const DEFAULT_STILL_MS: u32 = 300;
/// Default refinement ceiling in Mbit/s.
pub const DEFAULT_MAX_MBPS: u32 = 25;

/// The colour contract one refinement generation refines in (ADR 0016 R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineContract {
    /// Domain.
    pub domain: RefineDomain,
    /// Code depth.
    pub depth: BitDepth,
    /// Transfer.
    pub transfer: TransferCharacteristics,
    /// Primaries.
    pub primaries: ColorPrimaries,
    /// Range of the codes (always full for RGB tiles).
    pub range: ColorRange,
    /// Chroma of the stream the tiles overlay (4:4:4 in v1).
    pub chroma: ChromaSubsampling,
}

impl RefineContract {
    /// Bits per code.
    #[must_use]
    pub fn bits(self) -> u8 {
        u8::try_from(self.depth.bits()).unwrap_or(u8::MAX)
    }

    /// Wire bytes (H.273 values for transfer and primaries).
    #[must_use]
    pub fn to_bytes(self) -> RefineContractBytes {
        RefineContractBytes {
            domain: self.domain.wire(),
            depth: self.bits(),
            transfer: match self.transfer {
                TransferCharacteristics::Bt709 => 1,
                TransferCharacteristics::Srgb => 13,
                TransferCharacteristics::Pq => 16,
                TransferCharacteristics::Hlg => 18,
            },
            primaries: match self.primaries {
                ColorPrimaries::Bt709 => 1,
                ColorPrimaries::Bt2020 => 9,
                ColorPrimaries::DisplayP3 => 12,
            },
            // RGB tiles never pass a matrix.
            matrix: 0,
            range: match self.range {
                ColorRange::Limited => 0,
                ColorRange::Full => 1,
            },
            chroma: match self.chroma {
                ChromaSubsampling::Yuv420 => 0,
                ChromaSubsampling::Yuv422 => 1,
                ChromaSubsampling::Yuv444 => 2,
            },
        }
    }
}

/// Where a contract's refinement source lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineSource {
    /// Packed RGB10 on the CPU (Linux Xorg depth-30 `XShm`).
    CpuPackedRgb10,
    /// Y′CbCr 4:4:4 codes on the CPU (the macOS Pier's `xf44`; later slice).
    CpuYCbCr444,
    /// GPU-resident only (`NvFBC`, DMA-BUF): never refined.
    GpuOnly,
}

/// Runtime configuration (`video.lossless_refine`); off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineConfig {
    /// Whether the operator enabled refinement.
    pub enabled: bool,
    /// Per-block stillness before refinement.
    pub still_ms: u32,
    /// Ceiling in Mbit/s.
    pub max_mbps: u32,
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            still_ms: DEFAULT_STILL_MS,
            max_mbps: DEFAULT_MAX_MBPS,
        }
    }
}

/// Why refinement is off for a session or monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineOffReason {
    /// The build does not include refinement.
    NotCompiled,
    /// `video.lossless_refine` is off.
    OperatorOff,
    /// Eight-bit contracts (Auto/Speed) are never refined.
    EightBit,
    /// The stream is not 4:4:4.
    NotFourFourFour,
    /// The capture never reaches the CPU (zero-copy path).
    ZeroCopyCapture,
    /// Y′CbCr sources come in a later slice.
    YCbCrSourceNotYet,
    /// The Deck did not advertise a matching capability.
    DeckUnsupported,
    /// The Deck presents through its eight-bit fallback.
    DeckEightBitFallback,
}

impl RefineOffReason {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NotCompiled => "not_compiled",
            Self::OperatorOff => "operator_off",
            Self::EightBit => "eight_bit",
            Self::NotFourFourFour => "not_444",
            Self::ZeroCopyCapture => "zero_copy_capture",
            Self::YCbCrSourceNotYet => "ycbcr_source_not_yet",
            Self::DeckUnsupported => "deck_unsupported",
            Self::DeckEightBitFallback => "deck_eight_bit_fallback",
        }
    }
}

/// The resolved refinement plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinePlan {
    /// Refinement runs in `contract`.
    On {
        /// Contract.
        contract: RefineContract,
        /// Stillness.
        still_ms: u32,
        /// Ceiling.
        max_mbps: u32,
    },
    /// Refinement is off.
    Off(RefineOffReason),
}

/// Inputs of [`resolve_refine_plan`].
#[derive(Debug, Clone, Copy)]
pub struct RefinePlanInputs<'a> {
    /// Whether this build compiled the producer.
    pub compiled: bool,
    /// Operator configuration.
    pub config: RefineConfig,
    /// Stream depth.
    pub depth: BitDepth,
    /// Stream chroma.
    pub chroma: ChromaSubsampling,
    /// Stream transfer.
    pub transfer: TransferCharacteristics,
    /// Stream primaries.
    pub primaries: ColorPrimaries,
    /// Where the capture lives.
    pub source: RefineSource,
    /// The Deck's capability, if it sent one.
    pub deck: Option<&'a LosslessRefineCapability>,
    /// Whether the Deck presents through its native ten-bit layer.
    pub deck_ten_bit: bool,
}

/// Decides refinement for one stream. Platform code never decides it.
#[must_use]
pub fn resolve_refine_plan(inputs: &RefinePlanInputs<'_>) -> RefinePlan {
    use RefineOffReason as Why;
    if !inputs.compiled {
        return RefinePlan::Off(Why::NotCompiled);
    }
    if !inputs.config.enabled {
        return RefinePlan::Off(Why::OperatorOff);
    }
    if inputs.depth == BitDepth::Eight {
        return RefinePlan::Off(Why::EightBit);
    }
    if inputs.chroma != ChromaSubsampling::Yuv444 {
        return RefinePlan::Off(Why::NotFourFourFour);
    }
    let domain = match inputs.source {
        RefineSource::GpuOnly => return RefinePlan::Off(Why::ZeroCopyCapture),
        RefineSource::CpuYCbCr444 => return RefinePlan::Off(Why::YCbCrSourceNotYet),
        RefineSource::CpuPackedRgb10 => RefineDomain::Rgb,
    };
    if !inputs.deck.is_some_and(|deck| deck.accepts(domain)) {
        return RefinePlan::Off(Why::DeckUnsupported);
    }
    if !inputs.deck_ten_bit {
        return RefinePlan::Off(Why::DeckEightBitFallback);
    }
    RefinePlan::On {
        contract: RefineContract {
            domain,
            depth: inputs.depth,
            transfer: inputs.transfer,
            primaries: inputs.primaries,
            range: ColorRange::Full,
            chroma: inputs.chroma,
        },
        still_ms: inputs.config.still_ms,
        max_mbps: inputs.config.max_mbps,
    }
}

/// capenc argument that turns the refinement producer on (framed-v2 only).
pub const CAPENC_REFINE_ON: &str = "refine=on";

/// The refinement producer's capenc arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapencRefineArgs {
    /// `(monitor_id, topology_generation, stream_epoch)` the Pier assigned
    /// (`refine-scope=M:T:E`, default `1:1:1`). The digest commits to them,
    /// so the Pier forwards records unchanged.
    pub scope: (u16, u64, u64),
    /// `refine-still-ms=N`.
    pub still_ms: u32,
    /// `refine-mbps=N`: the refinement ceiling.
    pub max_mbps: u32,
}

/// Parses capenc's refinement arguments: `None` unless [`CAPENC_REFINE_ON`]
/// is present.
///
/// # Errors
///
/// A readable message for a malformed value, or refinement without
/// framed-v2 output (its records need the tagged framing).
pub fn capenc_refine_args(args: &[String]) -> Result<Option<CapencRefineArgs>, String> {
    if !args.iter().any(|arg| arg == CAPENC_REFINE_ON) {
        return Ok(None);
    }
    if !args
        .iter()
        .any(|arg| arg == crate::capenc_stream::FRAMED_OUTPUT_V2)
    {
        return Err("refine=on needs framed-v2 output".to_string());
    }
    let mut parsed = CapencRefineArgs {
        scope: (1, 1, 1),
        still_ms: DEFAULT_STILL_MS,
        max_mbps: DEFAULT_MAX_MBPS,
    };
    for arg in args {
        if let Some(value) = arg.strip_prefix("refine-scope=") {
            let parts: Vec<&str> = value.split(':').collect();
            let invalid = || format!("invalid refine-scope={value:?}; use MONITOR:TOPOLOGY:EPOCH");
            let [monitor, topology, epoch] = parts.as_slice() else {
                return Err(invalid());
            };
            parsed.scope = (
                monitor.parse().map_err(|_| invalid())?,
                topology.parse().map_err(|_| invalid())?,
                epoch.parse().map_err(|_| invalid())?,
            );
        } else if let Some(value) = arg.strip_prefix("refine-still-ms=") {
            parsed.still_ms = value
                .parse()
                .map_err(|_| format!("invalid refine-still-ms={value:?}"))?;
        } else if let Some(value) = arg.strip_prefix("refine-mbps=") {
            parsed.max_mbps = value
                .parse()
                .ok()
                .filter(|mbps| *mbps > 0)
                .ok_or_else(|| format!("invalid refine-mbps={value:?}"))?;
        }
    }
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capenc_refine_args_are_explicit_and_need_framed_v2() {
        let args = |list: &[&str]| {
            list.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(capenc_refine_args(&args(&["framed-v2"])), Ok(None));
        assert!(capenc_refine_args(&args(&["refine=on"])).is_err());
        assert_eq!(
            capenc_refine_args(&args(&[
                "framed-v2",
                "refine=on",
                "refine-scope=2:7:9",
                "refine-mbps=40"
            ])),
            Ok(Some(CapencRefineArgs {
                scope: (2, 7, 9),
                still_ms: DEFAULT_STILL_MS,
                max_mbps: 40
            }))
        );
        assert!(capenc_refine_args(&args(&["framed-v2", "refine=on", "refine-mbps=0"])).is_err());
    }

    fn inputs(deck: Option<&LosslessRefineCapability>) -> RefinePlanInputs<'_> {
        RefinePlanInputs {
            compiled: true,
            config: RefineConfig {
                enabled: true,
                ..RefineConfig::default()
            },
            depth: BitDepth::Ten,
            chroma: ChromaSubsampling::Yuv444,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
            source: RefineSource::CpuPackedRgb10,
            deck,
            deck_ten_bit: true,
        }
    }

    #[test]
    fn only_a_ten_bit_444_cpu_rgb_stream_to_a_capable_deck_is_refined() {
        let deck = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        assert!(matches!(
            resolve_refine_plan(&inputs(Some(&deck))),
            RefinePlan::On { .. }
        ));
        let cases: [(fn(&mut RefinePlanInputs<'_>), RefineOffReason); 7] = [
            (|i| i.compiled = false, RefineOffReason::NotCompiled),
            (|i| i.config.enabled = false, RefineOffReason::OperatorOff),
            (|i| i.depth = BitDepth::Eight, RefineOffReason::EightBit),
            (
                |i| i.chroma = ChromaSubsampling::Yuv420,
                RefineOffReason::NotFourFourFour,
            ),
            (
                |i| i.source = RefineSource::GpuOnly,
                RefineOffReason::ZeroCopyCapture,
            ),
            (|i| i.deck = None, RefineOffReason::DeckUnsupported),
            (
                |i| i.deck_ten_bit = false,
                RefineOffReason::DeckEightBitFallback,
            ),
        ];
        for (mutate, reason) in cases {
            let mut case = inputs(Some(&deck));
            mutate(&mut case);
            assert_eq!(resolve_refine_plan(&case), RefinePlan::Off(reason));
        }
        assert!(!RefineConfig::default().enabled, "refinement defaults off");
    }
}
