// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The pressure a compositor's applications see from a host's virtual tablet,
//! and the device pressure that makes them see exactly what the Deck sent.
//!
//! A virtual tablet read by a compositor's own libinput passes two transfers
//! before an application sees its pressure:
//!
//! 1. libinput normalises `ABS_PRESSURE` from its lower contact threshold, not
//!    from the axis minimum: `(value - lower) / (maximum - lower)`, with
//!    `lower` at 1 % of the range unless a quirk sets it (`normalize_pressure`
//!    and `update_pressure_range` in `evdev-tablet.c`, libinput 1.31).
//! 2. Mutter's native backend, since 46 (mutter!3399), multiplies the value of
//!    its pressure curve by the pressure itself (`pressure * factor` in
//!    `meta_input_device_tool_native_translate_pressure_in_impl`, unchanged in
//!    51.1). With GNOME's default linear curve (`pressure-curve`
//!    `[0, 0, 100, 100]`) applications see the pressure squared: a Deck's 0.5
//!    arrives as 0.245 (GNOME/mutter#5077, open). The curve is a table of 256
//!    samples, and the lookup interpolates between two samples by the pressure
//!    rather than by its fraction between them, so the curve's value at `q`
//!    is `(⌊256 q⌋ + q) / 256` below the last two samples.
//!
//! [`SeatPenPressure`] models both for a tablet on a desktop's own seat and
//! inverts them, so the device carries the pressure that arrives as sent:
//! within one device step without Mutter's curve, and within the curve's
//! 1/256 steps with it (at most 0.004, under 0.001 for most pressures).
//! It models GNOME's default curve; a user who changed the tip feel sees that
//! curve applied once, as on a local tablet with the upstream fix.
//!
//! A Mutter built with that fix (Arcen's lab GNOME prefix carries it as a
//! patch) says so with a [`PressureCurveMarker`] in its prefix, bound to its
//! library's digest. Only a marker that still matches the library and the
//! compositor's version turns the inverse off
//! ([`CompositorPressure::for_compositor_build`]); anything else keeps it.

use std::fmt::{Display, Formatter};

use arcen_media::video::compositor_fidelity::CompositorRelease;

/// First Mutter major version whose native backend multiplies its pressure
/// curve by the pressure (mutter!3399, 46.alpha).
pub const MUTTER_SQUARED_PRESSURE_MAJOR: u32 = 46;

/// Samples in Mutter's pressure curve table (`N_PRESSURECURVE_POINTS`).
const MUTTER_CURVE_SAMPLES: f64 = 256.0;

/// What the compositor does with libinput's normalised pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorPressure {
    /// Applications see libinput's pressure (an X server, Mutter before 46,
    /// or an unidentified compositor).
    Linear,
    /// Mutter 46 and later with its default curve: `q × curve(q)`.
    MutterSquared,
}

impl CompositorPressure {
    /// The transfer of the compositor `release`.
    #[must_use]
    pub const fn for_compositor(release: CompositorRelease) -> Self {
        match release {
            CompositorRelease::Mutter { major, .. } if major >= MUTTER_SQUARED_PRESSURE_MAJOR => {
                Self::MutterSquared
            }
            _ => Self::Linear,
        }
    }

    /// The transfer of the compositor `release` as built: a Mutter whose
    /// prefix proves that it applies its pressure curve once
    /// ([`PressureCurveMarker::check`] passed) delivers libinput's pressure.
    #[must_use]
    pub const fn for_compositor_build(
        release: CompositorRelease,
        curve_applied_once: bool,
    ) -> Self {
        if curve_applied_once {
            Self::Linear
        } else {
            Self::for_compositor(release)
        }
    }

    /// The pressure applications see for libinput's normalised pressure `q`.
    #[must_use]
    pub fn delivered(self, q: f64) -> f64 {
        let q = q.clamp(0.0, 1.0);
        match self {
            Self::Linear => q,
            Self::MutterSquared => q * mutter_default_curve(q),
        }
    }

    /// The normalised libinput pressure that applications see as `pressure`.
    #[must_use]
    pub fn libinput_pressure_for(self, pressure: f64) -> f64 {
        let pressure = if pressure.is_nan() {
            0.0
        } else {
            pressure.clamp(0.0, 1.0)
        };
        match self {
            Self::Linear => pressure,
            Self::MutterSquared => {
                // `delivered` is continuous and strictly increasing on 0..=1.
                let (mut low, mut high) = (0.0_f64, 1.0_f64);
                for _ in 0..64 {
                    let middle = f64::midpoint(low, high);
                    if self.delivered(middle) < pressure {
                        low = middle;
                    } else {
                        high = middle;
                    }
                }
                high
            }
        }
    }
}

/// Mutter's sampled default (linear) curve as its lookup reads it.
fn mutter_default_curve(q: f64) -> f64 {
    let last = MUTTER_CURVE_SAMPLES - 1.0;
    let low = (q * MUTTER_CURVE_SAMPLES).floor().clamp(0.0, last);
    let high = (low + 1.0).min(last);
    // Sample `i` holds `i / 256`, except the last one, which holds 1.
    let sample = |index: f64| {
        if index >= last {
            1.0
        } else {
            index / MUTTER_CURVE_SAMPLES
        }
    };
    let (low, high) = (sample(low), sample(high));
    low + (high - low) * q.fract()
}

/// libinput's view of a `0..=maximum` pressure axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibinputPressureAxis {
    maximum: i32,
    lower: i32,
}

impl LibinputPressureAxis {
    /// An axis libinput reads without a pressure quirk: its lower threshold
    /// is 1 % of the range, truncated (`axis_range_percentage`).
    #[must_use]
    pub const fn without_quirk(maximum: i32) -> Self {
        let maximum = if maximum < 1 { 1 } else { maximum };
        Self {
            maximum,
            lower: maximum / 100,
        }
    }

    /// libinput's normalised pressure for device `value`.
    #[must_use]
    pub fn normalised(self, value: i32) -> f64 {
        let span = f64::from(self.maximum - self.lower);
        (f64::from(value - self.lower) / span).clamp(0.0, 1.0)
    }

    /// The device value nearest to normalised pressure `q`.
    #[must_use]
    pub fn device_value(self, q: f64) -> i32 {
        let span = f64::from(self.maximum - self.lower);
        #[allow(clippy::cast_possible_truncation)]
        let value = (f64::from(self.lower) + q.clamp(0.0, 1.0) * span).round() as i32;
        value.clamp(0, self.maximum)
    }
}

/// How a desktop's own-seat virtual tablet carries pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeatPenPressure {
    axis: LibinputPressureAxis,
    compositor: CompositorPressure,
}

impl SeatPenPressure {
    /// A `0..=maximum` axis without a quirk, read by `compositor`.
    #[must_use]
    pub const fn new(maximum: i32, compositor: CompositorPressure) -> Self {
        Self {
            axis: LibinputPressureAxis::without_quirk(maximum),
            compositor,
        }
    }

    /// The compositor transfer this mapping inverts.
    #[must_use]
    pub const fn compositor(self) -> CompositorPressure {
        self.compositor
    }

    /// The pressure applications see for device `value`.
    #[must_use]
    pub fn delivered(self, value: i32) -> f64 {
        self.compositor.delivered(self.axis.normalised(value))
    }

    /// The device value whose delivered pressure is nearest to `pressure`
    /// (`0.0..=1.0`, from the wire). `0` for no pressure; the caller still
    /// applies its contact floor.
    #[must_use]
    pub fn device_value(self, pressure: f32) -> i32 {
        let pressure = f64::from(pressure);
        if pressure.is_nan() || pressure <= 0.0 {
            return 0;
        }
        let pressure = pressure.min(1.0);
        let estimate = self
            .axis
            .device_value(self.compositor.libinput_pressure_for(pressure));
        (estimate - 2..=estimate + 2)
            .map(|value| value.clamp(0, self.axis.maximum))
            .min_by(|a, b| {
                let error = |value| (self.delivered(value) - pressure).abs();
                error(*a).total_cmp(&error(*b))
            })
            .unwrap_or(estimate)
    }
}

/// Where, inside a compositor's installation prefix, a Mutter that applies
/// its pressure curve once says so (Arcen's lab GNOME prefix,
/// `packaging/linux/tools/gnome-prefix/write-pressure-marker.sh`).
pub const PRESSURE_CURVE_MARKER: &str = "share/arcen/mutter-pressure-curve";

/// The largest marker the Pier reads, in bytes.
pub const PRESSURE_CURVE_MARKER_MAX_BYTES: u64 = 4096;

/// The marker's format line and version.
const MARKER_FORMAT: &str = "arcen-mutter-pressure-curve";
const MARKER_VERSION: &str = "1";

/// A prefix's statement that its Mutter applies the tablet pressure curve
/// once (GNOME/mutter#5077 fixed), bound to the library it was written for.
///
/// Text, one `key=value` per line, `#` comments:
///
/// ```text
/// arcen-mutter-pressure-curve=1
/// mutter-version=51.1
/// library=lib64/libmutter-51.so.0
/// library-sha256=<64 hex digits>
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressureCurveMarker {
    /// Mutter `major.minor`.
    mutter_version: (u32, u32),
    library: String,
    library_sha256: String,
}

/// Why a marker does not prove that the compositor applies its pressure
/// curve once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PressureCurveMarkerError {
    /// The text is not a marker this Pier reads.
    Malformed(String),
    /// The marker was written for another Mutter version.
    OtherVersion {
        /// The marker's Mutter version.
        marker: String,
        /// The compositor's version.
        compositor: String,
    },
    /// The library no longer has the digest the marker was written for
    /// (replaced or restored since).
    LibraryChanged {
        /// The library, relative to the prefix.
        library: String,
    },
}

impl Display for PressureCurveMarkerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(reason) => {
                write!(formatter, "malformed pressure-curve marker: {reason}")
            }
            Self::OtherVersion { marker, compositor } => write!(
                formatter,
                "the pressure-curve marker is for Mutter {marker}, the compositor is {compositor}"
            ),
            Self::LibraryChanged { library } => write!(
                formatter,
                "{library} changed since the pressure-curve marker was written"
            ),
        }
    }
}

impl std::error::Error for PressureCurveMarkerError {}

impl PressureCurveMarker {
    /// Parses a marker's text.
    ///
    /// # Errors
    ///
    /// [`PressureCurveMarkerError::Malformed`] for a missing, repeated or
    /// unknown key, another format version, a library path that is not a
    /// plain relative path inside the prefix, or a digest that is not 64 hex
    /// digits.
    pub fn parse(text: &str) -> Result<Self, PressureCurveMarkerError> {
        let malformed = |reason: &str| PressureCurveMarkerError::Malformed(reason.to_owned());
        let (mut format, mut version, mut library, mut digest) = (None, None, None, None);
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| malformed("a line without `=`"))?;
            let slot = match key.trim() {
                MARKER_FORMAT => &mut format,
                "mutter-version" => &mut version,
                "library" => &mut library,
                "library-sha256" => &mut digest,
                _ => return Err(malformed("an unknown key")),
            };
            if slot.replace(value.trim()).is_some() {
                return Err(malformed("a repeated key"));
            }
        }
        if format != Some(MARKER_VERSION) {
            return Err(malformed("not format 1"));
        }
        let Some(CompositorRelease::Mutter { major, minor }) =
            version.map(CompositorRelease::parse_mutter)
        else {
            return Err(malformed("no Mutter version"));
        };
        let library = library.ok_or_else(|| malformed("no library"))?;
        let plain = !library.is_empty()
            && library.len() <= 256
            && library.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
            });
        if !plain {
            return Err(malformed(
                "the library is not a plain path inside the prefix",
            ));
        }
        let digest = digest.ok_or_else(|| malformed("no library digest"))?;
        if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(malformed("the library digest is not 64 hex digits"));
        }
        Ok(Self {
            mutter_version: (major, minor),
            library: library.to_owned(),
            library_sha256: digest.to_ascii_lowercase(),
        })
    }

    /// The library the marker was written for, relative to the prefix: a
    /// plain path (no leading `/`, no `.` or `..`).
    #[must_use]
    pub fn library(&self) -> &str {
        &self.library
    }

    /// Whether the marker speaks for the compositor about to run: written
    /// for its `major.minor` (`compositor_version` as it reports it) and for
    /// the library as it is now (`library_sha256`, hex).
    ///
    /// # Errors
    ///
    /// [`PressureCurveMarkerError::OtherVersion`] or
    /// [`PressureCurveMarkerError::LibraryChanged`].
    pub fn check(
        &self,
        compositor_version: &str,
        library_sha256: &str,
    ) -> Result<(), PressureCurveMarkerError> {
        let (major, minor) = self.mutter_version;
        if CompositorRelease::parse_mutter(compositor_version)
            != (CompositorRelease::Mutter { major, minor })
        {
            return Err(PressureCurveMarkerError::OtherVersion {
                marker: format!("{major}.{minor}"),
                compositor: compositor_version.to_owned(),
            });
        }
        if !library_sha256.eq_ignore_ascii_case(&self.library_sha256) {
            return Err(PressureCurveMarkerError::LibraryChanged {
                library: self.library.clone(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: i32 = 65_535;

    fn mutter(major: u32) -> CompositorRelease {
        CompositorRelease::Mutter { major, minor: 0 }
    }

    #[test]
    fn mutter_46_and_later_square_the_pressure_older_and_others_do_not() {
        assert_eq!(
            CompositorPressure::for_compositor(mutter(45)),
            CompositorPressure::Linear
        );
        for major in [46, 49, 51] {
            assert_eq!(
                CompositorPressure::for_compositor(mutter(major)),
                CompositorPressure::MutterSquared
            );
        }
        assert_eq!(
            CompositorPressure::for_compositor(CompositorRelease::Other),
            CompositorPressure::Linear
        );
    }

    #[test]
    fn the_model_reproduces_the_measured_uncompensated_pressure() {
        // The Pier wrote 0.5 linearly (32768); Chrome on GNOME 51 read 0.2446.
        let uncompensated = SeatPenPressure::new(MAX, CompositorPressure::MutterSquared);
        let delivered = uncompensated.delivered(32_768);
        assert!((delivered - 0.2446).abs() < 0.0001, "{delivered}");
        // libinput's lower threshold alone: 655 units of 65535.
        let linear = SeatPenPressure::new(MAX, CompositorPressure::Linear);
        assert!((linear.delivered(32_768) - 0.49495).abs() < 0.00001);
    }

    #[test]
    fn compensated_pressure_arrives_as_sent_within_the_curves_quantisation() {
        let linear = SeatPenPressure::new(MAX, CompositorPressure::Linear);
        let mutter = SeatPenPressure::new(MAX, CompositorPressure::MutterSquared);
        for step in 1..=1000 {
            #[allow(clippy::cast_precision_loss)]
            let sent = step as f32 / 1000.0;
            let error = |mapping: SeatPenPressure| {
                (mapping.delivered(mapping.device_value(sent)) - f64::from(sent)).abs()
            };
            // One device step.
            assert!(error(linear) < 0.00002, "linear {sent}");
            // Mutter's curve steps by 1/256 between its samples, so some
            // pressures fall between two deliverable ones; the largest gap
            // is next to the last sample.
            assert!(error(mutter) < 0.004, "mutter {sent}: {}", error(mutter));
        }
        // The levels the lab measures arrive within 0.001.
        for sent in [0.1_f32, 0.25, 0.5, 0.75, 1.0] {
            let delivered = mutter.delivered(mutter.device_value(sent));
            assert!(
                (delivered - f64::from(sent)).abs() < 0.001,
                "{sent} -> {delivered}"
            );
        }
    }

    #[test]
    fn full_and_no_pressure_map_to_the_axis_ends() {
        for compositor in [
            CompositorPressure::Linear,
            CompositorPressure::MutterSquared,
        ] {
            let mapping = SeatPenPressure::new(MAX, compositor);
            assert_eq!(mapping.device_value(1.0), MAX);
            assert_eq!(mapping.device_value(2.0), MAX);
            assert_eq!(mapping.device_value(0.0), 0);
            assert_eq!(mapping.device_value(-1.0), 0);
            assert_eq!(mapping.device_value(f32::NAN), 0);
        }
    }

    #[test]
    fn delivered_pressure_rises_with_the_device_value() {
        let mapping = SeatPenPressure::new(MAX, CompositorPressure::MutterSquared);
        let mut last = -1.0;
        for value in (0..=MAX).step_by(97) {
            let delivered = mapping.delivered(value);
            assert!(delivered >= last, "{value}");
            last = delivered;
        }
    }

    const DIGEST: &str = "ad2f4c780686ddc540a75da63de85091fde3bbf7718de32c432ae05dbbb1a4f6";

    fn marker_text(version: &str, library: &str, digest: &str) -> String {
        format!(
            "# Arcen lab: this prefix's Mutter applies the tablet pressure curve once\n\
             arcen-mutter-pressure-curve=1\nmutter-version={version}\nlibrary={library}\n\
             library-sha256={digest}\n"
        )
    }

    #[test]
    fn a_marker_that_matches_the_library_and_version_turns_the_inverse_off() {
        let marker =
            PressureCurveMarker::parse(&marker_text("51.1", "lib64/libmutter-51.so.0", DIGEST))
                .unwrap();
        assert_eq!(marker.library(), "lib64/libmutter-51.so.0");
        assert_eq!(marker.check("51.1", &DIGEST.to_ascii_uppercase()), Ok(()));
        let proven = marker.check("51.1", DIGEST).is_ok();
        assert_eq!(
            CompositorPressure::for_compositor_build(mutter(51), proven),
            CompositorPressure::Linear
        );
        // On the patched Mutter the seat pen only inverts libinput's
        // threshold: the levels the lab measures arrive within a device step.
        let mapping = SeatPenPressure::new(
            MAX,
            CompositorPressure::for_compositor_build(mutter(51), proven),
        );
        for sent in [0.1_f32, 0.25, 0.5, 0.75, 1.0] {
            let delivered = mapping.delivered(mapping.device_value(sent));
            assert!((delivered - f64::from(sent)).abs() < 0.00002, "{sent}");
        }
    }

    #[test]
    fn without_a_matching_marker_mutter_keeps_the_inverse() {
        assert_eq!(
            CompositorPressure::for_compositor_build(mutter(51), false),
            CompositorPressure::MutterSquared
        );
        // A marker never makes another compositor's transfer worse.
        assert_eq!(
            CompositorPressure::for_compositor_build(CompositorRelease::Other, false),
            CompositorPressure::Linear
        );
        let marker =
            PressureCurveMarker::parse(&marker_text("51.1", "lib64/libmutter-51.so.0", DIGEST))
                .unwrap();
        // The unpatched library restored (or any other digest).
        let restored = "3cc7c6bb2774b11ca3a0ae438e2c2695bbe4dd5b70b40e0e1766f0ceecba4ebb";
        assert_eq!(
            marker.check("51.1", restored),
            Err(PressureCurveMarkerError::LibraryChanged {
                library: "lib64/libmutter-51.so.0".into()
            })
        );
        // The prefix moved to another Mutter while the marker stayed.
        assert!(matches!(
            marker.check("52.0", DIGEST),
            Err(PressureCurveMarkerError::OtherVersion { .. })
        ));
        assert!(marker.check("51.2", DIGEST).is_err());
        assert!(marker.check("", DIGEST).is_err());
    }

    #[test]
    fn malformed_markers_are_refused() {
        let good = marker_text("51.1", "lib64/libmutter-51.so.0", DIGEST);
        for text in [
            String::new(),
            good.replace("curve=1", "curve=2"),
            good.replace("mutter-version=51.1", "mutter-version=mutter"),
            good.replace("lib64/libmutter-51.so.0", "/opt/x/libmutter.so"),
            good.replace("lib64/libmutter-51.so.0", "lib64/../../etc/shadow"),
            good.replace("lib64/libmutter-51.so.0", "lib64//libmutter.so"),
            good.replace("lib64/libmutter-51.so.0", "lib 64/libmutter.so"),
            good.replace(DIGEST, &DIGEST[1..]),
            good.replace(DIGEST, &DIGEST.replace('a', "g")),
            format!("{good}library=lib64/other.so\n"),
            format!("{good}unknown=1\n"),
            format!("{good}no equals sign\n"),
            good.replace("library-sha256", "# library-sha256"),
        ] {
            assert!(
                matches!(
                    PressureCurveMarker::parse(&text),
                    Err(PressureCurveMarkerError::Malformed(_))
                ),
                "{text}"
            );
        }
    }
}
