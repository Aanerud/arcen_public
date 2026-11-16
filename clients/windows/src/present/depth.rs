// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Ten bits at the glass are measured, not assumed (DRAFT §3.4).
//!
//! A ten-bit swapchain is necessary but not sufficient. DWM composes a
//! windowed swapchain at the desktop's depth unless Windows advanced colour
//! (HDR or Auto Color Management) is on or the swapchain is promoted to
//! independent flip / an overlay plane; and an 8-bit output scans out eight
//! bits whatever DWM composed. The presenter gathers these facts and the
//! Deck shows a persistent warning when the result is eight bits, like the
//! Mac's 8-bit fallback warning (`clients/macos/AGENTS.md`). Bit depth never
//! implies HDR.

use super::swapchain::PresentMode;

/// How DWM showed the last presents (`DXGI_FRAME_STATISTICS_MEDIA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositionPath {
    /// Composed by DWM into the desktop.
    Composed,
    /// Independent flip or a hardware overlay plane: DWM does not touch
    /// the pixels.
    Direct,
    /// Statistics unavailable (DWM reported them disjoint or unsupported).
    Unknown,
}

/// What the presenter knows about the path from swapchain to glass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthFacts {
    /// The mode presented (its swapchain format).
    pub mode: PresentMode,
    /// `IDXGIOutput6::GetDesc1().BitsPerColor` of the window's output.
    pub output_bits_per_color: u32,
    /// `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO`: advanced colour (HDR or
    /// ACM) enabled on the target, when known.
    pub advanced_color_enabled: Option<bool>,
    /// Composition path of the last presents.
    pub composition: CompositionPath,
}

/// Why the glass shows fewer bits than the swapchain holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthLimit {
    /// The output scans out fewer than ten bits per colour.
    OutputBits(u32),
    /// DWM composes the window at 8 bits: advanced colour is off and the
    /// swapchain is not presented directly.
    ComposedWithoutAdvancedColor,
}

/// The effective depth at the glass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveDepth {
    /// An 8-bit contract (Auto/Speed): nothing to prove.
    EightBitContract,
    /// Ten (or more) bits reach the output.
    TenBit,
    /// A wide swapchain reduced to eight bits on the way.
    ReducedToEightBit(DepthLimit),
}

impl EffectiveDepth {
    /// Whether the Deck must show the persistent warning.
    #[must_use]
    pub const fn warns(self) -> bool {
        matches!(self, Self::ReducedToEightBit(_))
    }

    /// The warning line, in the Mac's wording style.
    #[must_use]
    pub fn warning(self) -> Option<String> {
        let Self::ReducedToEightBit(limit) = self else {
            return None;
        };
        Some(match limit {
            DepthLimit::OutputBits(bits) => format!(
                "Wide-colour warning: this display shows {bits} bits per colour; the 10-bit picture is reduced to 8 bits"
            ),
            DepthLimit::ComposedWithoutAdvancedColor => {
                "Wide-colour warning: Windows composes this window at 8 bits; turn on HDR or Auto Color Management, or use full screen"
                    .to_owned()
            }
        })
    }
}

/// The effective depth for `facts`.
#[must_use]
pub fn effective_depth(facts: DepthFacts) -> EffectiveDepth {
    if facts.mode == PresentMode::Sdr8 {
        return EffectiveDepth::EightBitContract;
    }
    if facts.output_bits_per_color < 10 {
        return EffectiveDepth::ReducedToEightBit(DepthLimit::OutputBits(
            facts.output_bits_per_color,
        ));
    }
    let direct = facts.composition == CompositionPath::Direct;
    if !direct && facts.advanced_color_enabled != Some(true) {
        return EffectiveDepth::ReducedToEightBit(DepthLimit::ComposedWithoutAdvancedColor);
    }
    EffectiveDepth::TenBit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(
        mode: PresentMode,
        bits: u32,
        advanced: Option<bool>,
        path: CompositionPath,
    ) -> DepthFacts {
        DepthFacts {
            mode,
            output_bits_per_color: bits,
            advanced_color_enabled: advanced,
            composition: path,
        }
    }

    #[test]
    fn the_lab_vgpu_is_an_eight_bit_output() {
        // A virtual-GPU display: 8 bpc, advanced colour unsupported.
        let lab = facts(
            PresentMode::Grading10,
            8,
            Some(false),
            CompositionPath::Unknown,
        );
        assert_eq!(
            effective_depth(lab),
            EffectiveDepth::ReducedToEightBit(DepthLimit::OutputBits(8))
        );
        assert!(effective_depth(lab).warning().is_some());
    }

    #[test]
    fn a_ten_bit_panel_needs_advanced_colour_or_a_direct_path() {
        let composed = facts(
            PresentMode::Grading10,
            10,
            Some(false),
            CompositionPath::Composed,
        );
        assert_eq!(
            effective_depth(composed),
            EffectiveDepth::ReducedToEightBit(DepthLimit::ComposedWithoutAdvancedColor)
        );
        let acm = DepthFacts {
            advanced_color_enabled: Some(true),
            ..composed
        };
        assert_eq!(effective_depth(acm), EffectiveDepth::TenBit);
        let direct = DepthFacts {
            composition: CompositionPath::Direct,
            ..composed
        };
        assert_eq!(effective_depth(direct), EffectiveDepth::TenBit);
        let unknown = DepthFacts {
            advanced_color_enabled: None,
            composition: CompositionPath::Unknown,
            ..composed
        };
        assert!(effective_depth(unknown).warns(), "unknown is not proof");
    }

    #[test]
    fn eight_bit_contracts_never_warn() {
        let auto = facts(PresentMode::Sdr8, 8, Some(false), CompositionPath::Composed);
        assert_eq!(effective_depth(auto), EffectiveDepth::EightBitContract);
        assert!(!effective_depth(auto).warns());
    }
}
