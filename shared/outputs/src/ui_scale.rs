//! The desktop UI scale a host gives one virtual output.
//!
//! A compositor picks a monitor's default scale from the monitor's physical
//! size. A virtual output has none (no EDID), so a compositor leaves it at
//! 1.0. The host knows what the compositor does not: the millimetres the
//! client advertised for the display it stands for.
//!
//! Those millimetres are the client's UI-scale request, not a measurement.
//! A Deck advertises `pixels * 25.4 / (96 * percent / 100)` millimetres for
//! the remote UI scale the user chose (Automatic is 100% of the apparent
//! size; a stream in backing pixels doubles the percentage so the apparent
//! size stays the same), and 96 DPI is 100% by definition — the rule the
//! Windows Pier applies to the EDID it synthesises. This module reads the
//! same rule back (`pixels * 25.4 / (96 * millimetres)`, horizontal and
//! vertical averaged) and takes the nearest scale the compositor says the
//! output's mode supports; a request halfway between two supported scales
//! takes the smaller. Without millimetres, a stream in backing pixels (its
//! pixel size larger than its logical size) takes that ratio (ADR 0009);
//! otherwise the output stays unscaled.

/// The density that is 100% UI scale.
pub const REFERENCE_DPI: f64 = 96.0;
/// Millimetre rounding makes a requested scale inexact: two supported
/// scales whose distances differ by less than this count as a tie.
pub const TIE_TOLERANCE: f64 = 0.02;

/// What the host knows about the client display one output stands for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputScaleFacts {
    /// Output width in pixels.
    pub width_px: u32,
    /// Output height in pixels.
    pub height_px: u32,
    /// The client's logical width for this display, when it said one.
    pub logical_width: Option<u32>,
    /// Physical width in millimetres, zero when unknown.
    pub width_mm: u32,
    /// Physical height in millimetres, zero when unknown.
    pub height_mm: u32,
}

/// Why a scale was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleBasis {
    /// The client's advertised density (96 DPI is 100%).
    RequestedDensity,
    /// The client streams backing pixels: pixel size over logical size.
    BackingPixels,
    /// Nothing to go on: unscaled.
    Unknown,
}

impl ScaleBasis {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::RequestedDensity => "requested_density",
            Self::BackingPixels => "backing_pixels",
            Self::Unknown => "unknown",
        }
    }
}

/// A chosen output scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputScaleChoice {
    /// One of the supported scales (1.0 when none was offered).
    pub scale: f64,
    /// The scale the facts asked for before it met the supported set.
    pub ideal: f64,
    /// What decided it.
    pub basis: ScaleBasis,
}

/// The ideal scale for `facts`, before the compositor's supported set.
#[must_use]
pub fn ideal_output_scale(facts: &OutputScaleFacts) -> (f64, ScaleBasis) {
    if facts.width_mm > 0 && facts.height_mm > 0 && facts.width_px > 0 && facts.height_px > 0 {
        let ratio = |pixels: u32, millimetres: u32| {
            f64::from(pixels) * 25.4 / (REFERENCE_DPI * f64::from(millimetres))
        };
        let horizontal = ratio(facts.width_px, facts.width_mm);
        let vertical = ratio(facts.height_px, facts.height_mm);
        return (
            f64::midpoint(horizontal, vertical),
            ScaleBasis::RequestedDensity,
        );
    }
    if let Some(logical) = facts.logical_width.filter(|logical| *logical > 0) {
        if facts.width_px > logical {
            return (
                f64::from(facts.width_px) / f64::from(logical),
                ScaleBasis::BackingPixels,
            );
        }
    }
    (1.0, ScaleBasis::Unknown)
}

/// Chooses the scale for `facts` among `supported` (the compositor's
/// supported scales for the output's mode). An empty or unusable set means
/// 1.0.
#[must_use]
pub fn choose_output_scale(facts: &OutputScaleFacts, supported: &[f64]) -> OutputScaleChoice {
    let (ideal, basis) = ideal_output_scale(facts);
    let mut usable: Vec<f64> = supported
        .iter()
        .copied()
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .collect();
    usable.sort_by(f64::total_cmp);
    let Some(&first) = usable.first() else {
        return OutputScaleChoice {
            scale: 1.0,
            ideal,
            basis,
        };
    };
    // Ascending, so a later scale must be clearly nearer to win a tie.
    let mut best = first;
    for &scale in &usable[1..] {
        if (scale - ideal).abs() + TIE_TOLERANCE < (best - ideal).abs() {
            best = scale;
        }
    }
    OutputScaleChoice {
        scale: best,
        ideal,
        basis,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTEGER: [f64; 2] = [1.0, 2.0];

    fn facts(width_px: u32, height_px: u32, width_mm: u32, height_mm: u32) -> OutputScaleFacts {
        OutputScaleFacts {
            width_px,
            height_px,
            logical_width: None,
            width_mm,
            height_mm,
        }
    }

    /// What a Deck advertises for `percent` (whole millimetres on the wire
    /// to the host's plan).
    fn deck(width_px: u32, height_px: u32, percent: u32) -> OutputScaleFacts {
        let millimetres = |pixels: u32| {
            let value = f64::from(pixels) * 25.4 / (96.0 * f64::from(percent) / 100.0);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let rounded = value.round() as u32;
            rounded
        };
        facts(
            width_px,
            height_px,
            millimetres(width_px),
            millimetres(height_px),
        )
    }

    #[test]
    fn a_hidpi_retina_stream_gets_two() {
        // A 3024x1964 backing-pixel stream at Automatic: the Deck doubles
        // 100% to 200% (400x260 mm, 192 DPI).
        let choice = choose_output_scale(&deck(3024, 1964, 200), &INTEGER);
        assert_eq!(choice.scale, 2.0);
        assert_eq!(choice.basis, ScaleBasis::RequestedDensity);
        assert!((choice.ideal - 2.0).abs() < 0.01);
    }

    #[test]
    fn a_point_stream_at_automatic_stays_unscaled() {
        // The same panel streamed in points, or the backing pixels at an
        // explicit 100%: 96 DPI.
        assert_eq!(
            choose_output_scale(&deck(1512, 982, 100), &INTEGER).scale,
            1.0
        );
        assert_eq!(
            choose_output_scale(&deck(3024, 1964, 100), &INTEGER).scale,
            1.0
        );
    }

    #[test]
    fn the_millimetres_decide_over_the_backing_ratio() {
        // A HiDPI stream at an explicit 100% asks for an unscaled UI even
        // though its pixels are twice its points.
        let choice = choose_output_scale(
            &OutputScaleFacts {
                logical_width: Some(1512),
                ..deck(3024, 1964, 100)
            },
            &INTEGER,
        );
        assert_eq!(choice.scale, 1.0);
        assert_eq!(choice.basis, ScaleBasis::RequestedDensity);
    }

    #[test]
    fn backing_pixels_take_the_clients_ratio_without_millimetres() {
        let choice = choose_output_scale(
            &OutputScaleFacts {
                logical_width: Some(1512),
                ..facts(3024, 1964, 0, 0)
            },
            &INTEGER,
        );
        assert_eq!(choice.scale, 2.0);
        assert_eq!(choice.basis, ScaleBasis::BackingPixels);
        assert!((choice.ideal - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_requested_percentage_takes_the_nearest_supported_scale() {
        let fractional = [1.0, 1.25, 1.5, 1.75, 2.0];
        assert_eq!(
            choose_output_scale(&deck(2560, 1440, 150), &fractional).scale,
            1.5
        );
        assert_eq!(
            choose_output_scale(&deck(2560, 1440, 175), &fractional).scale,
            1.75
        );
        assert_eq!(
            choose_output_scale(&deck(2560, 1440, 300), &[1.0, 2.0, 3.0]).scale,
            3.0
        );
        // Integer-only: 125% is nearer 1, 175% nearer 2.
        assert_eq!(
            choose_output_scale(&deck(2560, 1440, 125), &INTEGER).scale,
            1.0
        );
        assert_eq!(
            choose_output_scale(&deck(2560, 1440, 175), &INTEGER).scale,
            2.0
        );
    }

    #[test]
    fn halfway_between_two_scales_takes_the_smaller() {
        // 150% with only 100% and 200%: millimetre rounding must not tip it.
        for (width, height) in [(1920, 1080), (2560, 1440), (3024, 1964), (1366, 768)] {
            assert_eq!(
                choose_output_scale(&deck(width, height, 150), &INTEGER).scale,
                1.0,
                "{width}x{height}"
            );
        }
        // The supported set's order does not matter.
        assert_eq!(
            choose_output_scale(&deck(1920, 1080, 150), &[2.0, 1.0]).scale,
            1.0
        );
    }

    #[test]
    fn unknown_size_or_an_output_without_scales_is_unscaled() {
        let choice = choose_output_scale(&facts(3840, 2160, 0, 0), &INTEGER);
        assert_eq!((choice.scale, choice.basis), (1.0, ScaleBasis::Unknown));
        assert_eq!(choose_output_scale(&deck(3024, 1964, 200), &[]).scale, 1.0);
        assert_eq!(
            choose_output_scale(&deck(3024, 1964, 200), &[1.0]).scale,
            1.0
        );
        assert_eq!(
            choose_output_scale(&deck(3024, 1964, 200), &[f64::NAN, 0.0]).scale,
            1.0
        );
    }
}
