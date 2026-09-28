//! What a client display can show, and the one rule every host uses to
//! describe it back to its own operating system.
//!
//! A Deck reports each display's gamut and HDR headroom as its OS states them
//! ([`arcen_protocol::messages::DisplayColorMsg`]). Hosts turn that into a
//! virtual display: Windows and Linux through a synthesized EDID
//! (`arcen_outputs::edid`), macOS through its virtual display API. Keeping
//! the interpretation here means a display is HDR, or 1,600 nits, on every
//! host for the same reason.
//!
//! Luminance the OS states is used as stated. When it states none (macOS
//! exposes no luminance), the peak is derived from the headroom against
//! [`EDR_REFERENCE_WHITE_NITS`]: macOS extended dynamic range treats 1.0 as
//! 100 nits for PQ content (Core Animation's HDR10 EDR metadata defaults its
//! optical output scale to 100), so a potential headroom of 16, measured on a
//! Liquid Retina XDR panel, is its 1,600-nit peak. Values nothing states or
//! derives are left unknown, never invented.

pub use arcen_protocol::messages::{DisplayColorMsg, DisplayGamutMsg};

use crate::ColorPrimaries;

/// The luminance macOS extended dynamic range assigns to 1.0 for PQ content.
pub const EDR_REFERENCE_WHITE_NITS: f32 = 100.0;

/// The ST 2084 ceiling. Nothing a display states or derives exceeds it.
const PQ_PEAK_NITS: f32 = 10_000.0;

/// A client display's colour capability, validated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayColor {
    gamut: ColorPrimaries,
    hdr_headroom: Option<f32>,
    stated: StatedLuminance,
}

impl Eq for DisplayColor {}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct StatedLuminance {
    peak: Option<f32>,
    frame_average: Option<f32>,
    min: Option<f32>,
}

/// A luminance value and its provenance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Luminance {
    /// The client's OS or display metadata stated this value.
    Measured(f32),
    /// Derived from HDR headroom against [`EDR_REFERENCE_WHITE_NITS`].
    Derived(f32),
    /// The client did not report enough information to know this value.
    Unknown,
}

impl Luminance {
    /// Returns the value in nits when this luminance is known.
    #[must_use]
    pub const fn nits(self) -> Option<f32> {
        match self {
            Self::Measured(nits) | Self::Derived(nits) => Some(nits),
            Self::Unknown => None,
        }
    }
}

/// An HDR display's luminance, as far as it is known.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HdrLuminance {
    /// Peak luminance and whether it was measured or derived.
    pub peak: Luminance,
    /// Maximum frame-average luminance and whether it was measured or unknown.
    pub frame_average: Luminance,
    /// Minimum luminance and whether it was measured or unknown.
    pub min: Luminance,
}

impl DisplayColor {
    /// A plain SDR display with sRGB primaries: what a host assumes when the
    /// client says nothing.
    pub const SDR_SRGB: Self = Self {
        gamut: ColorPrimaries::Bt709,
        hdr_headroom: Some(1.0),
        stated: StatedLuminance {
            peak: None,
            frame_average: None,
            min: None,
        },
    };

    /// Reads a client's report, discarding any value that is not a finite,
    /// physically sensible number rather than trusting it.
    #[must_use]
    pub fn from_msg(msg: &DisplayColorMsg) -> Self {
        let nits = |value: Option<f32>| {
            value.filter(|nits| nits.is_finite() && *nits > 0.0 && *nits <= PQ_PEAK_NITS)
        };
        let headroom = Some(msg.hdr_headroom)
            .filter(|headroom| headroom.is_finite() && *headroom >= 1.0 && *headroom <= 100.0);
        let peak = nits(msg.peak_nits);
        Self {
            gamut: match msg.gamut {
                DisplayGamutMsg::Srgb => ColorPrimaries::Bt709,
                DisplayGamutMsg::DisplayP3 => ColorPrimaries::DisplayP3,
                DisplayGamutMsg::Bt2020 => ColorPrimaries::Bt2020,
            },
            hdr_headroom: headroom,
            stated: StatedLuminance {
                peak,
                frame_average: nits(msg.max_frame_average_nits)
                    .filter(|average| peak.is_none_or(|peak| *average <= peak)),
                min: nits(msg.min_nits).filter(|min| peak.is_none_or(|peak| *min < peak)),
            },
        }
    }

    /// The display's widest standard gamut.
    #[must_use]
    pub const fn gamut(self) -> ColorPrimaries {
        self.gamut
    }

    /// The HDR headroom the client measured, when it measured one.
    #[must_use]
    pub const fn hdr_headroom(self) -> Option<f32> {
        self.hdr_headroom
    }

    /// Whether the display can show more than SDR white.
    ///
    /// Only measured headroom above SDR white makes the display HDR. A stated
    /// peak belongs to the luminance description of an HDR display; by itself
    /// it can also describe a bright SDR panel and must not flip the contract.
    #[must_use]
    pub fn is_hdr(self) -> bool {
        self.hdr_headroom.is_some_and(|headroom| headroom > 1.0)
    }

    /// The display's HDR luminance, or `None` for an SDR display.
    #[must_use]
    pub fn hdr_luminance(self) -> Option<HdrLuminance> {
        if !self.is_hdr() {
            return None;
        }
        let peak = match (self.stated.peak, self.hdr_headroom) {
            (Some(peak), _) => Luminance::Measured(peak),
            (None, Some(headroom)) => {
                Luminance::Derived((headroom * EDR_REFERENCE_WHITE_NITS).min(PQ_PEAK_NITS))
            }
            (None, None) => Luminance::Unknown,
        };
        Some(HdrLuminance {
            peak,
            frame_average: self
                .stated
                .frame_average
                .map_or(Luminance::Unknown, Luminance::Measured),
            min: self
                .stated
                .min
                .map_or(Luminance::Unknown, Luminance::Measured),
        })
    }
}

/// Whether any of a client's displays can show HDR. A Deck offers HDR
/// streaming only when this is true.
#[must_use]
pub fn any_display_is_hdr<'a>(displays: impl IntoIterator<Item = &'a DisplayColorMsg>) -> bool {
    displays
        .into_iter()
        .any(|display| DisplayColor::from_msg(display).is_hdr())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(gamut: DisplayGamutMsg, hdr_headroom: f32) -> DisplayColorMsg {
        DisplayColorMsg {
            gamut,
            hdr_headroom,
            ..DisplayColorMsg::default()
        }
    }

    #[test]
    fn an_xdr_panel_reads_as_its_1600_nit_peak() {
        let xdr = DisplayColor::from_msg(&msg(DisplayGamutMsg::DisplayP3, 16.0));
        assert!(xdr.is_hdr());
        assert_eq!(xdr.gamut(), ColorPrimaries::DisplayP3);
        let luminance = xdr.hdr_luminance().expect("HDR");
        assert_eq!(luminance.peak, Luminance::Derived(1600.0));
        assert_eq!(
            (luminance.frame_average, luminance.min),
            (Luminance::Unknown, Luminance::Unknown),
            "macOS states neither, so neither is invented"
        );
    }

    #[test]
    fn an_sdr_panel_is_not_hdr_whatever_its_gamut() {
        let p3_sdr = DisplayColor::from_msg(&msg(DisplayGamutMsg::DisplayP3, 1.0));
        assert!(!p3_sdr.is_hdr());
        assert_eq!(p3_sdr.hdr_luminance(), None);
        let bright_sdr = DisplayColor::from_msg(&DisplayColorMsg {
            peak_nits: Some(200.0),
            ..msg(DisplayGamutMsg::Srgb, 1.0)
        });
        assert!(
            !bright_sdr.is_hdr(),
            "brightness below reference white is not HDR"
        );
        let bright_500_nit_sdr = DisplayColor::from_msg(&DisplayColorMsg {
            peak_nits: Some(500.0),
            ..msg(DisplayGamutMsg::Srgb, 1.0)
        });
        assert!(
            !bright_500_nit_sdr.is_hdr(),
            "a stated peak alone does not make a display HDR"
        );
        assert_eq!(bright_500_nit_sdr.hdr_luminance(), None);
    }

    #[test]
    fn stated_luminance_wins_over_the_headroom() {
        let stated = DisplayColor::from_msg(&DisplayColorMsg {
            peak_nits: Some(1000.0),
            max_frame_average_nits: Some(600.0),
            min_nits: Some(0.05),
            ..msg(DisplayGamutMsg::Bt2020, 16.0)
        });
        let luminance = stated.hdr_luminance().expect("HDR");
        assert_eq!(luminance.peak, Luminance::Measured(1000.0));
        assert_eq!(luminance.frame_average, Luminance::Measured(600.0));
        assert_eq!(luminance.min, Luminance::Measured(0.05));
    }

    #[test]
    fn nonsense_is_discarded_not_trusted() {
        let bad = DisplayColor::from_msg(&DisplayColorMsg {
            gamut: DisplayGamutMsg::Srgb,
            hdr_headroom: f32::NAN,
            peak_nits: Some(-5.0),
            max_frame_average_nits: Some(f32::INFINITY),
            min_nits: Some(20_000.0),
        });
        assert_eq!(bad.hdr_headroom(), None);
        assert!(!bad.is_hdr());
        let inconsistent = DisplayColor::from_msg(&DisplayColorMsg {
            peak_nits: Some(500.0),
            max_frame_average_nits: Some(800.0),
            min_nits: Some(600.0),
            ..msg(DisplayGamutMsg::Srgb, 0.0)
        });
        assert_eq!(
            inconsistent.hdr_luminance(),
            None,
            "a stated peak without headroom is not HDR"
        );
    }

    #[test]
    fn hdr_is_offered_when_any_display_can_show_it() {
        let sdr = msg(DisplayGamutMsg::Srgb, 1.0);
        let xdr = msg(DisplayGamutMsg::DisplayP3, 16.0);
        assert!(!any_display_is_hdr([&sdr, &sdr]));
        assert!(any_display_is_hdr([&sdr, &xdr]));
        assert!(!any_display_is_hdr(std::iter::empty()));
    }
}
