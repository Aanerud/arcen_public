//! Whether a colour-managed compositor can serve Grading and HDR, decided
//! from facts.
//!
//! A Wayland compositor owns the desktop's composition. Grading (ten-bit
//! BT.709 SDR) and HDR (PQ / BT.2020) are possible only when it composes in
//! a wide colour state *and* hands the screencast consumer that composition
//! in a wide format, at the precision it claims. Each is a fact about the
//! host stack, gathered by a platform adapter and decided here once:
//!
//! 1. A **route** to a wide composition on the captured outputs
//!    ([`fidelity_route`]): the output offers an HDR colour mode Arcen can
//!    select ([`FidelityRoute::ColourMode`]), or the compositor is known to
//!    screencast HDR once its colour state is forced
//!    ([`FidelityRoute::ForcedHdr`], Mutter 50 and later, where HDR screen
//!    sharing arrived with
//!    [mutter!4928](https://gitlab.gnome.org/GNOME/mutter/-/merge_requests/4928)).
//! 2. A **proof** that the route delivers wide content
//!    ([`precision_from_ramp`]): a ten-bit ramp drawn into the composition
//!    comes back through the real capture and import with (nearly) every
//!    code, not 256 of them. A compositor that paints eight-bit pixels into
//!    a ten-bit buffer (Mutter's shared-memory screencast does, through 51)
//!    fails it.
//! 3. The measured **SDR white** of the composition
//!    ([`super::pq_white::is_graphics_white`]).
//!
//! Anything missing is a [`FidelityGap`]: a visible, specific reason the
//! preset is served as something else, never a silent fallback.

use std::fmt::{Display, Formatter};

use super::capture_surface::SamplePrecision;

/// First Mutter major version whose `ScreenCast` streams advertise ten-bit and
/// half-float formats tagged BT.2020 / PQ for a monitor in an HDR colour
/// state (mutter!4928, GNOME 50, merged 2026-02-27). Mutter 49 and older
/// offer eight-bit `BGRx`/`BGRA` only, in any colour state.
pub const MUTTER_HDR_SCREENCAST_MAJOR: u32 = 50;

/// A compositor and its version, as the session reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorRelease {
    /// GNOME's Mutter (or GNOME Shell, which embeds it), `major.minor`.
    Mutter {
        /// Major version.
        major: u32,
        /// Minor version.
        minor: u32,
    },
    /// Another compositor, or a version that could not be read.
    Other,
}

impl CompositorRelease {
    /// Parses `mutter 49.4`, `GNOME Shell 50.1`, `50.rc` or `51.0` style
    /// version strings; anything else is [`Self::Other`].
    #[must_use]
    pub fn parse_mutter(text: &str) -> Self {
        let Some(version) = text
            .split_whitespace()
            .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
        else {
            return Self::Other;
        };
        let mut parts = version.split('.');
        let Some(Ok(major)) = parts.next().map(str::parse::<u32>) else {
            return Self::Other;
        };
        let minor = parts
            .next()
            .and_then(|minor| minor.parse::<u32>().ok())
            .unwrap_or(0);
        Self::Mutter { major, minor }
    }

    /// Whether this compositor screencasts its HDR composition in a wide
    /// format.
    #[must_use]
    pub const fn screencasts_hdr(self) -> bool {
        match self {
            Self::Mutter { major, .. } => major >= MUTTER_HDR_SCREENCAST_MAJOR,
            Self::Other => false,
        }
    }
}

impl Display for CompositorRelease {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mutter { major, minor } => write!(formatter, "Mutter {major}.{minor}"),
            Self::Other => formatter.write_str("an unidentified compositor"),
        }
    }
}

/// How a session reaches a wide composition on its captured outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FidelityRoute {
    /// Every captured output offers an HDR (BT.2100) colour mode, selected
    /// per output through the compositor's display configuration.
    ColourMode,
    /// The compositor's colour state is forced to BT.2100 for the session
    /// (Mutter's `MUTTER_DEBUG_FORCE_HDR`, set only in the environment of
    /// the session's own compositor). All outputs together; a debug
    /// interface, used only when qualification proves it on this exact
    /// stack.
    ForcedHdr,
}

impl FidelityRoute {
    /// Every route, in order of preference.
    pub const ALL: &'static [Self] = &[Self::ColourMode, Self::ForcedHdr];

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::ColourMode => "colour-mode",
            Self::ForcedHdr => "force-hdr",
        }
    }

    /// Parses [`Self::token`].
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|route| route.token() == value)
    }
}

/// Colour facts of a compositor session, before any capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositorColourFacts {
    /// The compositor and version.
    pub compositor: CompositorRelease,
    /// The compositor implements colour management at all (a colour-mode
    /// property in its display configuration).
    pub colour_management: bool,
    /// Every captured output lists a BT.2100 colour mode.
    pub outputs_offer_bt2100: bool,
}

/// Why Grading and HDR are not available on a host or in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FidelityGap {
    /// The compositor has no colour management (GNOME 40 and older).
    NoColourManagement,
    /// The compositor composes HDR but never screencasts it in a wide format
    /// (Mutter before 50).
    NoHdrScreencast(CompositorRelease),
    /// No captured output offers an HDR colour mode and the compositor has
    /// no forced-HDR route.
    NoHdrRoute,
    /// The route was taken, but the stream offered no wide format tagged
    /// BT.2020 / PQ.
    WideFormatsNotOffered,
    /// No GPU import or read-back of the wide buffers was proven.
    WideImportUnproven,
    /// The wide samples were not proven to carry wide content.
    PrecisionUnproven,
    /// The wide samples carry eight-bit content.
    UpconvertedEightBit,
    /// The composition's SDR white was not measured at BT.2408's 203 nits.
    ReferenceWhiteUnproven,
    /// The host has not been qualified for Grading or HDR.
    NotQualified,
}

impl FidelityGap {
    /// Every gap without a payload, for token parsing.
    const TOKENED: &'static [Self] = &[
        Self::NoColourManagement,
        Self::NoHdrRoute,
        Self::WideFormatsNotOffered,
        Self::WideImportUnproven,
        Self::PrecisionUnproven,
        Self::UpconvertedEightBit,
        Self::ReferenceWhiteUnproven,
        Self::NotQualified,
    ];

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::NoColourManagement => "compositor_lacks_colour_management",
            Self::NoHdrScreencast(_) => "compositor_lacks_hdr_screencast",
            Self::NoHdrRoute => "no_hdr_route",
            Self::WideFormatsNotOffered => "wide_formats_not_offered",
            Self::WideImportUnproven => "wide_import_unproven",
            Self::PrecisionUnproven => "wide_precision_unproven",
            Self::UpconvertedEightBit => "upconverted_eight_bit_content",
            Self::ReferenceWhiteUnproven => "reference_white_unproven",
            Self::NotQualified => "fidelity_not_qualified",
        }
    }

    /// Parses [`Self::token`]; a compositor named by
    /// [`Self::NoHdrScreencast`] is not carried by the token.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        if value == "compositor_lacks_hdr_screencast" {
            return Some(Self::NoHdrScreencast(CompositorRelease::Other));
        }
        Self::TOKENED
            .iter()
            .copied()
            .find(|gap| gap.token() == value)
    }
}

impl Display for FidelityGap {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoColourManagement => {
                formatter.write_str("the compositor has no colour management")
            }
            Self::NoHdrScreencast(compositor) => write!(
                formatter,
                "{compositor} has no HDR screen sharing; it arrived in Mutter \
                 {MUTTER_HDR_SCREENCAST_MAJOR} (mutter!4928)"
            ),
            Self::NoHdrRoute => formatter.write_str(
                "no captured output offers an HDR colour mode and the compositor has no \
                 forced-HDR route",
            ),
            Self::WideFormatsNotOffered => formatter
                .write_str("the screencast offered no ten-bit or half-float BT.2020/PQ format"),
            Self::WideImportUnproven => {
                formatter.write_str("no import of the wide screencast buffers was proven")
            }
            Self::PrecisionUnproven => {
                formatter.write_str("the wide samples were not proven to carry wide content")
            }
            Self::UpconvertedEightBit => {
                formatter.write_str("the wide samples carry eight-bit content")
            }
            Self::ReferenceWhiteUnproven => {
                formatter.write_str("the composition's SDR white was not measured at 203 nits")
            }
            Self::NotQualified => {
                formatter.write_str("the host has not been qualified for Grading or HDR")
            }
        }
    }
}

/// The route to a wide composition, from compositor facts alone; proofs
/// come later and can still refuse it.
///
/// # Errors
///
/// The [`FidelityGap`] that rules every route out.
pub fn fidelity_route(facts: CompositorColourFacts) -> Result<FidelityRoute, FidelityGap> {
    if !facts.colour_management {
        return Err(FidelityGap::NoColourManagement);
    }
    if !facts.compositor.screencasts_hdr() {
        return Err(match facts.compositor {
            CompositorRelease::Mutter { .. } => FidelityGap::NoHdrScreencast(facts.compositor),
            CompositorRelease::Other => FidelityGap::NoHdrRoute,
        });
    }
    if facts.outputs_offer_bt2100 {
        return Ok(FidelityRoute::ColourMode);
    }
    match facts.compositor {
        CompositorRelease::Mutter { .. } => Ok(FidelityRoute::ForcedHdr),
        CompositorRelease::Other => Err(FidelityGap::NoHdrRoute),
    }
}

/// Smallest share of a ramp's codes, in percent, a capture must return for
/// its samples to count as natively wide. Measured on stock Mutter 50.6 with
/// a forced HDR colour state: a ten-bit PQ ramp came back through a ten-bit
/// DMA-BUF with all 1024 codes, and through a half-float DMA-BUF (PQ decoded
/// to linear, blended and re-encoded) with 986, every code within one of the
/// drawn value; eight-bit content has at most 256.
pub const NATIVE_WIDE_MIN_PERCENT: u32 = 90;

/// A ramp with this many distinct codes or fewer is eight-bit content,
/// whatever its container.
pub const EIGHT_BIT_CODES: u32 = 256;

/// One measurement of a known ramp drawn into the composition and read back
/// through the real capture and import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RampMeasurement {
    /// Distinct codes the ramp was drawn with.
    pub expected_codes: u32,
    /// Distinct codes read back.
    pub distinct_codes: u32,
    /// Largest difference, in codes, between a drawn and a read-back step.
    pub max_error_codes: u32,
}

/// The ten-bit code a ramp drawn across `width` pixels holds at `index`:
/// `index * 1023 / (width - 1)`, rounded down. The qualification's ramp
/// client draws exactly this, so a capture can be compared with it.
#[must_use]
pub fn expected_ramp_code(index: usize, width: usize) -> u16 {
    if width < 2 {
        return 0;
    }
    let code = index.min(width - 1) * 1023 / (width - 1);
    u16::try_from(code).unwrap_or(1023)
}

/// Measures one captured row of a drawn ramp: the ten-bit codes read back,
/// pixel by pixel, against [`expected_ramp_code`].
#[must_use]
pub fn measure_ramp(codes: &[u16]) -> RampMeasurement {
    let width = codes.len();
    let distinct = |values: &mut dyn Iterator<Item = u16>| {
        u32::try_from(values.collect::<std::collections::BTreeSet<u16>>().len()).unwrap_or(0)
    };
    let max_error_codes = codes
        .iter()
        .enumerate()
        .map(|(index, code)| u32::from(code.abs_diff(expected_ramp_code(index, width))))
        .max()
        .unwrap_or(0);
    RampMeasurement {
        expected_codes: distinct(&mut (0..width).map(|index| expected_ramp_code(index, width))),
        distinct_codes: distinct(&mut codes.iter().copied()),
        max_error_codes,
    }
}

/// The precision a ramp measurement proves, or `None` when it proves
/// neither (too few codes for wide, too many for eight-bit, or wrong
/// values).
#[must_use]
pub fn precision_from_ramp(measurement: RampMeasurement) -> Option<SamplePrecision> {
    if measurement.expected_codes <= EIGHT_BIT_CODES {
        return None;
    }
    if measurement.distinct_codes <= EIGHT_BIT_CODES {
        return Some(SamplePrecision::UpconvertedEightBit);
    }
    let enough = u64::from(measurement.distinct_codes) * 100
        >= u64::from(measurement.expected_codes) * u64::from(NATIVE_WIDE_MIN_PERCENT);
    (enough && measurement.max_error_codes <= 1).then_some(SamplePrecision::NativeWide)
}

/// The colour the qualification picture's channel patch holds, as ten-bit
/// red, green and blue codes: three distinct values, so a capture that
/// swaps or mislabels channels cannot pass the way a grey ramp would.
pub const CHANNEL_PROBE_CODES: [u16; 3] = [900, 520, 140];

/// Where the qualification picture's pieces are, for a `width` × `height`
/// picture: the ramp across the top half, a white patch across the left
/// three quarters of the bottom half and the channel patch in its right
/// quarter. The ramp client draws it and capenc measures it from the same
/// definition.
#[must_use]
pub fn qualification_picture_code(x: usize, y: usize, width: usize, height: usize) -> [u16; 3] {
    if y < height / 2 {
        let code = expected_ramp_code(x, width);
        [code; 3]
    } else if x >= width - width / 4 {
        CHANNEL_PROBE_CODES
    } else {
        [1023; 3]
    }
}

/// The row and column to read the white patch at.
#[must_use]
pub const fn white_patch_at(width: usize, height: usize) -> (usize, usize) {
    (height * 3 / 4, width * 3 / 8)
}

/// The row and column to read the channel patch at.
#[must_use]
pub const fn channel_patch_at(width: usize, height: usize) -> (usize, usize) {
    (height * 3 / 4, width - width / 8)
}

/// Whether red, green and blue read back from the channel patch are the
/// drawn codes, each within one code: the capture keeps channel order.
#[must_use]
pub fn channel_order_verified(read: [u16; 3]) -> bool {
    read.iter()
        .zip(CHANNEL_PROBE_CODES)
        .all(|(read, drawn)| read.abs_diff(drawn) <= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutter(major: u32, minor: u32) -> CompositorRelease {
        CompositorRelease::Mutter { major, minor }
    }

    fn facts(compositor: CompositorRelease, bt2100: bool) -> CompositorColourFacts {
        CompositorColourFacts {
            compositor,
            colour_management: true,
            outputs_offer_bt2100: bt2100,
        }
    }

    #[test]
    fn mutter_versions_parse_from_both_binaries() {
        assert_eq!(
            CompositorRelease::parse_mutter("mutter 49.4"),
            mutter(49, 4)
        );
        assert_eq!(
            CompositorRelease::parse_mutter("GNOME Shell 50.1\n"),
            mutter(50, 1)
        );
        assert_eq!(CompositorRelease::parse_mutter("51.rc"), mutter(51, 0));
        assert_eq!(
            CompositorRelease::parse_mutter("mutter"),
            CompositorRelease::Other
        );
    }

    #[test]
    fn mutter_49_has_no_hdr_screen_sharing_and_says_which_version_adds_it() {
        let gap = fidelity_route(facts(mutter(49, 4), false)).unwrap_err();
        assert_eq!(gap, FidelityGap::NoHdrScreencast(mutter(49, 4)));
        let text = gap.to_string();
        assert!(text.contains("Mutter 49.4"), "{text}");
        assert!(text.contains("Mutter 50"), "{text}");
        assert!(text.contains("mutter!4928"), "{text}");
        // An output colour mode does not help a compositor that cannot cast it.
        assert_eq!(
            fidelity_route(facts(mutter(49, 4), true)),
            Err(FidelityGap::NoHdrScreencast(mutter(49, 4)))
        );
    }

    #[test]
    fn gnome_40_has_no_colour_management() {
        let gnome40 = CompositorColourFacts {
            colour_management: false,
            ..facts(mutter(40, 10), false)
        };
        assert_eq!(
            fidelity_route(gnome40),
            Err(FidelityGap::NoColourManagement)
        );
    }

    #[test]
    fn mutter_50_prefers_an_output_colour_mode_over_forcing_hdr() {
        assert_eq!(
            fidelity_route(facts(mutter(50, 6), true)),
            Ok(FidelityRoute::ColourMode)
        );
        assert_eq!(
            fidelity_route(facts(mutter(51, 1), false)),
            Ok(FidelityRoute::ForcedHdr)
        );
        assert_eq!(
            fidelity_route(facts(CompositorRelease::Other, true)),
            Err(FidelityGap::NoHdrRoute)
        );
    }

    #[test]
    fn tokens_round_trip() {
        for route in FidelityRoute::ALL {
            assert_eq!(FidelityRoute::from_token(route.token()), Some(*route));
        }
        for gap in FidelityGap::TOKENED {
            assert_eq!(FidelityGap::from_token(gap.token()), Some(*gap));
        }
        assert_eq!(
            FidelityGap::from_token("compositor_lacks_hdr_screencast"),
            Some(FidelityGap::NoHdrScreencast(CompositorRelease::Other))
        );
    }

    #[test]
    fn a_captured_ramp_row_is_measured_against_the_drawn_ramp() {
        let width = 1920;
        let exact: Vec<u16> = (0..width).map(|x| expected_ramp_code(x, width)).collect();
        let measured = measure_ramp(&exact);
        assert_eq!(
            measured,
            RampMeasurement {
                expected_codes: 1024,
                distinct_codes: 1024,
                max_error_codes: 0,
            }
        );
        assert_eq!(
            precision_from_ramp(measured),
            Some(SamplePrecision::NativeWide)
        );
        // The same ramp through an eight-bit composition.
        let eight: Vec<u16> = exact
            .iter()
            .map(|code| u16::try_from(u32::from(code >> 2) * 1023 / 255).unwrap_or(1023))
            .collect();
        assert_eq!(
            precision_from_ramp(measure_ramp(&eight)),
            Some(SamplePrecision::UpconvertedEightBit)
        );
        assert_eq!(expected_ramp_code(0, 1), 0);
        assert_eq!(expected_ramp_code(5000, 1920), 1023);
    }

    #[test]
    fn a_ramp_proves_native_wide_only_with_nearly_every_code_and_right_values() {
        let ramp = |distinct, error| RampMeasurement {
            expected_codes: 1024,
            distinct_codes: distinct,
            max_error_codes: error,
        };
        assert_eq!(
            precision_from_ramp(ramp(1024, 0)),
            Some(SamplePrecision::NativeWide)
        );
        assert_eq!(
            precision_from_ramp(ramp(1000, 1)),
            Some(SamplePrecision::NativeWide)
        );
        // Mutter's shared-memory defect: 256 levels in a ten-bit container.
        assert_eq!(
            precision_from_ramp(ramp(256, 3)),
            Some(SamplePrecision::UpconvertedEightBit)
        );
        assert_eq!(
            precision_from_ramp(ramp(986, 1)),
            Some(SamplePrecision::NativeWide)
        );
        assert_eq!(precision_from_ramp(ramp(900, 1)), None);
        assert_eq!(precision_from_ramp(ramp(700, 1)), None);
        assert_eq!(precision_from_ramp(ramp(1024, 4)), None);
        assert_eq!(
            precision_from_ramp(RampMeasurement {
                expected_codes: 256,
                distinct_codes: 256,
                max_error_codes: 0,
            }),
            None
        );
    }

    #[test]
    fn the_qualification_picture_has_a_ramp_white_and_a_channel_patch() {
        let (width, height) = (1920, 1080);
        assert_eq!(
            qualification_picture_code(960, 10, width, height),
            [expected_ramp_code(960, width); 3]
        );
        let (row, column) = white_patch_at(width, height);
        assert_eq!(
            qualification_picture_code(column, row, width, height),
            [1023; 3]
        );
        let (row, column) = channel_patch_at(width, height);
        assert_eq!(
            qualification_picture_code(column, row, width, height),
            CHANNEL_PROBE_CODES
        );
        assert!(channel_order_verified([900, 521, 139]));
        // Red and blue swapped, or a grey read-back: refused.
        assert!(!channel_order_verified([140, 520, 900]));
        assert!(!channel_order_verified([520, 520, 520]));
    }
}
