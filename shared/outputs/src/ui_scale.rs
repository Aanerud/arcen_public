// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
    choose_output_scale_from_ideal(ideal, basis, supported)
}

/// The per-monitor UI scales Windows offers (Settings › Display › Scale),
/// in percent. A per-monitor scale takes no other value: Windows' custom
/// scaling is one value for every monitor and needs a sign-out.
pub const WINDOWS_UI_SCALE_PERCENTS: [u16; 12] =
    [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// A Windows monitor's UI scale: what the client asked for, and the Windows
/// step that serves it ([`choose_output_scale`] over
/// [`WINDOWS_UI_SCALE_PERCENTS`]: the nearest step, the smaller one when the
/// request sits halfway). A 1.5× Retina stream at a Remote UI scale of 125%
/// asks for 187.5% and gets 175%.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsUiScale {
    /// The scale the client asked for, in whole percent.
    pub requested_percent: u16,
    /// The Windows step the host applies, in percent.
    pub step_percent: u16,
    /// What decided the request.
    pub basis: ScaleBasis,
}

impl WindowsUiScale {
    /// The scale for the client display `facts` describe.
    #[must_use]
    pub fn from_facts(facts: &OutputScaleFacts) -> Self {
        let (ideal, basis) = ideal_output_scale(facts);
        Self::from_ratio(ideal, basis)
    }

    /// The scale for a request already expressed as a percentage.
    #[must_use]
    pub fn from_percent(requested_percent: u16, basis: ScaleBasis) -> Self {
        Self::from_ratio(f64::from(requested_percent) / 100.0, basis)
    }

    /// The scale for a request expressed as a ratio (`1.0` is 100%); a
    /// non-finite or non-positive ratio counts as 100%.
    #[must_use]
    pub fn from_ratio(ideal: f64, basis: ScaleBasis) -> Self {
        let supported = WINDOWS_UI_SCALE_PERCENTS.map(|percent| f64::from(percent) / 100.0);
        let ideal = if ideal.is_finite() && ideal > 0.0 {
            ideal
        } else {
            1.0
        };
        let step = choose_output_scale_from_ideal(ideal, basis, &supported).scale;
        Self {
            requested_percent: whole_percent(ideal),
            step_percent: whole_percent(step),
            basis,
        }
    }

    /// The same request served by the nearest Windows step from `lowest`
    /// to `highest` percent: the steps Windows offers this monitor (it caps
    /// the scale by resolution: a 2880x1800 display stops at 300%). An
    /// empty range keeps the step.
    #[must_use]
    pub fn within(self, lowest: u16, highest: u16) -> Self {
        let mut offered = WINDOWS_UI_SCALE_PERCENTS
            .iter()
            .copied()
            .filter(|percent| (lowest..=highest).contains(percent));
        let (Some(first), last) = (offered.next(), offered.next_back()) else {
            return self;
        };
        let last = last.unwrap_or(first);
        // The offered steps are contiguous, so a step outside them is
        // nearest to the request at the end it passed.
        Self {
            step_percent: self.step_percent.clamp(first, last),
            ..self
        }
    }

    /// Whether Windows cannot show the request exactly (more than the
    /// rounding of the client's millimetres away from the step).
    #[must_use]
    pub const fn snapped(self) -> bool {
        self.requested_percent.abs_diff(self.step_percent) > 1
    }
}

fn choose_output_scale_from_ideal(
    ideal: f64,
    basis: ScaleBasis,
    supported: &[f64],
) -> OutputScaleChoice {
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

fn whole_percent(scale: f64) -> u16 {
    let percent = (scale * 100.0).round().clamp(1.0, f64::from(u16::MAX));
    // Clamped to the u16 range above.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let percent = percent as u16;
    percent
}

/// Whole millimetres from a client's reported size; zero when it is not a
/// usable length.
#[must_use]
pub fn whole_millimetres(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 && value < 100_000.0 {
        // Range-checked above; rounding to whole millimetres is the
        // precision clients report.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rounded = value.round() as u32;
        rounded
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chosen scale is copied from the supported set, never computed, so
    /// it must equal the expected one exactly.
    #[track_caller]
    fn assert_scale(actual: f64, expected: f64) {
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "scale {actual}, expected {expected}"
        );
    }

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
        assert_scale(choice.scale, 2.0);
        assert_eq!(choice.basis, ScaleBasis::RequestedDensity);
        assert!((choice.ideal - 2.0).abs() < 0.01);
    }

    #[test]
    fn a_point_stream_at_automatic_stays_unscaled() {
        // The same panel streamed in points, or the backing pixels at an
        // explicit 100%: 96 DPI.
        assert_scale(
            choose_output_scale(&deck(1512, 982, 100), &INTEGER).scale,
            1.0,
        );
        assert_scale(
            choose_output_scale(&deck(3024, 1964, 100), &INTEGER).scale,
            1.0,
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
        assert_scale(choice.scale, 1.0);
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
        assert_scale(choice.scale, 2.0);
        assert_eq!(choice.basis, ScaleBasis::BackingPixels);
        assert!((choice.ideal - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_requested_percentage_takes_the_nearest_supported_scale() {
        let fractional = [1.0, 1.25, 1.5, 1.75, 2.0];
        assert_scale(
            choose_output_scale(&deck(2560, 1440, 150), &fractional).scale,
            1.5,
        );
        assert_scale(
            choose_output_scale(&deck(2560, 1440, 175), &fractional).scale,
            1.75,
        );
        assert_scale(
            choose_output_scale(&deck(2560, 1440, 300), &[1.0, 2.0, 3.0]).scale,
            3.0,
        );
        // Integer-only: 125% is nearer 1, 175% nearer 2.
        assert_scale(
            choose_output_scale(&deck(2560, 1440, 125), &INTEGER).scale,
            1.0,
        );
        assert_scale(
            choose_output_scale(&deck(2560, 1440, 175), &INTEGER).scale,
            2.0,
        );
    }

    #[test]
    fn halfway_between_two_scales_takes_the_smaller() {
        // 150% with only 100% and 200%: millimetre rounding must not tip it.
        for (width, height) in [(1920, 1080), (2560, 1440), (3024, 1964), (1366, 768)] {
            assert_eq!(
                choose_output_scale(&deck(width, height, 150), &INTEGER)
                    .scale
                    .to_bits(),
                1.0_f64.to_bits(),
                "{width}x{height}"
            );
        }
        // The supported set's order does not matter.
        assert_scale(
            choose_output_scale(&deck(1920, 1080, 150), &[2.0, 1.0]).scale,
            1.0,
        );
    }

    #[test]
    fn unknown_size_or_an_output_without_scales_is_unscaled() {
        let choice = choose_output_scale(&facts(3840, 2160, 0, 0), &INTEGER);
        assert_scale(choice.scale, 1.0);
        assert_eq!(choice.basis, ScaleBasis::Unknown);
        assert_scale(choose_output_scale(&deck(3024, 1964, 200), &[]).scale, 1.0);
        assert_scale(
            choose_output_scale(&deck(3024, 1964, 200), &[1.0]).scale,
            1.0,
        );
        assert_scale(
            choose_output_scale(&deck(3024, 1964, 200), &[f64::NAN, 0.0]).scale,
            1.0,
        );
    }

    /// What a Deck advertises for an effective `ratio` (the Remote UI
    /// scale times the stream's backing ratio).
    fn deck_ratio(width_px: u32, height_px: u32, ratio: f64) -> OutputScaleFacts {
        let millimetres = |pixels: u32| {
            #[allow(clippy::cast_possible_truncation)]
            let value = (f64::from(pixels) * 25.4 / (96.0 * ratio)) as f32;
            whole_millimetres(value)
        };
        facts(
            width_px,
            height_px,
            millimetres(width_px),
            millimetres(height_px),
        )
    }

    #[test]
    fn a_windows_monitor_gets_the_nearest_windows_step() {
        // A 1.5x scaled Retina mode streamed in backing pixels (2880x1800):
        // 125%, 175% and 250% ask for 187.5%, 262.5% and 375%, which
        // Windows does not have; halfway takes the smaller step.
        for (ratio, requested, step) in [(1.875, 188, 175), (2.625, 263, 250), (3.75, 375, 350)] {
            let scale = WindowsUiScale::from_facts(&deck_ratio(2880, 1800, ratio));
            assert_eq!(
                (scale.requested_percent, scale.step_percent, scale.basis),
                (requested, step, ScaleBasis::RequestedDensity),
                "{ratio}"
            );
            assert!(scale.snapped());
        }
        // Every menu value on a 2x stream is a Windows step.
        for percent in [100_u16, 125, 150, 175, 200] {
            let effective = f64::from(percent) * 2.0 / 100.0;
            let scale = WindowsUiScale::from_facts(&deck_ratio(5120, 2880, effective));
            assert_eq!(scale.step_percent, percent * 2, "{percent}");
            assert!(!scale.snapped(), "{percent}");
        }
        // A fixed 150% on a point stream is 150%, not the stream's 100%.
        let scale = WindowsUiScale::from_facts(&OutputScaleFacts {
            logical_width: Some(1920),
            ..deck(1920, 1080, 150)
        });
        assert_eq!((scale.requested_percent, scale.step_percent), (150, 150));
    }

    #[test]
    fn a_windows_monitor_without_millimetres_takes_its_backing_ratio() {
        let scale = WindowsUiScale::from_facts(&OutputScaleFacts {
            logical_width: Some(1920),
            ..facts(3840, 2160, 0, 0)
        });
        assert_eq!(
            (scale.requested_percent, scale.step_percent, scale.basis),
            (200, 200, ScaleBasis::BackingPixels)
        );
        let unscaled = WindowsUiScale::from_facts(&facts(3840, 2160, 0, 0));
        assert_eq!(
            (unscaled.step_percent, unscaled.basis),
            (100, ScaleBasis::Unknown)
        );
        assert_eq!(
            WindowsUiScale::from_ratio(f64::NAN, ScaleBasis::Unknown).step_percent,
            100
        );
        assert_eq!(
            WindowsUiScale::from_ratio(9.0, ScaleBasis::Unknown).step_percent,
            500
        );
    }

    #[test]
    fn a_windows_monitor_gets_the_nearest_step_windows_offers_it() {
        // 250% on a 1.5x Retina stream asks for 375%; Windows offers a
        // 2880x1800 display 100..=300%.
        let scale = WindowsUiScale::from_percent(375, ScaleBasis::RequestedDensity);
        assert_eq!(scale.step_percent, 350);
        let served = scale.within(100, 300);
        assert_eq!(
            (served.requested_percent, served.step_percent, served.basis),
            (375, 300, ScaleBasis::RequestedDensity)
        );
        assert!(served.snapped());
        assert_eq!(scale.within(400, 500).step_percent, 400);
        assert_eq!(scale.within(100, 500).step_percent, 350);
        assert_eq!(scale.within(310, 340).step_percent, 350);
        // 187.5% keeps its halfway choice (175%) when Windows offers it.
        let retina_125 = WindowsUiScale::from_ratio(1.875, ScaleBasis::RequestedDensity);
        assert_eq!(retina_125.within(100, 300).step_percent, 175);
    }
}
