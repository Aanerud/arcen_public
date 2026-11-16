// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use arcen_outputs::edid::{EdidRequest, generate_named};

use crate::abi::{
    EDID_BYTES, MAX_HEIGHT, MAX_REFRESH_MILLIHZ, MAX_WIDTH, MIN_HEIGHT, MIN_REFRESH_MILLIHZ,
    MIN_WIDTH, MONITOR_NAME, Mode,
};

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

/// Builds one checksum-valid EDID 1.4 base block for an Arcen virtual
/// monitor with the shared builder (`arcen_outputs::edid::generate`), the
/// one every Pier uses for a Deck display.
///
/// The provider contract bounds the mode. A mode whose reduced-blanking
/// pixel clock does not fit a base-block detailed timing (3840x2160 at
/// 120 Hz, for example) describes its preferred timing at the contract's
/// minimum refresh instead; the driver still offers the requested mode.
/// A physical size of 0 mm is derived at 96 DPI.
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
    let request = |refresh_millihz: u32| EdidRequest {
        width: mode.width,
        height: mode.height,
        refresh_hz: refresh_millihz.saturating_add(500) / 1_000,
        width_mm: millimetres(physical_width_mm),
        height_mm: millimetres(physical_height_mm),
        scale: 1.0,
        product_id: product_code,
        serial: serial_number,
        // shared-contract colourless-edid: the IddCx ABI's MonitorSpec carries no
        // Deck display colour; this dormant research provider is SDR-only and
        // keeps the sRGB base block it always described.
        color: None,
    };
    generate_named(request(mode.refresh_millihz), MONITOR_NAME)
        .or_else(|_| generate_named(request(MIN_REFRESH_MILLIHZ), MONITOR_NAME))
        .map_err(|_| EdidError::PixelClockOutOfRange)
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

/// The ABI's physical size (0 = unknown) as the shared request's.
fn millimetres(value: u32) -> f32 {
    // The ABI bounds nothing here; the shared builder clamps to 1..=4095 mm.
    #[allow(clippy::cast_precision_loss, reason = "clamped to 4095 mm before use")]
    let value = value.min(4_095) as f32;
    value
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
        assert_eq!(first[..8], [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
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

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
    }

    /// The bytes of the shared builder (`arcen_outputs::edid`) for the
    /// provider's modes, pinned: a change to either shows here.
    #[test]
    fn provider_edids_are_the_shared_builders_bytes() {
        let cases: [(Mode, [u32; 4], &str); 5] = [
            (
                mode(1_920, 1_080, 60_000),
                [0xa100, 0x0000_0101, 0, 0],
                concat!(
                    "00ffffffffffff00064e00a1010100000124010480331d780aee91a3544c9926",
                    "0f5054000000010101010101010101010101010101012a3680a070381f403020",
                    "3500fc1e1100001a000000fc00417263656e204944440a202020000000fd0038",
                    "423d470e0400000000000000000000ff0030303030303130310a2020202000a6",
                ),
            ),
            (
                mode(2_560, 1_440, 60_000),
                [0xa101, 0x0000_0102, 597, 336],
                concat!(
                    "00ffffffffffff00064e01a10201000001240104803c22780aee91a3544c9926",
                    "0f5054000000010101010101010101010101010101016a5e00a0a0a029503020",
                    "350055502100001a000000fc00417263656e204944440a202020000000fd0037",
                    "41535d190400000000000000000000ff0030303030303130320a20202020002b",
                ),
            ),
            (
                mode(3_840, 2_160, 120_000),
                [0xa102, 0x0000_0103, 0, 0],
                concat!(
                    "00ffffffffffff00064e02a10301000001240104806639780aee91a3544c9926",
                    "0f505400000001010101010101010101010101010101f05100a0f07019803020",
                    "3500f83c3200001a000000fc00417263656e204944440a202020000000fd0013",
                    "1d2f39150400000000000000000000ff0030303030303130330a202020200022",
                ),
            ),
            (
                mode(1_080, 1_920, 60_000),
                [0xa103, 0x0000_0104, 0, 0],
                concat!(
                    "00ffffffffffff00064e03a10401000001240104801d33780aee91a3544c9926",
                    "0f505400000001010101010101010101010101010101663938a0408037703020",
                    "3a001efc1100001a000000fc00417263656e204944440a202020000000fd0037",
                    "41717b0f0400000000000000000000ff0030303030303130340a2020202000da",
                ),
            ),
            (
                mode(320, 240, 24_000),
                [0xa100, 1, 0, 0],
                concat!(
                    "00ffffffffffff00064e00a10100000001240104800906780aee91a3544c9926",
                    "0f505400000001010101010101010101010101010101240140a010f00d003020",
                    "340055400000001a000000fc00417263656e204944440a202020000000fd0014",
                    "1e010b010400000000000000000000ff0030303030303030310a2020202000c2",
                ),
            ),
        ];
        for (mode, [product, serial, width_mm, height_mm], golden) in cases {
            let product = u16::try_from(product).expect("product code");
            let edid = build_base_edid(mode, product, serial, width_mm, height_mm).expect("EDID");
            assert_eq!(hex(&edid), golden, "{mode:?}");
        }
    }

    #[test]
    fn a_mode_past_the_base_block_clock_describes_the_minimum_refresh() {
        let edid = build_base_edid(mode(3_840, 2_160, 120_000), 0xa102, 3, 0, 0).expect("EDID");
        let timing = arcen_outputs::edid::cvt_reduced_blanking(3_840, 2_160, 24).expect("24 Hz");
        let pixel_clock = u32::from(u16::from_le_bytes([edid[54], edid[55]]));
        assert_eq!(pixel_clock, timing.pixel_clock_10_khz);
        arcen_outputs::edid::validate(&edid, 3_840, 2_160).expect("valid base block");
    }

    #[test]
    fn names_the_display_for_the_pier_and_uses_arcens_manufacturer() {
        let edid = build_base_edid(mode(1_920, 1_080, 60_000), 0xa100, 1, 0, 0).expect("EDID");
        assert_eq!(
            u16::from_be_bytes([edid[8], edid[9]]),
            crate::abi::EDID_MANUFACTURER_ID
        );
        assert_eq!(&edid[72..77], &[0, 0, 0, 0xfc, 0]);
        assert_eq!(&edid[77..87], b"Arcen IDD\n");
    }
}
