//! The Deck platform profile: which kind of Mac a Deck build runs on, and the
//! decisions that genuinely differ between Apple silicon and Intel.
//!
//! Apple silicon and Intel Macs get separate Deck apps built from one source.
//! The Deck gathers three facts once at start-up -- the architecture it was
//! compiled for, the macOS version it is running on, and whether Rosetta is
//! translating it -- and asks this profile every question whose answer depends
//! on them. Nothing here calls an operating-system API, so each decision is a
//! pure function of those facts and is tested on every platform.
//!
//! What deliberately is *not* switched on the architecture: frame pacing and
//! presentation confirmation. The two timing problems first seen on an Intel
//! Mac with AMD graphics on macOS 12 (a display-link output time running about
//! 49 ms ahead of "now", and drawables that never report `presentedTime`) are
//! handled from runtime evidence in [`crate::presentation`], because nothing
//! guarantees they are confined to one architecture or one macOS release.

use std::fmt::{Display, Formatter};

/// First macOS major version with `SMAppService`, which installs the Hard USB
/// (native tablet) helper.
pub const FIRST_MACOS_WITH_SM_APP_SERVICE: u32 = 13;

/// First macOS major version whose `VideoToolbox` decodes AV1. The only Macs
/// with an AV1 decoder (Apple M3 and later) shipped with it.
pub const FIRST_MACOS_WITH_AV1_DECODE: u32 = 14;

/// The CPU architecture a Deck build was compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckArchitecture {
    /// `aarch64-apple-darwin`: the `Arcen Deck` app.
    AppleSilicon,
    /// `x86_64-apple-darwin`: the `Arcen Deck Intel` app.
    Intel,
}

impl DeckArchitecture {
    /// Maps a Rust `target_arch` (or a Mach-O architecture name) to a Deck
    /// architecture. `None` for anything a Deck is not built for.
    #[must_use]
    pub fn from_target_arch(arch: &str) -> Option<Self> {
        match arch {
            "aarch64" | "arm64" => Some(Self::AppleSilicon),
            "x86_64" => Some(Self::Intel),
            _ => None,
        }
    }

    /// The Mach-O architecture name, as `lipo -archs` prints it.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::AppleSilicon => "arm64",
            Self::Intel => "x86_64",
        }
    }
}

/// A macOS release, as `NSProcessInfo.operatingSystemVersion` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MacOsVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl MacOsVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Builds a version from `NSOperatingSystemVersion`'s signed fields. A
    /// negative component, which the API never returns, reads as zero.
    #[must_use]
    pub fn from_reported(major: isize, minor: isize, patch: isize) -> Self {
        let component = |value: isize| u32::try_from(value).unwrap_or(0);
        Self::new(component(major), component(minor), component(patch))
    }
}

impl Display for MacOsVersion {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// How a decoder's hardware-acceleration answer reads on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DecodePathAssessment {
    /// `VideoToolbox` reported a hardware decoder.
    Hardware,
    /// Software decode where it is the normal outcome: an Intel Mac whose GPU
    /// has no decoder for the stream.
    ExpectedSoftware,
    /// Software decode on Apple silicon, whose media engine decodes every
    /// codec the Deck advertises there. Still accepted -- the frames are
    /// correct -- but worth a visible note in diagnostics.
    UnexpectedSoftware,
    /// No decoder yet, or `VideoToolbox` did not answer.
    Unknown,
}

impl DecodePathAssessment {
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::ExpectedSoftware => "software-expected",
            Self::UnexpectedSoftware => "software-unexpected",
            Self::Unknown => "unknown",
        }
    }
}

/// The one platform profile of a running Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeckPlatform {
    architecture: DeckArchitecture,
    macos: MacOsVersion,
    translated: bool,
}

impl DeckPlatform {
    /// `translated` is true when Rosetta runs an Intel build on Apple
    /// silicon. It never changes a decision below; the Intel app keeps the
    /// Intel answers, which are the conservative ones.
    #[must_use]
    pub const fn new(
        architecture: DeckArchitecture,
        macos: MacOsVersion,
        translated: bool,
    ) -> Self {
        Self {
            architecture,
            macos,
            translated,
        }
    }

    #[must_use]
    pub const fn architecture(self) -> DeckArchitecture {
        self.architecture
    }

    #[must_use]
    pub const fn macos(self) -> MacOsVersion {
        self.macos
    }

    #[must_use]
    pub const fn translated(self) -> bool {
        self.translated
    }

    /// The `deck_platform` field in logs and diagnostics.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self.architecture {
            DeckArchitecture::AppleSilicon => "macos-apple-silicon",
            DeckArchitecture::Intel => "macos-intel",
        }
    }

    /// Whether this Deck may advertise AV1 decode at all. Only Apple silicon
    /// from the M3 has an AV1 decoder, and `VideoToolbox` gained AV1 in macOS
    /// 14; no Intel Mac decodes AV1 in `VideoToolbox`. The capability probe
    /// still decides whether this particular Mac does.
    #[must_use]
    pub const fn av1_decode_possible(self) -> bool {
        matches!(self.architecture, DeckArchitecture::AppleSilicon)
            && self.macos.major >= FIRST_MACOS_WITH_AV1_DECODE
    }

    /// The AV1 claim to send: the probe's answer, never more than the
    /// platform allows.
    #[must_use]
    pub const fn advertised_av1(self, probe_claims_av1: bool) -> bool {
        probe_claims_av1 && self.av1_decode_possible()
    }

    /// Whether `VideoToolbox` decoding in software is the normal outcome here.
    ///
    /// Software decode is accepted on both architectures: the capability
    /// probe advertises only what a real decode session accepted, and the
    /// Pier negotiates within it. What differs is whether it is expected --
    /// many Intel Macs have no hardware decoder for HEVC, while Apple silicon
    /// decodes every codec it advertises in its media engine.
    #[must_use]
    pub const fn software_decode_expected(self) -> bool {
        matches!(self.architecture, DeckArchitecture::Intel)
    }

    /// Reads a decoder's `UsingHardwareAcceleratedVideoDecoder` answer
    /// against this platform's expectation.
    #[must_use]
    pub const fn assess_decode_path(
        self,
        hardware_accelerated: Option<bool>,
    ) -> DecodePathAssessment {
        match hardware_accelerated {
            Some(true) => DecodePathAssessment::Hardware,
            Some(false) if self.software_decode_expected() => {
                DecodePathAssessment::ExpectedSoftware
            }
            Some(false) => DecodePathAssessment::UnexpectedSoftware,
            None => DecodePathAssessment::Unknown,
        }
    }

    /// Whether the Hard USB (native tablet) helper can be installed. It is
    /// registered with `SMAppService`, which macOS 12 does not have. This is
    /// an operating-system limit, not an architecture one.
    #[must_use]
    pub const fn native_usb_helper_supported(self) -> bool {
        self.macos.major >= FIRST_MACOS_WITH_SM_APP_SERVICE
    }
}

impl Display for DeckPlatform {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} ({}, macOS {}{})",
            self.label(),
            self.architecture.token(),
            self.macos,
            if self.translated { ", Rosetta" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONTEREY: MacOsVersion = MacOsVersion::new(12, 7, 6);
    const VENTURA: MacOsVersion = MacOsVersion::new(13, 0, 0);
    const SONOMA: MacOsVersion = MacOsVersion::new(14, 0, 0);

    fn apple_silicon(macos: MacOsVersion) -> DeckPlatform {
        DeckPlatform::new(DeckArchitecture::AppleSilicon, macos, false)
    }

    fn intel(macos: MacOsVersion) -> DeckPlatform {
        DeckPlatform::new(DeckArchitecture::Intel, macos, false)
    }

    #[test]
    fn architecture_comes_from_the_compile_target() {
        assert_eq!(
            DeckArchitecture::from_target_arch("aarch64"),
            Some(DeckArchitecture::AppleSilicon)
        );
        assert_eq!(
            DeckArchitecture::from_target_arch("arm64"),
            Some(DeckArchitecture::AppleSilicon)
        );
        assert_eq!(
            DeckArchitecture::from_target_arch("x86_64"),
            Some(DeckArchitecture::Intel)
        );
        assert_eq!(DeckArchitecture::from_target_arch("riscv64"), None);
        assert_eq!(DeckArchitecture::AppleSilicon.token(), "arm64");
        assert_eq!(DeckArchitecture::Intel.token(), "x86_64");
    }

    #[test]
    fn labels_name_the_two_apps() {
        assert_eq!(apple_silicon(SONOMA).label(), "macos-apple-silicon");
        assert_eq!(intel(MONTEREY).label(), "macos-intel");
        assert_eq!(
            intel(MONTEREY).to_string(),
            "macos-intel (x86_64, macOS 12.7.6)"
        );
        assert_eq!(
            DeckPlatform::new(DeckArchitecture::Intel, SONOMA, true).to_string(),
            "macos-intel (x86_64, macOS 14.0.0, Rosetta)"
        );
    }

    #[test]
    fn av1_is_never_advertised_on_intel() {
        for macos in [MONTEREY, VENTURA, SONOMA, MacOsVersion::new(26, 0, 0)] {
            assert!(!intel(macos).av1_decode_possible());
            assert!(!intel(macos).advertised_av1(true));
        }
        // An Intel build under Rosetta keeps the Intel answer.
        assert!(!DeckPlatform::new(DeckArchitecture::Intel, SONOMA, true).advertised_av1(true));
    }

    #[test]
    fn av1_on_apple_silicon_follows_the_probe_from_macos_14() {
        assert!(!apple_silicon(MONTEREY).advertised_av1(true));
        assert!(!apple_silicon(VENTURA).advertised_av1(true));
        assert!(apple_silicon(SONOMA).advertised_av1(true));
        assert!(!apple_silicon(SONOMA).advertised_av1(false));
    }

    #[test]
    fn software_decode_is_expected_only_on_intel() {
        assert!(intel(MONTEREY).software_decode_expected());
        assert!(!apple_silicon(MONTEREY).software_decode_expected());
        assert_eq!(
            intel(MONTEREY).assess_decode_path(Some(false)),
            DecodePathAssessment::ExpectedSoftware
        );
        assert_eq!(
            apple_silicon(SONOMA).assess_decode_path(Some(false)),
            DecodePathAssessment::UnexpectedSoftware
        );
        for platform in [intel(MONTEREY), apple_silicon(SONOMA)] {
            assert_eq!(
                platform.assess_decode_path(Some(true)),
                DecodePathAssessment::Hardware
            );
            assert_eq!(
                platform.assess_decode_path(None),
                DecodePathAssessment::Unknown
            );
        }
        assert_eq!(
            DecodePathAssessment::ExpectedSoftware.token(),
            "software-expected"
        );
    }

    #[test]
    fn native_usb_helper_needs_macos_13_on_either_architecture() {
        for platform in [intel(MONTEREY), apple_silicon(MONTEREY)] {
            assert!(!platform.native_usb_helper_supported());
        }
        for platform in [
            intel(VENTURA),
            apple_silicon(VENTURA),
            apple_silicon(SONOMA),
        ] {
            assert!(platform.native_usb_helper_supported());
        }
    }

    #[test]
    fn reported_versions_never_go_negative() {
        assert_eq!(MacOsVersion::from_reported(12, 7, 6), MONTEREY);
        assert_eq!(
            MacOsVersion::from_reported(-1, 0, -3),
            MacOsVersion::new(0, 0, 0)
        );
        assert!(MONTEREY < VENTURA);
    }
}
