// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use png::{BitDepth, ColorType, Decoder, Encoder, Limits, Transformations};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io::{Cursor, Error as IoError, ErrorKind, Write};
use zeroize::Zeroize;

use super::{HARD_MAX_CLIPBOARD_BYTES, MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_DIMENSION};

const DIBV5_HEADER_BYTES: usize = 124;
const DIBV5_HEADER_SIZE_FIELD: u32 = 124;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const RED_MASK: u32 = 0x00ff_0000;
const GREEN_MASK: u32 = 0x0000_ff00;
const BLUE_MASK: u32 = 0x0000_00ff;
const ALPHA_MASK: u32 = 0xff00_0000;
const LCS_SRGB: u32 = 0x7352_4742;
const LCS_GM_IMAGES: u32 = 4;

/// Independent encoded and decoded image limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// Maximum PNG wire bytes.
    pub max_encoded_bytes: usize,
    /// Maximum normalized RGBA/BGRA bytes.
    pub max_decoded_bytes: usize,
    /// Maximum accepted width or height.
    pub max_dimension: u32,
}

impl Default for ImageLimits {
    fn default() -> Self {
        Self {
            max_encoded_bytes: HARD_MAX_CLIPBOARD_BYTES,
            max_decoded_bytes: MAX_DECODED_IMAGE_BYTES,
            max_dimension: MAX_IMAGE_DIMENSION,
        }
    }
}

/// Validated normalized image metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Bytes required by normalized 8-bit RGBA.
    pub rgba_bytes: usize,
}

/// Bounded image validation or conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardImageError {
    /// Encoded PNG exceeds policy.
    EncodedSize {
        /// Observed bytes.
        bytes: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Width or height is zero or exceeds the configured maximum.
    Dimensions {
        /// Observed width.
        width: u32,
        /// Observed height.
        height: u32,
        /// Configured maximum dimension.
        maximum: u32,
    },
    /// Checked dimension, stride, or allocation arithmetic overflowed.
    ArithmeticOverflow,
    /// The source uses a format outside clipboard v1.
    UnsupportedFormat,
    /// The source is malformed or truncated.
    MalformedInput,
    /// The decoder's bounded allocation limit was exceeded.
    DecoderLimit,
    /// PNG encoding exceeded the configured output cap.
    CappedOutput,
    /// A bounded allocation could not be reserved.
    AllocationFailed,
}

impl Display for ClipboardImageError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EncodedSize { bytes, maximum } => {
                write!(
                    formatter,
                    "encoded image has {bytes} bytes; maximum is {maximum}"
                )
            }
            Self::Dimensions {
                width,
                height,
                maximum,
            } => write!(
                formatter,
                "image dimensions {width}x{height} exceed maximum {maximum}"
            ),
            Self::ArithmeticOverflow => formatter.write_str("image arithmetic overflow"),
            Self::UnsupportedFormat => formatter.write_str("unsupported clipboard image format"),
            Self::MalformedInput => formatter.write_str("malformed clipboard image"),
            Self::DecoderLimit => formatter.write_str("clipboard image decoder limit exceeded"),
            Self::CappedOutput => formatter.write_str("encoded clipboard image exceeded cap"),
            Self::AllocationFailed => formatter.write_str("clipboard image allocation failed"),
        }
    }
}

impl Error for ClipboardImageError {}

/// Fully validates a PNG under the configured encoded, dimension, and decoded limits.
///
/// # Errors
///
/// Returns the first bounded validation failure. APNG is not accepted.
pub fn validate_png(encoded: &[u8], limits: ImageLimits) -> Result<ImageInfo, ClipboardImageError> {
    let (info, mut rgba) = decode_png_rgba(encoded, limits)?;
    rgba.zeroize();
    Ok(info)
}

/// Converts strict `BITMAPV5HEADER` BGRA pixels to a bounded PNG.
///
/// # Errors
///
/// Rejects malformed, non-sRGB, unsupported-mask, oversize, or unencodable input.
pub fn dibv5_to_png(dib: &[u8], limits: ImageLimits) -> Result<Vec<u8>, ClipboardImageError> {
    let (info, mut rgba) = decode_dibv5_rgba(dib, limits)?;
    let encoded = encode_png_rgba(info, &rgba, limits.max_encoded_bytes);
    rgba.zeroize();
    encoded
}

/// Converts a bounded PNG to a top-down, 32-bpp sRGB DIBV5 with BGRA pixels.
///
/// # Errors
///
/// Rejects APNG, malformed, unsupported, or oversize images.
pub fn png_to_dibv5(encoded: &[u8], limits: ImageLimits) -> Result<Vec<u8>, ClipboardImageError> {
    let (info, mut rgba) = decode_png_rgba(encoded, limits)?;
    let total = DIBV5_HEADER_BYTES
        .checked_add(info.rgba_bytes)
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    let mut dib = Vec::new();
    dib.try_reserve_exact(total)
        .map_err(|_| ClipboardImageError::AllocationFailed)?;
    dib.resize(total, 0);

    write_u32(&mut dib, 0, DIBV5_HEADER_SIZE_FIELD);
    write_i32(
        &mut dib,
        4,
        i32::try_from(info.width).map_err(|_| ClipboardImageError::ArithmeticOverflow)?,
    );
    write_i32(
        &mut dib,
        8,
        -i32::try_from(info.height).map_err(|_| ClipboardImageError::ArithmeticOverflow)?,
    );
    write_u16(&mut dib, 12, 1);
    write_u16(&mut dib, 14, 32);
    write_u32(&mut dib, 16, BI_BITFIELDS);
    write_u32(
        &mut dib,
        20,
        u32::try_from(info.rgba_bytes).map_err(|_| ClipboardImageError::ArithmeticOverflow)?,
    );
    write_u32(&mut dib, 40, RED_MASK);
    write_u32(&mut dib, 44, GREEN_MASK);
    write_u32(&mut dib, 48, BLUE_MASK);
    write_u32(&mut dib, 52, ALPHA_MASK);
    write_u32(&mut dib, 56, LCS_SRGB);
    write_u32(&mut dib, 108, LCS_GM_IMAGES);

    for (source, target) in rgba
        .chunks_exact(4)
        .zip(dib[DIBV5_HEADER_BYTES..].chunks_exact_mut(4))
    {
        target.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
    }
    rgba.zeroize();
    Ok(dib)
}

/// Converts any packed device-independent bitmap a Windows application
/// offers (`CF_DIB` or `CF_DIBV5`) to a bounded PNG.
///
/// The Windows twin of the macOS Deck's TIFF → PNG: a Deck reads it when an
/// application offers no `PNG` format. Accepts `BITMAPINFOHEADER`,
/// `BITMAPV4HEADER` and `BITMAPV5HEADER`; 24-bit `BI_RGB` and 32-bit
/// `BI_RGB` or `BI_BITFIELDS` with the standard BGR(A) masks; top-down or
/// bottom-up rows padded to four bytes; an optional colour table, skipped.
/// A 32-bit `BI_RGB` image whose alpha bytes are all zero is opaque (the
/// field is reserved there and most writers leave it zero). The colour space
/// must be sRGB or the Windows default; a linked profile is refused, an
/// embedded one is ignored. Everything else (palettes, 16-bit, RLE, JPEG or
/// PNG inside a DIB) is [`ClipboardImageError::UnsupportedFormat`].
///
/// # Errors
///
/// Rejects malformed, truncated, unsupported, oversize or unencodable input.
pub fn dib_to_png(dib: &[u8], limits: ImageLimits) -> Result<Vec<u8>, ClipboardImageError> {
    let (info, mut rgba) = decode_dib_rgba(dib, limits)?;
    let encoded = encode_png_rgba(info, &rgba, limits.max_encoded_bytes);
    rgba.zeroize();
    encoded
}

const BITMAPINFOHEADER_BYTES: u32 = 40;
const BITMAPV4HEADER_BYTES: u32 = 108;
const LCS_WINDOWS_COLOR_SPACE: u32 = 0x5769_6E20;
const PROFILE_EMBEDDED: u32 = 0x4D42_4544;
const MAX_DIB_COLOR_TABLE_ENTRIES: u32 = 256;

#[allow(
    clippy::too_many_lines,
    reason = "one bounded parse, kept in one place for review"
)]
fn decode_dib_rgba(
    dib: &[u8],
    limits: ImageLimits,
) -> Result<(ImageInfo, Vec<u8>), ClipboardImageError> {
    let header_bytes = read_u32(dib, 0)?;
    if !matches!(
        header_bytes,
        BITMAPINFOHEADER_BYTES | BITMAPV4HEADER_BYTES | DIBV5_HEADER_SIZE_FIELD
    ) {
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    let header_len =
        usize::try_from(header_bytes).map_err(|_| ClipboardImageError::ArithmeticOverflow)?;
    if dib.len() < header_len {
        return Err(ClipboardImageError::MalformedInput);
    }
    let width_signed = read_i32(dib, 4)?;
    let height_signed = read_i32(dib, 8)?;
    let planes = read_u16(dib, 12)?;
    let bits = read_u16(dib, 14)?;
    let compression = read_u32(dib, 16)?;
    let declared_size = read_u32(dib, 20)?;
    let colors_used = read_u32(dib, 32)?;
    if planes != 1 || !matches!(bits, 24 | 32) || colors_used > MAX_DIB_COLOR_TABLE_ENTRIES {
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    if width_signed <= 0 || height_signed == 0 || height_signed == i32::MIN {
        return Err(ClipboardImageError::UnsupportedFormat);
    }

    // BITMAPINFOHEADER keeps its three BI_BITFIELDS masks after the header;
    // the V4/V5 headers carry four masks inside.
    let mut pixels_offset = header_len;
    let masks = if compression == BI_BITFIELDS {
        if bits != 32 {
            return Err(ClipboardImageError::UnsupportedFormat);
        }
        if header_bytes == BITMAPINFOHEADER_BYTES {
            pixels_offset += 12;
            (
                read_u32(dib, 40)?,
                read_u32(dib, 44)?,
                read_u32(dib, 48)?,
                0,
            )
        } else {
            (
                read_u32(dib, 40)?,
                read_u32(dib, 44)?,
                read_u32(dib, 48)?,
                read_u32(dib, 52)?,
            )
        }
    } else if compression == BI_RGB {
        (0, 0, 0, 0)
    } else {
        return Err(ClipboardImageError::UnsupportedFormat);
    };
    let has_alpha_mask = match (compression, masks) {
        (BI_RGB, _) | (_, (RED_MASK, GREEN_MASK, BLUE_MASK, 0)) => false,
        (_, (RED_MASK, GREEN_MASK, BLUE_MASK, ALPHA_MASK)) => true,
        _ => return Err(ClipboardImageError::UnsupportedFormat),
    };
    if header_bytes != BITMAPINFOHEADER_BYTES {
        let color_space = read_u32(dib, 56)?;
        if !matches!(
            color_space,
            LCS_SRGB | LCS_WINDOWS_COLOR_SPACE | PROFILE_EMBEDDED
        ) {
            return Err(ClipboardImageError::UnsupportedFormat);
        }
    }
    let table_bytes = usize::try_from(colors_used)
        .ok()
        .and_then(|entries| entries.checked_mul(4))
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    pixels_offset = pixels_offset
        .checked_add(table_bytes)
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;

    let width = u32::try_from(width_signed).map_err(|_| ClipboardImageError::ArithmeticOverflow)?;
    let height = height_signed.unsigned_abs();
    let info = image_info(width, height, limits)?;
    let width_px = usize::try_from(width).map_err(|_| ClipboardImageError::ArithmeticOverflow)?;
    let height_px = usize::try_from(height).map_err(|_| ClipboardImageError::ArithmeticOverflow)?;
    let source_pixel_bytes = usize::from(bits / 8);
    let stride = width_px
        .checked_mul(source_pixel_bytes)
        .and_then(|bytes| bytes.checked_add(3))
        .map(|bytes| bytes & !3)
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    let image_bytes = stride
        .checked_mul(height_px)
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    let pixels_end = pixels_offset
        .checked_add(image_bytes)
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    if dib.len() < pixels_end {
        return Err(ClipboardImageError::MalformedInput);
    }
    if declared_size != 0
        && usize::try_from(declared_size).map_err(|_| ClipboardImageError::ArithmeticOverflow)?
            < image_bytes
    {
        return Err(ClipboardImageError::MalformedInput);
    }

    let source = &dib[pixels_offset..pixels_end];
    let row_bytes = width_px * 4;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(info.rgba_bytes)
        .map_err(|_| ClipboardImageError::AllocationFailed)?;
    rgba.resize(info.rgba_bytes, 0);
    let mut any_alpha = false;
    for output_row in 0..height_px {
        let source_row = if height_signed < 0 {
            output_row
        } else {
            height_px - 1 - output_row
        };
        let row = &source[source_row * stride..source_row * stride + width_px * source_pixel_bytes];
        let target = &mut rgba[output_row * row_bytes..(output_row + 1) * row_bytes];
        for (pixel, out) in row
            .chunks_exact(source_pixel_bytes)
            .zip(target.chunks_exact_mut(4))
        {
            let alpha = if source_pixel_bytes == 4 && (has_alpha_mask || compression == BI_RGB) {
                pixel[3]
            } else {
                u8::MAX
            };
            any_alpha |= alpha != 0;
            out.copy_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
        }
    }
    if source_pixel_bytes == 4 && compression == BI_RGB && !any_alpha {
        for pixel in rgba.chunks_exact_mut(4) {
            pixel[3] = u8::MAX;
        }
    }
    Ok((info, rgba))
}

pub(crate) fn image_info(
    width: u32,
    height: u32,
    limits: ImageLimits,
) -> Result<ImageInfo, ClipboardImageError> {
    if width == 0 || height == 0 || width > limits.max_dimension || height > limits.max_dimension {
        return Err(ClipboardImageError::Dimensions {
            width,
            height,
            maximum: limits.max_dimension,
        });
    }
    let rgba_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    if rgba_bytes > limits.max_decoded_bytes {
        return Err(ClipboardImageError::DecoderLimit);
    }
    Ok(ImageInfo {
        width,
        height,
        rgba_bytes,
    })
}

pub(crate) fn decode_png_rgba(
    encoded: &[u8],
    limits: ImageLimits,
) -> Result<(ImageInfo, Vec<u8>), ClipboardImageError> {
    if encoded.len() > limits.max_encoded_bytes {
        return Err(ClipboardImageError::EncodedSize {
            bytes: encoded.len(),
            maximum: limits.max_encoded_bytes,
        });
    }
    let mut decoder = Decoder::new_with_limits(
        Cursor::new(encoded),
        Limits {
            bytes: limits.max_decoded_bytes,
        },
    );
    decoder.set_ignore_text_chunk(true);
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|error| map_decode_error(&error))?;
    if reader.info().animation_control.is_some() || reader.info().frame_control.is_some() {
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    let info = image_info(reader.info().width, reader.info().height, limits)?;
    let output_size = reader
        .output_buffer_size()
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    if output_size > info.rgba_bytes {
        return Err(ClipboardImageError::DecoderLimit);
    }

    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(info.rgba_bytes)
        .map_err(|_| ClipboardImageError::AllocationFailed)?;
    pixels.resize(output_size, 0);
    let output = reader
        .next_frame(&mut pixels)
        .map_err(|error| map_decode_error(&error))?;
    if output.width != info.width
        || output.height != info.height
        || output.bit_depth != BitDepth::Eight
    {
        pixels.zeroize();
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    let used = output.buffer_size();
    if used > pixels.len() {
        pixels.zeroize();
        return Err(ClipboardImageError::MalformedInput);
    }
    if let Err(error) = reader.finish() {
        pixels.zeroize();
        return Err(map_decode_error(&error));
    }
    if reader.info().animation_control.is_some() || reader.info().frame_control.is_some() {
        pixels.zeroize();
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    pixels.truncate(used);
    normalize_rgba(&mut pixels, output.color_type, info.rgba_bytes)?;
    Ok((info, pixels))
}

fn normalize_rgba(
    pixels: &mut Vec<u8>,
    color: ColorType,
    rgba_bytes: usize,
) -> Result<(), ClipboardImageError> {
    let source_channels = color.samples();
    let expected_source = rgba_bytes
        .checked_div(4)
        .and_then(|pixels| pixels.checked_mul(source_channels))
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    if pixels.len() != expected_source {
        pixels.zeroize();
        return Err(ClipboardImageError::MalformedInput);
    }
    if color == ColorType::Rgba {
        return Ok(());
    }
    pixels.resize(rgba_bytes, 0);
    let pixel_count = rgba_bytes / 4;
    for index in (0..pixel_count).rev() {
        let source = index * source_channels;
        let target = index * 4;
        let rgba = match color {
            ColorType::Grayscale => {
                let gray = pixels[source];
                [gray, gray, gray, u8::MAX]
            }
            ColorType::GrayscaleAlpha => {
                let gray = pixels[source];
                [gray, gray, gray, pixels[source + 1]]
            }
            ColorType::Rgb => [
                pixels[source],
                pixels[source + 1],
                pixels[source + 2],
                u8::MAX,
            ],
            ColorType::Rgba => unreachable!("handled above"),
            ColorType::Indexed => {
                pixels.zeroize();
                return Err(ClipboardImageError::UnsupportedFormat);
            }
        };
        pixels[target..target + 4].copy_from_slice(&rgba);
    }
    Ok(())
}

fn decode_dibv5_rgba(
    dib: &[u8],
    limits: ImageLimits,
) -> Result<(ImageInfo, Vec<u8>), ClipboardImageError> {
    if dib.len() < DIBV5_HEADER_BYTES
        || read_u32(dib, 0)? != DIBV5_HEADER_SIZE_FIELD
        || read_u16(dib, 12)? != 1
        || read_u16(dib, 14)? != 32
        || read_u32(dib, 56)? != LCS_SRGB
        || read_u32(dib, 112)? != 0
        || read_u32(dib, 116)? != 0
        || read_u32(dib, 120)? != 0
    {
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    let compression = read_u32(dib, 16)?;
    let masks = (
        read_u32(dib, 40)?,
        read_u32(dib, 44)?,
        read_u32(dib, 48)?,
        read_u32(dib, 52)?,
    );
    match compression {
        BI_RGB if masks == (0, 0, 0, 0) => {}
        BI_BITFIELDS if masks == (RED_MASK, GREEN_MASK, BLUE_MASK, ALPHA_MASK) => {}
        _ => return Err(ClipboardImageError::UnsupportedFormat),
    }

    let width_signed = read_i32(dib, 4)?;
    let height_signed = read_i32(dib, 8)?;
    if width_signed <= 0 || height_signed == 0 || height_signed == i32::MIN {
        return Err(ClipboardImageError::UnsupportedFormat);
    }
    let width = u32::try_from(width_signed).map_err(|_| ClipboardImageError::ArithmeticOverflow)?;
    let height = height_signed.unsigned_abs();
    let info = image_info(width, height, limits)?;
    let expected = DIBV5_HEADER_BYTES
        .checked_add(info.rgba_bytes)
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    if dib.len() != expected {
        return Err(ClipboardImageError::MalformedInput);
    }
    let declared_size = read_u32(dib, 20)?;
    if declared_size != 0
        && usize::try_from(declared_size).map_err(|_| ClipboardImageError::ArithmeticOverflow)?
            != info.rgba_bytes
    {
        return Err(ClipboardImageError::MalformedInput);
    }

    let row_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or(ClipboardImageError::ArithmeticOverflow)?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(info.rgba_bytes)
        .map_err(|_| ClipboardImageError::AllocationFailed)?;
    rgba.resize(info.rgba_bytes, 0);
    let source_pixels = &dib[DIBV5_HEADER_BYTES..];
    for output_row in
        0..usize::try_from(height).map_err(|_| ClipboardImageError::ArithmeticOverflow)?
    {
        let source_row = if height_signed < 0 {
            output_row
        } else {
            usize::try_from(height).map_err(|_| ClipboardImageError::ArithmeticOverflow)?
                - 1
                - output_row
        };
        let source_start = source_row
            .checked_mul(row_bytes)
            .ok_or(ClipboardImageError::ArithmeticOverflow)?;
        let target_start = output_row
            .checked_mul(row_bytes)
            .ok_or(ClipboardImageError::ArithmeticOverflow)?;
        for column in
            0..usize::try_from(width).map_err(|_| ClipboardImageError::ArithmeticOverflow)?
        {
            let source = source_start + column * 4;
            let target = target_start + column * 4;
            let alpha = if compression == BI_RGB {
                u8::MAX
            } else {
                source_pixels[source + 3]
            };
            rgba[target..target + 4].copy_from_slice(&[
                source_pixels[source + 2],
                source_pixels[source + 1],
                source_pixels[source],
                alpha,
            ]);
        }
    }
    Ok((info, rgba))
}

pub(crate) fn encode_png_rgba(
    info: ImageInfo,
    rgba: &[u8],
    maximum: usize,
) -> Result<Vec<u8>, ClipboardImageError> {
    let mut output = CappedWriter::new(maximum);
    {
        let mut encoder = Encoder::new(&mut output, info.width, info.height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| ClipboardImageError::CappedOutput)?;
        writer
            .write_image_data(rgba)
            .map_err(|_| ClipboardImageError::CappedOutput)?;
        writer
            .finish()
            .map_err(|_| ClipboardImageError::CappedOutput)?;
    }
    Ok(output.into_inner())
}

fn map_decode_error(error: &png::DecodingError) -> ClipboardImageError {
    if matches!(error, png::DecodingError::LimitsExceeded) {
        ClipboardImageError::DecoderLimit
    } else {
        ClipboardImageError::MalformedInput
    }
}

struct CappedWriter {
    bytes: Vec<u8>,
    maximum: usize,
}

impl CappedWriter {
    const fn new(maximum: usize) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for CappedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let new_len =
            self.bytes.len().checked_add(buffer.len()).ok_or_else(|| {
                IoError::new(ErrorKind::OutOfMemory, "clipboard PNG size overflow")
            })?;
        if new_len > self.maximum {
            return Err(IoError::new(
                ErrorKind::StorageFull,
                "clipboard PNG output cap exceeded",
            ));
        }
        self.bytes
            .try_reserve(buffer.len())
            .map_err(|_| IoError::new(ErrorKind::OutOfMemory, "clipboard PNG allocation failed"))?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ClipboardImageError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(ClipboardImageError::MalformedInput)?;
    Ok(u16::from_le_bytes(
        value
            .try_into()
            .map_err(|_| ClipboardImageError::MalformedInput)?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ClipboardImageError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(ClipboardImageError::MalformedInput)?;
    Ok(u32::from_le_bytes(
        value
            .try_into()
            .map_err(|_| ClipboardImageError::MalformedInput)?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, ClipboardImageError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(ClipboardImageError::MalformedInput)?;
    Ok(i32::from_le_bytes(
        value
            .try_into()
            .map_err(|_| ClipboardImageError::MalformedInput)?,
    ))
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_i32(bytes: &mut [u8], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        encode_png_rgba(
            image_info(width, height, ImageLimits::default()).expect("valid dimensions"),
            pixels,
            HARD_MAX_CLIPBOARD_BYTES,
        )
        .expect("PNG encodes")
    }

    #[test]
    fn png_and_dibv5_roundtrip_top_down_bgra() {
        let png = rgba_png(
            2,
            2,
            &[255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 9, 8, 7, 6],
        );
        let info = validate_png(&png, ImageLimits::default()).expect("valid PNG");
        assert_eq!(
            info,
            ImageInfo {
                width: 2,
                height: 2,
                rgba_bytes: 16
            }
        );
        let dib = png_to_dibv5(&png, ImageLimits::default()).expect("DIB encodes");
        assert_eq!(read_i32(&dib, 8).expect("height"), -2);
        assert_eq!(&dib[124..128], &[0, 0, 255, 255]);
        let roundtrip = dibv5_to_png(&dib, ImageLimits::default()).expect("PNG encodes");
        let (_, decoded) =
            decode_png_rgba(&roundtrip, ImageLimits::default()).expect("roundtrip decodes");
        assert_eq!(
            decoded,
            [255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 64, 9, 8, 7, 6]
        );
    }

    #[test]
    fn bottom_up_dib_rows_are_normalized() {
        let png = rgba_png(1, 2, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut dib = png_to_dibv5(&png, ImageLimits::default()).expect("DIB encodes");
        write_i32(&mut dib, 8, 2);
        dib[124..128].copy_from_slice(&[7, 6, 5, 8]);
        dib[128..132].copy_from_slice(&[3, 2, 1, 4]);
        let normalized = dibv5_to_png(&dib, ImageLimits::default()).expect("PNG encodes");
        let (_, rgba) = decode_png_rgba(&normalized, ImageLimits::default()).expect("PNG decodes");
        assert_eq!(rgba, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn bi_rgb_reserved_byte_normalizes_to_opaque_alpha() {
        let png = rgba_png(1, 1, &[1, 2, 3, 4]);
        let mut dib = png_to_dibv5(&png, ImageLimits::default()).expect("DIB encodes");
        write_u32(&mut dib, 16, BI_RGB);
        for offset in [40, 44, 48, 52] {
            write_u32(&mut dib, offset, 0);
        }
        dib[127] = 0;
        let normalized = dibv5_to_png(&dib, ImageLimits::default()).expect("PNG encodes");
        let (_, rgba) = decode_png_rgba(&normalized, ImageLimits::default()).expect("PNG decodes");
        assert_eq!(rgba, [1, 2, 3, 255]);
    }

    #[test]
    fn rejects_masks_dimensions_malformed_and_caps() {
        let png = rgba_png(1, 1, &[1, 2, 3, 4]);
        assert_eq!(
            validate_png(
                &png,
                ImageLimits {
                    max_encoded_bytes: png.len() - 1,
                    ..ImageLimits::default()
                }
            ),
            Err(ClipboardImageError::EncodedSize {
                bytes: png.len(),
                maximum: png.len() - 1
            })
        );
        let mut dib = png_to_dibv5(&png, ImageLimits::default()).expect("DIB encodes");
        write_u32(&mut dib, 40, 0x00ff_00ff);
        assert_eq!(
            dibv5_to_png(&dib, ImageLimits::default()),
            Err(ClipboardImageError::UnsupportedFormat)
        );
        assert_eq!(
            validate_png(
                &png,
                ImageLimits {
                    max_dimension: 0,
                    ..ImageLimits::default()
                }
            ),
            Err(ClipboardImageError::Dimensions {
                width: 1,
                height: 1,
                maximum: 0
            })
        );
        assert_eq!(
            validate_png(&png[..png.len() - 1], ImageLimits::default()),
            Err(ClipboardImageError::MalformedInput)
        );
        assert_eq!(
            dibv5_to_png(
                &png_to_dibv5(&png, ImageLimits::default()).expect("DIB encodes"),
                ImageLimits {
                    max_encoded_bytes: 8,
                    ..ImageLimits::default()
                }
            ),
            Err(ClipboardImageError::CappedOutput)
        );
    }

    #[test]
    fn capped_writer_never_grows_past_limit() {
        let mut writer = CappedWriter::new(3);
        assert_eq!(writer.write(&[1, 2]).expect("fits"), 2);
        assert!(writer.write(&[3, 4]).is_err());
        assert_eq!(writer.bytes, [1, 2]);
    }

    #[test]
    fn rejects_apng_and_decoded_raster_limit_before_output_growth() {
        let mut animated = Vec::new();
        {
            let mut encoder = Encoder::new(&mut animated, 1, 1);
            encoder.set_color(ColorType::Rgba);
            encoder.set_depth(BitDepth::Eight);
            encoder.set_animated(1, 0).expect("animation metadata");
            let mut writer = encoder.write_header().expect("animated header");
            writer
                .write_image_data(&[1, 2, 3, 4])
                .expect("animated frame");
            writer.finish().expect("animated IEND");
        }
        assert_eq!(
            validate_png(&animated, ImageLimits::default()),
            Err(ClipboardImageError::UnsupportedFormat)
        );

        let png = rgba_png(2, 2, &[0; 16]);
        assert_eq!(
            validate_png(
                &png,
                ImageLimits {
                    max_decoded_bytes: 15,
                    ..ImageLimits::default()
                }
            ),
            Err(ClipboardImageError::DecoderLimit)
        );
    }

    fn info_header(width: i32, height: i32, bits: u16, compression: u32, colors: u32) -> Vec<u8> {
        let mut header = vec![0_u8; 40];
        write_u32(&mut header, 0, 40);
        write_i32(&mut header, 4, width);
        write_i32(&mut header, 8, height);
        write_u16(&mut header, 12, 1);
        write_u16(&mut header, 14, bits);
        write_u32(&mut header, 16, compression);
        write_u32(&mut header, 32, colors);
        header
    }

    fn decoded(png: &[u8]) -> Vec<u8> {
        decode_png_rgba(png, ImageLimits::default())
            .expect("PNG decodes")
            .1
    }

    #[test]
    fn a_bottom_up_24_bit_dib_with_row_padding_and_a_colour_table_converts() {
        let mut dib = info_header(3, 2, 24, BI_RGB, 1);
        dib.extend_from_slice(&[1, 2, 3, 0]);
        // Bottom row first; each 9-byte row is padded to 12.
        dib.extend_from_slice(&[10, 20, 30, 11, 21, 31, 12, 22, 32, 0, 0, 0]);
        dib.extend_from_slice(&[40, 50, 60, 41, 51, 61, 42, 52, 62, 0, 0, 0]);
        dib.extend_from_slice(&[0xAA; 7]);
        let png = dib_to_png(&dib, ImageLimits::default()).expect("converts");
        assert_eq!(
            decoded(&png),
            [
                60, 50, 40, 255, 61, 51, 41, 255, 62, 52, 42, 255, 30, 20, 10, 255, 31, 21, 11,
                255, 32, 22, 12, 255
            ]
        );
    }

    #[test]
    fn thirty_two_bit_dibs_keep_declared_alpha_and_treat_zero_reserved_bytes_as_opaque() {
        let mut fields = info_header(1, -1, 32, BI_BITFIELDS, 0);
        for mask in [RED_MASK, GREEN_MASK, BLUE_MASK] {
            fields.extend_from_slice(&mask.to_le_bytes());
        }
        fields.extend_from_slice(&[1, 2, 3, 9]);
        let png = dib_to_png(&fields, ImageLimits::default()).expect("converts");
        assert_eq!(decoded(&png), [3, 2, 1, 255]);

        let mut reserved = info_header(2, -1, 32, BI_RGB, 0);
        reserved.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        let png = dib_to_png(&reserved, ImageLimits::default()).expect("converts");
        assert_eq!(decoded(&png), [3, 2, 1, 255, 6, 5, 4, 255]);

        let source = rgba_png(1, 1, &[9, 8, 7, 100]);
        let v5 = png_to_dibv5(&source, ImageLimits::default()).expect("DIB");
        let png = dib_to_png(&v5, ImageLimits::default()).expect("converts");
        assert_eq!(decoded(&png), [9, 8, 7, 100]);
    }

    #[test]
    fn unsupported_or_hostile_dibs_are_refused_before_allocation() {
        let limits = ImageLimits::default();
        let mut palette = info_header(1, 1, 8, BI_RGB, 0);
        palette.extend_from_slice(&[0; 4]);
        assert_eq!(
            dib_to_png(&palette, limits),
            Err(ClipboardImageError::UnsupportedFormat)
        );
        let mut odd_masks = info_header(1, 1, 32, BI_BITFIELDS, 0);
        for mask in [0x0000_00ff_u32, 0x0000_ff00, 0x00ff_0000] {
            odd_masks.extend_from_slice(&mask.to_le_bytes());
        }
        odd_masks.extend_from_slice(&[0; 4]);
        assert_eq!(
            dib_to_png(&odd_masks, limits),
            Err(ClipboardImageError::UnsupportedFormat)
        );
        let truncated = info_header(4, 4, 32, BI_RGB, 0);
        assert_eq!(
            dib_to_png(&truncated, limits),
            Err(ClipboardImageError::MalformedInput)
        );
        let huge = info_header(i32::MAX, i32::MAX, 32, BI_RGB, 0);
        assert!(matches!(
            dib_to_png(&huge, limits),
            Err(ClipboardImageError::Dimensions { .. })
        ));
        let many_colours = info_header(1, 1, 24, BI_RGB, u32::MAX);
        assert_eq!(
            dib_to_png(&many_colours, limits),
            Err(ClipboardImageError::UnsupportedFormat)
        );
        let source = rgba_png(1, 1, &[9, 8, 7, 100]);
        let mut linked = png_to_dibv5(&source, limits).expect("DIB");
        write_u32(&mut linked, 56, 0x4C49_4E4B);
        assert_eq!(
            dib_to_png(&linked, limits),
            Err(ClipboardImageError::UnsupportedFormat)
        );
        assert_eq!(
            dib_to_png(&[40, 0, 0], limits),
            Err(ClipboardImageError::MalformedInput)
        );
        let mut understated = info_header(1, 1, 32, BI_RGB, 0);
        write_u32(&mut understated, 20, 2);
        understated.extend_from_slice(&[0; 4]);
        assert_eq!(
            dib_to_png(&understated, limits),
            Err(ClipboardImageError::MalformedInput)
        );
    }
}
