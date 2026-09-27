//! How many bits a picture of a given shape is worth.
//!
//! One formula, because a host that invents its own produces a stream the
//! others cannot be compared against. The Linux Pier has sized its encoder
//! this way since NVENC was added; this is that arithmetic with nothing
//! vendor-specific left in it.

use crate::{BitDepth, ChromaSubsampling};

/// Samples carried per pixel at each chroma subsampling.
const fn samples_per_pixel(chroma: ChromaSubsampling) -> f64 {
    match chroma {
        ChromaSubsampling::Yuv420 => 1.5,
        ChromaSubsampling::Yuv422 => 2.0,
        ChromaSubsampling::Yuv444 => 3.0,
    }
}

/// How much more a deeper sample costs.
///
/// Not the ratio of container sizes. Ten-bit video does not need 25% more
/// bits because the samples are wider; it needs them because the extra
/// precision is only worth carrying if the encoder is allowed to spend on it.
const fn depth_scale(depth: BitDepth) -> f64 {
    match depth {
        BitDepth::Eight => 1.0,
        BitDepth::Ten => 1.25,
        BitDepth::Twelve => 1.5,
    }
}

/// Bits per sample per second the ladder is built on.
const BASE_BITS_PER_SAMPLE: f64 = 0.05;

/// The average bitrate a stream of this shape should be encoded at.
///
/// Derived from what is actually being encoded — pixels, rate, chroma and
/// depth — rather than picked. A fixed figure is wrong in both directions at
/// once: generous enough to waste a link at 720p and mean enough to smear
/// 4K, and it moves the moment any of the four inputs changes.
///
/// Returns bits per second, clamped into `u32` because every encoder API that
/// consumes it takes a 32-bit value.
///
/// # Examples
///
/// ```
/// # use arcen_media::{BitDepth, ChromaSubsampling};
/// # use arcen_media::video::average_bitrate_bps;
/// let hd = average_bitrate_bps(1920, 1080, 30, ChromaSubsampling::Yuv420, BitDepth::Eight);
/// let uhd = average_bitrate_bps(3840, 2160, 30, ChromaSubsampling::Yuv420, BitDepth::Eight);
/// assert_eq!(uhd, hd * 4, "four times the pixels is four times the bits");
/// ```
#[must_use]
pub fn average_bitrate_bps(
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
) -> u32 {
    let pixels_per_second = f64::from(width) * f64::from(height) * f64::from(fps.max(1));
    let samples_per_second = pixels_per_second * samples_per_pixel(chroma);
    let bits_per_second = samples_per_second * depth_scale(depth) * BASE_BITS_PER_SAMPLE;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        bits_per_second.round().clamp(0.0, f64::from(u32::MAX)) as u32
    }
}

/// The average bitrate a Pier encodes a session at: [`average_bitrate_bps`],
/// held to what 1080p of the same cadence costs on the fast path.
///
/// The cap exists because hardware encoders spend their average rather than
/// treating it as a ceiling: measured against a real Deck, bytes per frame
/// matched the configured average to within one percent whether the desktop
/// moved or not. A target above what the link carries is therefore a
/// permanent backlog: frames wait, input feels late, and audio sharing the
/// path starves.
///
/// - 1080p at up to 30 fps is the reference, so anything at or below it is
///   untouched.
/// - A faster stream is billed at 30 fps: on the lab link, Speed's
///   frame-rate-scaled 9.3 Mbps delivered 42 fps with 382 ms p50 frame age,
///   and the capped figure 57.5 fps.
/// - 4:4:4 and ten bits get the fast path's own 1080p budget rather than the
///   2.5 times [`average_bitrate_bps`] charges them. At 1800x1130, 30 fps,
///   HEVC 4:4:4 10-bit: 4 Mbps held frame age p95 at 5 ms; 8 Mbps reached
///   590 ms, and the uncapped 10.8 Mbps 441 ms at 19 fps; 6 Mbps was the knee.
///   The Windows Pier's uncapped 22.9 Mbps for the same shape made HDR
///   sessions sluggish and dropped 12-22% of their audio.
#[must_use]
pub fn link_capped_average_bitrate_bps(
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
) -> u32 {
    const CAP_WIDTH: u32 = 1920;
    const CAP_HEIGHT: u32 = 1080;
    const CAP_FPS: u32 = 30;
    let requested = average_bitrate_bps(width, height, fps, chroma, depth);
    let high_fidelity = chroma == ChromaSubsampling::Yuv444 || depth != BitDepth::Eight;
    let cap = if high_fidelity {
        average_bitrate_bps(
            CAP_WIDTH,
            CAP_HEIGHT,
            fps.min(CAP_FPS),
            ChromaSubsampling::Yuv420,
            BitDepth::Eight,
        )
    } else {
        average_bitrate_bps(CAP_WIDTH, CAP_HEIGHT, fps.min(CAP_FPS), chroma, depth)
    };
    requested.min(cap)
}

#[cfg(test)]
mod link_cap_tests {
    use super::{average_bitrate_bps, link_capped_average_bitrate_bps};
    use crate::{BitDepth, ChromaSubsampling};

    const SDR: (ChromaSubsampling, BitDepth) = (ChromaSubsampling::Yuv420, BitDepth::Eight);

    #[test]
    fn at_or_below_1080p30_the_shared_sizing_is_untouched() {
        for (width, height) in [(1280, 720), (1800, 1130), (1920, 1080)] {
            assert_eq!(
                link_capped_average_bitrate_bps(width, height, 30, SDR.0, SDR.1),
                average_bitrate_bps(width, height, 30, SDR.0, SDR.1)
            );
        }
    }

    #[test]
    fn a_bigger_or_faster_stream_is_billed_as_1080p30() {
        let fast_path = average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        assert_eq!(
            link_capped_average_bitrate_bps(2560, 1440, 30, SDR.0, SDR.1),
            fast_path
        );
        assert_eq!(
            link_capped_average_bitrate_bps(1800, 1130, 60, SDR.0, SDR.1),
            fast_path
        );
    }

    #[test]
    fn fidelity_gets_the_fast_path_budget_not_two_and_a_half_times_it() {
        let fast_path = average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        let hdr = link_capped_average_bitrate_bps(
            1800,
            1130,
            30,
            ChromaSubsampling::Yuv444,
            BitDepth::Ten,
        );
        assert_eq!(
            hdr, fast_path,
            "the Windows HDR shape was 22.9 Mbps uncapped"
        );
        let small =
            link_capped_average_bitrate_bps(640, 360, 30, ChromaSubsampling::Yuv444, BitDepth::Ten);
        assert_eq!(
            small,
            average_bitrate_bps(640, 360, 30, ChromaSubsampling::Yuv444, BitDepth::Ten)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::average_bitrate_bps;
    use crate::{BitDepth, ChromaSubsampling};

    const SDR: (ChromaSubsampling, BitDepth) = (ChromaSubsampling::Yuv420, BitDepth::Eight);

    #[test]
    fn a_thirty_fps_1080p_stream_is_sized_sensibly() {
        // 1920*1080*30*1.5*1.0*0.05 = 4.665 Mbps, which is a reasonable figure
        // for desktop H.264 and nothing like the flat 20 Mbps macOS used.
        let bitrate = average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        assert_eq!(bitrate, 4_665_600);
    }

    #[test]
    fn doubling_the_frame_rate_doubles_the_budget() {
        let thirty = average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        let sixty = average_bitrate_bps(1920, 1080, 60, SDR.0, SDR.1);
        assert_eq!(sixty, thirty * 2);
    }

    #[test]
    fn four_four_four_costs_twice_four_two_zero() {
        let subsampled = average_bitrate_bps(1920, 1080, 30, ChromaSubsampling::Yuv420, SDR.1);
        let full = average_bitrate_bps(1920, 1080, 30, ChromaSubsampling::Yuv444, SDR.1);
        assert_eq!(full, subsampled * 2);
    }

    #[test]
    fn ten_bit_buys_a_quarter_more() {
        let eight = average_bitrate_bps(1920, 1080, 30, SDR.0, BitDepth::Eight);
        let ten = average_bitrate_bps(1920, 1080, 30, SDR.0, BitDepth::Ten);
        assert_eq!(ten, eight + eight / 4);
    }

    #[test]
    fn a_zero_frame_rate_is_treated_as_one_rather_than_producing_nothing() {
        // A zero here means a caller with no rate in hand. Sizing the encoder
        // at zero bits would refuse to encode at all.
        assert_eq!(
            average_bitrate_bps(1920, 1080, 0, SDR.0, SDR.1),
            average_bitrate_bps(1920, 1080, 1, SDR.0, SDR.1)
        );
    }

    #[test]
    fn an_absurd_shape_saturates_rather_than_wrapping() {
        let bitrate = average_bitrate_bps(
            u32::MAX,
            u32::MAX,
            u32::MAX,
            ChromaSubsampling::Yuv444,
            BitDepth::Twelve,
        );
        assert_eq!(bitrate, u32::MAX);
    }

    #[test]
    fn a_smaller_picture_costs_less() {
        let hd = average_bitrate_bps(1920, 1080, 30, SDR.0, SDR.1);
        let smaller = average_bitrate_bps(1280, 720, 30, SDR.0, SDR.1);
        assert!(smaller < hd, "720p must not be billed as 1080p");
    }
}
