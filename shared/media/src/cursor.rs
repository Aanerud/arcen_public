//! Host cursor images.
//!
//! A compositor that reports its cursor separately from the video (a
//! Wayland `ScreenCast` stream in `metadata` cursor mode) hands over a small
//! bitmap and a hotspot. This module turns that bitmap into a bounded,
//! straight-alpha [`CursorImage`], fits it to the wire
//! ([`arcen_protocol::cursor`]), encodes and decodes its PNG, and prepares it
//! for presentation as a native cursor on the Deck.
//!
//! Pixels are always 8-bit sRGB RGBA in memory order R, G, B, A. Resampling
//! happens on premultiplied values so that transparent pixels never bleed
//! their colour into the result.

use std::sync::Arc;

use arcen_protocol::cursor::{MAX_CURSOR_PNG_BYTES, MAX_CURSOR_SIDE};

use crate::clipboard::image::{ImageLimits, decode_png_rgba, encode_png_rgba, image_info};

/// Largest source bitmap side a host accepts from its compositor. Mutter
/// offers cursor metadata only with room for a 384x384 bitmap.
pub const MAX_SOURCE_CURSOR_SIDE: u32 = 384;
/// Largest presented cursor side. Bounds the Deck's resampled image when
/// the video is shown larger than its stream size.
pub const MAX_PRESENTED_CURSOR_SIDE: u32 = 512;

/// Why a cursor image was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorImageError {
    /// Zero or oversized dimensions.
    Dimensions {
        /// Observed width.
        width: u32,
        /// Observed height.
        height: u32,
    },
    /// The pixel buffer does not hold exactly `width * height` pixels.
    Length,
    /// The PNG would not fit the wire even at the smallest size tried.
    TooLargeForWire,
    /// The PNG is malformed, unsupported, oversized, or disagrees with the
    /// size its record declared.
    Png,
}

impl std::fmt::Display for CursorImageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dimensions { width, height } => {
                write!(formatter, "cursor image {width}x{height} is out of range")
            }
            Self::Length => formatter.write_str("cursor image pixel buffer has the wrong length"),
            Self::TooLargeForWire => {
                formatter.write_str("cursor image does not fit the wire at any tried size")
            }
            Self::Png => formatter.write_str("cursor image PNG is invalid"),
        }
    }
}

impl std::error::Error for CursorImageError {}

/// A validated straight-alpha cursor image with its hotspot inside it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CursorImage {
    width: u32,
    height: u32,
    hotspot: (u32, u32),
    rgba: Arc<[u8]>,
}

fn pixel_bytes(width: u32, height: u32) -> Result<usize, CursorImageError> {
    if width == 0
        || height == 0
        || width > MAX_SOURCE_CURSOR_SIDE
        || height > MAX_SOURCE_CURSOR_SIDE
    {
        return Err(CursorImageError::Dimensions { width, height });
    }
    Ok(width as usize * height as usize * 4)
}

fn clamp_hotspot(hotspot: (i64, i64), width: u32, height: u32) -> (u32, u32) {
    let clamp = |value: i64, size: u32| -> u32 {
        u32::try_from(value.clamp(0, i64::from(size) - 1)).unwrap_or(0)
    };
    (clamp(hotspot.0, width), clamp(hotspot.1, height))
}

fn unpremultiply(value: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        0
    } else {
        let scaled = (u32::from(value) * 255 + u32::from(alpha) / 2) / u32::from(alpha);
        u8::try_from(scaled).unwrap_or(u8::MAX)
    }
}

fn premultiply(value: u8, alpha: u8) -> u8 {
    u8::try_from((u32::from(value) * u32::from(alpha) + 127) / 255).unwrap_or(u8::MAX)
}

impl CursorImage {
    /// Wraps straight-alpha RGBA pixels. A hotspot outside the image is
    /// clamped to its nearest edge pixel.
    ///
    /// # Errors
    ///
    /// [`CursorImageError`] for out-of-range dimensions or a buffer of the
    /// wrong length.
    pub fn from_straight_rgba(
        width: u32,
        height: u32,
        hotspot: (i64, i64),
        rgba: Vec<u8>,
    ) -> Result<Self, CursorImageError> {
        if rgba.len() != pixel_bytes(width, height)? {
            return Err(CursorImageError::Length);
        }
        Ok(Self {
            width,
            height,
            hotspot: clamp_hotspot(hotspot, width, height),
            rgba: rgba.into(),
        })
    }

    /// Converts premultiplied RGBA (Mutter's `SPA_VIDEO_FORMAT_RGBA` cursor
    /// bitmap, read from a `RGBA_8888_PRE` texture), tightly packed.
    ///
    /// # Errors
    ///
    /// As [`Self::from_straight_rgba`].
    pub fn from_premultiplied_rgba(
        width: u32,
        height: u32,
        hotspot: (i64, i64),
        pixels: &[u8],
    ) -> Result<Self, CursorImageError> {
        if pixels.len() != pixel_bytes(width, height)? {
            return Err(CursorImageError::Length);
        }
        let mut rgba = Vec::with_capacity(pixels.len());
        for pixel in pixels.chunks_exact(4) {
            let alpha = pixel[3];
            rgba.extend_from_slice(&[
                unpremultiply(pixel[0], alpha),
                unpremultiply(pixel[1], alpha),
                unpremultiply(pixel[2], alpha),
                alpha,
            ]);
        }
        Self::from_straight_rgba(width, height, hotspot, rgba)
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Hotspot within the image.
    #[must_use]
    pub const fn hotspot(&self) -> (u32, u32) {
        self.hotspot
    }

    /// Straight-alpha RGBA pixels.
    #[must_use]
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }

    /// Whether no pixel is visible at all.
    #[must_use]
    pub fn is_fully_transparent(&self) -> bool {
        self.rgba.chunks_exact(4).all(|pixel| pixel[3] == 0)
    }

    /// Crops fully transparent borders, keeping the hotspot inside the
    /// result. `None` when no pixel is visible.
    #[must_use]
    pub fn trimmed(&self) -> Option<Self> {
        let (width, height) = (self.width as usize, self.height as usize);
        let alpha = |x: usize, y: usize| self.rgba[(y * width + x) * 4 + 3];
        let mut bounds: Option<(usize, usize, usize, usize)> = None;
        for y in 0..height {
            for x in 0..width {
                if alpha(x, y) != 0 {
                    bounds = Some(match bounds {
                        None => (x, y, x, y),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    });
                }
            }
        }
        let (mut x0, mut y0, mut x1, mut y1) = bounds?;
        let (hx, hy) = (self.hotspot.0 as usize, self.hotspot.1 as usize);
        x0 = x0.min(hx);
        y0 = y0.min(hy);
        x1 = x1.max(hx);
        y1 = y1.max(hy);
        if (x0, y0, x1, y1) == (0, 0, width - 1, height - 1) {
            return Some(self.clone());
        }
        let (new_width, new_height) = (x1 - x0 + 1, y1 - y0 + 1);
        let mut rgba = Vec::with_capacity(new_width * new_height * 4);
        for y in y0..=y1 {
            let start = (y * width + x0) * 4;
            rgba.extend_from_slice(&self.rgba[start..start + new_width * 4]);
        }
        Some(Self {
            width: u32::try_from(new_width).ok()?,
            height: u32::try_from(new_height).ok()?,
            hotspot: (u32::try_from(hx - x0).ok()?, u32::try_from(hy - y0).ok()?),
            rgba: rgba.into(),
        })
    }

    /// Resamples to `width` x `height` (area average when shrinking,
    /// nearest pixel when growing), on premultiplied values. Returns
    /// premultiplied RGBA.
    fn resample_premultiplied(&self, width: u32, height: u32) -> Vec<u8> {
        let (source_width, source_height) = (self.width as usize, self.height as usize);
        let (width, height) = (width as usize, height as usize);
        let premultiplied: Vec<[u32; 4]> = self
            .rgba
            .chunks_exact(4)
            .map(|pixel| {
                let alpha = pixel[3];
                [
                    u32::from(premultiply(pixel[0], alpha)),
                    u32::from(premultiply(pixel[1], alpha)),
                    u32::from(premultiply(pixel[2], alpha)),
                    u32::from(alpha),
                ]
            })
            .collect();
        let mut out = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            let sy0 = y * source_height / height;
            let sy1 = ((y + 1) * source_height).div_ceil(height).max(sy0 + 1);
            for x in 0..width {
                let sx0 = x * source_width / width;
                let sx1 = ((x + 1) * source_width).div_ceil(width).max(sx0 + 1);
                let mut sum = [0u32; 4];
                let mut count = 0u32;
                for sy in sy0..sy1.min(source_height) {
                    for sx in sx0..sx1.min(source_width) {
                        let pixel = premultiplied[sy * source_width + sx];
                        for (total, value) in sum.iter_mut().zip(pixel) {
                            *total += value;
                        }
                        count += 1;
                    }
                }
                let count = count.max(1);
                for total in sum {
                    out.push(u8::try_from((total + count / 2) / count).unwrap_or(u8::MAX));
                }
            }
        }
        out
    }

    fn scaled_hotspot(&self, width: u32, height: u32) -> (i64, i64) {
        let scale = |value: u32, from: u32, to: u32| -> i64 {
            i64::from(value) * i64::from(to) / i64::from(from)
        };
        (
            scale(self.hotspot.0, self.width, width),
            scale(self.hotspot.1, self.height, height),
        )
    }

    /// Shrinks the image so that neither side exceeds `max_side`, keeping
    /// its aspect ratio. Unchanged when it already fits.
    #[must_use]
    pub fn fit_within(&self, max_side: u32) -> Self {
        let max_side = max_side.max(1);
        let longest = self.width.max(self.height);
        if longest <= max_side {
            return self.clone();
        }
        let width = (u64::from(self.width) * u64::from(max_side) / u64::from(longest)).max(1);
        let height = (u64::from(self.height) * u64::from(max_side) / u64::from(longest)).max(1);
        let (width, height) = (
            u32::try_from(width).unwrap_or(max_side),
            u32::try_from(height).unwrap_or(max_side),
        );
        let premultiplied = self.resample_premultiplied(width, height);
        let mut rgba = Vec::with_capacity(premultiplied.len());
        for pixel in premultiplied.chunks_exact(4) {
            let alpha = pixel[3];
            rgba.extend_from_slice(&[
                unpremultiply(pixel[0], alpha),
                unpremultiply(pixel[1], alpha),
                unpremultiply(pixel[2], alpha),
                alpha,
            ]);
        }
        Self {
            width,
            height,
            hotspot: clamp_hotspot(self.scaled_hotspot(width, height), width, height),
            rgba: rgba.into(),
        }
    }

    /// Encodes the image as a PNG of at most `max_bytes`.
    ///
    /// # Errors
    ///
    /// [`CursorImageError::TooLargeForWire`] when the PNG exceeds the cap.
    pub fn encode_png(&self, max_bytes: usize) -> Result<Vec<u8>, CursorImageError> {
        let limits = ImageLimits {
            max_encoded_bytes: max_bytes,
            max_decoded_bytes: self.rgba.len(),
            max_dimension: MAX_SOURCE_CURSOR_SIDE,
        };
        let info = image_info(self.width, self.height, limits).map_err(|_| {
            CursorImageError::Dimensions {
                width: self.width,
                height: self.height,
            }
        })?;
        encode_png_rgba(info, &self.rgba, max_bytes).map_err(|_| CursorImageError::TooLargeForWire)
    }

    /// Fits the image to the wire: trims transparent borders, shrinks it to
    /// [`MAX_CURSOR_SIDE`], and halves it again until its PNG fits
    /// [`MAX_CURSOR_PNG_BYTES`]. `Ok(None)` when no pixel is visible.
    ///
    /// # Errors
    ///
    /// [`CursorImageError::TooLargeForWire`] when even an 8-pixel image does
    /// not fit, which only a pathological producer reaches.
    pub fn to_wire(&self) -> Result<Option<WireCursorImage>, CursorImageError> {
        let Some(trimmed) = self.trimmed() else {
            return Ok(None);
        };
        let mut side = u32::from(MAX_CURSOR_SIDE);
        loop {
            let image = trimmed.fit_within(side);
            match image.encode_png(MAX_CURSOR_PNG_BYTES) {
                Ok(png) => return Ok(Some(WireCursorImage { image, png })),
                Err(CursorImageError::TooLargeForWire) if side > 8 => {
                    side = image.width.max(image.height) / 2;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Decodes a wire PNG whose record declared `width` x `height` and
    /// `hotspot`.
    ///
    /// # Errors
    ///
    /// [`CursorImageError::Png`] for an invalid PNG or one whose size
    /// disagrees with the record.
    pub fn from_wire_png(
        png: &[u8],
        width: u32,
        height: u32,
        hotspot: (u32, u32),
    ) -> Result<Self, CursorImageError> {
        let limits = ImageLimits {
            max_encoded_bytes: MAX_CURSOR_PNG_BYTES,
            max_decoded_bytes: pixel_bytes(width, height)?,
            max_dimension: u32::from(MAX_CURSOR_SIDE),
        };
        let (info, rgba) = decode_png_rgba(png, limits).map_err(|_| CursorImageError::Png)?;
        if (info.width, info.height) != (width, height) || hotspot.0 >= width || hotspot.1 >= height
        {
            return Err(CursorImageError::Png);
        }
        Self::from_straight_rgba(
            width,
            height,
            (i64::from(hotspot.0), i64::from(hotspot.1)),
            rgba,
        )
    }

    /// Prepares the image for a native cursor whose image is
    /// `points_per_pixel` display points per stream pixel: resampled to that
    /// size and premultiplied (as `NSBitmapImageRep` expects by default).
    #[must_use]
    pub fn present(&self, points_per_pixel: f32) -> PresentedCursor {
        let factor = if points_per_pixel.is_finite() && points_per_pixel > 0.0 {
            f64::from(points_per_pixel)
        } else {
            1.0
        };
        let size = |value: u32| -> u32 {
            let scaled = (f64::from(value) * factor).round();
            if scaled < 1.0 {
                1
            } else if scaled > f64::from(MAX_PRESENTED_CURSOR_SIDE) {
                MAX_PRESENTED_CURSOR_SIDE
            } else {
                // Bounded above, so the cast is exact.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let scaled = scaled as u32;
                scaled
            }
        };
        let (width, height) = (size(self.width), size(self.height));
        let rgba = self.resample_premultiplied(width, height);
        let (hx, hy) = clamp_hotspot(self.scaled_hotspot(width, height), width, height);
        PresentedCursor {
            rgba_premultiplied: rgba.into(),
            width: u16::try_from(width).unwrap_or(u16::MAX),
            height: u16::try_from(height).unwrap_or(u16::MAX),
            hotspot: (
                u16::try_from(hx).unwrap_or(0),
                u16::try_from(hy).unwrap_or(0),
            ),
        }
    }
}

/// An image fitted to the wire and its PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireCursorImage {
    /// The fitted image (what the receiver will decode).
    pub image: CursorImage,
    /// Its PNG, at most [`MAX_CURSOR_PNG_BYTES`].
    pub png: Vec<u8>,
}

/// A cursor ready for a native cursor API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedCursor {
    /// Premultiplied RGBA, `width * height * 4` bytes.
    pub rgba_premultiplied: Arc<[u8]>,
    /// Width in display points.
    pub width: u16,
    /// Height in display points.
    pub height: u16,
    /// Hotspot in display points, inside the image.
    pub hotspot: (u16, u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        rgba.repeat((width * height) as usize)
    }

    #[test]
    fn premultiplied_pixels_become_straight_alpha() {
        // Opaque and transparent pixels are exact; half alpha is rounded.
        let pixels = [
            10, 20, 30, 255, 0, 0, 0, 0, 64, 32, 0, 128, 255, 255, 255, 255,
        ];
        let image = CursorImage::from_premultiplied_rgba(2, 2, (0, 0), &pixels).unwrap();
        assert_eq!(&image.rgba()[0..4], &[10, 20, 30, 255]);
        assert_eq!(&image.rgba()[4..8], &[0, 0, 0, 0]);
        assert_eq!(&image.rgba()[8..12], &[128, 64, 0, 128]);
    }

    #[test]
    fn invalid_sources_are_refused_and_hotspots_clamped() {
        assert_eq!(
            CursorImage::from_straight_rgba(0, 4, (0, 0), vec![]),
            Err(CursorImageError::Dimensions {
                width: 0,
                height: 4
            })
        );
        assert_eq!(
            CursorImage::from_straight_rgba(385, 1, (0, 0), vec![0; 385 * 4]),
            Err(CursorImageError::Dimensions {
                width: 385,
                height: 1
            })
        );
        assert_eq!(
            CursorImage::from_straight_rgba(2, 2, (0, 0), vec![0; 15]),
            Err(CursorImageError::Length)
        );
        let image =
            CursorImage::from_straight_rgba(4, 4, (9, -3), solid(4, 4, [1, 2, 3, 255])).unwrap();
        assert_eq!(image.hotspot(), (3, 0));
    }

    #[test]
    fn trimming_keeps_the_hotspot_and_drops_empty_borders() {
        let mut rgba = solid(8, 8, [0, 0, 0, 0]);
        // One visible pixel at (5, 4); hotspot at (1, 1), outside it.
        let at = (4 * 8 + 5) * 4;
        rgba[at..at + 4].copy_from_slice(&[9, 9, 9, 255]);
        let image = CursorImage::from_straight_rgba(8, 8, (1, 1), rgba).unwrap();
        let trimmed = image.trimmed().unwrap();
        assert_eq!((trimmed.width(), trimmed.height()), (5, 4));
        assert_eq!(trimmed.hotspot(), (0, 0));
        let empty =
            CursorImage::from_straight_rgba(3, 3, (0, 0), solid(3, 3, [5, 5, 5, 0])).unwrap();
        assert!(empty.is_fully_transparent());
        assert!(empty.trimmed().is_none());
    }

    #[test]
    fn fitting_shrinks_without_colour_bleed() {
        let mut rgba = Vec::new();
        for index in 0..(384 * 192) {
            // Alternating opaque red and fully transparent green.
            rgba.extend_from_slice(if index % 2 == 0 {
                &[255, 0, 0, 255]
            } else {
                &[0, 255, 0, 0]
            });
        }
        let image = CursorImage::from_straight_rgba(384, 192, (383, 191), rgba).unwrap();
        let fitted = image.fit_within(96);
        assert_eq!((fitted.width(), fitted.height()), (96, 48));
        assert_eq!(fitted.hotspot(), (95, 47));
        let pixel = &fitted.rgba()[0..4];
        assert_eq!(pixel[0], 255, "transparent green must not tint red");
        assert_eq!(pixel[1], 0);
        assert!((120..=136).contains(&pixel[3]));
    }

    #[test]
    fn wire_png_round_trips_and_checks_its_declared_size() {
        let mut rgba = solid(24, 24, [0, 0, 0, 0]);
        for y in 2..20 {
            for x in 3..15 {
                let at = (y * 24 + x) * 4;
                rgba[at..at + 4].copy_from_slice(&[255, 255, 255, 200]);
            }
        }
        let image = CursorImage::from_straight_rgba(24, 24, (3, 2), rgba).unwrap();
        let wire = image.to_wire().unwrap().unwrap();
        assert_eq!((wire.image.width(), wire.image.height()), (12, 18));
        assert_eq!(wire.image.hotspot(), (0, 0));
        let decoded = CursorImage::from_wire_png(&wire.png, 12, 18, (0, 0)).unwrap();
        assert_eq!(decoded, wire.image);
        assert_eq!(
            CursorImage::from_wire_png(&wire.png, 13, 18, (0, 0)),
            Err(CursorImageError::Png)
        );
        assert_eq!(
            CursorImage::from_wire_png(&wire.png, 12, 18, (12, 0)),
            Err(CursorImageError::Png)
        );
        assert_eq!(
            CursorImage::from_wire_png(b"not a png", 12, 18, (0, 0)),
            Err(CursorImageError::Png)
        );
    }

    #[test]
    fn noisy_large_images_are_shrunk_until_they_fit_the_wire() {
        let mut state = 0x1234_5678_u32;
        let mut rgba = Vec::with_capacity(384 * 384 * 4);
        for _ in 0..(384 * 384 * 4) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            rgba.push(state.to_le_bytes()[0] | 1);
        }
        let image = CursorImage::from_straight_rgba(384, 384, (100, 100), rgba).unwrap();
        let wire = image.to_wire().unwrap().unwrap();
        assert!(wire.png.len() <= MAX_CURSOR_PNG_BYTES);
        assert!(wire.image.width() <= u32::from(MAX_CURSOR_SIDE));
        assert!(wire.image.width() < 256, "noise forces a halving step");
    }

    #[test]
    fn presentation_scales_and_premultiplies() {
        let image =
            CursorImage::from_straight_rgba(4, 4, (2, 2), solid(4, 4, [200, 100, 0, 128])).unwrap();
        let half = image.present(0.5);
        assert_eq!((half.width, half.height, half.hotspot), (2, 2, (1, 1)));
        assert_eq!(&half.rgba_premultiplied[0..4], &[100, 50, 0, 128]);
        let double = image.present(2.0);
        assert_eq!(
            (double.width, double.height, double.hotspot),
            (8, 8, (4, 4))
        );
        let invalid = image.present(f32::NAN);
        assert_eq!((invalid.width, invalid.height), (4, 4));
    }
}
