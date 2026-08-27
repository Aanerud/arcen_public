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

fn image_info(
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

fn decode_png_rgba(
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

fn encode_png_rgba(
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
}
