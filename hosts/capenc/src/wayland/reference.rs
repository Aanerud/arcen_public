//! CPU reference for the host-copy route.
//!
//! The host-copy upload must hand the encoder exactly the producer's pixels
//! with row padding removed. [`host_copy_reference`] is that expectation; the
//! capture harness compares a CUDA read-back of the real upload against it.
//! The tests pin the conversion side: the shared `video::convert` functions
//! see the same frame whether it arrives padded or tight, and the unused BGRx
//! byte (not `0xff` on GNOME 40) never influences a converted sample.

/// Tightly packed BGRA rows of a `width` x `height` frame stored `stride`
/// bytes apart in `rows`.
///
/// # Panics
///
/// When `rows` is shorter than the frame; callers pass validated planes.
#[must_use]
pub fn host_copy_reference(rows: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let row_bytes = width * 4;
    let mut packed = Vec::with_capacity(row_bytes * height);
    for row in 0..height {
        packed.extend_from_slice(&rows[row * stride..row * stride + row_bytes]);
    }
    packed
}

/// How a GPU-imported frame compares with the host-copy reference of the
/// same desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameComparison {
    /// Pixels compared.
    pub pixels: usize,
    /// Pixels whose B, G or R byte differs.
    pub mismatched: usize,
    /// Largest difference of one B, G or R byte.
    pub max_channel_diff: u8,
}

impl FrameComparison {
    /// Most pixels a verified import may differ in, per million: room for a
    /// caret or clock that changed between the two captures, never for a
    /// wrong tiling, swizzle, row order or stale frame (which differ almost
    /// everywhere).
    pub const MAX_MISMATCH_PER_MILLION: usize = 1000;

    /// Whether the import reproduced the reference.
    #[must_use]
    pub const fn verified(&self) -> bool {
        self.pixels > 0
            && self.mismatched.saturating_mul(1_000_000)
                <= self.pixels.saturating_mul(Self::MAX_MISMATCH_PER_MILLION)
    }
}

/// Compares two tightly packed BGRA/BGRx frames of equal size, ignoring the
/// fourth byte (`X` in BGRx is undefined; an import of it reads opaque).
#[must_use]
pub fn compare_bgr(imported: &[u8], reference: &[u8]) -> FrameComparison {
    let pixels = imported.len().min(reference.len()) / 4;
    let mut mismatched = 0;
    let mut max_channel_diff = 0u8;
    for (a, b) in imported
        .chunks_exact(4)
        .zip(reference.chunks_exact(4))
        .take(pixels)
    {
        let diff = (0..3)
            .map(|channel| a[channel].abs_diff(b[channel]))
            .max()
            .unwrap_or(0);
        if diff > 0 {
            mismatched += 1;
            max_channel_diff = max_channel_diff.max(diff);
        }
    }
    let mismatched = if imported.len() == reference.len() {
        mismatched
    } else {
        // A size mismatch is a failed import, never a partial match.
        pixels.max(1)
    };
    FrameComparison {
        pixels,
        mismatched,
        max_channel_diff,
    }
}

/// Whether `reference` can tell a correct import from one with red and blue
/// exchanged: a frame of greys (or one colour) cannot, so it proves nothing
/// about the import's channel order.
#[must_use]
pub fn discriminates_channel_order(reference: &[u8]) -> bool {
    let mut swapped = reference.to_vec();
    for pixel in swapped.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    !compare_bgr(&swapped, reference).verified()
}

#[cfg(test)]
mod tests {
    use arcen_keel::BgraFrame;
    use arcen_media::video::{
        convert_bgra_to_i420, convert_bgra_to_i444, convert_bgra_to_nv12, ColorTransform,
        I420FrameMut, I444FrameMut, Nv12FrameMut,
    };
    use arcen_media::{BitDepth, ColorMatrix, ColorRange};

    use super::*;

    /// A deterministic BGRx frame with `stride` padding and a non-0xff X byte.
    fn padded_frame(width: usize, height: usize, stride: usize, x_byte: u8) -> Vec<u8> {
        let mut bytes = vec![0xA5; stride * height];
        for row in 0..height {
            for column in 0..width {
                let at = row * stride + column * 4;
                bytes[at] = u8::try_from((column * 7 + row * 3) % 256).unwrap();
                bytes[at + 1] = u8::try_from((column * 11 + row * 5 + 40) % 256).unwrap();
                bytes[at + 2] = u8::try_from((column * 13 + row * 17 + 90) % 256).unwrap();
                bytes[at + 3] = x_byte;
            }
        }
        bytes
    }

    fn transform() -> ColorTransform {
        ColorTransform::new(ColorMatrix::Bt709, ColorRange::Limited, BitDepth::Eight)
    }

    fn i420(bytes: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
        let (w, h) = (
            u32::try_from(width).unwrap(),
            u32::try_from(height).unwrap(),
        );
        let chroma = width.div_ceil(2) * height.div_ceil(2);
        let mut y = vec![0; width * height];
        let mut u = vec![0; chroma];
        let mut v = vec![0; chroma];
        let mut destination = I420FrameMut::new(
            w,
            h,
            &mut y,
            width,
            &mut u,
            width.div_ceil(2),
            &mut v,
            width.div_ceil(2),
        )
        .unwrap();
        let source = BgraFrame::new(bytes, width, height, stride).unwrap();
        convert_bgra_to_i420(source, &mut destination, transform()).unwrap();
        [y, u, v].concat()
    }

    fn nv12(bytes: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
        let (w, h) = (
            u32::try_from(width).unwrap(),
            u32::try_from(height).unwrap(),
        );
        let mut y = vec![0; width * height];
        let mut uv = vec![0; width * height / 2];
        let mut destination = Nv12FrameMut::new(w, h, &mut y, width, &mut uv, width).unwrap();
        let source = BgraFrame::new(bytes, width, height, stride).unwrap();
        convert_bgra_to_nv12(source, &mut destination, transform()).unwrap();
        [y, uv].concat()
    }

    fn i444(bytes: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
        let (w, h) = (
            u32::try_from(width).unwrap(),
            u32::try_from(height).unwrap(),
        );
        let (mut a, mut b, mut c) = (
            vec![0; width * height],
            vec![0; width * height],
            vec![0; width * height],
        );
        let mut destination =
            I444FrameMut::new(w, h, [&mut a, &mut b, &mut c], [width, width, width]).unwrap();
        let source = BgraFrame::new(bytes, width, height, stride).unwrap();
        convert_bgra_to_i444(source, &mut destination, transform()).unwrap();
        [a, b, c].concat()
    }

    #[test]
    fn reference_drops_padding_and_keeps_pixels() {
        let (width, height, stride) = (5, 3, 32);
        let frame = padded_frame(width, height, stride, 0xcf);
        let packed = host_copy_reference(&frame, stride, width, height);
        assert_eq!(packed.len(), width * height * 4);
        assert!(packed.chunks_exact(4).all(|pixel| pixel[3] == 0xcf));
        assert_eq!(
            &packed[20..24],
            &frame[32..36],
            "row 1 starts after padding"
        );
    }

    #[test]
    fn padded_and_tight_frames_convert_identically() {
        // Odd 4:4:4 size and an even 4:2:0 size, each with 64-byte row
        // alignment as a producer might allocate.
        for (width, height) in [(37usize, 23usize), (64, 18)] {
            let stride = (width * 4).div_ceil(64) * 64;
            let padded = padded_frame(width, height, stride, 0xcf);
            let tight = host_copy_reference(&padded, stride, width, height);
            assert_eq!(
                i444(&padded, width, height, stride),
                i444(&tight, width, height, width * 4)
            );
            if width % 2 == 0 && height % 2 == 0 {
                assert_eq!(
                    i420(&padded, width, height, stride),
                    i420(&tight, width, height, width * 4)
                );
                assert_eq!(
                    nv12(&padded, width, height, stride),
                    nv12(&tight, width, height, width * 4)
                );
            }
        }
    }

    #[test]
    fn bgrx_padding_byte_never_reaches_a_sample() {
        let (width, height) = (16, 8);
        let stride = width * 4;
        let opaque = padded_frame(width, height, stride, 0xff);
        let mutter = padded_frame(width, height, stride, 0xcf);
        let zero = padded_frame(width, height, stride, 0);
        let reference = i420(&opaque, width, height, stride);
        assert_eq!(i420(&mutter, width, height, stride), reference);
        assert_eq!(i420(&zero, width, height, stride), reference);
        assert_eq!(
            i444(&mutter, width, height, stride),
            i444(&opaque, width, height, stride)
        );
    }

    #[test]
    fn the_import_comparison_ignores_x_and_catches_swizzles() {
        use super::{compare_bgr, FrameComparison};
        let reference: Vec<u8> = (0..1000u32)
            .flat_map(|pixel| [(pixel % 251) as u8, (pixel % 7) as u8 * 30, 200, 0x11])
            .collect();
        let mut opaque = reference.clone();
        for pixel in opaque.chunks_exact_mut(4) {
            pixel[3] = 0xff;
        }
        let same = compare_bgr(&opaque, &reference);
        assert_eq!(same.mismatched, 0);
        assert!(same.verified());
        // Red and blue exchanged: wrong almost everywhere.
        let mut swapped = opaque.clone();
        for pixel in swapped.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        assert!(!compare_bgr(&swapped, &reference).verified());
        assert!(super::discriminates_channel_order(&reference));
        let grey: Vec<u8> = (0..1000u32).flat_map(|_| [90, 90, 90, 0]).collect();
        assert!(!super::discriminates_channel_order(&grey));
        // One changed pixel in a million-pixel budget passes; a size
        // mismatch never does.
        let mut caret = opaque.clone();
        caret[0] ^= 0x80;
        let one = compare_bgr(&caret, &reference);
        assert_eq!(one.mismatched, 1);
        assert_eq!(one.max_channel_diff, 0x80);
        assert!(one.verified());
        assert!(!compare_bgr(&opaque[4..], &reference).verified());
        assert!(!FrameComparison {
            pixels: 0,
            mismatched: 0,
            max_channel_diff: 0
        }
        .verified());
    }
}
