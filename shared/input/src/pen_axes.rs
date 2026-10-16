//! Vendor-neutral pen axis normalisation.
//!
//! Every pen arrives with its own ranges: one tablet reports 1024 pressure
//! levels, another 16384; one reports tilt in whole degrees, another in
//! hundredths with a physical unit, `AppKit` as a normalized `-1..=1`. Every host
//! wants its own ranges back: a Linux evdev axis, Windows Ink's `0..=1024`,
//! `CoreGraphics`' normalized doubles.
//!
//! Between the two sits one vendor-neutral contract, the one
//! [`crate::PenEvent`] carries: pressure `0.0..=1.0`, tilt in degrees
//! `-90.0..=90.0`, rotation in degrees `0.0..360.0`. This module is the only
//! place that converts into and out of it, so the Deck and every host agree on
//! rounding, clamping and non-finite handling, and no product carries a
//! vendor's range as an assumption.

/// Inclusive integer extent of one axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalRange {
    min: i64,
    max: i64,
}

impl LogicalRange {
    /// Creates a range; `None` unless `max > min`.
    #[must_use]
    pub const fn new(min: i64, max: i64) -> Option<Self> {
        if max > min {
            Some(Self { min, max })
        } else {
            None
        }
    }

    /// Inclusive minimum.
    #[must_use]
    pub const fn min(self) -> i64 {
        self.min
    }

    /// Inclusive maximum.
    #[must_use]
    pub const fn max(self) -> i64 {
        self.max
    }

    /// Number of distinct steps.
    #[must_use]
    pub const fn levels(self) -> u64 {
        self.max.abs_diff(self.min).saturating_add(1)
    }
}

/// Pressure resolution of the virtual pen a host creates for typed (Tablet
/// support) input: 16 bits, `0..=65535`.
///
/// The wire carries pressure as a normalized float, so the virtual device
/// should not impose any particular tablet's resolution. Sixteen bits is finer
/// than every shipping pen's report (which top out at 16384 levels), so no
/// source loses precision and no vendor's maximum is baked in.
pub const TYPED_PEN_PRESSURE_RANGE: LogicalRange = LogicalRange {
    min: 0,
    max: 65_535,
};

/// Windows Ink's documented `POINTER_PEN_INFO.pressure` range, `0..=1024`.
///
/// An operating-system API ceiling, not a device property.
pub const WINDOWS_INK_PRESSURE_RANGE: LogicalRange = LogicalRange { min: 0, max: 1024 };

/// Largest tilt magnitude, in degrees, on the wire.
pub const TILT_DEGREES_FULL_SCALE: f32 = 90.0;
/// One full barrel rotation, in degrees.
pub const ROTATION_DEGREES_FULL_TURN: f32 = 360.0;

/// Maps a raw device value onto `0.0..=1.0`, clamping out-of-range input.
#[must_use]
pub fn normalize_logical(raw: i64, range: LogicalRange) -> f32 {
    let clamped = raw.clamp(range.min, range.max);
    let offset = clamped.abs_diff(range.min);
    let span = range.max.abs_diff(range.min);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let unit = (offset as f64 / span as f64) as f32;
    unit.clamp(0.0, 1.0)
}

/// Maps a normalized `0.0..=1.0` value onto a device range, rounding to the
/// nearest step. Out-of-range input clamps; non-finite input maps to the
/// minimum, the safe "no pressure" end.
#[must_use]
pub fn scale_unit(value: f32, range: LogicalRange) -> i64 {
    let unit = if value.is_finite() {
        f64::from(value.clamp(0.0, 1.0))
    } else {
        0.0
    };
    #[allow(clippy::cast_precision_loss)]
    let span = range.max.abs_diff(range.min) as f64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let steps = (unit * span).round() as u64;
    range
        .min
        .saturating_add_unsigned(steps.min(range.max.abs_diff(range.min)))
}

/// Clamps a native pressure reading into the wire's `0.0..=1.0`. Non-finite
/// input is no pressure.
#[must_use]
pub fn clamp_pressure(pressure: f64) -> f32 {
    if pressure.is_finite() {
        #[allow(clippy::cast_possible_truncation)]
        let narrowed = pressure.clamp(0.0, 1.0) as f32;
        narrowed
    } else {
        0.0
    }
}

/// Converts a normalized `-1.0..=1.0` tilt (`AppKit`, `CoreGraphics`) to degrees.
///
/// Matches the W3C Pointer Events convention: full deflection is 90 degrees.
#[must_use]
pub fn tilt_unit_to_degrees(unit: f64) -> f32 {
    if unit.is_finite() {
        #[allow(clippy::cast_possible_truncation)]
        let degrees = (unit.clamp(-1.0, 1.0) * f64::from(TILT_DEGREES_FULL_SCALE)) as f32;
        degrees.clamp(-TILT_DEGREES_FULL_SCALE, TILT_DEGREES_FULL_SCALE)
    } else {
        0.0
    }
}

/// Converts wire tilt degrees back to a normalized `-1.0..=1.0` axis.
#[must_use]
pub fn tilt_degrees_to_unit(degrees: f32) -> f64 {
    if degrees.is_finite() {
        f64::from((degrees / TILT_DEGREES_FULL_SCALE).clamp(-1.0, 1.0))
    } else {
        0.0
    }
}

/// Converts a raw device tilt to degrees.
///
/// When the device declares its physical extent in degrees, that extent is
/// used. Otherwise the logical extremes are taken to be full deflection, which
/// is how the HID Digitizers page defines tilt without a physical unit.
#[must_use]
pub fn tilt_logical_to_degrees(
    raw: i64,
    logical: LogicalRange,
    physical_degrees: Option<(f64, f64)>,
) -> f32 {
    let unit = f64::from(normalize_logical(raw, logical));
    let degrees = match physical_degrees {
        Some((low, high)) if low.is_finite() && high.is_finite() && high > low => {
            low + unit * (high - low)
        }
        _ => (unit * 2.0 - 1.0) * f64::from(TILT_DEGREES_FULL_SCALE),
    };
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = degrees as f32;
    narrowed.clamp(-TILT_DEGREES_FULL_SCALE, TILT_DEGREES_FULL_SCALE)
}

/// Wraps any rotation reading into `0.0..360.0`.
///
/// Tablet drivers disagree on `0..360` versus `-180..180`; both describe the
/// same angle, so wrapping is lossless. Non-finite input is zero.
#[must_use]
pub fn wrap_rotation_degrees(degrees: f64) -> f32 {
    if !degrees.is_finite() {
        return 0.0;
    }
    let wrapped = degrees.rem_euclid(f64::from(ROTATION_DEGREES_FULL_TURN));
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = wrapped as f32;
    // Rounding up to exactly 360 in f32 still names zero degrees.
    if narrowed >= ROTATION_DEGREES_FULL_TURN {
        0.0
    } else {
        narrowed
    }
}

/// Converts a raw device twist to `0.0..360.0` degrees, the logical range
/// covering one full turn.
#[must_use]
pub fn twist_logical_to_degrees(raw: i64, logical: LogicalRange) -> f32 {
    let clamped = raw.clamp(logical.min, logical.max);
    #[allow(clippy::cast_precision_loss)]
    let fraction = clamped.abs_diff(logical.min) as f64 / logical.levels() as f64;
    wrap_rotation_degrees(fraction * f64::from(ROTATION_DEGREES_FULL_TURN))
}

/// Rounds wire tilt to whole degrees in `-90..=90`, for hosts whose tilt axis
/// is integral (Linux evdev, Windows Ink).
#[must_use]
pub fn whole_tilt_degrees(degrees: f32) -> i32 {
    if !degrees.is_finite() {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let whole = degrees
        .clamp(-TILT_DEGREES_FULL_SCALE, TILT_DEGREES_FULL_SCALE)
        .round() as i32;
    whole.clamp(-90, 90)
}

/// Rounds wire rotation to whole degrees in `0..=359` (`360` is `0`).
#[must_use]
pub fn whole_rotation_degrees(degrees: f32) -> u32 {
    if !degrees.is_finite() {
        return 0;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = degrees.clamp(0.0, ROTATION_DEGREES_FULL_TURN).round() as u32;
    whole % 360
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(max: i64) -> LogicalRange {
        LogicalRange::new(0, max).unwrap()
    }

    #[test]
    fn every_common_pressure_resolution_normalizes_consistently() {
        for max in [1023, 2047, 4095, 8191, 16383] {
            let range = range(max);
            assert_eq!(range.levels(), u64::try_from(max).unwrap() + 1);
            assert!(normalize_logical(0, range).abs() < f32::EPSILON);
            assert!((normalize_logical(max, range) - 1.0).abs() < f32::EPSILON);
            let half = normalize_logical((max + 1) / 2, range);
            assert!((half - 0.5).abs() < 0.001, "{max}: half was {half}");
            // Out-of-range reports clamp rather than overflow the contract.
            assert!(normalize_logical(-5, range).abs() < f32::EPSILON);
            assert!((normalize_logical(max * 2, range) - 1.0).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn the_same_physical_force_lands_on_the_same_host_value_whatever_the_tablet() {
        // A quarter of full scale on five tablets of different resolution
        // must reach the host's virtual pen within one host step.
        let targets: Vec<i64> = [1023_i64, 2047, 4095, 8191, 16383]
            .into_iter()
            .map(|max| {
                let raw = (max + 1) / 4;
                scale_unit(normalize_logical(raw, range(max)), TYPED_PEN_PRESSURE_RANGE)
            })
            .collect();
        let lowest = *targets.iter().min().unwrap();
        let highest = *targets.iter().max().unwrap();
        // 1023 levels quantize coarser than the host; allow its step.
        assert!(highest - lowest <= 65, "{targets:?}");
    }

    #[test]
    fn device_ranges_round_trip_through_the_neutral_contract() {
        for max in [1023, 2047, 4095, 8191, 16383] {
            let range = range(max);
            for raw in [0, 1, max / 3, max / 2, max - 1, max] {
                assert_eq!(scale_unit(normalize_logical(raw, range), range), raw);
            }
        }
    }

    #[test]
    fn host_pressure_ranges_hit_their_endpoints() {
        assert_eq!(scale_unit(0.0, TYPED_PEN_PRESSURE_RANGE), 0);
        assert_eq!(scale_unit(1.0, TYPED_PEN_PRESSURE_RANGE), 65_535);
        assert_eq!(scale_unit(0.5, WINDOWS_INK_PRESSURE_RANGE), 512);
        assert_eq!(scale_unit(1.0, WINDOWS_INK_PRESSURE_RANGE), 1024);
        assert_eq!(scale_unit(2.0, WINDOWS_INK_PRESSURE_RANGE), 1024);
        assert_eq!(scale_unit(-1.0, WINDOWS_INK_PRESSURE_RANGE), 0);
        assert_eq!(scale_unit(f32::NAN, WINDOWS_INK_PRESSURE_RANGE), 0);
        let signed = LogicalRange::new(-100, 100).unwrap();
        assert_eq!(scale_unit(0.5, signed), 0);
        assert_eq!(scale_unit(1.0, signed), 100);
    }

    #[test]
    fn pressure_clamps_and_rejects_non_finite() {
        assert!((clamp_pressure(0.25) - 0.25).abs() < f32::EPSILON);
        assert!((clamp_pressure(7.0) - 1.0).abs() < f32::EPSILON);
        assert!(clamp_pressure(-1.0).abs() < f32::EPSILON);
        assert!(clamp_pressure(f64::NAN).abs() < f32::EPSILON);
    }

    #[test]
    fn unit_tilt_maps_to_degrees_and_back() {
        assert!((tilt_unit_to_degrees(1.0) - 90.0).abs() < f32::EPSILON);
        assert!((tilt_unit_to_degrees(-0.5) + 45.0).abs() < f32::EPSILON);
        assert!((tilt_unit_to_degrees(3.0) - 90.0).abs() < f32::EPSILON);
        assert!(tilt_unit_to_degrees(f64::INFINITY).abs() < f32::EPSILON);
        assert!((tilt_degrees_to_unit(45.0) - 0.5).abs() < f64::EPSILON);
        assert!((tilt_degrees_to_unit(-200.0) + 1.0).abs() < f64::EPSILON);
        assert!(tilt_degrees_to_unit(f32::NAN).abs() < f64::EPSILON);
    }

    #[test]
    fn device_tilt_ranges_map_to_degrees() {
        // Whole degrees with no physical unit: extremes are full deflection.
        let degrees = LogicalRange::new(-90, 90).unwrap();
        assert!((tilt_logical_to_degrees(-90, degrees, None) + 90.0).abs() < 0.001);
        assert!((tilt_logical_to_degrees(45, degrees, None) - 45.0).abs() < 0.001);
        // Signed byte without a unit.
        let byte = LogicalRange::new(-127, 127).unwrap();
        assert!((tilt_logical_to_degrees(127, byte, None) - 90.0).abs() < 0.001);
        assert!(tilt_logical_to_degrees(0, byte, None).abs() < 0.001);
        // A +/-60 pen that declares its physical extent in degrees.
        let sixty = LogicalRange::new(-60, 60).unwrap();
        assert!((tilt_logical_to_degrees(60, sixty, Some((-60.0, 60.0))) - 60.0).abs() < 0.001);
        assert!((tilt_logical_to_degrees(-30, sixty, Some((-60.0, 60.0))) + 30.0).abs() < 0.001);
        // Hundredths of a degree.
        let hundredths = LogicalRange::new(-9000, 9000).unwrap();
        assert!(
            (tilt_logical_to_degrees(4500, hundredths, Some((-90.0, 90.0))) - 45.0).abs() < 0.001
        );
        // A nonsensical physical extent falls back to the logical extremes.
        assert!((tilt_logical_to_degrees(60, sixty, Some((5.0, 5.0))) - 90.0).abs() < 0.001);
    }

    #[test]
    fn device_twist_ranges_map_to_one_turn() {
        let degrees = LogicalRange::new(0, 359).unwrap();
        assert!(twist_logical_to_degrees(0, degrees).abs() < 0.001);
        assert!((twist_logical_to_degrees(90, degrees) - 90.0).abs() < 0.001);
        assert!((twist_logical_to_degrees(359, degrees) - 359.0).abs() < 0.001);
        let fine = LogicalRange::new(0, 1023).unwrap();
        assert!((twist_logical_to_degrees(512, fine) - 180.0).abs() < 0.001);
        let signed = LogicalRange::new(-180, 179).unwrap();
        assert!(twist_logical_to_degrees(-180, signed).abs() < 0.001);
        assert!((twist_logical_to_degrees(0, signed) - 180.0).abs() < 0.001);
    }

    #[test]
    fn rotation_wraps_both_driver_conventions() {
        assert!((wrap_rotation_degrees(-90.0) - 270.0).abs() < 0.001);
        assert!((wrap_rotation_degrees(450.0) - 90.0).abs() < 0.001);
        assert!(wrap_rotation_degrees(360.0).abs() < f32::EPSILON);
        assert!(wrap_rotation_degrees(f64::NAN).abs() < f32::EPSILON);
        assert!(wrap_rotation_degrees(-1e-12) < 360.0);
    }

    #[test]
    fn whole_degree_hosts_round_and_clamp() {
        assert_eq!(whole_tilt_degrees(44.6), 45);
        assert_eq!(whole_tilt_degrees(-120.0), -90);
        assert_eq!(whole_tilt_degrees(f32::NAN), 0);
        assert_eq!(whole_rotation_degrees(359.6), 0);
        assert_eq!(whole_rotation_degrees(360.0), 0);
        assert_eq!(whole_rotation_degrees(181.2), 181);
        assert_eq!(whole_rotation_degrees(-3.0), 0);
        assert_eq!(whole_rotation_degrees(f32::INFINITY), 0);
    }

    #[test]
    fn degenerate_ranges_are_refused() {
        assert!(LogicalRange::new(5, 5).is_none());
        assert!(LogicalRange::new(5, 4).is_none());
        let full = LogicalRange::new(i64::MIN, i64::MAX).unwrap();
        assert_eq!(full.levels(), u64::MAX);
    }
}
