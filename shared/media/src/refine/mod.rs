// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lossless refinement of still regions (ADR 0016) — "exact still image":
//! once part of the picture stops moving, the Pier sends its exact source
//! pixels losslessly and the Deck paints them over the video and checks
//! them. This module is the **one** implementation; products only adapt.
//!
//! Everything here is codec-free and OS-free: the plan
//! ([`resolve_refine_plan`]) and its hello truth, the capenc arguments, the
//! normative limits, tile extraction and the residual transform, the
//! canonical digest, the Pier-side scheduler (generations, source fences,
//! damage union, motion gate, budget), the Pier's forwarding carry
//! ([`carry`]), and the Deck-side state owner, decode worker and
//! verification levels ([`deck`], [`hub`]).
//!
//! Two adapter traits are all a product implements:
//!
//! - Pier: [`source::RefineSource`] — exact source pixels of still blocks,
//!   in the stream's code values (Linux Xorg depth 30:
//!   [`source::PackedRgb10Source`]; Windows FP16 scRGB:
//!   [`scrgb::ScrgbHalfSource`]; planar or biplanar ten-bit Y′CbCr 4:4:4,
//!   the macOS Pier's `xf44`: [`ycbcr::YCbCr444Source`]).
//! - Deck: [`hub::RefineCompositor`] — paint exact tiles over the presented
//!   frame and read them back for verification (macOS: the ten-bit Metal
//!   presenter).
//!
//! The Zstandard adapters are separate and feature-gated:
//! `refine-zstd-encode` (native `zstd`, for Piers) and `refine-zstd-decode`
//! (pure-Rust `ruzstd`, for Decks). Neither is in the default graph.

pub mod budget;
pub mod carry;
pub mod compositor;
pub mod deck;
pub mod digest;
pub mod hub;
pub mod limits;
pub mod pier;
pub mod producer;
pub mod runtime;
pub mod scrgb;
pub mod source;
pub mod tile;
pub mod ycbcr;

#[cfg(feature = "refine-zstd-decode")]
pub mod zstd_decode;
#[cfg(feature = "refine-zstd-encode")]
pub mod zstd_encode;

use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
};
use arcen_protocol::refine::{
    CODEC_DELTA_ZSTD1_TOKEN, LosslessRefineCapability, LosslessRefineStatusMsg,
    RefineContractBytes, RefineDomain, RefineStateMsg,
};

/// The refine monitor id of a single-monitor session (first slice): the
/// Pier's capenc scope and the Deck's status both use it.
pub const PRIMARY_MONITOR: u16 = 1;

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
    /// Range of the codes (always full for RGB tiles; the stream's own for
    /// Y′CbCr tiles).
    pub range: ColorRange,
    /// Chroma of the stream the tiles overlay (4:4:4 in v1).
    pub chroma: ChromaSubsampling,
    /// Matrix of Y′CbCr codes ([`ColorMatrix::Identity`] for RGB tiles,
    /// which never pass a matrix).
    pub matrix: ColorMatrix,
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
            matrix: match self.domain {
                RefineDomain::Rgb => 0,
                RefineDomain::YCbCr => matrix_h273(self.matrix),
            },
            range: range_byte(self.range),
            chroma: match self.chroma {
                ChromaSubsampling::Yuv420 => 0,
                ChromaSubsampling::Yuv422 => 1,
                ChromaSubsampling::Yuv444 => 2,
            },
        }
    }
}

impl RefineContract {
    /// Whether this contract's codes are the ones a stream of `depth`,
    /// `transfer` and `primaries` presents: a Deck composites tiles only
    /// over a picture in their own coding domain (PQ codes never over an
    /// SDR picture, nor SDR codes over a PQ one).
    #[must_use]
    pub fn describes_stream(
        bytes: RefineContractBytes,
        depth: BitDepth,
        transfer: TransferCharacteristics,
        primaries: ColorPrimaries,
    ) -> bool {
        let stream = Self {
            domain: RefineDomain::Rgb,
            depth,
            transfer,
            primaries,
            range: ColorRange::Full,
            chroma: ChromaSubsampling::Yuv444,
            matrix: ColorMatrix::Identity,
        }
        .to_bytes();
        (bytes.depth, bytes.transfer, bytes.primaries)
            == (stream.depth, stream.transfer, stream.primaries)
    }

    /// [`Self::describes_stream`], and for Y′CbCr tiles also the stream's
    /// matrix and range: Y′CbCr codes go through the presenter's own matrix
    /// and range, so they are composited only over a picture that uses the
    /// same ones. RGB tiles ignore both (they never pass a matrix).
    #[must_use]
    pub fn describes_coded_stream(
        bytes: RefineContractBytes,
        depth: BitDepth,
        transfer: TransferCharacteristics,
        primaries: ColorPrimaries,
        matrix: ColorMatrix,
        range: ColorRange,
    ) -> bool {
        if !Self::describes_stream(bytes, depth, transfer, primaries) {
            return false;
        }
        match RefineDomain::from_wire(bytes.domain) {
            Some(RefineDomain::Rgb) => true,
            Some(RefineDomain::YCbCr) => {
                bytes.matrix == matrix_h273(matrix)
                    && bytes.range == range_byte(range)
                    && matrix != ColorMatrix::Identity
            }
            None => false,
        }
    }
}

/// H.273 `matrix_coefficients` of a matrix.
const fn matrix_h273(matrix: ColorMatrix) -> u8 {
    match matrix {
        ColorMatrix::Identity => 0,
        ColorMatrix::Bt709 => 1,
        ColorMatrix::Bt601 => 6,
        ColorMatrix::Bt2020Ncl => 9,
    }
}

const fn range_byte(range: ColorRange) -> u8 {
    match range {
        ColorRange::Limited => 0,
        ColorRange::Full => 1,
    }
}

/// Where a contract's refinement source lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineSourceKind {
    /// R′G′B′ codes on the CPU at the stream's depth: Linux Xorg depth-30
    /// `XShm`, and the Linux Wayland wide composition, which reads every
    /// `RecordMonitor` DMA-BUF back into host memory before the encoder's
    /// conversion ([`source::PackedRgb10Source`] over either).
    CpuRgb,
    /// Y′CbCr 4:4:4 codes on the CPU, as the encoder is given them (the
    /// macOS Pier's `xf44`, [`ycbcr::YCbCr444Source`]): refined in the
    /// Y′CbCr domain, in the source's own matrix and range.
    CpuYCbCr444 {
        /// Matrix the codes were made with.
        matrix: ColorMatrix,
        /// Range of the codes.
        range: ColorRange,
    },
    /// GPU-resident only (`NvFBC`, DMA-BUF): never refined.
    GpuOnly,
}

impl RefineSourceKind {
    /// Where a Linux Pier's capture of a `depth` stream lives, on either
    /// desktop provider: every ten-bit capture (Xorg depth-30 `XShm`, the
    /// Wayland wide read-back) is staged from host memory, every eight-bit
    /// one (`NvFBC`, the Wayland eight-bit imports) stays on the GPU.
    #[must_use]
    pub const fn for_linux_capture(depth: BitDepth) -> Self {
        match depth {
            BitDepth::Eight => Self::GpuOnly,
            BitDepth::Ten | BitDepth::Twelve => Self::CpuRgb,
        }
    }
}

/// Runtime configuration (`video.lossless_refinement`); on by default (the
/// plan still refuses every ineligible session and says why).
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
            enabled: true,
            still_ms: DEFAULT_STILL_MS,
            max_mbps: DEFAULT_MAX_MBPS,
        }
    }
}

impl RefineConfig {
    /// The operator's switch with the default stillness and ceiling.
    #[must_use]
    pub fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            ..Self::default()
        }
    }
}

/// Why refinement is off for a session or monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineOffReason {
    /// The build does not include refinement.
    NotCompiled,
    /// `video.lossless_refinement` is `disabled`.
    OperatorOff,
    /// Eight-bit contracts (Auto/Speed) are never refined.
    EightBit,
    /// The stream is not 4:4:4.
    NotFourFourFour,
    /// The capture never reaches the CPU (zero-copy path).
    ZeroCopyCapture,
    /// A Y′CbCr source this build cannot refine (kept for older Piers'
    /// hello tokens).
    YCbCrSourceNotYet,
    /// The Deck did not advertise a matching capability.
    DeckUnsupported,
    /// The Deck presents through its eight-bit fallback.
    DeckEightBitFallback,
    /// The session shows more than one monitor (later slice).
    MultiMonitor,
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
            Self::MultiMonitor => "multi_monitor",
        }
    }

    /// Every reason, for token round trips.
    pub const ALL: [Self; 9] = [
        Self::NotCompiled,
        Self::OperatorOff,
        Self::EightBit,
        Self::NotFourFourFour,
        Self::ZeroCopyCapture,
        Self::YCbCrSourceNotYet,
        Self::DeckUnsupported,
        Self::DeckEightBitFallback,
        Self::MultiMonitor,
    ];

    /// The reason a token names, if any.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.token() == token)
    }

    /// Words for the Deck's Stream Statistics.
    #[must_use]
    pub const fn user_text(self) -> &'static str {
        match self {
            Self::NotCompiled => "not available in this host build",
            Self::OperatorOff => "off on this host (video.lossless_refinement)",
            Self::EightBit => "only for Grading and HDR",
            Self::NotFourFourFour => "needs a 4:4:4 stream",
            Self::ZeroCopyCapture => "not on this host's capture path",
            Self::YCbCrSourceNotYet => "not yet on this host",
            Self::DeckUnsupported => "not requested by this Deck",
            Self::DeckEightBitFallback => "off: this Deck is showing the 8-bit fallback",
            Self::MultiMonitor => "one display only for now",
        }
    }
}

/// Words for an off-state reason token from a Pier's hello; an unknown
/// (newer) token is shown as it is.
#[must_use]
pub fn off_reason_text(token: &str) -> String {
    RefineOffReason::from_token(token).map_or_else(
        || format!("off ({token})"),
        |reason| reason.user_text().to_owned(),
    )
}

/// One operator line about exact still image for a Pier's `validate-config`
/// and start-up log: what the administrator's switch resolves to and when it
/// engages.
#[must_use]
pub fn operator_status_line(compiled: bool, enabled: bool) -> String {
    if !compiled {
        return format!(
            "Exact still image: {}",
            RefineOffReason::NotCompiled.user_text()
        );
    }
    if !enabled {
        return "Exact still image: off (video.lossless_refinement is \"disabled\")".to_owned();
    }
    "Exact still image: enabled; it engages only for a Deck that asks for it, on a Grading or \
     HDR session (4:4:4 ten-bit) with a CPU refine source and one display, and the Deck shows \
     the reason on every other session"
        .to_owned()
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

impl RefinePlan {
    /// Whether refinement runs.
    #[must_use]
    pub const fn is_on(&self) -> bool {
        matches!(self, Self::On { .. })
    }

    /// The hello truth for this plan (`ServerHelloMsg.lossless_refine`).
    #[must_use]
    pub fn hello_status(&self) -> LosslessRefineStatusMsg {
        match *self {
            Self::On {
                contract,
                still_ms,
                max_mbps,
            } => LosslessRefineStatusMsg {
                state: RefineStateMsg::On,
                reason: None,
                domain: Some(contract.domain.token().to_owned()),
                bit_depth: Some(contract.bits()),
                codec: Some(CODEC_DELTA_ZSTD1_TOKEN.to_owned()),
                still_ms: Some(still_ms),
                max_mbps: Some(max_mbps),
            },
            Self::Off(reason) => LosslessRefineStatusMsg {
                state: RefineStateMsg::Off,
                reason: Some(reason.token().to_owned()),
                domain: None,
                bit_depth: None,
                codec: None,
                still_ms: None,
                max_mbps: None,
            },
        }
    }

    /// The capenc arguments that turn the producer on for this plan, scoped
    /// to `(monitor_id, topology_generation, stream_epoch)`; empty when off.
    /// The caller must also request framed-v2 output
    /// ([`crate::capenc_stream::FRAMED_OUTPUT_V2`]).
    #[must_use]
    pub fn capenc_args(&self, scope: (u16, u64, u64)) -> Vec<String> {
        let Self::On {
            still_ms, max_mbps, ..
        } = *self
        else {
            return Vec::new();
        };
        let (monitor, topology, epoch) = scope;
        vec![
            CAPENC_REFINE_ON.to_owned(),
            format!("refine-scope={monitor}:{topology}:{epoch}"),
            format!("refine-still-ms={still_ms}"),
            format!("refine-mbps={max_mbps}"),
        ]
    }
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
    pub source: RefineSourceKind,
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
    let (domain, matrix, range) = match inputs.source {
        RefineSourceKind::GpuOnly => return RefinePlan::Off(Why::ZeroCopyCapture),
        // A Y′CbCr code without a matrix names no colour.
        RefineSourceKind::CpuYCbCr444 {
            matrix: ColorMatrix::Identity,
            ..
        } => return RefinePlan::Off(Why::YCbCrSourceNotYet),
        RefineSourceKind::CpuYCbCr444 { matrix, range } => (RefineDomain::YCbCr, matrix, range),
        RefineSourceKind::CpuRgb => (RefineDomain::Rgb, ColorMatrix::Identity, ColorRange::Full),
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
            range,
            chroma: inputs.chroma,
            matrix,
        },
        still_ms: inputs.config.still_ms,
        max_mbps: inputs.config.max_mbps,
    }
}

/// What a host knows about one capture, for [`plan_for_capture`].
#[derive(Debug, Clone, Copy)]
pub struct CaptureRefineFacts {
    /// Stream depth.
    pub depth: BitDepth,
    /// Stream chroma.
    pub chroma: ChromaSubsampling,
    /// Stream transfer.
    pub transfer: TransferCharacteristics,
    /// Stream primaries.
    pub primaries: ColorPrimaries,
    /// Where the captured pixels live.
    pub source: RefineSourceKind,
    /// The session shows exactly one monitor (the first slice refines one).
    pub single_monitor: bool,
}

/// The plan for one capture of a host session: the operator's switch,
/// whether the host binary contains the producer, the Deck's capability
/// (absent when its switch is off) and the capture's facts. A Deck
/// advertises only while its ten-bit presenter runs, and turns a monitor
/// off itself (`refine_control`) if it falls back to eight bits.
#[must_use]
pub fn plan_for_capture(
    compiled: bool,
    operator_enabled: bool,
    deck: Option<&LosslessRefineCapability>,
    facts: CaptureRefineFacts,
) -> RefinePlan {
    if !facts.single_monitor {
        return RefinePlan::Off(RefineOffReason::MultiMonitor);
    }
    resolve_refine_plan(&RefinePlanInputs {
        compiled,
        config: RefineConfig::with_enabled(operator_enabled),
        depth: facts.depth,
        chroma: facts.chroma,
        transfer: facts.transfer,
        primaries: facts.primaries,
        source: facts.source,
        deck,
        deck_ten_bit: true,
    })
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
            source: RefineSourceKind::CpuRgb,
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
                |i| i.source = RefineSourceKind::GpuOnly,
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
        assert!(RefineConfig::default().enabled, "refinement defaults on");
    }

    #[test]
    fn an_hdr_stream_is_refined_in_its_own_pq_codes() {
        let deck = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        let mut hdr = inputs(Some(&deck));
        hdr.transfer = TransferCharacteristics::Pq;
        hdr.primaries = ColorPrimaries::Bt2020;
        let RefinePlan::On { contract, .. } = resolve_refine_plan(&hdr) else {
            panic!("HDR 4:4:4 ten-bit is refined");
        };
        let bytes = contract.to_bytes();
        assert_eq!((bytes.depth, bytes.transfer, bytes.primaries), (10, 16, 9));
        assert_eq!((bytes.matrix, bytes.range, bytes.chroma), (0, 1, 2));
        let ten = BitDepth::Ten;
        assert!(RefineContract::describes_stream(
            bytes,
            ten,
            TransferCharacteristics::Pq,
            ColorPrimaries::Bt2020
        ));
        // PQ tiles are never composited over an SDR picture, nor SDR tiles
        // over a PQ one.
        for (transfer, primaries) in [
            (TransferCharacteristics::Bt709, ColorPrimaries::Bt709),
            (TransferCharacteristics::Srgb, ColorPrimaries::Bt709),
            (TransferCharacteristics::Pq, ColorPrimaries::Bt709),
            (TransferCharacteristics::Hlg, ColorPrimaries::Bt2020),
        ] {
            assert!(!RefineContract::describes_stream(
                bytes, ten, transfer, primaries
            ));
        }
        assert!(!RefineContract::describes_stream(
            bytes,
            BitDepth::Twelve,
            TransferCharacteristics::Pq,
            ColorPrimaries::Bt2020
        ));
        let RefinePlan::On { contract, .. } = resolve_refine_plan(&inputs(Some(&deck))) else {
            panic!("Grading is refined");
        };
        assert!(!RefineContract::describes_stream(
            contract.to_bytes(),
            ten,
            TransferCharacteristics::Pq,
            ColorPrimaries::Bt2020
        ));
    }

    #[test]
    fn a_ycbcr_444_source_is_refined_in_its_own_matrix_for_grading_and_hdr() {
        let both = LosslessRefineCapability {
            domains: vec!["rgb".into(), "ycbcr".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        let rgb_only = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        let grading = CaptureRefineFacts {
            depth: BitDepth::Ten,
            chroma: ChromaSubsampling::Yuv444,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
            source: RefineSourceKind::CpuYCbCr444 {
                matrix: ColorMatrix::Bt709,
                range: ColorRange::Full,
            },
            single_monitor: true,
        };
        let hdr = CaptureRefineFacts {
            transfer: TransferCharacteristics::Pq,
            primaries: ColorPrimaries::Bt2020,
            source: RefineSourceKind::CpuYCbCr444 {
                matrix: ColorMatrix::Bt2020Ncl,
                range: ColorRange::Full,
            },
            ..grading
        };
        for (facts, wire) in [(grading, (1, 1, 1)), (hdr, (16, 9, 9))] {
            let plan = plan_for_capture(true, true, Some(&both), facts);
            let RefinePlan::On { contract, .. } = plan else {
                panic!("a ten-bit 4:4:4 Y′CbCr capture is refined: {plan:?}");
            };
            let bytes = contract.to_bytes();
            assert_eq!(bytes.domain, RefineDomain::YCbCr.wire());
            assert_eq!((bytes.transfer, bytes.primaries, bytes.matrix), wire);
            assert_eq!((bytes.depth, bytes.range, bytes.chroma), (10, 1, 2));
            assert_eq!(plan.hello_status().domain.as_deref(), Some("ycbcr"));
            // A Deck that only composites R′G′B′ tiles is not sent Y′CbCr.
            assert_eq!(
                plan_for_capture(true, true, Some(&rgb_only), facts),
                RefinePlan::Off(RefineOffReason::DeckUnsupported)
            );
            // The eight-bit contracts stay off.
            let auto = CaptureRefineFacts {
                depth: BitDepth::Eight,
                chroma: ChromaSubsampling::Yuv420,
                ..facts
            };
            assert_eq!(
                plan_for_capture(true, true, Some(&both), auto),
                RefinePlan::Off(RefineOffReason::EightBit)
            );
        }
        // A Y′CbCr code without a matrix names no colour: refused.
        let no_matrix = CaptureRefineFacts {
            source: RefineSourceKind::CpuYCbCr444 {
                matrix: ColorMatrix::Identity,
                range: ColorRange::Full,
            },
            ..grading
        };
        assert_eq!(
            plan_for_capture(true, true, Some(&both), no_matrix),
            RefinePlan::Off(RefineOffReason::YCbCrSourceNotYet)
        );
        // The Deck composites Y′CbCr tiles only over its own matrix and range.
        let RefinePlan::On { contract, .. } = plan_for_capture(true, true, Some(&both), grading)
        else {
            panic!("on");
        };
        let bytes = contract.to_bytes();
        let ten = BitDepth::Ten;
        let (sdr, bt709) = (TransferCharacteristics::Bt709, ColorPrimaries::Bt709);
        assert!(RefineContract::describes_coded_stream(
            bytes,
            ten,
            sdr,
            bt709,
            ColorMatrix::Bt709,
            ColorRange::Full
        ));
        for (matrix, range) in [
            (ColorMatrix::Bt2020Ncl, ColorRange::Full),
            (ColorMatrix::Identity, ColorRange::Full),
            (ColorMatrix::Bt709, ColorRange::Limited),
        ] {
            assert!(!RefineContract::describes_coded_stream(
                bytes, ten, sdr, bt709, matrix, range
            ));
        }
        // R′G′B′ tiles never pass a matrix: any matrix and range will do.
        let RefinePlan::On { contract, .. } = resolve_refine_plan(&inputs(Some(&both))) else {
            panic!("on");
        };
        assert_eq!(contract.to_bytes().matrix, 0);
        assert!(RefineContract::describes_coded_stream(
            contract.to_bytes(),
            ten,
            sdr,
            bt709,
            ColorMatrix::Bt709,
            ColorRange::Limited
        ));
    }

    #[test]
    fn a_linux_capture_is_a_cpu_source_only_when_it_is_wide() {
        assert_eq!(
            RefineSourceKind::for_linux_capture(BitDepth::Eight),
            RefineSourceKind::GpuOnly
        );
        assert_eq!(
            RefineSourceKind::for_linux_capture(BitDepth::Ten),
            RefineSourceKind::CpuRgb
        );
        let deck = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        // The Wayland HDR composition: PQ / BT.2020, 4:4:4 ten-bit.
        let hdr = CaptureRefineFacts {
            depth: BitDepth::Ten,
            chroma: ChromaSubsampling::Yuv444,
            transfer: TransferCharacteristics::Pq,
            primaries: ColorPrimaries::Bt2020,
            source: RefineSourceKind::for_linux_capture(BitDepth::Ten),
            single_monitor: true,
        };
        assert!(plan_for_capture(true, true, Some(&deck), hdr).is_on());
        let auto = CaptureRefineFacts {
            depth: BitDepth::Eight,
            chroma: ChromaSubsampling::Yuv420,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
            source: RefineSourceKind::for_linux_capture(BitDepth::Eight),
            ..hdr
        };
        assert_eq!(
            plan_for_capture(true, true, Some(&deck), auto),
            RefinePlan::Off(RefineOffReason::EightBit)
        );
    }

    #[test]
    fn the_operator_line_names_each_switch_state() {
        assert!(operator_status_line(true, true).starts_with("Exact still image: enabled;"));
        assert!(operator_status_line(true, false).contains("\"disabled\""));
        assert_eq!(
            operator_status_line(false, true),
            "Exact still image: not available in this host build"
        );
    }

    #[test]
    fn a_host_capture_is_refined_only_with_both_switches_on_one_cpu_monitor() {
        let deck = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        let grading = CaptureRefineFacts {
            depth: BitDepth::Ten,
            chroma: ChromaSubsampling::Yuv444,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
            source: RefineSourceKind::CpuRgb,
            single_monitor: true,
        };
        assert!(plan_for_capture(true, true, Some(&deck), grading).is_on());
        let off = |plan: RefinePlan| match plan {
            RefinePlan::Off(reason) => Some(reason),
            RefinePlan::On { .. } => None,
        };
        assert_eq!(
            off(plan_for_capture(true, false, Some(&deck), grading)),
            Some(RefineOffReason::OperatorOff)
        );
        assert_eq!(
            off(plan_for_capture(true, true, None, grading)),
            Some(RefineOffReason::DeckUnsupported)
        );
        let speed = CaptureRefineFacts {
            depth: BitDepth::Eight,
            chroma: ChromaSubsampling::Yuv420,
            source: RefineSourceKind::GpuOnly,
            ..grading
        };
        assert_eq!(
            off(plan_for_capture(true, true, Some(&deck), speed)),
            Some(RefineOffReason::EightBit)
        );
        let wayland = CaptureRefineFacts {
            source: RefineSourceKind::GpuOnly,
            ..grading
        };
        assert_eq!(
            off(plan_for_capture(true, true, Some(&deck), wayland)),
            Some(RefineOffReason::ZeroCopyCapture)
        );
        let two = CaptureRefineFacts {
            single_monitor: false,
            ..grading
        };
        assert_eq!(
            off(plan_for_capture(true, true, Some(&deck), two)),
            Some(RefineOffReason::MultiMonitor)
        );
    }

    #[test]
    fn the_plan_states_its_truth_and_its_capenc_arguments() {
        let deck = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec!["delta-zstd1".into()],
        };
        let on = resolve_refine_plan(&inputs(Some(&deck)));
        let status = on.hello_status();
        assert!(status.is_on());
        assert_eq!(status.bit_depth, Some(10));
        assert_eq!(status.domain.as_deref(), Some("rgb"));
        let args = on.capenc_args((1, 2, 3));
        assert_eq!(args[0], CAPENC_REFINE_ON);
        let mut with_framing: Vec<String> = vec![crate::capenc_stream::FRAMED_OUTPUT_V2.into()];
        with_framing.extend(args);
        assert_eq!(
            capenc_refine_args(&with_framing),
            Ok(Some(CapencRefineArgs {
                scope: (1, 2, 3),
                still_ms: DEFAULT_STILL_MS,
                max_mbps: DEFAULT_MAX_MBPS
            }))
        );
        let off = RefinePlan::Off(RefineOffReason::OperatorOff);
        assert!(off.capenc_args((1, 1, 1)).is_empty());
        assert_eq!(off.hello_status().reason.as_deref(), Some("operator_off"));
        for reason in RefineOffReason::ALL {
            assert_eq!(RefineOffReason::from_token(reason.token()), Some(reason));
        }
        assert_eq!(off_reason_text("from_the_future"), "off (from_the_future)");
    }
}
