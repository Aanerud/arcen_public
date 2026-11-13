// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The colour check: a checklist a colourist reads before trusting what the
//! Deck shows.
//!
//! It answers four questions, item by item, each with a stable code
//! (documented in `docs/operations/error-codes.md#colour-check`), a verdict, a
//! plain sentence, the facts behind it and — where the Deck can act — a
//! one-click fix:
//!
//! 1. **What is the host serving?** From the host's own truth: the hello's
//!    `active_*` colour description, the served pipeline and its reason, and
//!    the headers of the frames actually decoded, compared with what this Deck
//!    asked for.
//! 2. **What is this Mac doing with it?** The display's reference mode, colour
//!    profile and HDR headroom, the presentation path (ten-bit Metal layer,
//!    EDR only for PQ) and whether one stream pixel lands on one screen pixel.
//! 3. **Is it exact?** A measurement of a known test pattern in the stream's
//!    own coded samples ([`crate::test_pattern::measure_biplanar`]), every
//!    pixel and the worst one, never a share of "flat" pixels.
//! 4. **What does the host need?** The host-side prerequisites for Grading and
//!    HDR on its platform, with the documentation that explains them. The Deck
//!    never changes a host setting.
//!
//! A platform adapter gathers the facts ([`ColourCheckInput`]); [`assess`]
//! owns every rule and every word. Nothing here touches an operating system.

use std::fmt::Write as _;

use arcen_protocol::messages::{
    HostPlatform, ServedPipelineMsg, ServedStreamPipeline, ServerHelloMsg, StreamPipeline,
    VideoSelectionIntent,
};

use crate::test_pattern::{CodeAccuracy, ColorAccuracy, TestPattern};
use crate::video::{ColorTransform, StreamingPreset};
use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
    VideoCodec, VideoConfiguration,
};

/// Version of the plain-text report layout.
pub const REPORT_FORMAT_VERSION: u32 = 1;

/// Where the codes in a report are explained.
pub const ERROR_CODES_DOCUMENT: &str = "docs/operations/error-codes.md#colour-check";

/// Where the published documentation lives; [`DocLink::url`] appends a path.
pub const PUBLIC_DOCS_BASE: &str = "https://github.com/Aanerud/arcen_public/tree/main/";

/// Largest error, in eight-bit codes, still called "near" rather than "off".
/// Lossy HEVC at ten bits lands within it on a still pattern; a viewer that
/// scales, or a pipeline that converts colour, does not.
pub const NEAR_EXACT_CODES: u16 = 2;

/// Most characters of one host-supplied string a report repeats.
pub const MAX_HOST_TEXT_CHARS: usize = 200;

/// A verdict, ordered from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// As it should be for colour-critical work.
    Pass,
    /// A fact worth knowing; nothing is wrong.
    Info,
    /// Usable, with a caveat a colourist must know.
    Warn,
    /// What you see is not what the host has.
    Fail,
}

impl Status {
    /// The mark shown beside an item.
    #[must_use]
    pub const fn symbol(self) -> &'static str {
        match self {
            Self::Pass => "✔",
            Self::Info => "ℹ",
            Self::Warn => "⚠",
            Self::Fail => "✖",
        }
    }

    /// The word used in the text report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
        }
    }
}

/// The four groups of the checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// What the host is serving.
    Host,
    /// What this Mac is doing with it.
    Mac,
    /// Measured exactness.
    Exactness,
    /// What the host needs for Grading and HDR.
    Prerequisites,
}

impl Section {
    /// Every section, in display order.
    pub const ALL: [Self; 4] = [Self::Host, Self::Mac, Self::Exactness, Self::Prerequisites];

    /// Heading.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Host => "What the host is serving",
            Self::Mac => "What this Mac is doing",
            Self::Exactness => "Exactness",
            Self::Prerequisites => "Host prerequisites",
        }
    }
}

/// One finding: a stable code, its verdict, what it means and what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finding {
    /// No session: the host items cannot be checked.
    NotConnected,
    /// The host serves the preset this Deck asked for.
    PresetServed,
    /// The host served a different pipeline from the one requested.
    PresetDowngraded,
    /// Auto or Speed: eight-bit 4:2:0 by design.
    PresetEightBit,
    /// A developer-defined stream rather than a preset.
    PresetCustom,
    /// The host does not report its served pipeline.
    PresetUnreported,
    /// The codec and profile carry the requested chroma and depth.
    CodecProfile,
    /// The host chose a different codec from the one requested.
    CodecChanged,
    /// The host did not name its codec.
    CodecUnknown,
    /// Full 4:4:4 chroma.
    ChromaFull,
    /// Subsampled chroma although 4:4:4 was requested.
    ChromaReduced,
    /// Subsampled chroma by the preset's design.
    ChromaSubsampled,
    /// The host did not describe its chroma.
    ChromaUnknown,
    /// Ten bits or more per sample.
    DepthTen,
    /// Eight bits although ten were requested.
    DepthReduced,
    /// Eight bits by the preset's design.
    DepthEight,
    /// The host did not describe its depth.
    DepthUnknown,
    /// SDR, as requested.
    SignalSdr,
    /// HDR (PQ, BT.2020), as requested.
    SignalHdr,
    /// Transfer or primaries differ from the request.
    SignalChanged,
    /// The host did not describe transfer or primaries.
    SignalUnknown,
    /// Full range and the requested matrix.
    CodingFull,
    /// Limited range by the preset's design.
    CodingLimited,
    /// Range or matrix differs from the request.
    CodingChanged,
    /// The host did not describe range or matrix.
    CodingUnknown,
    /// Decoded frame headers agree with the host's description.
    FramesAgree,
    /// Decoded frame headers disagree with the host's description.
    FramesDisagree,
    /// No frame decoded yet.
    FramesNone,
    /// The capture source the served pipeline implies.
    CaptureImplied,
    /// Hardware encoder.
    EncoderHardware,
    /// Software encoder.
    EncoderSoftware,
    /// Encoder class unknown.
    EncoderUnknown,
    /// macOS reports a reference mode on this display.
    DisplayReference,
    /// macOS reports no reference mode on this display.
    DisplayNotReference,
    /// The display's colour state could not be read.
    DisplayUnknown,
    /// The display's colour profile and what macOS converts.
    DisplayProfile,
    /// HDR stream on a display with HDR headroom.
    HdrReady,
    /// HDR stream on a display without HDR headroom.
    HdrNoHeadroom,
    /// SDR stream: headroom unused.
    HdrNotUsed,
    /// Ten-bit stream on the ten-bit Metal layer.
    PresentTenBit,
    /// Ten-bit stream through the eight-bit fallback.
    PresentFallback,
    /// Eight-bit stream on the SDR path.
    PresentEightBit,
    /// Nothing presented yet.
    PresentNone,
    /// EDR on exactly when the stream is PQ.
    EdrCorrect,
    /// EDR state contradicts the stream's transfer.
    EdrMismatch,
    /// EDR state unknown.
    EdrUnknown,
    /// One stream pixel per screen pixel.
    ScaleOneToOne,
    /// Enlarged by a whole number.
    ScaleInteger,
    /// Resampled to fit.
    ScaleResampled,
    /// Scale unknown.
    ScaleUnknown,
    /// Not measured yet.
    ExactNotRun,
    /// Waiting for a frame to measure.
    ExactWaiting,
    /// Every pixel exact.
    ExactExact,
    /// Within [`NEAR_EXACT_CODES`].
    ExactNear,
    /// Beyond [`NEAR_EXACT_CODES`].
    ExactOff,
    /// Off as desktop codes, near once sRGB is re-encoded as BT.709.
    ExactReencoded,
    /// Near everywhere except one small area drawn over the pattern.
    ExactLocalised,
    /// PQ: the desktop's codes are remapped by design.
    ExactNotApplicable,
    /// The frame could not be read back.
    ExactUnavailable,
    /// Lossless refinement is not in this release.
    ExactRefinement,
    /// Nothing on the host stands in the way.
    HostReady,
    /// What Grading and HDR need on this host's platform.
    HostGradingNeeds,
    /// A depth-24 Xorg desktop cannot serve ten-bit.
    HostLinuxDepth,
    /// Xorg cannot report HDR.
    HostXorgHdr,
    /// This Wayland desktop cannot serve Grading or HDR.
    HostWaylandFidelity,
    /// Windows HDR needs a proven HDR display.
    HostWindowsHdr,
    /// macOS Pier preview limits.
    HostMacPier,
    /// The host gave a reason this check has no advice for.
    HostReason,
}

impl Finding {
    /// Every finding, in checklist order.
    pub const ALL: &'static [Self] = &[
        Self::NotConnected,
        Self::PresetServed,
        Self::PresetDowngraded,
        Self::PresetEightBit,
        Self::PresetCustom,
        Self::PresetUnreported,
        Self::CodecProfile,
        Self::CodecChanged,
        Self::CodecUnknown,
        Self::ChromaFull,
        Self::ChromaReduced,
        Self::ChromaSubsampled,
        Self::ChromaUnknown,
        Self::DepthTen,
        Self::DepthReduced,
        Self::DepthEight,
        Self::DepthUnknown,
        Self::SignalSdr,
        Self::SignalHdr,
        Self::SignalChanged,
        Self::SignalUnknown,
        Self::CodingFull,
        Self::CodingLimited,
        Self::CodingChanged,
        Self::CodingUnknown,
        Self::FramesAgree,
        Self::FramesDisagree,
        Self::FramesNone,
        Self::CaptureImplied,
        Self::EncoderHardware,
        Self::EncoderSoftware,
        Self::EncoderUnknown,
        Self::DisplayReference,
        Self::DisplayNotReference,
        Self::DisplayUnknown,
        Self::DisplayProfile,
        Self::HdrReady,
        Self::HdrNoHeadroom,
        Self::HdrNotUsed,
        Self::PresentTenBit,
        Self::PresentFallback,
        Self::PresentEightBit,
        Self::PresentNone,
        Self::EdrCorrect,
        Self::EdrMismatch,
        Self::EdrUnknown,
        Self::ScaleOneToOne,
        Self::ScaleInteger,
        Self::ScaleResampled,
        Self::ScaleUnknown,
        Self::ExactNotRun,
        Self::ExactWaiting,
        Self::ExactExact,
        Self::ExactNear,
        Self::ExactOff,
        Self::ExactReencoded,
        Self::ExactLocalised,
        Self::ExactNotApplicable,
        Self::ExactUnavailable,
        Self::ExactRefinement,
        Self::HostReady,
        Self::HostGradingNeeds,
        Self::HostLinuxDepth,
        Self::HostXorgHdr,
        Self::HostWaylandFidelity,
        Self::HostWindowsHdr,
        Self::HostMacPier,
        Self::HostReason,
    ];

    /// The stable code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotConnected => "colour.host.not_connected",
            Self::PresetServed => "colour.preset.served",
            Self::PresetDowngraded => "colour.preset.downgraded",
            Self::PresetEightBit => "colour.preset.eight_bit",
            Self::PresetCustom => "colour.preset.custom",
            Self::PresetUnreported => "colour.preset.unreported",
            Self::CodecProfile => "colour.codec.profile",
            Self::CodecChanged => "colour.codec.changed",
            Self::CodecUnknown => "colour.codec.unknown",
            Self::ChromaFull => "colour.chroma.full",
            Self::ChromaReduced => "colour.chroma.reduced",
            Self::ChromaSubsampled => "colour.chroma.subsampled",
            Self::ChromaUnknown => "colour.chroma.unknown",
            Self::DepthTen => "colour.depth.ten",
            Self::DepthReduced => "colour.depth.reduced",
            Self::DepthEight => "colour.depth.eight",
            Self::DepthUnknown => "colour.depth.unknown",
            Self::SignalSdr => "colour.signal.sdr",
            Self::SignalHdr => "colour.signal.hdr",
            Self::SignalChanged => "colour.signal.changed",
            Self::SignalUnknown => "colour.signal.unknown",
            Self::CodingFull => "colour.coding.full",
            Self::CodingLimited => "colour.coding.limited",
            Self::CodingChanged => "colour.coding.changed",
            Self::CodingUnknown => "colour.coding.unknown",
            Self::FramesAgree => "colour.frames.agree",
            Self::FramesDisagree => "colour.frames.disagree",
            Self::FramesNone => "colour.frames.none",
            Self::CaptureImplied => "colour.capture.implied",
            Self::EncoderHardware => "colour.encoder.hardware",
            Self::EncoderSoftware => "colour.encoder.software",
            Self::EncoderUnknown => "colour.encoder.unknown",
            Self::DisplayReference => "colour.display.reference",
            Self::DisplayNotReference => "colour.display.not_reference",
            Self::DisplayUnknown => "colour.display.unknown",
            Self::DisplayProfile => "colour.display.profile",
            Self::HdrReady => "colour.display.hdr_ready",
            Self::HdrNoHeadroom => "colour.display.no_headroom",
            Self::HdrNotUsed => "colour.display.sdr",
            Self::PresentTenBit => "colour.present.ten_bit",
            Self::PresentFallback => "colour.present.fallback",
            Self::PresentEightBit => "colour.present.eight_bit",
            Self::PresentNone => "colour.present.none",
            Self::EdrCorrect => "colour.edr.correct",
            Self::EdrMismatch => "colour.edr.mismatch",
            Self::EdrUnknown => "colour.edr.unknown",
            Self::ScaleOneToOne => "colour.scale.one_to_one",
            Self::ScaleInteger => "colour.scale.integer",
            Self::ScaleResampled => "colour.scale.resampled",
            Self::ScaleUnknown => "colour.scale.unknown",
            Self::ExactNotRun => "colour.exact.not_run",
            Self::ExactWaiting => "colour.exact.waiting",
            Self::ExactExact => "colour.exact.exact",
            Self::ExactNear => "colour.exact.near",
            Self::ExactOff => "colour.exact.off",
            Self::ExactReencoded => "colour.exact.bt709_reencoded",
            Self::ExactLocalised => "colour.exact.localised",
            Self::ExactNotApplicable => "colour.exact.hdr",
            Self::ExactUnavailable => "colour.exact.unavailable",
            Self::ExactRefinement => "colour.exact.refinement",
            Self::HostReady => "colour.host.ready",
            Self::HostGradingNeeds => "colour.host.grading_needs",
            Self::HostLinuxDepth => "colour.host.linux_depth",
            Self::HostXorgHdr => "colour.host.xorg_hdr",
            Self::HostWaylandFidelity => "colour.host.wayland",
            Self::HostWindowsHdr => "colour.host.windows_hdr",
            Self::HostMacPier => "colour.host.macos_pier",
            Self::HostReason => "colour.host.reason",
        }
    }

    /// The verdict.
    #[must_use]
    pub const fn status(self) -> Status {
        match self {
            Self::PresetServed
            | Self::CodecProfile
            | Self::ChromaFull
            | Self::DepthTen
            | Self::SignalSdr
            | Self::SignalHdr
            | Self::CodingFull
            | Self::FramesAgree
            | Self::EncoderHardware
            | Self::DisplayReference
            | Self::HdrReady
            | Self::PresentTenBit
            | Self::PresentEightBit
            | Self::EdrCorrect
            | Self::ScaleOneToOne
            | Self::ExactExact
            | Self::HostReady => Status::Pass,
            Self::NotConnected
            | Self::PresetUnreported
            | Self::CodecUnknown
            | Self::ChromaUnknown
            | Self::DepthUnknown
            | Self::SignalUnknown
            | Self::CodingUnknown
            | Self::FramesNone
            | Self::CaptureImplied
            | Self::EncoderUnknown
            | Self::DisplayUnknown
            | Self::DisplayProfile
            | Self::HdrNotUsed
            | Self::PresentNone
            | Self::EdrUnknown
            | Self::ScaleUnknown
            | Self::ExactNotRun
            | Self::ExactWaiting
            | Self::ExactNotApplicable
            | Self::ExactUnavailable
            | Self::ExactRefinement
            | Self::HostGradingNeeds
            | Self::HostXorgHdr
            | Self::HostMacPier => Status::Info,
            Self::PresetEightBit
            | Self::PresetCustom
            | Self::CodecChanged
            | Self::ChromaSubsampled
            | Self::DepthEight
            | Self::CodingLimited
            | Self::EncoderSoftware
            | Self::DisplayNotReference
            | Self::ScaleInteger
            | Self::ExactNear
            | Self::ExactReencoded
            | Self::ExactLocalised
            | Self::HostLinuxDepth
            | Self::HostWaylandFidelity
            | Self::HostWindowsHdr
            | Self::HostReason => Status::Warn,
            Self::PresetDowngraded
            | Self::ChromaReduced
            | Self::DepthReduced
            | Self::SignalChanged
            | Self::CodingChanged
            | Self::FramesDisagree
            | Self::HdrNoHeadroom
            | Self::PresentFallback
            | Self::EdrMismatch
            | Self::ScaleResampled
            | Self::ExactOff => Status::Fail,
        }
    }

    /// What the finding means, in one plain sentence.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub const fn meaning(self) -> &'static str {
        match self {
            Self::NotConnected => {
                "Not connected: what a host serves can only be checked in a session."
            }
            Self::PresetServed => "The host serves the preset this Deck asked for.",
            Self::PresetDowngraded => {
                "The host served a different pipeline from the one this Deck asked for."
            }
            Self::PresetEightBit => {
                "Auto and Speed are eight-bit 4:2:0 streams: right for everyday work, not for \
                 judging colour."
            }
            Self::PresetCustom => {
                "A developer-defined stream is active instead of one of the four presets."
            }
            Self::PresetUnreported => {
                "This host does not report which pipeline it served; the items below rely on \
                 its colour description alone."
            }
            Self::CodecProfile => "The codec and profile carry the chroma and depth asked for.",
            Self::CodecChanged => "The host chose a different codec from the one asked for.",
            Self::CodecUnknown => "The host did not name its codec.",
            Self::ChromaFull => "Full 4:4:4 chroma: every pixel carries its own colour.",
            Self::ChromaReduced => {
                "The host subsampled chroma although 4:4:4 was asked for: fine colour detail \
                 and thin coloured lines are averaged."
            }
            Self::ChromaSubsampled => {
                "4:2:0 chroma: colour is stored once per 2×2 pixels, so coloured text and thin \
                 lines fringe."
            }
            Self::ChromaUnknown => "The host did not describe its chroma sampling.",
            Self::DepthTen => "Ten bits per sample: the stream adds no banding.",
            Self::DepthReduced => "The host served eight bits although ten were asked for.",
            Self::DepthEight => "Eight bits per sample: smooth gradients can band.",
            Self::DepthUnknown => "The host did not describe its bit depth.",
            Self::SignalSdr => {
                "SDR with the transfer and primaries asked for (Grading is SDR by design)."
            }
            Self::SignalHdr => "HDR as asked for: PQ transfer with BT.2020 primaries.",
            Self::SignalChanged => {
                "The host's transfer or primaries differ from the request (for example HDR \
                 served as SDR)."
            }
            Self::SignalUnknown => "The host did not describe its transfer or primaries.",
            Self::CodingFull => {
                "Full-range coding with the matrix asked for: every code value is used."
            }
            Self::CodingLimited => {
                "Limited (video) range: on the eight-bit scale, codes below 16 and above 235 \
                 cannot be told apart."
            }
            Self::CodingChanged => "The host's range or matrix differs from the request.",
            Self::CodingUnknown => "The host did not describe its range or matrix.",
            Self::FramesAgree => "The decoded frames' own headers match what the host announced.",
            Self::FramesDisagree => {
                "Decoded frames describe a different picture from the host's announcement."
            }
            Self::FramesNone => "No frame has been decoded yet.",
            Self::CaptureImplied => {
                "The host does not tell the Deck its capture source; this is the source its \
                 served pipeline uses on this platform."
            }
            Self::EncoderHardware => "Hardware encoder.",
            Self::EncoderSoftware => {
                "Software encoder: colour follows the contract, but frame rate is limited."
            }
            Self::EncoderUnknown => "The host did not say whether its encoder is hardware.",
            Self::DisplayReference => {
                "macOS reports a reference mode on this display: it renders without tone \
                 mapping up to its reference level."
            }
            Self::DisplayNotReference => {
                "macOS reports no reference mode on this display: brightness, True Tone and \
                 ambient adaptation change what you see."
            }
            Self::DisplayUnknown => "The Deck could not read this display's colour state.",
            Self::DisplayProfile => {
                "macOS converts the Deck's tagged picture into this display's profile: \
                 colours match by appearance; the codes sent to the panel equal the stream's \
                 only when the profile is the tag."
            }
            Self::HdrReady => "This display has HDR headroom for the PQ stream.",
            Self::HdrNoHeadroom => {
                "The stream is HDR but this display shows no HDR headroom: highlights clip at \
                 SDR white."
            }
            Self::HdrNotUsed => "SDR stream: the display's HDR headroom is not used.",
            Self::PresentTenBit => {
                "Ten-bit stream on the dedicated ten-bit Metal layer: no eight-bit step on this \
                 Mac."
            }
            Self::PresentFallback => "A ten-bit stream is shown through the eight-bit fallback.",
            Self::PresentEightBit => {
                "Eight-bit stream on the ordinary SDR path, as its contract says."
            }
            Self::PresentNone => "Nothing is being presented yet.",
            Self::EdrCorrect => "EDR follows the transfer: on only for PQ.",
            Self::EdrMismatch => "EDR does not match the stream's transfer.",
            Self::EdrUnknown => "The presentation layer's EDR state is not known yet.",
            Self::ScaleOneToOne => "One stream pixel lands on one screen pixel.",
            Self::ScaleInteger => {
                "The picture is enlarged by a whole number: each stream pixel covers several \
                 screen pixels."
            }
            Self::ScaleResampled => {
                "The picture is resized to fit: resampling blends neighbouring pixels, so the \
                 values on screen are not the stream's."
            }
            Self::ScaleUnknown => "The picture's scale is not known yet.",
            Self::ExactNotRun => "Exactness has not been measured in this session.",
            Self::ExactWaiting => "Waiting for the next decoded frame to measure.",
            Self::ExactExact => "Every pixel of the pattern arrived with its exact code values.",
            Self::ExactNear => {
                "Within two codes of the pattern: the lossy codec's quantisation. Close, not \
                 bit-exact."
            }
            Self::ExactOff => "Code values differ from the pattern by more than two codes.",
            Self::ExactLocalised => {
                "Within two codes everywhere except a few pixels or one small area: something \
                 on the host is drawn over the pattern there (a notification, a watermark, a \
                 pointer)."
            }
            Self::ExactReencoded => {
                "The host re-encoded the desktop's sRGB values with the BT.709 transfer: the \
                 code values differ from the desktop's (up to 16/255 in the shadows), and \
                 shown as SDR they make dark tones darker than on the host."
            }
            Self::ExactNotApplicable => {
                "HDR maps the SDR desktop to PQ at 203-nit white, so the desktop's code values \
                 change by design: there is nothing to compare code for code."
            }
            Self::ExactUnavailable => "This frame could not be read back for measurement.",
            Self::ExactRefinement => {
                "Bit-exact still frames need lossless refinement (ADR 0016), which is proposed \
                 and not in this release: a lossy stream's exactness is measured here, not \
                 assumed."
            }
            Self::HostReady => "Nothing on the host stands between it and the preset asked for.",
            Self::HostGradingNeeds => "What Grading and HDR need on this host.",
            Self::HostLinuxDepth => {
                "This Xorg desktop runs at depth 24, so it cannot serve ten-bit colour."
            }
            Self::HostXorgHdr => {
                "Xorg cannot report HDR: HDR is served as Grading unless the host's \
                 administrator declares a Rec.2100 PQ desktop."
            }
            Self::HostWaylandFidelity => {
                "This Wayland desktop cannot serve Grading or HDR: GNOME's screen sharing \
                 carries ten-bit and HDR only from Mutter 50."
            }
            Self::HostWindowsHdr => {
                "Windows serves HDR only when the display Arcen captures is proven to be in \
                 HDR mode."
            }
            Self::HostMacPier => {
                "The macOS Pier is a preview: HDR needs its virtual display to report headroom \
                 above SDR white."
            }
            Self::HostReason => "The host gave a reason for serving less than asked for.",
        }
    }

    /// What to do. Empty when there is nothing to do.
    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::NotConnected => {
                "Connect to a host, then open Colour check from the Session menu."
            }
            Self::PresetDowngraded => {
                "Read the host's reason and its prerequisite below. Fix that on the host, or \
                 choose a preset this host can serve; the Deck never changes host settings."
            }
            Self::PresetEightBit
            | Self::ChromaSubsampled
            | Self::DepthEight
            | Self::CodingLimited => {
                "Switch to Grading (ten-bit 4:4:4, full range, BT.709) — or HDR on an HDR \
                 display — for colour-critical review."
            }
            Self::PresetCustom => {
                "Choose Grading or HDR to return to a supported, documented contract."
            }
            Self::PresetUnreported => "Update the Pier to the same Arcen release as this Deck.",
            Self::CodecChanged => {
                "Nothing, if chroma and depth pass; otherwise read the host's reason."
            }
            Self::ChromaReduced
            | Self::DepthReduced
            | Self::SignalChanged
            | Self::CodingChanged => {
                "See the host's reason under Preset and its prerequisite below."
            }
            Self::FramesDisagree => {
                "Copy this report and send it with the Pier's log: this is a defect."
            }
            Self::EncoderSoftware => {
                "Check the host's GPU and its free encoder sessions; see the Pier's log."
            }
            Self::DisplayNotReference => {
                "For judging colour, use a calibrated display or a reference preset (System \
                 Settings › Displays › Preset, for example HDTV Video BT.709-BT.1886 for \
                 Grading), and turn off True Tone and automatic brightness."
            }
            Self::HdrNoHeadroom => {
                "Use an HDR display (Liquid Retina XDR, Pro Display XDR, or an external display \
                 with HDR turned on in System Settings › Displays), or switch to Grading."
            }
            Self::PresentFallback => {
                "Reconnect to rebuild the layer. If it persists, copy this report: the reason is \
                 below."
            }
            Self::EdrMismatch => "Copy this report and reconnect: this is a defect.",
            Self::ScaleInteger => {
                "Stream at Retina resolution (HiDPI) for this display so one stream pixel is one \
                 screen pixel."
            }
            Self::ScaleResampled => {
                "Enter fullscreen or make the window large enough for 1:1; when already \
                 fullscreen, reconnect so the host can match this screen's size."
            }
            Self::ExactNotRun => {
                "Show the Arcen test pattern full-screen at one image pixel per screen pixel on \
                 the host (Save pattern writes a PNG at this stream's exact size), then press \
                 Measure."
            }
            Self::ExactNear => {
                "Expected for a lossy stream. For bit-exact still frames, see lossless \
                 refinement below."
            }
            Self::ExactOff => {
                "Check that the pattern fills the host's screen at 1:1 with no viewer scaling, \
                 border or colour management, that it is the pattern selected here, and that \
                 the preset is Grading."
            }
            Self::ExactNotApplicable => "Measure code values in Grading.",
            Self::ExactLocalised => {
                "Close or move what covers the area named below, then measure again; the rest \
                 of the picture is already measured."
            }
            Self::ExactReencoded => {
                "Expected from a Windows Pier before 0.18.0, which captures linear light and \
                 re-encodes it as BT.709, and from a Linux Pier before 0.19.0 serving a \
                 declared Rec.2100 PQ desktop or a Wayland HDR composition as Grading. \
                 Current Piers serve this Deck the desktop's own codes (sRGB transfer)."
            }
            Self::ExactUnavailable => "Measure again; the detail line says why.",
            Self::HostLinuxDepth => {
                "An Xorg desktop keeps the depth it started with: sign out of it (that closes \
                 its applications) and reconnect with Grading, which starts a depth-30 desktop \
                 under x11_desktop_depth auto — or ask the host's administrator to set \
                 x11_desktop_depth to 30. The Deck never changes it."
            }
            Self::HostGradingNeeds
            | Self::HostXorgHdr
            | Self::HostWaylandFidelity
            | Self::HostWindowsHdr
            | Self::HostMacPier
            | Self::HostReason => {
                "This is a host setting: ask the host's administrator, using the linked \
                 documentation. The Deck never changes it."
            }
            _ => "",
        }
    }
}

/// A page of Arcen's documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocLink {
    /// Link text.
    pub title: &'static str,
    /// Repository path, with an optional `#anchor`.
    pub path: &'static str,
}

impl DocLink {
    /// The published address.
    #[must_use]
    pub fn url(self) -> String {
        format!("{PUBLIC_DOCS_BASE}{}", self.path)
    }
}

/// The colour check guide.
pub const DOC_COLOUR_CHECK: DocLink = DocLink {
    title: "Colour check guide",
    path: "docs/operations/colour-check.md",
};
/// Streaming presets and their pipelines.
pub const DOC_PRESETS: DocLink = DocLink {
    title: "Streaming presets and pipelines",
    path: "docs/features.md#streaming-presets-and-independent-pipelines",
};
/// Linux depth-30 desktops.
pub const DOC_LINUX_DEPTH: DocLink = DocLink {
    title: "Linux desktop depth (depth 30 for Grading)",
    path: "docs/operations/pier-administration.md#linux-desktop-depth",
};
/// Declaring a Rec.2100 PQ Xorg desktop.
pub const DOC_XORG_HDR: DocLink = DocLink {
    title: "Linux HDR gate (video.desktop_encoding)",
    path: "docs/architecture/color-fidelity.md#linux-depth-30-capture-and-the-hdr-gate",
};
/// Wayland Grading and HDR (Mutter 50).
pub const DOC_WAYLAND: DocLink = DocLink {
    title: "Wayland Grading and HDR (Mutter 50 or later)",
    path: "docs/architecture/linux-wayland-provider.md#wayland-grading-and-hdr",
};
/// Windows ten-bit and HDR capture.
pub const DOC_WINDOWS_HDR: DocLink = DocLink {
    title: "Windows ten-bit and HDR capture",
    path: "docs/architecture/color-fidelity.md#windows-fp16-scrgb-to-ten-bit-sdr-and-hdr10",
};
/// Lossless refinement.
pub const DOC_LOSSLESS: DocLink = DocLink {
    title: "ADR 0016: lossless refinement",
    path: "docs/adr/0016-lossless-refinement.md",
};

/// Something the user can do from an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColourFix {
    /// Choose this preset on the Deck and reconnect. The desktop and its
    /// applications stay; the host chooses its pipeline per connection.
    SwitchPreset(StreamingPreset),
    /// Enter fullscreen so a pinned stream is shown at 1:1.
    EnterFullscreen,
    /// Request Retina (backing-pixel) resolution for this display and
    /// reconnect.
    StreamAtRetinaResolution,
    /// Measure the next decoded frame against the selected pattern.
    Measure,
    /// Save the selected pattern as a PNG at the stream's size.
    SavePattern,
    /// Open a documentation page.
    ReadDoc(DocLink),
}

impl ColourFix {
    /// Button text.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::SwitchPreset(preset) => {
                format!("Switch to {} and reconnect", preset_title(preset))
            }
            Self::EnterFullscreen => "Enter fullscreen".to_owned(),
            Self::StreamAtRetinaResolution => "Stream at Retina resolution".to_owned(),
            Self::Measure => "Measure".to_owned(),
            Self::SavePattern => "Save pattern…".to_owned(),
            Self::ReadDoc(link) => format!("Read: {}", link.title),
        }
    }

    /// Whether applying this changes the connection (the Deck reconnects).
    #[must_use]
    pub const fn reconnects(self) -> bool {
        matches!(self, Self::SwitchPreset(_) | Self::StreamAtRetinaResolution)
    }

    /// Whether this changes something on this Mac (rather than measuring or
    /// reading). No fix ever changes a host setting.
    #[must_use]
    pub const fn changes_deck(self) -> bool {
        matches!(
            self,
            Self::SwitchPreset(_) | Self::EnterFullscreen | Self::StreamAtRetinaResolution
        )
    }
}

const fn preset_title(preset: StreamingPreset) -> &'static str {
    match preset {
        StreamingPreset::Auto => "Auto",
        StreamingPreset::Speed => "Speed",
        StreamingPreset::Grading => "Grading",
        StreamingPreset::Hdr => "HDR",
    }
}

/// Best-effort chroma read from `ServerColorCaps::advertised_pix_fmt`.
///
/// Chroma has no dedicated `active_*` field, and the hosts do not agree on
/// the string's shape: the Linux Pier sends `yuv444p`/`yuv420p`, the Windows
/// Pier `yuv444`/`yuv420`, and real fourCCs (`p010le`, `nv12`, `gbrp`) carry
/// no digits at all. Anything else is unknown, never guessed.
#[must_use]
pub fn chroma_from_pix_fmt(pix_fmt: &str) -> Option<ChromaSubsampling> {
    let lowered = pix_fmt.to_ascii_lowercase();
    if lowered.contains("444") || lowered.starts_with("gbrp") {
        Some(ChromaSubsampling::Yuv444)
    } else if lowered.contains("422") {
        Some(ChromaSubsampling::Yuv422)
    } else if lowered.contains("420") || lowered.starts_with("p010") || lowered.starts_with("nv12")
    {
        Some(ChromaSubsampling::Yuv420)
    } else {
        None
    }
}

/// Every colour axis of a stream, each unknown (`None`) unless stated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColourAxes {
    /// Codec.
    pub codec: Option<VideoCodec>,
    /// Chroma sampling.
    pub chroma: Option<ChromaSubsampling>,
    /// Bits per sample.
    pub bit_depth: Option<BitDepth>,
    /// Range.
    pub range: Option<ColorRange>,
    /// Matrix coefficients.
    pub matrix: Option<ColorMatrix>,
    /// Colour primaries.
    pub primaries: Option<ColorPrimaries>,
    /// Transfer characteristics.
    pub transfer: Option<TransferCharacteristics>,
}

impl ColourAxes {
    /// What the host says it actually serves: `codec` and the `active_*`
    /// fields of its colour capabilities. A token this build does not know is
    /// unknown, never guessed.
    #[must_use]
    pub fn from_hello(hello: &ServerHelloMsg) -> Self {
        let caps = &hello.color_caps;
        Self {
            codec: VideoCodec::from_token(&hello.codec),
            chroma: chroma_from_pix_fmt(&caps.advertised_pix_fmt),
            bit_depth: BitDepth::from_token(&caps.active_bit_depth),
            range: ColorRange::from_token(&caps.active_range),
            matrix: ColorMatrix::from_token(&caps.active_matrix),
            primaries: ColorPrimaries::from_token(&caps.active_primaries),
            transfer: TransferCharacteristics::from_token(&caps.active_transfer),
        }
    }

    /// Every axis of a complete configuration.
    #[must_use]
    pub const fn from_configuration(video: VideoConfiguration) -> Self {
        Self {
            codec: Some(video.codec),
            chroma: Some(video.chroma),
            bit_depth: Some(video.bit_depth),
            range: Some(video.range),
            matrix: Some(video.matrix),
            primaries: Some(video.primaries),
            transfer: Some(video.transfer),
        }
    }

    /// The coding transform, when matrix, range and depth are all known.
    #[must_use]
    pub fn color_transform(&self) -> Option<ColorTransform> {
        Some(ColorTransform::new(
            self.matrix?,
            self.range?,
            self.bit_depth?,
        ))
    }
}

/// What a session asked for, as the preset it chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestedPreset {
    /// One of the four product presets.
    Preset(StreamingPreset),
    /// A developer-defined combination of axes.
    Custom,
}

impl RequestedPreset {
    /// The preset a wire pipeline names.
    #[must_use]
    pub const fn from_pipeline(pipeline: Option<&StreamPipeline>) -> Self {
        match pipeline {
            Some(StreamPipeline::Auto) => Self::Preset(StreamingPreset::Auto),
            Some(StreamPipeline::Speed) => Self::Preset(StreamingPreset::Speed),
            Some(StreamPipeline::Grading) => Self::Preset(StreamingPreset::Grading),
            Some(StreamPipeline::Hdr) => Self::Preset(StreamingPreset::Hdr),
            Some(StreamPipeline::Unknown(_)) | None => Self::Custom,
        }
    }

    const fn token(self) -> &'static str {
        match self {
            Self::Preset(StreamingPreset::Auto) => "auto",
            Self::Preset(StreamingPreset::Speed) => "speed",
            Self::Preset(StreamingPreset::Grading) => "grading",
            Self::Preset(StreamingPreset::Hdr) => "hdr",
            Self::Custom => "custom",
        }
    }

    const fn is_colour_critical(self) -> bool {
        matches!(
            self,
            Self::Preset(StreamingPreset::Grading | StreamingPreset::Hdr)
        )
    }
}

/// What the host told this Deck about the stream it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// The host's operating system.
    pub platform: HostPlatform,
    /// `x11`, `wayland`, `aqua`, or empty.
    pub session_type: String,
    /// The Pier's name, as it states it.
    pub server_name: String,
    /// The Pier's version, as it states it.
    pub version: String,
    /// What this Deck asked for.
    pub preset: RequestedPreset,
    /// The colour axes this Deck asked for.
    pub requested: VideoConfiguration,
    /// The codec was the host's to rank (Auto/Speed).
    pub adaptive_codec: bool,
    /// The pipeline the host says it served; `None` from older hosts.
    pub served: Option<ServedStreamPipeline>,
    /// The host's stable reason for a downgrade.
    pub served_reason: Option<String>,
    /// The host's sentence about a downgrade.
    pub served_detail: Option<String>,
    /// What the host says it actually serves.
    pub active: ColourAxes,
    /// The encoder backend's name.
    pub encoder_backend: String,
    /// Hardware (`true`) or software (`false`) encoding, when known.
    pub encoder_hardware: Option<bool>,
}

impl HostFacts {
    /// Reads the host's truth from its hello.
    #[must_use]
    pub fn from_hello(
        hello: &ServerHelloMsg,
        requested_pipeline: Option<&StreamPipeline>,
        requested: VideoConfiguration,
        selection: VideoSelectionIntent,
        encoder_hardware: Option<bool>,
    ) -> Self {
        Self {
            platform: hello.host_platform(),
            session_type: hello.session_type.to_ascii_lowercase(),
            server_name: hello.server_name.clone(),
            version: hello.version.clone(),
            preset: RequestedPreset::from_pipeline(requested_pipeline),
            requested,
            adaptive_codec: selection == VideoSelectionIntent::AdaptivePerformance,
            served: hello.active_pipeline.clone(),
            served_reason: None,
            served_detail: None,
            active: ColourAxes::from_hello(hello),
            encoder_backend: hello.encoder_backend.clone(),
            encoder_hardware,
        }
    }

    /// Applies a later `served_pipeline` message, which supersedes the
    /// hello's provisional answer.
    pub fn apply_served(&mut self, served: &ServedPipelineMsg) {
        self.served = Some(served.served.clone());
        self.served_reason.clone_from(&served.reason);
        self.served_detail.clone_from(&served.detail);
        self.encoder_hardware = match served.backend.as_str() {
            "hardware" => Some(true),
            "software" => Some(false),
            _ => None,
        };
    }

    fn served_preset(&self) -> Option<Result<StreamingPreset, &str>> {
        self.served.as_ref().map(|served| match served {
            ServedStreamPipeline::Auto => Ok(StreamingPreset::Auto),
            ServedStreamPipeline::Speed => Ok(StreamingPreset::Speed),
            ServedStreamPipeline::Grading => Ok(StreamingPreset::Grading),
            ServedStreamPipeline::Hdr => Ok(StreamingPreset::Hdr),
            other => Err(other.token()),
        })
    }

    fn is_pq(&self) -> bool {
        self.active.transfer == Some(TransferCharacteristics::Pq)
    }

    fn is_x11(&self) -> bool {
        self.session_type == "x11"
    }

    fn is_wayland(&self) -> bool {
        self.session_type == "wayland"
    }
}

/// What the decoder saw in the frames themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameFacts {
    /// The frame header's codec, chroma, depth, range and matrix, with the
    /// session's primaries and transfer.
    pub video: VideoConfiguration,
    /// Decoded width.
    pub width: usize,
    /// Decoded height.
    pub height: usize,
    /// The decoder's pixel format, for example `xf44`.
    pub pixel_format: String,
}

/// The Mac display showing the session, as macOS states it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DisplayFacts {
    /// The display's name.
    pub name: Option<String>,
    /// The colour profile's name.
    pub profile: Option<String>,
    /// The display can show Display P3.
    pub wide_gamut: Option<bool>,
    /// EDR headroom available now, as a multiple of SDR white.
    pub edr_current: Option<f32>,
    /// EDR headroom the panel can reach.
    pub edr_potential: Option<f32>,
    /// Reference-rendering headroom; zero when no reference mode is active.
    pub edr_reference: Option<f32>,
}

/// How the Deck presents the picture.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PresentationPath {
    /// Nothing presented yet.
    #[default]
    None,
    /// The ordinary eight-bit path.
    EightBit,
    /// The dedicated ten-bit Metal layer.
    TenBitLayer,
    /// A ten-bit stream on the eight-bit fallback, and why.
    EightBitFallback(String),
}

/// The presentation layer's state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PresentationFacts {
    /// Which path.
    pub path: PresentationPath,
    /// Whether the layer asks for extended dynamic range.
    pub edr: Option<bool>,
    /// The colour space the layer is tagged with.
    pub colour_space: Option<String>,
}

/// How the stream is scaled onto the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaleFacts {
    /// Stream size in pixels.
    pub stream: [u32; 2],
    /// Size on screen in backing (physical) pixels.
    pub shown: [f32; 2],
    /// The display's backing scale (2.0 on Retina).
    pub backing_scale: f32,
    /// The Deck asks the host for backing-pixel resolution.
    pub hidpi_streaming: bool,
    /// The stream size is pinned to a display rather than following the
    /// window.
    pub pinned: bool,
    /// The Deck is fullscreen.
    pub fullscreen: bool,
}

/// Everything the Deck knows about this Mac's side.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeckFacts {
    /// The display showing the session.
    pub display: Option<DisplayFacts>,
    /// The presentation layer.
    pub presentation: PresentationFacts,
    /// The stream's scale on screen.
    pub scale: Option<ScaleFacts>,
    /// Some display of this Mac can show HDR, so HDR may be offered.
    pub hdr_display: bool,
}

/// One exactness measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExactnessMeasurement {
    /// The pattern compared against.
    pub pattern: TestPattern,
    /// Frame width.
    pub width: usize,
    /// Frame height.
    pub height: usize,
    /// Back in eight-bit RGB.
    pub rgb: ColorAccuracy,
    /// In the stream's own coded samples (4:4:4 only).
    pub codes: Option<CodeAccuracy>,
    /// Back in eight-bit RGB, against the pattern re-encoded from sRGB with
    /// the BT.709 transfer ([`crate::test_pattern::PatternSignal`]), when
    /// measured.
    pub reencoded: Option<ColorAccuracy>,
}

/// Where the exactness reading stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Exactness {
    /// Not measured in this session.
    NotRun,
    /// Waiting for a frame.
    Waiting(TestPattern),
    /// Measured.
    Measured(Box<ExactnessMeasurement>),
    /// The frame could not be read, and why.
    Unreadable(TestPattern, String),
}

/// Everything [`assess`] reads.
#[derive(Debug, Clone, PartialEq)]
pub struct ColourCheckInput {
    /// The host's truth; `None` outside a session.
    pub host: Option<HostFacts>,
    /// The last decoded frame.
    pub frame: Option<FrameFacts>,
    /// This Mac.
    pub deck: DeckFacts,
    /// The exactness reading.
    pub exactness: Exactness,
    /// Outside a session: why the last connection ended, as the Deck was
    /// told (for example a host refusing Grading during setup).
    pub last_close: Option<String>,
}

/// One line of the checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckItem {
    /// Group.
    pub section: Section,
    /// What is checked.
    pub title: &'static str,
    /// The finding.
    pub finding: Finding,
    /// The facts behind it.
    pub details: Vec<String>,
    /// What can be done from here.
    pub fixes: Vec<ColourFix>,
}

impl CheckItem {
    fn new(section: Section, title: &'static str, finding: Finding) -> Self {
        Self {
            section,
            title,
            finding,
            details: Vec::new(),
            fixes: Vec::new(),
        }
    }

    fn detail(mut self, line: impl Into<String>) -> Self {
        self.details.push(line.into());
        self
    }

    fn fix(mut self, fix: ColourFix) -> Self {
        if !self.fixes.contains(&fix) {
            self.fixes.push(fix);
        }
        self
    }

    /// The verdict.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.finding.status()
    }
}

/// Who and when, for the report's heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportHeader {
    /// The Deck: product, version, OS and architecture.
    pub deck: String,
    /// When, in UTC, `YYYY-MM-DD HH:MM:SS`.
    pub generated_utc: String,
}

/// The assessed checklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColourCheckReport {
    /// Every item, grouped by section in [`Section::ALL`] order.
    pub items: Vec<CheckItem>,
    /// `server_name · platform · version` of the host, when connected.
    pub host_label: Option<String>,
}

impl ColourCheckReport {
    /// The worst verdict; informational items never lower it.
    #[must_use]
    pub fn overall(&self) -> Status {
        self.items
            .iter()
            .map(CheckItem::status)
            .filter(|status| *status != Status::Info)
            .max()
            .unwrap_or(Status::Pass)
    }

    /// How many items carry `status`.
    #[must_use]
    pub fn count(&self, status: Status) -> usize {
        self.items
            .iter()
            .filter(|item| item.status() == status)
            .count()
    }

    /// One line for the top of the panel.
    #[must_use]
    pub fn summary(&self) -> String {
        let failures = self.count(Status::Fail);
        let warnings = self.count(Status::Warn);
        match self.overall() {
            Status::Fail => format!(
                "Not ready for colour-critical work: {failures} problem{}, {warnings} caveat{}.",
                plural(failures),
                plural(warnings)
            ),
            Status::Warn => format!(
                "Usable with {warnings} caveat{} for colour-critical work.",
                plural(warnings)
            ),
            Status::Pass | Status::Info => "Ready for colour-critical work.".to_owned(),
        }
    }

    /// Items of one section.
    pub fn section(&self, section: Section) -> impl Iterator<Item = &CheckItem> {
        self.items
            .iter()
            .filter(move |item| item.section == section)
    }

    /// The Deck-side fixes of every failing or cautioned item, in order, each
    /// once: what a single "Apply" offers.
    #[must_use]
    pub fn recommended_fixes(&self) -> Vec<ColourFix> {
        let mut fixes = Vec::new();
        for item in &self.items {
            if matches!(item.status(), Status::Fail | Status::Warn) {
                for fix in &item.fixes {
                    if fix.changes_deck() && !fixes.contains(fix) {
                        fixes.push(*fix);
                    }
                }
            }
        }
        fixes
    }

    /// The report as plain text, for the clipboard or a file.
    #[must_use]
    pub fn render_text(&self, header: &ReportHeader) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Arcen colour check (report format {REPORT_FORMAT_VERSION})"
        );
        let _ = writeln!(out, "Generated: {} UTC", header.generated_utc);
        let _ = writeln!(out, "Deck: {}", header.deck);
        let _ = writeln!(
            out,
            "Host: {}",
            self.host_label.as_deref().unwrap_or("not connected")
        );
        let _ = writeln!(
            out,
            "Overall: {} — {}",
            self.overall().label(),
            self.summary()
        );
        for section in Section::ALL {
            let mut items = self.section(section).peekable();
            if items.peek().is_none() {
                continue;
            }
            let _ = writeln!(out);
            let _ = writeln!(out, "{}", section.title());
            for item in items {
                let status = item.status();
                let _ = writeln!(
                    out,
                    "  [{}] {} {} — {}",
                    status.label(),
                    status.symbol(),
                    item.title,
                    item.finding.code()
                );
                let _ = writeln!(out, "      {}", item.finding.meaning());
                for detail in &item.details {
                    let _ = writeln!(out, "      · {detail}");
                }
                let action = item.finding.action();
                if !action.is_empty() && status != Status::Pass {
                    let _ = writeln!(out, "      What to do: {action}");
                }
                for fix in &item.fixes {
                    match fix {
                        ColourFix::ReadDoc(link) => {
                            let _ = writeln!(out, "      Read: {} <{}>", link.title, link.url());
                        }
                        other => {
                            let _ = writeln!(out, "      Fix: {}", other.label());
                        }
                    }
                }
            }
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Codes are explained in {ERROR_CODES_DOCUMENT}. The Deck never changes a host setting."
        );
        out
    }
}

/// A file name for a report generated at `compact_utc` (`YYYYMMDD-HHMMSS`).
#[must_use]
pub fn report_file_name(compact_utc: &str) -> String {
    format!("arcen-colour-check-{compact_utc}.txt")
}

/// `epoch_secs` as `(YYYY-MM-DD HH:MM:SS, YYYYMMDD-HHMMSS)` in UTC.
#[must_use]
pub fn utc_stamps(epoch_secs: u64) -> (String, String) {
    let days = epoch_secs / 86_400;
    let seconds = epoch_secs % 86_400;
    let (hour, minute, second) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    // Days to civil date (proleptic Gregorian), after H. Hinnant.
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (
        format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"),
        format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}"),
    )
}

/// A file name for a pattern PNG.
#[must_use]
pub fn pattern_file_name(pattern: TestPattern, width: usize, height: usize) -> String {
    format!("arcen-pattern-{}-{width}x{height}.png", pattern.token())
}

const fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Text that came from the host, made safe to show and copy: control and
/// bidirectional-override characters become visible escapes and the text is
/// cut to [`MAX_HOST_TEXT_CHARS`].
#[must_use]
pub fn host_text(text: &str) -> String {
    let mut safe = String::new();
    for (count, character) in text.chars().enumerate() {
        if count == MAX_HOST_TEXT_CHARS {
            safe.push('…');
            break;
        }
        let unsafe_to_print = character.is_control()
            || matches!(
                character,
                '\u{061C}'
                    | '\u{200B}'..='\u{200F}'
                    | '\u{2028}'..='\u{202E}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{FEFF}'
            );
        if unsafe_to_print {
            let _ = write!(safe, "\\u{{{:04x}}}", u32::from(character));
        } else {
            safe.push(character);
        }
    }
    safe
}

/// The name of a codec, profile included, for the chroma and depth served.
#[must_use]
pub fn profile_name(codec: VideoCodec, chroma: ChromaSubsampling, depth: BitDepth) -> String {
    use ChromaSubsampling::{Yuv420, Yuv422, Yuv444};
    let ten_plus = depth >= BitDepth::Ten;
    let name = match (codec, chroma) {
        (VideoCodec::H265, Yuv420) if ten_plus => "HEVC Main 10",
        (VideoCodec::H265, Yuv420) => "HEVC Main",
        (VideoCodec::H265, Yuv422) => "HEVC Main 4:2:2 10 (range extensions)",
        (VideoCodec::H265, Yuv444) if ten_plus => "HEVC Main 4:4:4 10 (range extensions)",
        (VideoCodec::H265, Yuv444) => "HEVC Main 4:4:4 (range extensions)",
        (VideoCodec::H264, Yuv420) if ten_plus => "H.264 High 10",
        (VideoCodec::H264, Yuv420) => "H.264 High",
        (VideoCodec::H264, Yuv422) => "H.264 High 4:2:2",
        (VideoCodec::H264, Yuv444) => "H.264 High 4:4:4 Predictive",
        (VideoCodec::Av1, Yuv420) => "AV1 Main",
        (VideoCodec::Av1, Yuv444) => "AV1 High",
        (VideoCodec::Av1, Yuv422) => "AV1 Professional",
        (VideoCodec::Jpeg, _) => "JPEG",
        (VideoCodec::Vp9, _) => "VP9",
    };
    format!("{name}, {}-bit, {}", depth.bits(), chroma_label(chroma))
}

const fn chroma_label(chroma: ChromaSubsampling) -> &'static str {
    match chroma {
        ChromaSubsampling::Yuv420 => "4:2:0",
        ChromaSubsampling::Yuv422 => "4:2:2",
        ChromaSubsampling::Yuv444 => "4:4:4",
    }
}

fn axis<T: Copy>(value: Option<T>, name: impl Fn(T) -> &'static str) -> &'static str {
    value.map_or("unknown", name)
}

/// The capture source the served pipeline uses on `platform`, as the
/// architecture defines it. The host does not report it, so this is labelled
/// as implied wherever it is shown.
#[must_use]
pub fn implied_capture_source(
    platform: HostPlatform,
    session_type: &str,
    served: Option<&ServedStreamPipeline>,
) -> Option<&'static str> {
    use ServedStreamPipeline as P;
    let served = served?;
    Some(match (platform, session_type, served) {
        (_, _, P::Software) => "CPU capture → OpenH264 software encoder",
        (HostPlatform::Windows, _, P::Auto | P::Speed) => {
            "Desktop Duplication after real-frame proof, otherwise WGC BGRA8 → NVENC"
        }
        (HostPlatform::Windows, _, P::Grading) => {
            "WGC FP16 scRGB → shared sRGB re-encode (desktop codes) and matrix → NVENC P16"
        }
        (HostPlatform::Windows, _, P::Hdr) => {
            "proven HDR display state → WGC FP16 scRGB → BT.2020/PQ → NVENC P16"
        }
        (HostPlatform::Linux, "wayland", P::Auto | P::Speed) => {
            "Mutter ScreenCast (PipeWire) → NVENC"
        }
        (HostPlatform::Linux, "wayland", P::Grading | P::Hdr) => {
            "Mutter 50+ HDR ScreenCast (DMA-BUF, wide format) → NVENC"
        }
        (HostPlatform::Linux, _, P::Auto | P::Speed) => {
            "NvFBC → CUDA → NVENC (device-to-device fast path)"
        }
        (HostPlatform::Linux, _, P::Grading) => {
            "depth-30 Xorg → XShm RGB10 → shared conversion → CUDA upload → NVENC"
        }
        (HostPlatform::Linux, _, P::Hdr) => {
            "declared Rec.2100 PQ depth-30 Xorg → XShm → BT.2020 NCL → NVENC"
        }
        (HostPlatform::MacOs, _, P::Auto | P::Speed) => "ScreenCaptureKit 420v → VideoToolbox",
        (HostPlatform::MacOs, _, P::Grading) => {
            "ScreenCaptureKit xf44 → VideoToolbox HEVC Main 4:4:4 10, BT.709"
        }
        (HostPlatform::MacOs, _, P::Hdr) => {
            "ScreenCaptureKit xf44 PQ/BT.2020 → Core Image 203-nit white → VideoToolbox"
        }
        _ => return None,
    })
}

/// Assesses every item. See the module documentation.
#[must_use]
pub fn assess(input: &ColourCheckInput) -> ColourCheckReport {
    let mut items = Vec::new();
    let hdr_offer = input.deck.hdr_display;
    match &input.host {
        None => {
            let mut item = CheckItem::new(Section::Host, "Session", Finding::NotConnected);
            if let Some(reason) = &input.last_close {
                item = item.detail(format!("last connection ended: {}", host_text(reason)));
            }
            items.push(item);
        }
        Some(host) => host_items(
            host,
            input.frame.as_ref(),
            offers_hdr(host, hdr_offer),
            &mut items,
        ),
    }
    mac_items(input, &mut items);
    exactness_items(input, &mut items);
    if let Some(host) = &input.host {
        prerequisite_items(host, &mut items);
    }
    items.sort_by_key(|item| item.section);
    ColourCheckReport {
        items,
        host_label: input.host.as_ref().map(|host| {
            let platform = host.platform.token().unwrap_or("unknown platform");
            let session = if host.session_type.is_empty() {
                String::new()
            } else {
                format!(" ({})", host_text(&host.session_type))
            };
            format!(
                "{} · {platform}{session} · Pier {}",
                host_text(&host.server_name),
                host_text(&host.version)
            )
        }),
    }
}

/// Whether HDR is worth offering: this Mac can show it and the host's HDR
/// path keeps PQ (an Xorg or Wayland host serves an HDR request as Grading
/// unless its administrator declared a PQ desktop).
fn offers_hdr(host: &HostFacts, deck_hdr: bool) -> bool {
    deck_hdr && matches!(host.platform, HostPlatform::Windows | HostPlatform::MacOs)
}

fn colour_critical_fixes(item: CheckItem, hdr_offer: bool) -> CheckItem {
    let item = item.fix(ColourFix::SwitchPreset(StreamingPreset::Grading));
    if hdr_offer {
        item.fix(ColourFix::SwitchPreset(StreamingPreset::Hdr))
    } else {
        item
    }
}

#[allow(clippy::too_many_lines)]
fn host_items(
    host: &HostFacts,
    frame: Option<&FrameFacts>,
    hdr_offer: bool,
    items: &mut Vec<CheckItem>,
) {
    let requested = host.requested;
    let active = host.active;
    let critical = host.preset.is_colour_critical();

    // Preset.
    let served_token = host
        .served
        .as_ref()
        .map_or("not reported", ServedStreamPipeline::token);
    let preset_line = format!(
        "requested {} · served {}",
        host.preset.token(),
        host_text(served_token)
    );
    let served = host.served_preset();
    let downgraded = match (host.preset, served) {
        (RequestedPreset::Preset(asked), Some(Ok(got))) => asked != got,
        (RequestedPreset::Preset(_), Some(Err(_))) => true,
        _ => false,
    };
    let preset_finding = if downgraded {
        Finding::PresetDowngraded
    } else if host.preset == RequestedPreset::Custom {
        Finding::PresetCustom
    } else if !critical {
        Finding::PresetEightBit
    } else if host.served.is_none() {
        Finding::PresetUnreported
    } else {
        Finding::PresetServed
    };
    let mut preset = CheckItem::new(Section::Host, "Preset", preset_finding).detail(preset_line);
    if let Some(reason) = &host.served_reason {
        preset = preset.detail(format!("host's reason: {}", host_text(reason)));
    }
    if let Some(detail) = &host.served_detail {
        preset = preset.detail(format!("host says: {}", host_text(detail)));
    }
    preset = match preset_finding {
        Finding::PresetEightBit | Finding::PresetCustom => colour_critical_fixes(preset, hdr_offer),
        Finding::PresetDowngraded
            if host.preset == RequestedPreset::Preset(StreamingPreset::Hdr) =>
        {
            preset.fix(ColourFix::SwitchPreset(StreamingPreset::Grading))
        }
        _ => preset,
    };
    items.push(preset.fix(ColourFix::ReadDoc(DOC_PRESETS)));

    // Codec and profile.
    let codec_finding = match active.codec {
        None => Finding::CodecUnknown,
        Some(codec) if codec != requested.codec && !host.adaptive_codec => Finding::CodecChanged,
        Some(_) => Finding::CodecProfile,
    };
    let mut codec = CheckItem::new(Section::Host, "Codec and profile", codec_finding);
    if let (Some(name), Some(chroma), Some(depth)) = (active.codec, active.chroma, active.bit_depth)
    {
        codec = codec.detail(profile_name(name, chroma, depth));
    }
    codec = codec.detail(format!(
        "requested {} · served {}",
        requested.codec.token(),
        axis(active.codec, VideoCodec::token)
    ));
    items.push(codec);

    // Chroma.
    let chroma_finding = match active.chroma {
        None => Finding::ChromaUnknown,
        Some(ChromaSubsampling::Yuv444) => Finding::ChromaFull,
        Some(_) if requested.chroma == ChromaSubsampling::Yuv444 => Finding::ChromaReduced,
        Some(_) => Finding::ChromaSubsampled,
    };
    let mut chroma = CheckItem::new(Section::Host, "Chroma", chroma_finding).detail(format!(
        "requested {} · served {}",
        chroma_label(requested.chroma),
        active.chroma.map_or("unknown", chroma_label)
    ));
    if chroma_finding == Finding::ChromaSubsampled {
        chroma = colour_critical_fixes(chroma, hdr_offer);
    }
    items.push(chroma);

    // Bit depth.
    let depth_finding = match active.bit_depth {
        None => Finding::DepthUnknown,
        Some(depth) if depth >= BitDepth::Ten => Finding::DepthTen,
        Some(_) if requested.bit_depth >= BitDepth::Ten => Finding::DepthReduced,
        Some(_) => Finding::DepthEight,
    };
    let mut depth = CheckItem::new(Section::Host, "Bit depth", depth_finding).detail(format!(
        "requested {}-bit · served {}",
        requested.bit_depth.bits(),
        active.bit_depth.map_or_else(
            || "unknown".to_owned(),
            |depth| format!("{}-bit", depth.bits())
        )
    ));
    if depth_finding == Finding::DepthEight {
        depth = colour_critical_fixes(depth, hdr_offer);
    }
    items.push(depth);

    // Transfer and primaries.
    let signal_finding = match (active.transfer, active.primaries) {
        (Some(transfer), Some(primaries)) => {
            if !transfer.serves_request(requested.transfer) || primaries != requested.primaries {
                Finding::SignalChanged
            } else if transfer == TransferCharacteristics::Pq {
                Finding::SignalHdr
            } else {
                Finding::SignalSdr
            }
        }
        _ => Finding::SignalUnknown,
    };
    let mut signal = CheckItem::new(Section::Host, "Transfer and primaries", signal_finding)
        .detail(format!(
            "requested {} / {} · served {} / {}",
            requested.transfer.token(),
            requested.primaries.token(),
            axis(active.transfer, TransferCharacteristics::token),
            axis(active.primaries, ColorPrimaries::token)
        ));
    if active.transfer.is_some_and(|transfer| {
        transfer != requested.transfer && transfer.serves_request(requested.transfer)
    }) {
        signal = signal.detail(
            "sRGB transfer: the stream keeps the desktop's own code values, the exact answer \
             to a BT.709 SDR request",
        );
    }
    items.push(signal);

    // Range and matrix.
    let coding_finding = match (active.range, active.matrix) {
        (Some(range), Some(matrix)) => {
            if range != requested.range || matrix != requested.matrix {
                Finding::CodingChanged
            } else if range == ColorRange::Limited {
                Finding::CodingLimited
            } else {
                Finding::CodingFull
            }
        }
        _ => Finding::CodingUnknown,
    };
    let mut coding =
        CheckItem::new(Section::Host, "Range and matrix", coding_finding).detail(format!(
            "requested {} / {} · served {} / {}",
            requested.range.token(),
            requested.matrix.token(),
            axis(active.range, ColorRange::token),
            axis(active.matrix, ColorMatrix::token)
        ));
    if coding_finding == Finding::CodingLimited {
        coding = colour_critical_fixes(coding, hdr_offer);
    }
    items.push(coding);

    // The frames themselves.
    let frames = match frame {
        None => CheckItem::new(Section::Host, "Frame headers", Finding::FramesNone),
        Some(frame) => {
            let video = frame.video;
            let mut mismatched = Vec::new();
            if active.codec.is_some_and(|codec| codec != video.codec) {
                mismatched.push("codec");
            }
            if active.chroma.is_some_and(|chroma| chroma != video.chroma) {
                mismatched.push("chroma");
            }
            if active
                .bit_depth
                .is_some_and(|depth| depth != video.bit_depth)
            {
                mismatched.push("bit depth");
            }
            if active.range.is_some_and(|range| range != video.range) {
                mismatched.push("range");
            }
            if active.matrix.is_some_and(|matrix| matrix != video.matrix) {
                mismatched.push("matrix");
            }
            let finding = if mismatched.is_empty() {
                Finding::FramesAgree
            } else {
                Finding::FramesDisagree
            };
            let mut item = CheckItem::new(Section::Host, "Frame headers", finding).detail(format!(
                "decoded {}×{} {} · {} {} {}-bit {} {}",
                frame.width,
                frame.height,
                host_text(&frame.pixel_format),
                video.codec.token(),
                chroma_label(video.chroma),
                video.bit_depth.bits(),
                video.range.token(),
                video.matrix.token()
            ));
            if !mismatched.is_empty() {
                item = item.detail(format!(
                    "differs from the hello in: {}",
                    mismatched.join(", ")
                ));
            }
            item
        }
    };
    items.push(frames);

    // Capture source (implied).
    if let Some(source) =
        implied_capture_source(host.platform, &host.session_type, host.served.as_ref())
    {
        items.push(
            CheckItem::new(Section::Host, "Capture source", Finding::CaptureImplied)
                .detail(format!("implied by the served pipeline: {source}")),
        );
    }

    // Encoder.
    let encoder_finding = match host.encoder_hardware {
        Some(true) => Finding::EncoderHardware,
        Some(false) => Finding::EncoderSoftware,
        None => Finding::EncoderUnknown,
    };
    let backend = if host.encoder_backend.is_empty() {
        "not named".to_owned()
    } else {
        host_text(&host.encoder_backend)
    };
    items.push(
        CheckItem::new(Section::Host, "Encoder", encoder_finding)
            .detail(format!("backend: {backend}")),
    );
}

#[allow(clippy::too_many_lines)]
fn mac_items(input: &ColourCheckInput, items: &mut Vec<CheckItem>) {
    let deck = &input.deck;
    let host = input.host.as_ref();
    let pq = host.is_some_and(HostFacts::is_pq);

    // Display mode.
    match &deck.display {
        None => items.push(CheckItem::new(
            Section::Mac,
            "Display",
            Finding::DisplayUnknown,
        )),
        Some(display) => {
            let reference = display.edr_reference.unwrap_or(0.0);
            let finding = if reference > 0.0 {
                Finding::DisplayReference
            } else {
                Finding::DisplayNotReference
            };
            let mut item = CheckItem::new(Section::Mac, "Display", finding).detail(
                display
                    .name
                    .as_deref()
                    .map_or_else(|| "unnamed display".to_owned(), host_text),
            );
            if reference > 0.0 {
                item = item.detail(format!(
                    "reference headroom {reference:.1}× SDR white (reference preset active)"
                ));
            } else {
                item = item.detail(
                    "reference headroom 0 (macOS reports no reference preset in use)".to_owned(),
                );
            }
            items.push(item);

            let tag = deck
                .presentation
                .colour_space
                .clone()
                .unwrap_or_else(|| "not tagged yet".to_owned());
            items.push(
                CheckItem::new(Section::Mac, "Colour profile", Finding::DisplayProfile)
                    .detail(format!(
                        "profile: {}",
                        display
                            .profile
                            .as_deref()
                            .map_or_else(|| "unknown".to_owned(), host_text)
                    ))
                    .detail(format!("picture tagged as: {}", host_text(&tag)))
                    .detail(format!(
                        "wide gamut (P3): {}",
                        match display.wide_gamut {
                            Some(true) => "yes",
                            Some(false) => "no",
                            None => "unknown",
                        }
                    )),
            );

            let potential = display.edr_potential.unwrap_or(1.0);
            let headroom = format!(
                "HDR headroom: {potential:.1}× SDR white possible (≈{:.0} nits), {:.1}× now",
                potential * crate::display_color::EDR_REFERENCE_WHITE_NITS,
                display.edr_current.unwrap_or(1.0)
            );
            let finding = if !pq {
                Finding::HdrNotUsed
            } else if potential > 1.0 {
                Finding::HdrReady
            } else {
                Finding::HdrNoHeadroom
            };
            let mut item = CheckItem::new(Section::Mac, "HDR", finding).detail(headroom);
            if finding == Finding::HdrNoHeadroom {
                item = item.fix(ColourFix::SwitchPreset(StreamingPreset::Grading));
            }
            // An SDR session on an HDR-capable Mac, against a host whose HDR
            // path keeps PQ, may switch on purpose; it is an offer, not a
            // recommendation, because Grading is the right SDR reference.
            let host_keeps_pq = host.is_some_and(|host| {
                matches!(host.platform, HostPlatform::Windows | HostPlatform::MacOs)
                    && host.preset != RequestedPreset::Preset(StreamingPreset::Hdr)
            });
            if finding == Finding::HdrNotUsed
                && potential > 1.0
                && deck.hdr_display
                && host_keeps_pq
            {
                item = item.fix(ColourFix::SwitchPreset(StreamingPreset::Hdr));
            }
            items.push(item);
        }
    }

    // Presentation path.
    let ten_bit_stream = host
        .and_then(|host| host.active.bit_depth)
        .is_some_and(|depth| depth >= BitDepth::Ten);
    let presentation = match &deck.presentation.path {
        PresentationPath::None => {
            CheckItem::new(Section::Mac, "Presentation", Finding::PresentNone)
        }
        PresentationPath::TenBitLayer => {
            CheckItem::new(Section::Mac, "Presentation", Finding::PresentTenBit)
                .detail("dedicated CAMetalLayer, RGB10A2, decoded planes converted on the GPU")
        }
        PresentationPath::EightBit if ten_bit_stream => {
            CheckItem::new(Section::Mac, "Presentation", Finding::PresentFallback)
                .detail("ten-bit stream on the eight-bit path")
        }
        PresentationPath::EightBit => {
            CheckItem::new(Section::Mac, "Presentation", Finding::PresentEightBit)
        }
        PresentationPath::EightBitFallback(reason) => {
            CheckItem::new(Section::Mac, "Presentation", Finding::PresentFallback)
                .detail(format!("fallback reason: {}", host_text(reason)))
        }
    };
    items.push(presentation);

    // EDR.
    let edr = match (deck.presentation.edr, &deck.presentation.path) {
        (Some(on), PresentationPath::TenBitLayer | PresentationPath::EightBit)
            if host.is_some() =>
        {
            let finding = if on == pq {
                Finding::EdrCorrect
            } else {
                Finding::EdrMismatch
            };
            CheckItem::new(Section::Mac, "EDR", finding).detail(format!(
                "EDR {} · stream transfer {}",
                if on { "on" } else { "off" },
                axis(
                    host.and_then(|host| host.active.transfer),
                    TransferCharacteristics::token
                )
            ))
        }
        _ => CheckItem::new(Section::Mac, "EDR", Finding::EdrUnknown),
    };
    items.push(edr);

    // Scale.
    let scale = match deck.scale {
        None => CheckItem::new(Section::Mac, "Scale", Finding::ScaleUnknown),
        Some(scale) => scale_item(scale),
    };
    items.push(scale);
}

fn scale_item(scale: ScaleFacts) -> CheckItem {
    let [stream_width, stream_height] = scale.stream;
    let [shown_width, shown_height] = scale.shown;
    if stream_width == 0 || stream_height == 0 {
        return CheckItem::new(Section::Mac, "Scale", Finding::ScaleUnknown);
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio_x = shown_width / stream_width as f32;
    // Within one screen pixel over the whole picture, or it is resampled:
    // a 3600-pixel stream drawn 3596 pixels wide blends every column.
    #[allow(clippy::cast_precision_loss)]
    let exact = |factor: f32| {
        (shown_width - stream_width as f32 * factor).abs() <= 1.0
            && (shown_height - stream_height as f32 * factor).abs() <= 1.0
    };
    let whole = ratio_x.round();
    let finding = if exact(1.0) {
        Finding::ScaleOneToOne
    } else if whole >= 2.0 && exact(whole) {
        Finding::ScaleInteger
    } else {
        Finding::ScaleResampled
    };
    let mut item = CheckItem::new(Section::Mac, "Scale", finding)
        .detail(format!(
            "stream {stream_width}×{stream_height} px shown at {shown_width:.0}×{shown_height:.0} \
             screen px ({ratio_x:.3}×)"
        ))
        .detail(format!(
            "backing scale {:.1} · Retina-resolution streaming {} · {} · {}",
            scale.backing_scale,
            if scale.hidpi_streaming { "on" } else { "off" },
            if scale.pinned {
                "stream pinned to the display"
            } else {
                "stream follows the window"
            },
            if scale.fullscreen {
                "fullscreen"
            } else {
                "windowed"
            }
        ));
    let retina_off = !scale.hidpi_streaming && scale.backing_scale > 1.0;
    match finding {
        Finding::ScaleInteger if retina_off => {
            item = item.fix(ColourFix::StreamAtRetinaResolution);
        }
        Finding::ScaleResampled => {
            if scale.fullscreen && scale.pinned {
                item = item.detail(
                    "already fullscreen: the host's desktop is not this screen's size, so the \
                     Deck fits it; reconnecting lets the host match the screen again",
                );
            }
            if !scale.fullscreen {
                item = item.fix(ColourFix::EnterFullscreen);
            }
            if retina_off {
                item = item.fix(ColourFix::StreamAtRetinaResolution);
            }
        }
        _ => {}
    }
    item
}

fn exactness_items(input: &ColourCheckInput, items: &mut Vec<CheckItem>) {
    let Some(host) = &input.host else {
        items.push(CheckItem::new(
            Section::Exactness,
            "Test pattern",
            Finding::ExactNotRun,
        ));
        items.push(
            CheckItem::new(Section::Exactness, "Bit-exact", Finding::ExactRefinement)
                .fix(ColourFix::ReadDoc(DOC_LOSSLESS)),
        );
        return;
    };
    let item = if host.is_pq() {
        CheckItem::new(
            Section::Exactness,
            "Test pattern",
            Finding::ExactNotApplicable,
        )
        .fix(ColourFix::SwitchPreset(StreamingPreset::Grading))
    } else {
        match &input.exactness {
            Exactness::NotRun => {
                CheckItem::new(Section::Exactness, "Test pattern", Finding::ExactNotRun)
                    .fix(ColourFix::SavePattern)
                    .fix(ColourFix::Measure)
            }
            Exactness::Waiting(pattern) => {
                CheckItem::new(Section::Exactness, "Test pattern", Finding::ExactWaiting)
                    .detail(pattern_line(*pattern))
            }
            Exactness::Unreadable(pattern, why) => CheckItem::new(
                Section::Exactness,
                "Test pattern",
                Finding::ExactUnavailable,
            )
            .detail(pattern_line(*pattern))
            .detail(host_text(why))
            .fix(ColourFix::Measure),
            Exactness::Measured(measurement) => measured_item(measurement),
        }
    };
    items.push(item.fix(ColourFix::ReadDoc(DOC_COLOUR_CHECK)));
    items.push(
        CheckItem::new(Section::Exactness, "Bit-exact", Finding::ExactRefinement)
            .fix(ColourFix::ReadDoc(DOC_LOSSLESS)),
    );
}

fn pattern_line(pattern: TestPattern) -> String {
    format!(
        "pattern: {} (exposes {})",
        pattern.token(),
        pattern.exposes()
    )
}

/// Whether the pixels off by more than [`NEAR_EXACT_CODES`] are at most a
/// thousandth of the picture, or all lie in one area no larger than a
/// fiftieth of it.
fn localised(accuracy: ColorAccuracy) -> bool {
    accuracy
        .outlier_bounds
        .is_some_and(|[left, top, right, bottom]| {
            accuracy.outliers * 1000 <= accuracy.pixels
                || (right - left + 1) * (bottom - top + 1) * 50 <= accuracy.pixels
        })
}

fn outlier_line(label: &str, accuracy: ColorAccuracy) -> Option<String> {
    let [left, top, right, bottom] = accuracy.outlier_bounds?;
    Some(format!(
        "{label}: {} pixels off by more than {NEAR_EXACT_CODES}, all inside x {left}–{right}, \
         y {top}–{bottom}",
        accuracy.outliers
    ))
}

fn measured_item(measurement: &ExactnessMeasurement) -> CheckItem {
    let rgb = measurement.rgb;
    let codes_exact = measurement.codes.is_none_or(CodeAccuracy::is_exact);
    let finding = if rgb.is_exact() && codes_exact {
        Finding::ExactExact
    } else if rgb.max_error <= NEAR_EXACT_CODES {
        Finding::ExactNear
    } else if measurement
        .reencoded
        .is_some_and(|reencoded| reencoded.max_error <= NEAR_EXACT_CODES)
    {
        Finding::ExactReencoded
    } else if localised(rgb) || measurement.reencoded.is_some_and(localised) {
        Finding::ExactLocalised
    } else {
        Finding::ExactOff
    };
    let mut item = CheckItem::new(Section::Exactness, "Test pattern", finding)
        .detail(pattern_line(measurement.pattern))
        .detail(format!(
            "measured {}×{}: {} pixels, every one compared",
            measurement.width, measurement.height, rgb.pixels
        ))
        .detail(format!(
            "RGB as the desktop drew it: max {}/255, mean {:.3}/255, worst at ({}, {})",
            rgb.max_error, rgb.mean_error, rgb.worst_at.0, rgb.worst_at.1
        ));
    item = match measurement.codes {
        Some(codes) => item.detail(format!(
            "coded Y′CbCr samples: max {}/{}, mean {:.3}, worst at ({}, {})",
            codes.max_error, codes.max_code, codes.mean_error, codes.worst_at.0, codes.worst_at.1
        )),
        None => item
            .detail("coded samples not compared: subsampled chroma has no single expected value"),
    };
    if let Some(line) = outlier_line("as desktop codes", rgb) {
        item = item.detail(line);
    }
    if let Some(reencoded) = measurement.reencoded {
        if let Some(line) = outlier_line("as BT.709 re-encoded", reencoded) {
            item = item.detail(line);
        }
        if finding == Finding::ExactLocalised && localised(reencoded) && !localised(rgb) {
            item = item.detail(
                "outside that area the picture matches the desktop's sRGB re-encoded as BT.709: \
                 code values changed (see colour.exact.bt709_reencoded)",
            );
        }
        item = item.detail(format!(
            "RGB against the desktop's sRGB re-encoded as BT.709: max {}/255, mean {:.3}/255, \
             worst at ({}, {})",
            reencoded.max_error, reencoded.mean_error, reencoded.worst_at.0, reencoded.worst_at.1
        ));
    }
    item.detail("read from the decoder's own planes, before any display conversion or scaling")
        .fix(ColourFix::Measure)
}

fn prerequisite_items(host: &HostFacts, items: &mut Vec<CheckItem>) {
    let downgraded = matches!(
        items.iter().find(|item| item.title == "Preset"),
        Some(item) if item.finding == Finding::PresetDowngraded
    );
    let asked_hdr = host.preset == RequestedPreset::Preset(StreamingPreset::Hdr);
    let reason = host.served_reason.as_deref().unwrap_or("");
    let item = if downgraded {
        let finding = match host.platform {
            HostPlatform::Linux if host.is_wayland() => Finding::HostWaylandFidelity,
            HostPlatform::Linux
                if asked_hdr && host.active.bit_depth.is_some_and(|d| d >= BitDepth::Ten) =>
            {
                Finding::HostXorgHdr
            }
            HostPlatform::Linux if host.is_x11() || reason.contains("eight_bit") => {
                Finding::HostLinuxDepth
            }
            HostPlatform::Windows if asked_hdr => Finding::HostWindowsHdr,
            HostPlatform::MacOs if asked_hdr => Finding::HostMacPier,
            _ => Finding::HostReason,
        };
        let mut item = CheckItem::new(Section::Prerequisites, "Host", finding);
        if !reason.is_empty() {
            item = item.detail(format!("host's reason: {}", host_text(reason)));
        }
        with_platform_docs(item, host)
    } else if host.preset.is_colour_critical() {
        CheckItem::new(Section::Prerequisites, "Host", Finding::HostReady)
    } else {
        let lines: &[&str] = match host.platform {
            HostPlatform::Linux if host.is_wayland() => &[
                "Grading and HDR need Mutter 50 or later with HDR screen sharing (GNOME 49 serves eight-bit).",
            ],
            HostPlatform::Linux => &[
                "Grading needs a depth-30 Xorg desktop (video.x11_desktop_depth auto starts one for a Grading sign-in, or 30).",
                "HDR on Xorg needs a declared Rec.2100 PQ desktop (video.desktop_encoding); otherwise it is served as Grading.",
            ],
            HostPlatform::Windows => &[
                "Grading works on any NVENC host (WGC FP16 capture).",
                "HDR needs the captured display to be proven in HDR mode; Arcen's virtual display for an HDR Deck display does this.",
            ],
            HostPlatform::MacOs => {
                &["The macOS Pier is a preview; HDR needs its virtual display to report headroom."]
            }
            HostPlatform::Unknown => &["The host did not say which platform it runs."],
        };
        let mut item = CheckItem::new(Section::Prerequisites, "Host", Finding::HostGradingNeeds);
        for line in lines {
            item = item.detail(*line);
        }
        with_platform_docs(item, host)
    };
    items.push(item);
}

fn with_platform_docs(item: CheckItem, host: &HostFacts) -> CheckItem {
    match host.platform {
        HostPlatform::Linux if host.is_wayland() => item.fix(ColourFix::ReadDoc(DOC_WAYLAND)),
        HostPlatform::Linux => item
            .fix(ColourFix::ReadDoc(DOC_LINUX_DEPTH))
            .fix(ColourFix::ReadDoc(DOC_XORG_HDR)),
        HostPlatform::Windows => item.fix(ColourFix::ReadDoc(DOC_WINDOWS_HDR)),
        HostPlatform::MacOs | HostPlatform::Unknown => item.fix(ColourFix::ReadDoc(DOC_PRESETS)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grading() -> VideoConfiguration {
        StreamingPreset::Grading.pipeline_contract().colour
    }

    fn hdr() -> VideoConfiguration {
        StreamingPreset::Hdr.pipeline_contract().colour
    }

    fn auto() -> VideoConfiguration {
        StreamingPreset::Auto.pipeline_contract().colour
    }

    fn host(
        platform: HostPlatform,
        session_type: &str,
        preset: StreamingPreset,
        requested: VideoConfiguration,
        served: ServedStreamPipeline,
        active: VideoConfiguration,
    ) -> HostFacts {
        HostFacts {
            platform,
            session_type: session_type.to_owned(),
            server_name: "pier.example.internal".to_owned(),
            version: "0.17.0".to_owned(),
            preset: RequestedPreset::Preset(preset),
            requested,
            adaptive_codec: matches!(preset, StreamingPreset::Auto | StreamingPreset::Speed),
            served: Some(served),
            served_reason: None,
            served_detail: None,
            active: ColourAxes::from_configuration(active),
            encoder_backend: "nvenc".to_owned(),
            encoder_hardware: Some(true),
        }
    }

    fn xdr_display() -> DisplayFacts {
        DisplayFacts {
            name: Some("Built-in Retina Display".to_owned()),
            profile: Some("Color LCD".to_owned()),
            wide_gamut: Some(true),
            edr_current: Some(1.0),
            edr_potential: Some(16.0),
            edr_reference: Some(0.0),
        }
    }

    fn deck(path: PresentationPath, edr: bool, shown: [f32; 2]) -> DeckFacts {
        DeckFacts {
            display: Some(xdr_display()),
            presentation: PresentationFacts {
                path,
                edr: Some(edr),
                colour_space: Some("sRGB".to_owned()),
            },
            scale: Some(ScaleFacts {
                stream: [3024, 1890],
                shown,
                backing_scale: 2.0,
                hidpi_streaming: true,
                pinned: false,
                fullscreen: false,
            }),
            hdr_display: true,
        }
    }

    fn frame(video: VideoConfiguration) -> FrameFacts {
        FrameFacts {
            video,
            width: 3024,
            height: 1890,
            pixel_format: "xf44-full-iosurface".to_owned(),
        }
    }

    fn finding(report: &ColourCheckReport, title: &str) -> Finding {
        report
            .items
            .iter()
            .find(|item| item.title == title)
            .unwrap_or_else(|| panic!("no {title} item"))
            .finding
    }

    fn input(host: HostFacts, deck: DeckFacts, exactness: Exactness) -> ColourCheckInput {
        let video = VideoConfiguration {
            codec: host.active.codec.unwrap(),
            chroma: host.active.chroma.unwrap(),
            bit_depth: host.active.bit_depth.unwrap(),
            range: host.active.range.unwrap(),
            matrix: host.active.matrix.unwrap(),
            primaries: host.active.primaries.unwrap(),
            transfer: host.active.transfer.unwrap(),
        };
        ColourCheckInput {
            host: Some(host),
            frame: Some(frame(video)),
            deck,
            exactness,
            last_close: None,
        }
    }

    fn exact_measurement(max_rgb: u16, max_codes: u16) -> ExactnessMeasurement {
        ExactnessMeasurement {
            pattern: TestPattern::GreyRamp,
            width: 3024,
            height: 1890,
            rgb: ColorAccuracy {
                max_error: max_rgb,
                mean_error: 0.0,
                pixels: 3024 * 1890,
                worst_at: (0, 0),
                outliers: if max_rgb > 2 { 3024 * 1890 } else { 0 },
                outlier_bounds: (max_rgb > 2).then_some([0, 0, 3023, 1889]),
            },
            codes: Some(CodeAccuracy {
                max_error: max_codes,
                mean_error: 0.0,
                max_code: 1023,
                pixels: 3024 * 1890,
                worst_at: (0, 0),
            }),
            reencoded: None,
        }
    }

    #[test]
    fn a_served_grading_session_on_a_ten_bit_layer_at_one_to_one_passes_every_host_and_mac_item() {
        let host = host(
            HostPlatform::Windows,
            "",
            StreamingPreset::Grading,
            grading(),
            ServedStreamPipeline::Grading,
            grading(),
        );
        let report = assess(&input(
            host,
            deck(PresentationPath::TenBitLayer, false, [3024.0, 1890.0]),
            Exactness::Measured(Box::new(exact_measurement(0, 0))),
        ));
        for title in [
            "Preset",
            "Codec and profile",
            "Chroma",
            "Bit depth",
            "Transfer and primaries",
            "Range and matrix",
            "Frame headers",
            "Presentation",
            "EDR",
            "Scale",
            "Test pattern",
        ] {
            assert_eq!(
                report
                    .items
                    .iter()
                    .find(|item| item.title == title)
                    .unwrap()
                    .status(),
                Status::Pass,
                "{title}"
            );
        }
        assert_eq!(finding(&report, "Host"), Finding::HostReady);
        // The display is not in a reference preset: a caveat, not a pass.
        assert_eq!(finding(&report, "Display"), Finding::DisplayNotReference);
        assert_eq!(report.overall(), Status::Warn);
        assert!(report.recommended_fixes().is_empty());
        // HDR is offered on an HDR-capable Mac, never recommended over Grading.
        let hdr = report
            .items
            .iter()
            .find(|item| item.title == "HDR")
            .unwrap();
        assert_eq!(hdr.finding, Finding::HdrNotUsed);
        assert_eq!(
            hdr.fixes,
            vec![ColourFix::SwitchPreset(StreamingPreset::Hdr)]
        );
        let codec = report
            .items
            .iter()
            .find(|item| item.title == "Codec and profile")
            .unwrap();
        assert!(
            codec.details[0].starts_with("HEVC Main 4:4:4 10"),
            "{:?}",
            codec.details
        );
    }

    #[test]
    fn auto_is_a_caveat_with_a_one_click_switch_to_grading_and_hdr() {
        let host = host(
            HostPlatform::Linux,
            "x11",
            StreamingPreset::Auto,
            auto(),
            ServedStreamPipeline::Auto,
            auto(),
        );
        let report = assess(&input(
            host,
            deck(PresentationPath::EightBit, false, [3024.0, 1890.0]),
            Exactness::NotRun,
        ));
        assert_eq!(finding(&report, "Preset"), Finding::PresetEightBit);
        assert_eq!(finding(&report, "Chroma"), Finding::ChromaSubsampled);
        assert_eq!(finding(&report, "Presentation"), Finding::PresentEightBit);
        assert_eq!(finding(&report, "Host"), Finding::HostGradingNeeds);
        // An Xorg host serves HDR as Grading, so HDR is not offered there.
        assert_eq!(
            report.recommended_fixes(),
            vec![ColourFix::SwitchPreset(StreamingPreset::Grading)]
        );
    }

    #[test]
    fn xorg_hdr_served_as_grading_fails_the_signal_and_names_the_xorg_gate() {
        let mut host = host(
            HostPlatform::Linux,
            "x11",
            StreamingPreset::Hdr,
            hdr(),
            ServedStreamPipeline::Grading,
            grading(),
        );
        host.served_reason = Some("hdr_output_not_proven".to_owned());
        let report = assess(&input(
            host,
            deck(PresentationPath::TenBitLayer, false, [3024.0, 1890.0]),
            Exactness::NotRun,
        ));
        assert_eq!(finding(&report, "Preset"), Finding::PresetDowngraded);
        assert_eq!(
            finding(&report, "Transfer and primaries"),
            Finding::SignalChanged
        );
        assert_eq!(finding(&report, "Host"), Finding::HostXorgHdr);
        assert_eq!(finding(&report, "EDR"), Finding::EdrCorrect);
        let host_item = report
            .items
            .iter()
            .find(|item| item.title == "Host")
            .unwrap();
        assert!(host_item.fixes.contains(&ColourFix::ReadDoc(DOC_XORG_HDR)));
        assert!(
            report
                .recommended_fixes()
                .contains(&ColourFix::SwitchPreset(StreamingPreset::Grading))
        );
        assert_eq!(report.overall(), Status::Fail);
    }

    #[test]
    fn a_depth_24_xorg_desktop_serving_grading_as_auto_points_at_desktop_depth() {
        let mut host = host(
            HostPlatform::Linux,
            "x11",
            StreamingPreset::Grading,
            grading(),
            ServedStreamPipeline::Auto,
            auto(),
        );
        host.served_reason = Some("eight_bit_source".to_owned());
        host.served_detail = Some("Sign out of it\u{202E} and reconnect".to_owned());
        let report = assess(&input(
            host,
            deck(PresentationPath::EightBit, false, [3024.0, 1890.0]),
            Exactness::NotRun,
        ));
        assert_eq!(finding(&report, "Chroma"), Finding::ChromaReduced);
        assert_eq!(finding(&report, "Bit depth"), Finding::DepthReduced);
        assert_eq!(finding(&report, "Host"), Finding::HostLinuxDepth);
        let preset = report
            .items
            .iter()
            .find(|item| item.title == "Preset")
            .unwrap();
        assert!(preset.details.iter().any(|line| line.contains("\\u{202e}")));
    }

    #[test]
    fn wayland_downgrade_names_mutter_fifty() {
        let host = host(
            HostPlatform::Linux,
            "wayland",
            StreamingPreset::Grading,
            grading(),
            ServedStreamPipeline::Auto,
            auto(),
        );
        let report = assess(&input(
            host,
            deck(PresentationPath::EightBit, false, [3024.0, 1890.0]),
            Exactness::NotRun,
        ));
        assert_eq!(finding(&report, "Host"), Finding::HostWaylandFidelity);
    }

    /// Every Pier keeps the desktop's codes and says `srgb` for this Deck's
    /// `bt709` Grading request (Windows from 0.18.0, Linux Xorg and macOS from
    /// 0.19.0): the request is kept, and the report says why the tokens differ.
    #[test]
    fn grading_served_as_srgb_keeps_the_signal_item_passing_on_every_pier() {
        let served = VideoConfiguration {
            transfer: TransferCharacteristics::Srgb,
            ..grading()
        };
        for (platform, session_type) in [
            (HostPlatform::Windows, ""),
            (HostPlatform::Linux, "x11"),
            (HostPlatform::MacOs, ""),
        ] {
            let host = host(
                platform,
                session_type,
                StreamingPreset::Grading,
                grading(),
                ServedStreamPipeline::Grading,
                served,
            );
            let report = assess(&input(
                host,
                deck(PresentationPath::TenBitLayer, false, [3024.0, 1890.0]),
                Exactness::Measured(Box::new(exact_measurement(0, 0))),
            ));
            assert_eq!(
                finding(&report, "Transfer and primaries"),
                Finding::SignalSdr,
                "{platform:?}"
            );
            let item = report
                .items
                .iter()
                .find(|item| item.title == "Transfer and primaries")
                .expect("signal item");
            assert!(
                item.details
                    .iter()
                    .any(|line| line.contains("served srgb / bt709")),
                "{platform:?}: {:?}",
                item.details
            );
            assert!(
                item.details
                    .iter()
                    .any(|line| line.contains("desktop's own code values")),
                "{platform:?}"
            );
        }
    }

    #[test]
    fn hdr_on_windows_needs_headroom_edr_and_skips_code_comparison() {
        let host = host(
            HostPlatform::Windows,
            "",
            StreamingPreset::Hdr,
            hdr(),
            ServedStreamPipeline::Hdr,
            hdr(),
        );
        let mut facts = deck(PresentationPath::TenBitLayer, true, [3024.0, 1890.0]);
        let report = assess(&input(host.clone(), facts.clone(), Exactness::NotRun));
        assert_eq!(
            finding(&report, "Transfer and primaries"),
            Finding::SignalHdr
        );
        assert_eq!(finding(&report, "HDR"), Finding::HdrReady);
        assert_eq!(finding(&report, "EDR"), Finding::EdrCorrect);
        assert_eq!(
            finding(&report, "Test pattern"),
            Finding::ExactNotApplicable
        );

        facts.presentation.edr = Some(false);
        if let Some(display) = facts.display.as_mut() {
            display.edr_potential = Some(1.0);
        }
        let report = assess(&input(host, facts, Exactness::NotRun));
        assert_eq!(finding(&report, "EDR"), Finding::EdrMismatch);
        assert_eq!(finding(&report, "HDR"), Finding::HdrNoHeadroom);
    }

    #[test]
    fn edr_on_for_an_sdr_stream_is_a_failure() {
        let host = host(
            HostPlatform::Windows,
            "",
            StreamingPreset::Grading,
            grading(),
            ServedStreamPipeline::Grading,
            grading(),
        );
        let report = assess(&input(
            host,
            deck(PresentationPath::TenBitLayer, true, [3024.0, 1890.0]),
            Exactness::NotRun,
        ));
        assert_eq!(finding(&report, "EDR"), Finding::EdrMismatch);
    }

    #[test]
    fn a_ten_bit_stream_on_the_eight_bit_path_fails_presentation() {
        let host = host(
            HostPlatform::Windows,
            "",
            StreamingPreset::Grading,
            grading(),
            ServedStreamPipeline::Grading,
            grading(),
        );
        let report = assess(&input(
            host,
            deck(
                PresentationPath::EightBitFallback("pixel format refused".to_owned()),
                false,
                [3024.0, 1890.0],
            ),
            Exactness::NotRun,
        ));
        assert_eq!(finding(&report, "Presentation"), Finding::PresentFallback);
    }

    #[test]
    fn scale_distinguishes_one_to_one_whole_number_and_resampled() {
        let base = ScaleFacts {
            stream: [1512, 945],
            shown: [3024.0, 1890.0],
            backing_scale: 2.0,
            hidpi_streaming: false,
            pinned: false,
            fullscreen: false,
        };
        let doubled = scale_item(base);
        assert_eq!(doubled.finding, Finding::ScaleInteger);
        assert_eq!(doubled.fixes, vec![ColourFix::StreamAtRetinaResolution]);

        let resampled = scale_item(ScaleFacts {
            stream: [3840, 2160],
            shown: [3024.0, 1701.0],
            hidpi_streaming: true,
            pinned: true,
            ..base
        });
        assert_eq!(resampled.finding, Finding::ScaleResampled);
        assert_eq!(resampled.fixes, vec![ColourFix::EnterFullscreen]);

        let exact = scale_item(ScaleFacts {
            stream: [3024, 1890],
            shown: [3024.0, 1890.0],
            hidpi_streaming: true,
            ..base
        });
        assert_eq!(exact.finding, Finding::ScaleOneToOne);
        assert!(exact.fixes.is_empty());

        // Measured on the lab: the host served two rows more than the screen
        // has, so the picture was drawn 0.1% smaller — resampled, not 1:1.
        let almost = scale_item(ScaleFacts {
            stream: [3600, 2262],
            shown: [3596.0, 2260.0],
            hidpi_streaming: true,
            pinned: true,
            fullscreen: true,
            ..base
        });
        assert_eq!(almost.finding, Finding::ScaleResampled);
    }

    #[test]
    fn exactness_grades_exact_near_and_off_on_every_pixel() {
        assert_eq!(
            measured_item(&exact_measurement(0, 0)).finding,
            Finding::ExactExact
        );
        // Eight-bit exact but one ten-bit code off is not bit-exact.
        assert_eq!(
            measured_item(&exact_measurement(0, 1)).finding,
            Finding::ExactNear
        );
        assert_eq!(
            measured_item(&exact_measurement(2, 6)).finding,
            Finding::ExactNear
        );
        assert_eq!(
            measured_item(&exact_measurement(3, 12)).finding,
            Finding::ExactOff
        );
        // Off as desktop codes but exact to the host's BT.709 re-encoding.
        let mut reencoded = exact_measurement(16, 64);
        reencoded.reencoded = Some(ColorAccuracy {
            max_error: 1,
            mean_error: 0.1,
            pixels: 3024 * 1890,
            worst_at: (0, 0),
            outliers: 0,
            outlier_bounds: None,
        });
        assert_eq!(measured_item(&reencoded).finding, Finding::ExactReencoded);

        // Measured on the lab: re-encoded everywhere, but Windows' activation
        // watermark sits over one corner of the pattern.
        let mut watermark = exact_measurement(64, 258);
        watermark.reencoded = Some(ColorAccuracy {
            max_error: 62,
            mean_error: 0.156,
            pixels: 3600 * 2262,
            worst_at: (3424, 2055),
            outliers: 9_000,
            outlier_bounds: Some([3300, 2010, 3590, 2080]),
        });
        let item = measured_item(&watermark);
        assert_eq!(item.finding, Finding::ExactLocalised);
        assert!(item.details.iter().any(|line| line.contains("x 3300–3590")));
        assert!(
            item.details
                .iter()
                .any(|line| line.contains("bt709_reencoded"))
        );
    }

    #[test]
    fn frames_that_disagree_with_the_hello_are_a_defect() {
        let host = host(
            HostPlatform::Windows,
            "",
            StreamingPreset::Grading,
            grading(),
            ServedStreamPipeline::Grading,
            grading(),
        );
        let mut input = input(
            host,
            deck(PresentationPath::TenBitLayer, false, [3024.0, 1890.0]),
            Exactness::NotRun,
        );
        if let Some(frame) = input.frame.as_mut() {
            frame.video.chroma = ChromaSubsampling::Yuv420;
        }
        let report = assess(&input);
        assert_eq!(finding(&report, "Frame headers"), Finding::FramesDisagree);
    }

    #[test]
    fn without_a_session_the_mac_side_is_still_checked() {
        let report = assess(&ColourCheckInput {
            host: None,
            frame: None,
            deck: DeckFacts {
                display: Some(xdr_display()),
                ..DeckFacts::default()
            },
            exactness: Exactness::NotRun,
            last_close: Some(
                "host closed the connection during setup: Grading requires a hardware 10-bit \
                 4:4:4 encoder"
                    .to_owned(),
            ),
        });
        assert_eq!(finding(&report, "Session"), Finding::NotConnected);
        let session = report
            .items
            .iter()
            .find(|item| item.title == "Session")
            .unwrap();
        assert!(session.details[0].contains("Grading requires a hardware"));
        assert_eq!(finding(&report, "Display"), Finding::DisplayNotReference);
        assert_eq!(finding(&report, "HDR"), Finding::HdrNotUsed);
        assert!(report.host_label.is_none());
    }

    #[test]
    fn the_text_report_carries_every_item_its_code_and_its_fixes() {
        let host = host(
            HostPlatform::Linux,
            "x11",
            StreamingPreset::Auto,
            auto(),
            ServedStreamPipeline::Auto,
            auto(),
        );
        let report = assess(&input(
            host,
            deck(PresentationPath::EightBit, false, [3024.0, 1890.0]),
            Exactness::NotRun,
        ));
        let text = report.render_text(&ReportHeader {
            deck: "Arcen Deck 0.18.0".to_owned(),
            generated_utc: "2026-11-09 00:00:00".to_owned(),
        });
        assert!(text.starts_with("Arcen colour check (report format 1)\n"));
        assert!(text.contains("Host: pier.example.internal · linux (x11) · Pier 0.17.0"));
        for item in &report.items {
            assert!(
                text.contains(item.finding.code()),
                "{}",
                item.finding.code()
            );
        }
        assert!(text.contains("Fix: Switch to Grading and reconnect"));
        assert!(text.contains(&DOC_LINUX_DEPTH.url()));
        assert!(text.contains("The Deck never changes a host setting."));
        assert_eq!(
            report_file_name("20261109-000000"),
            "arcen-colour-check-20261109-000000.txt"
        );
    }

    #[test]
    fn utc_stamps_format_civil_time() {
        assert_eq!(
            utc_stamps(0),
            (
                "1970-01-01 00:00:00".to_owned(),
                "19700101-000000".to_owned()
            )
        );
        assert_eq!(
            utc_stamps(1_794_186_123),
            (
                "2026-11-09 01:02:03".to_owned(),
                "20261109-010203".to_owned()
            )
        );
        assert_eq!(utc_stamps(951_782_400).0, "2000-02-29 00:00:00");
    }

    #[test]
    fn every_finding_has_a_unique_code_and_a_meaning() {
        let all = Finding::ALL;
        let mut codes: Vec<_> = all.iter().map(|finding| finding.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        for &finding in all {
            assert!(finding.code().starts_with("colour."));
            assert!(!finding.meaning().is_empty());
            if matches!(finding.status(), Status::Warn | Status::Fail) {
                assert!(!finding.action().is_empty(), "{}", finding.code());
            }
        }
    }

    #[test]
    fn chroma_from_pix_fmt_reads_both_hosts_conventions_and_never_guesses() {
        assert_eq!(
            chroma_from_pix_fmt("yuv444"),
            Some(ChromaSubsampling::Yuv444)
        );
        assert_eq!(
            chroma_from_pix_fmt("yuv444p"),
            Some(ChromaSubsampling::Yuv444)
        );
        assert_eq!(
            chroma_from_pix_fmt("yuv420p"),
            Some(ChromaSubsampling::Yuv420)
        );
        assert_eq!(
            chroma_from_pix_fmt("yuv422p10le"),
            Some(ChromaSubsampling::Yuv422)
        );
        assert_eq!(
            chroma_from_pix_fmt("p010le"),
            Some(ChromaSubsampling::Yuv420)
        );
        assert_eq!(chroma_from_pix_fmt("nv12"), Some(ChromaSubsampling::Yuv420));
        assert_eq!(
            chroma_from_pix_fmt("gbrp10le"),
            Some(ChromaSubsampling::Yuv444)
        );
        assert_eq!(chroma_from_pix_fmt("completely-unknown"), None);
        assert_eq!(chroma_from_pix_fmt(""), None);
    }
}
