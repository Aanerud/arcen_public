//! The colour depth of a desktop whose depth is fixed when it is created.
//!
//! An X11 server has one depth for every client, chosen when it starts, and
//! it cannot change while the desktop's applications run. A ten-bit (depth
//! 30) desktop is what makes a genuine ten-bit capture possible, but it costs
//! the applications inside it: Firefox refuses EGL on any depth but 24 and
//! its GLX fallback then fails, and Chromium's ANGLE finds no 8-bit-alpha
//! config, so both render in software. An eight-bit (depth 24) desktop gives
//! every application its GPU but can only ever yield eight-bit samples.
//!
//! So the depth is a decision about the desktop, made once from the session
//! that creates it ([`choose_desktop_depth`]), and every later attachment is
//! served from what that desktop can actually supply
//! ([`serve_on_fixed_depth`]): never eight-bit samples in a ten-bit
//! container, and never a desktop destroyed to change it.

use crate::BitDepth;
use crate::video::{
    AdmissionReason, DesktopSignalEncoding, PipelineId, ResolvedHostInitialVideo, pipeline_contract,
};

/// The depth a fixed-depth desktop runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DesktopDepth {
    /// Eight bits per channel (X depth 24).
    Eight,
    /// Ten bits per channel (X depth 30).
    Ten,
}

impl DesktopDepth {
    pub const ALL: &'static [Self] = &[Self::Eight, Self::Ten];

    /// The X11 screen depth, which is also the configuration token.
    #[must_use]
    pub const fn x11_depth(self) -> u8 {
        match self {
            Self::Eight => 24,
            Self::Ten => 30,
        }
    }

    /// Stable token: the X11 screen depth.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Eight => "24",
            Self::Ten => "30",
        }
    }

    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|depth| depth.token() == value)
    }

    /// The depth an X server reported for its root window; `None` for any
    /// depth this module does not serve from.
    #[must_use]
    pub const fn from_x11_depth(depth: u8) -> Option<Self> {
        match depth {
            24 => Some(Self::Eight),
            30 => Some(Self::Ten),
            _ => None,
        }
    }

    /// The deepest coded component depth this desktop can supply honestly.
    #[must_use]
    pub const fn max_bit_depth(self) -> BitDepth {
        match self {
            Self::Eight => BitDepth::Eight,
            Self::Ten => BitDepth::Ten,
        }
    }
}

/// The operator's `video.x11_desktop_depth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesktopDepthPolicy {
    /// Ten bits only for a desktop created for ten-bit work.
    #[default]
    Auto,
    /// Every new desktop at this depth.
    Pinned(DesktopDepth),
}

impl DesktopDepthPolicy {
    pub const TOKENS: &'static [&'static str] = &["auto", "24", "30"];

    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pinned(depth) => depth.token(),
        }
    }

    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        if value == "auto" {
            return Some(Self::Auto);
        }
        DesktopDepth::from_token(value).map(Self::Pinned)
    }
}

/// Why a new desktop got its depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthReason {
    /// The creating session resolved a ten-bit plan.
    TenBitSession,
    /// The operator declared that a colour-managed application writes
    /// Rec.2100 PQ into the desktop.
    PqDesktop,
    /// `video.x11_desktop_depth` pins the depth.
    OperatorPin,
    /// The creating session is eight-bit, so its applications keep the GPU.
    EightBitSession,
}

impl DepthReason {
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::TenBitSession => "ten_bit_session",
            Self::PqDesktop => "pq_desktop",
            Self::OperatorPin => "operator_pin",
            Self::EightBitSession => "eight_bit_session",
        }
    }
}

/// The depth chosen for a desktop being created, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthDecision {
    pub depth: DesktopDepth,
    pub reason: DepthReason,
}

/// The depth for a desktop being created by a session that resolved
/// `session_bit_depth` against a desktop the operator declared as
/// `desktop_encoding`.
///
/// Bit depth, not transfer, decides: Grading is ten-bit SDR and needs a
/// ten-bit source exactly as HDR does.
#[must_use]
pub fn choose_desktop_depth(
    policy: DesktopDepthPolicy,
    session_bit_depth: BitDepth,
    desktop_encoding: DesktopSignalEncoding,
) -> DepthDecision {
    let (depth, reason) = match policy {
        DesktopDepthPolicy::Pinned(depth) => (depth, DepthReason::OperatorPin),
        DesktopDepthPolicy::Auto if desktop_encoding == DesktopSignalEncoding::Rec2100Pq => {
            (DesktopDepth::Ten, DepthReason::PqDesktop)
        }
        DesktopDepthPolicy::Auto if session_bit_depth != BitDepth::Eight => {
            (DesktopDepth::Ten, DepthReason::TenBitSession)
        }
        DesktopDepthPolicy::Auto => (DesktopDepth::Eight, DepthReason::EightBitSession),
    };
    DepthDecision { depth, reason }
}

/// What an attachment to an existing fixed-depth desktop is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FixedDepthOutcome {
    /// The desktop can supply what the session resolved.
    Served(ResolvedHostInitialVideo),
    /// The session asked for more bits than the desktop has; it is served
    /// visibly as Auto instead.
    Downgraded {
        served: ResolvedHostInitialVideo,
        requested: Option<PipelineId>,
        reason: AdmissionReason,
    },
}

impl FixedDepthOutcome {
    /// The plan to serve.
    #[must_use]
    pub fn resolved(&self) -> &ResolvedHostInitialVideo {
        match self {
            Self::Served(resolved)
            | Self::Downgraded {
                served: resolved, ..
            } => resolved,
        }
    }

    #[must_use]
    pub fn into_resolved(self) -> ResolvedHostInitialVideo {
        match self {
            Self::Served(resolved)
            | Self::Downgraded {
                served: resolved, ..
            } => resolved,
        }
    }
}

/// Serves `resolved` from a desktop fixed at `depth`.
///
/// A ten-bit desktop serves every plan: its eight-bit sessions take the
/// eight-bit capture path as they would anywhere. An eight-bit desktop
/// serves every eight-bit plan, and turns a deeper one (Grading, HDR, or
/// custom ten-bit axes) into Auto with
/// [`AdmissionReason::EightBitSource`], keeping the codec it resolved.
#[must_use]
pub fn serve_on_fixed_depth(
    depth: DesktopDepth,
    resolved: ResolvedHostInitialVideo,
) -> FixedDepthOutcome {
    if resolved.video.bit_depth <= depth.max_bit_depth() {
        return FixedDepthOutcome::Served(resolved);
    }
    let contract = pipeline_contract(PipelineId::Auto);
    let served = ResolvedHostInitialVideo {
        video: crate::VideoConfiguration {
            codec: resolved.video.codec,
            ..contract.colour
        },
        selection: contract.selection,
        pipeline: Some(PipelineId::Auto),
        encode_intent: contract.intent,
        max_fps: resolved.max_fps.min(contract.max_fps),
    };
    FixedDepthOutcome::Downgraded {
        served,
        requested: resolved.pipeline,
        reason: AdmissionReason::EightBitSource,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EncodeIntent, VideoCodec};

    fn resolved(pipeline: PipelineId) -> ResolvedHostInitialVideo {
        let contract = pipeline_contract(pipeline);
        ResolvedHostInitialVideo {
            video: crate::VideoConfiguration {
                codec: VideoCodec::H265,
                ..contract.colour
            },
            selection: contract.selection,
            pipeline: Some(pipeline),
            encode_intent: contract.intent,
            max_fps: contract.max_fps,
        }
    }

    #[test]
    fn tokens_round_trip_and_name_the_x11_depth() {
        for depth in DesktopDepth::ALL {
            assert_eq!(DesktopDepth::from_token(depth.token()), Some(*depth));
            assert_eq!(depth.token(), depth.x11_depth().to_string());
            assert_eq!(
                DesktopDepth::from_x11_depth(depth.x11_depth()),
                Some(*depth)
            );
        }
        assert_eq!(DesktopDepth::from_x11_depth(16), None);
        assert_eq!(DesktopDepth::from_x11_depth(32), None);
        for token in DesktopDepthPolicy::TOKENS {
            let policy = DesktopDepthPolicy::from_token(token).expect("documented token parses");
            assert_eq!(policy.token(), *token);
        }
        assert_eq!(DesktopDepthPolicy::from_token("10"), None);
        assert_eq!(DesktopDepthPolicy::from_token("Auto"), None);
        assert_eq!(DesktopDepthPolicy::default(), DesktopDepthPolicy::Auto);
    }

    #[test]
    fn auto_gives_ten_bits_only_to_a_ten_bit_or_pq_desktop() {
        let sdr = DesktopSignalEncoding::Sdr;
        let pq = DesktopSignalEncoding::Rec2100Pq;
        let auto = DesktopDepthPolicy::Auto;
        assert_eq!(
            choose_desktop_depth(auto, BitDepth::Eight, sdr),
            DepthDecision {
                depth: DesktopDepth::Eight,
                reason: DepthReason::EightBitSession
            }
        );
        assert_eq!(
            choose_desktop_depth(auto, BitDepth::Ten, sdr),
            DepthDecision {
                depth: DesktopDepth::Ten,
                reason: DepthReason::TenBitSession
            }
        );
        assert_eq!(
            choose_desktop_depth(auto, BitDepth::Twelve, sdr).depth,
            DesktopDepth::Ten
        );
        for bits in [BitDepth::Eight, BitDepth::Ten] {
            assert_eq!(
                choose_desktop_depth(auto, bits, pq),
                DepthDecision {
                    depth: DesktopDepth::Ten,
                    reason: DepthReason::PqDesktop
                }
            );
        }
    }

    #[test]
    fn a_pin_wins_over_the_session_and_the_declared_encoding() {
        for depth in DesktopDepth::ALL {
            for bits in [BitDepth::Eight, BitDepth::Ten] {
                for encoding in DesktopSignalEncoding::ALL {
                    assert_eq!(
                        choose_desktop_depth(DesktopDepthPolicy::Pinned(*depth), bits, *encoding),
                        DepthDecision {
                            depth: *depth,
                            reason: DepthReason::OperatorPin
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn a_ten_bit_desktop_serves_every_preset_unchanged() {
        for pipeline in [
            PipelineId::Auto,
            PipelineId::Speed,
            PipelineId::Grading,
            PipelineId::Hdr,
        ] {
            let plan = resolved(pipeline);
            assert_eq!(
                serve_on_fixed_depth(DesktopDepth::Ten, plan),
                FixedDepthOutcome::Served(plan)
            );
        }
    }

    #[test]
    fn an_eight_bit_desktop_serves_eight_bit_presets_unchanged() {
        for pipeline in [PipelineId::Auto, PipelineId::Speed] {
            let plan = resolved(pipeline);
            assert_eq!(
                serve_on_fixed_depth(DesktopDepth::Eight, plan),
                FixedDepthOutcome::Served(plan)
            );
        }
    }

    #[test]
    fn an_eight_bit_desktop_serves_ten_bit_presets_visibly_as_auto() {
        for pipeline in [PipelineId::Grading, PipelineId::Hdr] {
            let plan = resolved(pipeline);
            let outcome = serve_on_fixed_depth(DesktopDepth::Eight, plan);
            let FixedDepthOutcome::Downgraded {
                served,
                requested,
                reason,
            } = outcome
            else {
                panic!("{pipeline:?} on an eight-bit desktop must be downgraded");
            };
            assert_eq!(requested, Some(pipeline));
            assert_eq!(reason, AdmissionReason::EightBitSource);
            assert_eq!(served.pipeline, Some(PipelineId::Auto));
            assert_eq!(served.video.bit_depth, BitDepth::Eight);
            assert_eq!(
                served.video.codec,
                VideoCodec::H265,
                "the resolved codec is kept"
            );
            assert_eq!(
                served.encode_intent,
                pipeline_contract(PipelineId::Auto).intent
            );
            assert!(served.max_fps <= pipeline_contract(PipelineId::Auto).max_fps);
        }
    }

    #[test]
    fn custom_ten_bit_axes_on_an_eight_bit_desktop_are_also_served_as_auto() {
        let mut plan = resolved(PipelineId::Grading);
        plan.pipeline = None;
        plan.encode_intent = EncodeIntent::default();
        let outcome = serve_on_fixed_depth(DesktopDepth::Eight, plan);
        assert!(matches!(
            outcome,
            FixedDepthOutcome::Downgraded {
                requested: None,
                ..
            }
        ));
        assert_eq!(outcome.resolved().video.bit_depth, BitDepth::Eight);
    }
}
