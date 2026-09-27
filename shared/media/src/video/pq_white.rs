//! Where SDR white sits inside a PQ stream, and how to move it.
//!
//! PQ is absolute: a code is a luminance in nits, not a fraction of the
//! display. So an HDR desktop has to decide how bright its ordinary windows —
//! SDR content, most of any desktop — are, and every Pier has to decide the
//! same way or the same Deck shows one host's desktop at half the brightness
//! of another's.
//!
//! Arcen's contract is ITU-R BT.2408: SDR/graphics white at
//! [`GRAPHICS_WHITE_NITS`], 203 nits. A capture path that delivers PQ with
//! white somewhere else is rescaled in linear light, which keeps HDR
//! highlights in proportion: at 203-nit white, macOS's five-times headroom
//! peaks at about 1000 nits, the HDR10 grade the Deck presents against.

use super::linear_nits_to_pq_signal;

/// BT.2408 reference white for graphics and SDR content in PQ, in nits.
pub const GRAPHICS_WHITE_NITS: f64 = 203.0;

/// Where macOS `ScreenCaptureKit` puts SDR white in its PQ output, in nits.
///
/// Measured on the lab, not taken from documentation: with the same content
/// captured SDR and HDR, the brightest SDR code corresponded to 58.6 nits in
/// PQ at 0.62 of white in linear light, which puts white at about 95 nits;
/// the canonical-display reference measured the same. Apple's convention of
/// 100 nits for extended-range 1.0 is the value used.
pub const MACOS_CAPTURE_WHITE_NITS: f64 = 100.0;

/// Where Windows composes SDR content in an HDR output's scRGB, in nits, from
/// the `DISPLAYCONFIG_SDR_WHITE_LEVEL` value (a multiple of 80 nits, times
/// 1000). The lab's GRID HDR output reports 3000, 240 nits: Windows 11's
/// default, and what the Deck measured as the desktop's white before this
/// was applied. `None` for zero, which a failed query leaves.
#[must_use]
pub fn windows_sdr_white_level_nits(level: u32) -> Option<f64> {
    (level > 0).then(|| f64::from(level) / 1000.0 * 80.0)
}

/// The linear-light gain that moves `source_white_nits` to
/// [`GRAPHICS_WHITE_NITS`]. A non-positive or non-finite source is left
/// unscaled rather than guessed at.
#[must_use]
pub fn reference_white_gain(source_white_nits: f64) -> f64 {
    if source_white_nits.is_finite() && source_white_nits > 0.0 {
        GRAPHICS_WHITE_NITS / source_white_nits
    } else {
        1.0
    }
}

/// The reference remap of one full-range 10-bit PQ code by `gain` in linear
/// light, clamped at PQ's 10 000-nit ceiling.
///
/// A GPU stage applies this to every component; this is what it is tested
/// against.
#[must_use]
pub fn rescale_pq_code(code: u16, gain: f64) -> u16 {
    let nits = pq_code_to_nits(code) * gain;
    #[allow(clippy::cast_possible_truncation)]
    let signal = f64::from(linear_nits_to_pq_signal(nits as f32));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        (signal * 1023.0).round().clamp(0.0, 1023.0) as u16
    }
}

/// SMPTE ST 2084 EOTF for a full-range 10-bit code, in nits.
#[must_use]
pub fn pq_code_to_nits(code: u16) -> f64 {
    const M1: f64 = 2610.0 / 16_384.0;
    const M2: f64 = 2523.0 / 32.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 128.0;
    const C3: f64 = 2392.0 / 128.0;
    let signal = f64::from(code.min(1023)) / 1023.0;
    let power = signal.powf(1.0 / M2);
    ((power - C1).max(0.0) / (C2 - C3 * power)).powf(1.0 / M1) * 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_white_is_moved_to_graphics_white() {
        let gain = reference_white_gain(MACOS_CAPTURE_WHITE_NITS);
        assert!((gain - 2.03).abs() < 1e-9);
        // 100 nits is code 520; 203 nits is code 594.
        assert_eq!(rescale_pq_code(520, gain), 594);
    }

    #[test]
    fn highlights_keep_their_proportion_and_the_ceiling_holds() {
        let gain = reference_white_gain(MACOS_CAPTURE_WHITE_NITS);
        // Five times SDR white at the source stays five times at the output.
        let five_times = rescale_pq_code(rescale_pq_code(520, 5.0), gain);
        let expected = (pq_code_to_nits(594) * 5.0).round();
        assert!((pq_code_to_nits(five_times) - expected).abs() / expected < 0.02);
        assert_eq!(rescale_pq_code(1023, gain), 1023);
        assert_eq!(rescale_pq_code(0, gain), 0);
    }

    #[test]
    fn an_unknown_source_is_not_rescaled() {
        assert!((reference_white_gain(0.0) - 1.0).abs() < f64::EPSILON);
        assert!((reference_white_gain(f64::NAN) - 1.0).abs() < f64::EPSILON);
        assert_eq!(rescale_pq_code(700, 1.0), 700);
    }

    #[test]
    fn the_curve_matches_its_reference_points() {
        assert!((pq_code_to_nits(520) - 100.0).abs() < 3.0);
        assert!((pq_code_to_nits(594) - 203.0).abs() < 5.0);
        assert!((pq_code_to_nits(1023) - 10_000.0).abs() < 1.0);
    }
}
