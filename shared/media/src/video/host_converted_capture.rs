// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! How large a picture a host-converted ten-bit capture may carry.
//!
//! The fidelity capture paths that cannot hand the encoder a GPU surface copy
//! every frame through system memory and convert it on the CPU (the Linux
//! depth-30 `XShm` path: packed RGB10 → 4:4:4 16-bit planes → one CUDA upload).
//! Their cost scales with the number of pixels, not with either edge, so the
//! budget is a pixel count — one 3840x2160 frame — and not a 3840 by 2160
//! box. A box refused a 3600x2262 Retina desktop (fewer pixels than UHD) and
//! the session fell through to the software encoder, which refuses Grading.
//!
//! [`fit_host_converted_capture`] is what the host arms the desktop at before
//! any capture exists; the capture adapter checks the same
//! [`host_converted_capture_fits`] rule, so the two cannot disagree.

/// Most pixels one host-converted capture frame may carry: one UHD frame.
pub const HOST_CONVERTED_CAPTURE_MAX_PIXELS: u64 = 3840 * 2160;

/// Longest edge a host-converted capture may have: the hardware encoder's own
/// contract ceiling (`EncoderBackend::NativeNvenc.contract()`).
pub const HOST_CONVERTED_CAPTURE_MAX_EDGE: u32 = 8192;

/// Whether a `width`x`height` capture is within the host-converted budget.
///
/// Zero, odd or over-long edges and more than
/// [`HOST_CONVERTED_CAPTURE_MAX_PIXELS`] pixels are refused.
#[must_use]
pub const fn host_converted_capture_fits(width: u32, height: u32) -> bool {
    width != 0
        && height != 0
        && width % 2 == 0
        && height % 2 == 0
        && width <= HOST_CONVERTED_CAPTURE_MAX_EDGE
        && height <= HOST_CONVERTED_CAPTURE_MAX_EDGE
        && (width as u64) * (height as u64) <= HOST_CONVERTED_CAPTURE_MAX_PIXELS
}

/// The geometry a host-converted capture of a requested `width`x`height`
/// desktop is served at.
///
/// A request inside the budget is returned unchanged. A larger one keeps its
/// aspect ratio and is scaled down until it fits, with the width a multiple
/// of four and the height even (what an X screen mode and a 4:4:4 encoder both
/// accept). A zero edge is returned unchanged for the caller to refuse.
#[must_use]
pub fn fit_host_converted_capture(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || host_converted_capture_fits(width, height) {
        return (width, height);
    }
    let area = f64::from(width) * f64::from(height);
    #[allow(clippy::cast_precision_loss)]
    let budget = HOST_CONVERTED_CAPTURE_MAX_PIXELS as f64;
    let max_edge = f64::from(HOST_CONVERTED_CAPTURE_MAX_EDGE);
    let scale = (budget / area)
        .sqrt()
        .min(max_edge / f64::from(width))
        .min(max_edge / f64::from(height))
        .min(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut fitted_width = ((f64::from(width) * scale) as u32) & !3;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let mut fitted_height = ((f64::from(height) * scale) as u32) & !1;
    // Rounding can only shrink; this loop guards the floating-point edge.
    while fitted_width >= 4
        && fitted_height >= 2
        && !host_converted_capture_fits(fitted_width, fitted_height)
    {
        fitted_width -= 4;
        fitted_height =
            u32::try_from(u64::from(fitted_width) * u64::from(height) / u64::from(width))
                .unwrap_or(u32::MAX)
                & !1;
    }
    (fitted_width.max(4), fitted_height.max(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retina_desktop_with_fewer_pixels_than_uhd_fits_unchanged() {
        // 3600x2262 is taller than 2160 but has fewer pixels than one UHD
        // frame.
        assert!(host_converted_capture_fits(3600, 2262));
        assert_eq!(fit_host_converted_capture(3600, 2262), (3600, 2262));
        assert_eq!(fit_host_converted_capture(3840, 2160), (3840, 2160));
        assert_eq!(fit_host_converted_capture(1800, 1130), (1800, 1130));
    }

    #[test]
    fn a_larger_desktop_is_scaled_into_the_budget_keeping_its_aspect() {
        for (width, height) in [(5120, 2880), (6016, 3384), (3840, 2400), (7680, 4320)] {
            let (fitted_width, fitted_height) = fit_host_converted_capture(width, height);
            assert!(host_converted_capture_fits(fitted_width, fitted_height));
            assert_eq!(fitted_width % 4, 0);
            let requested = f64::from(width) / f64::from(height);
            let served = f64::from(fitted_width) / f64::from(fitted_height);
            assert!((requested - served).abs() < 0.01, "{width}x{height}");
            // Not needlessly small: within 1% of the pixel budget.
            #[allow(clippy::cast_precision_loss)]
            let used = (u64::from(fitted_width) * u64::from(fitted_height)) as f64
                / HOST_CONVERTED_CAPTURE_MAX_PIXELS as f64;
            assert!(
                used > 0.99,
                "{width}x{height} -> {fitted_width}x{fitted_height}"
            );
        }
        assert_eq!(fit_host_converted_capture(5120, 2880), (3840, 2160));
    }

    #[test]
    fn odd_zero_and_over_long_edges_never_fit() {
        assert!(!host_converted_capture_fits(0, 1080));
        assert!(!host_converted_capture_fits(1921, 1080));
        assert!(!host_converted_capture_fits(1920, 1081));
        assert!(!host_converted_capture_fits(8194, 1000));
        let (width, height) = fit_host_converted_capture(16_000, 400);
        assert!(
            host_converted_capture_fits(width, height),
            "{width}x{height}"
        );
        assert_eq!(fit_host_converted_capture(0, 1080), (0, 1080));
    }
}
