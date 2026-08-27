use crate::abi::{
    EDID_BYTES, MAX_HEIGHT, MAX_REFRESH_MILLIHZ, MAX_WIDTH, MIN_HEIGHT, MIN_REFRESH_MILLIHZ,
    MIN_WIDTH, Mode,
};

const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdidError {
    ModeOutOfRange,
    PixelClockOutOfRange,
}

impl core::fmt::Display for EdidError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ModeOutOfRange => formatter.write_str("mode is outside the Arcen EDID bounds"),
            Self::PixelClockOutOfRange => {
                formatter.write_str("mode pixel clock does not fit an EDID 1.4 detailed timing")
            }
        }
    }
}

impl std::error::Error for EdidError {}

/// Builds one checksum-valid EDID 1.4 base block for an Arcen virtual monitor.
///
/// # Errors
///
/// Returns [`EdidError::ModeOutOfRange`] when the mode exceeds the provider
/// contract, or [`EdidError::PixelClockOutOfRange`] when even the contract's
/// minimum refresh cannot represent the active dimensions in a base-block
/// detailed timing.
pub fn build_base_edid(
    mode: Mode,
    product_code: u16,
    serial_number: u32,
    physical_width_mm: u32,
    physical_height_mm: u32,
) -> Result<[u8; EDID_BYTES], EdidError> {
    validate_mode(mode)?;
    let (width_mm, height_mm) = physical_size(
        mode.width,
        mode.height,
        physical_width_mm,
        physical_height_mm,
    );
    let timing = match detailed_timing(mode, width_mm, height_mm) {
        Ok(timing) => timing,
        Err(EdidError::PixelClockOutOfRange) => detailed_timing(
            Mode {
                refresh_millihz: MIN_REFRESH_MILLIHZ,
                ..mode
            },
            width_mm,
            height_mm,
        )?,
        Err(error) => return Err(error),
    };

    let mut edid = [0u8; EDID_BYTES];
    edid[..EDID_HEADER.len()].copy_from_slice(&EDID_HEADER);
    edid[8..10].copy_from_slice(&crate::abi::EDID_MANUFACTURER_ID.to_be_bytes());
    edid[10..12].copy_from_slice(&product_code.to_le_bytes());
    edid[12..16].copy_from_slice(&serial_number.to_le_bytes());
    edid[16] = 1;
    edid[17] = 36;
    edid[18] = 1;
    edid[19] = 4;
    edid[20] = 0x80;
    edid[21] = low_byte((width_mm / 10).min(255));
    edid[22] = low_byte((height_mm / 10).min(255));
    edid[23] = 120;
    edid[24] = 0x0a;
    edid[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
    for standard_timing in edid[38..54].chunks_exact_mut(2) {
        standard_timing.copy_from_slice(&[0x01, 0x01]);
    }
    edid[54..72].copy_from_slice(&timing);
    write_text_descriptor(&mut edid[72..90], 0xfc, b"Arcen IDD");
    write_text_descriptor(
        &mut edid[90..108],
        0xff,
        serial_text(serial_number).as_bytes(),
    );
    write_range_descriptor(&mut edid[108..126], mode);
    edid[126] = 0;
    edid[127] = checksum(&edid[..127]);
    Ok(edid)
}

fn validate_mode(mode: Mode) -> Result<(), EdidError> {
    if mode.width < MIN_WIDTH
        || mode.width > MAX_WIDTH
        || mode.height < MIN_HEIGHT
        || mode.height > MAX_HEIGHT
        || mode.refresh_millihz < MIN_REFRESH_MILLIHZ
        || mode.refresh_millihz > MAX_REFRESH_MILLIHZ
    {
        return Err(EdidError::ModeOutOfRange);
    }
    Ok(())
}

fn physical_size(
    width: u32,
    height: u32,
    requested_width_mm: u32,
    requested_height_mm: u32,
) -> (u32, u32) {
    let derived_width = width.saturating_mul(254).div_ceil(960).clamp(1, 4_095);
    let derived_height = height.saturating_mul(254).div_ceil(960).clamp(1, 4_095);
    (
        if requested_width_mm == 0 {
            derived_width
        } else {
            requested_width_mm.clamp(1, 4_095)
        },
        if requested_height_mm == 0 {
            derived_height
        } else {
            requested_height_mm.clamp(1, 4_095)
        },
    )
}

fn detailed_timing(mode: Mode, width_mm: u32, height_mm: u32) -> Result<[u8; 18], EdidError> {
    let horizontal_blank = ((mode.width / 5).max(160) + 7) & !7;
    let vertical_blank = 45u32;
    if horizontal_blank > 4_095 || vertical_blank > 4_095 {
        return Err(EdidError::ModeOutOfRange);
    }
    let horizontal_total = mode.width + horizontal_blank;
    let vertical_total = mode.height + vertical_blank;
    let pixel_clock_10khz = u64::from(horizontal_total)
        .saturating_mul(u64::from(vertical_total))
        .saturating_mul(u64::from(mode.refresh_millihz))
        .div_ceil(10_000_000);
    let pixel_clock_10khz =
        u16::try_from(pixel_clock_10khz).map_err(|_| EdidError::PixelClockOutOfRange)?;

    let h_sync_offset = 48u32.min(horizontal_blank.saturating_sub(1));
    let h_sync_width = 32u32.min(horizontal_blank.saturating_sub(h_sync_offset));
    let v_sync_offset = 3u32;
    let v_sync_width = 5u32;

    let mut timing = [0u8; 18];
    timing[0..2].copy_from_slice(&pixel_clock_10khz.to_le_bytes());
    timing[2] = low_byte(mode.width);
    timing[3] = low_byte(horizontal_blank);
    timing[4] = low_byte((((mode.width >> 8) & 0x0f) << 4) | ((horizontal_blank >> 8) & 0x0f));
    timing[5] = low_byte(mode.height);
    timing[6] = low_byte(vertical_blank);
    timing[7] = low_byte((((mode.height >> 8) & 0x0f) << 4) | ((vertical_blank >> 8) & 0x0f));
    timing[8] = low_byte(h_sync_offset);
    timing[9] = low_byte(h_sync_width);
    timing[10] = low_byte(((v_sync_offset & 0x0f) << 4) | (v_sync_width & 0x0f));
    timing[11] = low_byte(
        (((h_sync_offset >> 8) & 0x03) << 6)
            | (((h_sync_width >> 8) & 0x03) << 4)
            | (((v_sync_offset >> 4) & 0x03) << 2)
            | ((v_sync_width >> 4) & 0x03),
    );
    timing[12] = low_byte(width_mm);
    timing[13] = low_byte(height_mm);
    timing[14] = low_byte((((width_mm >> 8) & 0x0f) << 4) | ((height_mm >> 8) & 0x0f));
    timing[17] = 0x1a;
    Ok(timing)
}

fn write_text_descriptor(target: &mut [u8], descriptor_type: u8, text: &[u8]) {
    target.fill(0x20);
    target[..5].copy_from_slice(&[0, 0, 0, descriptor_type, 0]);
    let length = text.len().min(12);
    target[5..5 + length].copy_from_slice(&text[..length]);
    target[17] = b'\n';
}

fn write_range_descriptor(target: &mut [u8], mode: Mode) {
    target.fill(0);
    target[..5].copy_from_slice(&[0, 0, 0, 0xfd, 0]);
    target[5] = low_byte(MIN_REFRESH_MILLIHZ / 1_000);
    target[6] = low_byte(MAX_REFRESH_MILLIHZ / 1_000);
    let horizontal_total = mode.width + (((mode.width / 5).max(160) + 7) & !7);
    let vertical_total = mode.height + 45;
    let max_horizontal_khz =
        u64::from(horizontal_total).saturating_mul(u64::from(MAX_REFRESH_MILLIHZ)) / 1_000_000;
    target[7] = 15;
    target[8] = low_byte_u64(max_horizontal_khz.min(255));
    let max_pixel_clock_mhz = u64::from(horizontal_total)
        .saturating_mul(u64::from(vertical_total))
        .saturating_mul(u64::from(MAX_REFRESH_MILLIHZ))
        / 1_000_000_000;
    target[9] = low_byte_u64(max_pixel_clock_mhz.div_ceil(10).min(255));
    target[10] = 0x0a;
}

fn serial_text(serial: u32) -> String {
    format!("{serial:08X}")
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(
        bytes
            .iter()
            .fold(0u8, |sum, value| sum.wrapping_add(*value)),
    )
}

fn low_byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

fn low_byte_u64(value: u64) -> u8 {
    value.to_le_bytes()[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u32, height: u32, refresh_millihz: u32) -> Mode {
        Mode {
            width,
            height,
            refresh_millihz,
        }
    }

    #[test]
    fn generates_checksum_valid_unique_arc_edids() {
        let first =
            build_base_edid(mode(3_840, 2_160, 60_000), 0xa100, 1, 0, 0).expect("first EDID");
        let second =
            build_base_edid(mode(1_920, 1_080, 60_000), 0xa101, 2, 0, 0).expect("second EDID");
        assert_eq!(&first[..8], &EDID_HEADER);
        assert_eq!(
            u16::from_be_bytes([first[8], first[9]]),
            crate::abi::EDID_MANUFACTURER_ID
        );
        assert_eq!(
            first
                .iter()
                .fold(0u8, |sum, value| sum.wrapping_add(*value)),
            0
        );
        assert_eq!(
            second
                .iter()
                .fold(0u8, |sum, value| sum.wrapping_add(*value)),
            0
        );
        assert_ne!(first, second);
    }

    #[test]
    fn encodes_preferred_active_dimensions() {
        let edid = build_base_edid(mode(2_560, 1_440, 120_000), 0xa100, 7, 600, 340).expect("EDID");
        let width = u32::from(edid[56]) | (u32::from(edid[58] >> 4) << 8);
        let height = u32::from(edid[59]) | (u32::from(edid[61] >> 4) << 8);
        assert_eq!((width, height), (2_560, 1_440));
    }

    #[test]
    fn rejects_edid_unrepresentable_dimensions() {
        assert_eq!(
            build_base_edid(mode(4_096, 2_160, 60_000), 1, 1, 0, 0),
            Err(EdidError::ModeOutOfRange)
        );
    }

    #[test]
    fn maximum_contract_mode_uses_representable_base_timing() {
        let edid = build_base_edid(
            mode(MAX_WIDTH, MAX_HEIGHT, MAX_REFRESH_MILLIHZ),
            0xa103,
            4,
            0,
            0,
        )
        .expect("bounded EDID");
        let width = u32::from(edid[56]) | (u32::from(edid[58] >> 4) << 8);
        let height = u32::from(edid[59]) | (u32::from(edid[61] >> 4) << 8);
        assert_eq!((width, height), (MAX_WIDTH, MAX_HEIGHT));
    }
}
