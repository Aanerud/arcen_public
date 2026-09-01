const EDID_LEN: usize = 128;
const DETAILED_TIMING_OFFSET: usize = 54;
const DESCRIPTOR_LEN: usize = 18;
const CVT_RB_HORIZONTAL_BLANK: u32 = 160;
const CVT_RB_HORIZONTAL_FRONT_PORCH: u32 = 48;
const CVT_RB_HORIZONTAL_SYNC: u32 = 32;
const CVT_RB_MIN_VERTICAL_BLANK_US: f64 = 460.0;
const CVT_RB_VERTICAL_FRONT_PORCH: u32 = 3;
const MAX_DTD_VALUE: u32 = 4095;
const MAX_DTD_PIXEL_CLOCK_10_KHZ: u32 = u16::MAX as u32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdidRequest {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub width_mm: f32,
    pub height_mm: f32,
    pub scale: f32,
    pub product_id: u16,
    pub serial: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetailedTiming {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub pixel_clock_10_khz: u32,
    pub horizontal_blank: u32,
    pub horizontal_front_porch: u32,
    pub horizontal_sync: u32,
    pub vertical_blank: u32,
    pub vertical_front_porch: u32,
    pub vertical_sync: u32,
}

impl DetailedTiming {
    fn horizontal_total(self) -> u32 {
        self.width + self.horizontal_blank
    }

    fn horizontal_khz(self) -> u32 {
        let pixel_clock_hz = self.pixel_clock_10_khz as u64 * 10_000;
        ((pixel_clock_hz + self.horizontal_total() as u64 / 2)
            / self.horizontal_total() as u64
            / 1000) as u32
    }
}

/// EDID manufacturer ID for the display Arcen synthesizes.
///
/// This is deliberately **not** `TRG`, which earlier builds inherited from the
/// predecessor codebase. `TRG` is not in the UEFI Forum PNP registry, so it was
/// never anyone's to use, and sharing it put Arcen's virtual monitor in the same
/// `DISPLAY\TRG*` namespace as the predecessor's — on a host that has both, the
/// two are indistinguishable in the PnP tree and in `Get-PnpDevice -Class
/// Monitor`.
///
/// `ARN` is likewise unregistered: the UEFI Forum sunset new three-letter PNP ID
/// assignments at the end of 2024, so no new one can be obtained. Release/Security
/// owns any change here, and should confirm against the published PNP ID list
/// that this code is unassigned before general availability.
const MANUFACTURER_ID: [u8; 3] = *b"ARN";

pub fn generate(request: EdidRequest) -> Result<[u8; EDID_LEN], String> {
    let timing = cvt_reduced_blanking(request.width, request.height, request.refresh_hz)?;
    let (width_mm, height_mm) = physical_size_mm(request);
    let mut edid = [0u8; EDID_LEN];

    edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    edid[8..10].copy_from_slice(&manufacturer_id(MANUFACTURER_ID).to_be_bytes());
    edid[10..12].copy_from_slice(&request.product_id.to_le_bytes());
    edid[12..16].copy_from_slice(&request.serial.to_le_bytes());
    edid[16] = 1;
    edid[17] = 36; // 2026 - 1990
    edid[18] = 1;
    edid[19] = 4;
    edid[20] = 0x80;
    edid[21] = ((width_mm + 5) / 10).min(u8::MAX as u32) as u8;
    edid[22] = ((height_mm + 5) / 10).min(u8::MAX as u32) as u8;
    edid[23] = 120; // gamma 2.20
    edid[24] = 0x0a; // preferred timing + continuous frequency
    chromaticity_srgb(&mut edid);
    edid[35] = 0;
    edid[36] = 0;
    edid[37] = 0;
    for standard_timing in edid[38..54].chunks_exact_mut(2) {
        standard_timing.copy_from_slice(&[0x01, 0x01]);
    }

    encode_detailed_timing(
        &mut edid[DETAILED_TIMING_OFFSET..DETAILED_TIMING_OFFSET + DESCRIPTOR_LEN],
        timing,
        width_mm,
        height_mm,
    )?;
    encode_text_descriptor(&mut edid[72..90], 0xfc, "Arcen");
    encode_range_descriptor(&mut edid[90..108], timing);
    encode_text_descriptor(
        &mut edid[108..126],
        0xff,
        &format!("{:08X}", request.serial),
    );
    edid[126] = 0;
    edid[127] = checksum(&edid[..127]);
    validate(&edid, request.width, request.height)?;
    Ok(edid)
}

pub fn validate(edid: &[u8], expected_width: u32, expected_height: u32) -> Result<(), String> {
    if edid.len() != EDID_LEN {
        return Err(format!(
            "EDID base block must be {EDID_LEN} bytes, got {}",
            edid.len()
        ));
    }
    if edid[..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return Err("EDID header is invalid".to_string());
    }
    if edid.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) != 0 {
        return Err("EDID checksum is invalid".to_string());
    }
    let dtd = &edid[DETAILED_TIMING_OFFSET..DETAILED_TIMING_OFFSET + DESCRIPTOR_LEN];
    let width = dtd[2] as u32 | (((dtd[4] >> 4) as u32) << 8);
    let height = dtd[5] as u32 | (((dtd[7] >> 4) as u32) << 8);
    if width != expected_width || height != expected_height {
        return Err(format!(
            "EDID preferred timing is {width}x{height}, expected {expected_width}x{expected_height}"
        ));
    }
    let pixel_clock = u16::from_le_bytes([dtd[0], dtd[1]]);
    if pixel_clock == 0 {
        return Err("EDID preferred timing has a zero pixel clock".to_string());
    }
    Ok(())
}

pub fn cvt_reduced_blanking(
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<DetailedTiming, String> {
    if width == 0 || height == 0 || refresh_hz == 0 {
        return Err("timing dimensions and refresh must be non-zero".to_string());
    }
    if width > MAX_DTD_VALUE || height > MAX_DTD_VALUE {
        return Err(format!(
            "{width}x{height} exceeds the EDID 1.4 detailed-timing 12-bit limit"
        ));
    }

    let vertical_sync = cvt_vertical_sync(width, height);
    let frame_period_us = 1_000_000.0 / refresh_hz as f64;
    let estimated_line_us = (frame_period_us - CVT_RB_MIN_VERTICAL_BLANK_US) / height as f64;
    if estimated_line_us <= 0.0 {
        return Err("requested refresh leaves no time for active scanout".to_string());
    }
    let minimum_vertical_blank = (CVT_RB_MIN_VERTICAL_BLANK_US / estimated_line_us).ceil() as u32;
    let vertical_blank = minimum_vertical_blank.max(
        CVT_RB_VERTICAL_FRONT_PORCH
            .saturating_add(vertical_sync)
            .saturating_add(6),
    );
    if vertical_blank > MAX_DTD_VALUE {
        return Err("calculated vertical blanking exceeds EDID limits".to_string());
    }

    let horizontal_total = width + CVT_RB_HORIZONTAL_BLANK;
    let vertical_total = height + vertical_blank;
    let ideal_clock_mhz =
        horizontal_total as f64 * vertical_total as f64 * refresh_hz as f64 / 1_000_000.0;
    let pixel_clock_10_khz = (ideal_clock_mhz * 100.0).ceil() as u32;
    if pixel_clock_10_khz > MAX_DTD_PIXEL_CLOCK_10_KHZ {
        return Err(format!(
            "calculated {:.2}MHz pixel clock exceeds EDID 1.4 limits",
            pixel_clock_10_khz as f64 / 100.0
        ));
    }
    let refresh_millihz = ((pixel_clock_10_khz as u64 * 10_000 * 1000)
        / (horizontal_total as u64 * vertical_total as u64)) as u32;

    Ok(DetailedTiming {
        width,
        height,
        refresh_millihz,
        pixel_clock_10_khz,
        horizontal_blank: CVT_RB_HORIZONTAL_BLANK,
        horizontal_front_porch: CVT_RB_HORIZONTAL_FRONT_PORCH,
        horizontal_sync: CVT_RB_HORIZONTAL_SYNC,
        vertical_blank,
        vertical_front_porch: CVT_RB_VERTICAL_FRONT_PORCH,
        vertical_sync,
    })
}

fn physical_size_mm(request: EdidRequest) -> (u32, u32) {
    let scale = if request.scale.is_finite() && request.scale > 0.0 {
        request.scale
    } else {
        1.0
    };
    let fallback_width = request.width as f32 * 25.4 / (96.0 * scale);
    let fallback_height = request.height as f32 * 25.4 / (96.0 * scale);
    let width = if request.width_mm.is_finite() && request.width_mm > 0.0 {
        request.width_mm
    } else {
        fallback_width
    };
    let height = if request.height_mm.is_finite() && request.height_mm > 0.0 {
        request.height_mm
    } else {
        fallback_height
    };
    (
        width.round().clamp(1.0, MAX_DTD_VALUE as f32) as u32,
        height.round().clamp(1.0, MAX_DTD_VALUE as f32) as u32,
    )
}

fn cvt_vertical_sync(width: u32, height: u32) -> u32 {
    let aspect = width as f64 / height as f64;
    [
        (4.0 / 3.0, 4),
        (16.0 / 9.0, 5),
        (16.0 / 10.0, 6),
        (5.0 / 4.0, 7),
        (15.0 / 9.0, 7),
    ]
    .into_iter()
    .min_by(|(left, _), (right, _)| (aspect - left).abs().total_cmp(&(aspect - right).abs()))
    .filter(|(candidate, _)| (aspect - candidate).abs() < 0.025)
    .map_or(10, |(_, sync)| sync)
}

fn manufacturer_id(code: [u8; 3]) -> u16 {
    let letter = |byte: u8| u16::from(byte.to_ascii_uppercase().saturating_sub(b'@') & 0x1f);
    (letter(code[0]) << 10) | (letter(code[1]) << 5) | letter(code[2])
}

fn chromaticity_srgb(edid: &mut [u8; EDID_LEN]) {
    edid[25..35].copy_from_slice(&[0xee, 0x91, 0xa3, 0x54, 0x4c, 0x99, 0x26, 0x0f, 0x50, 0x54]);
}

fn encode_detailed_timing(
    descriptor: &mut [u8],
    timing: DetailedTiming,
    width_mm: u32,
    height_mm: u32,
) -> Result<(), String> {
    let h_back_porch = timing
        .horizontal_blank
        .checked_sub(timing.horizontal_front_porch + timing.horizontal_sync)
        .ok_or_else(|| "horizontal porch exceeds blanking".to_string())?;
    let v_back_porch = timing
        .vertical_blank
        .checked_sub(timing.vertical_front_porch + timing.vertical_sync)
        .ok_or_else(|| "vertical porch exceeds blanking".to_string())?;
    if h_back_porch == 0 || v_back_porch == 0 {
        return Err("timing requires non-zero back porches".to_string());
    }
    descriptor.fill(0);
    descriptor[0..2].copy_from_slice(&(timing.pixel_clock_10_khz as u16).to_le_bytes());
    descriptor[2] = timing.width as u8;
    descriptor[3] = timing.horizontal_blank as u8;
    descriptor[4] =
        (((timing.width >> 8) & 0x0f) << 4 | ((timing.horizontal_blank >> 8) & 0x0f)) as u8;
    descriptor[5] = timing.height as u8;
    descriptor[6] = timing.vertical_blank as u8;
    descriptor[7] =
        (((timing.height >> 8) & 0x0f) << 4 | ((timing.vertical_blank >> 8) & 0x0f)) as u8;
    descriptor[8] = timing.horizontal_front_porch as u8;
    descriptor[9] = timing.horizontal_sync as u8;
    descriptor[10] =
        ((timing.vertical_front_porch & 0x0f) << 4 | (timing.vertical_sync & 0x0f)) as u8;
    descriptor[11] = ((((timing.horizontal_front_porch >> 8) & 0x03) << 6)
        | (((timing.horizontal_sync >> 8) & 0x03) << 4)
        | (((timing.vertical_front_porch >> 4) & 0x03) << 2)
        | ((timing.vertical_sync >> 4) & 0x03)) as u8;
    descriptor[12] = width_mm as u8;
    descriptor[13] = height_mm as u8;
    descriptor[14] = ((((width_mm >> 8) & 0x0f) << 4) | ((height_mm >> 8) & 0x0f)) as u8;
    descriptor[17] = 0x1a; // digital separate sync, positive H / negative V
    Ok(())
}

fn encode_text_descriptor(descriptor: &mut [u8], tag: u8, value: &str) {
    descriptor.fill(b' ');
    descriptor[..5].copy_from_slice(&[0, 0, 0, tag, 0]);
    let bytes = value.as_bytes();
    let length = bytes.len().min(12);
    descriptor[5..5 + length].copy_from_slice(&bytes[..length]);
    descriptor[5 + length] = b'\n';
}

fn encode_range_descriptor(descriptor: &mut [u8], timing: DetailedTiming) {
    descriptor.fill(0);
    descriptor[..5].copy_from_slice(&[0, 0, 0, 0xfd, 0]);
    let refresh_hz = timing.refresh_millihz.div_ceil(1000);
    descriptor[5] = refresh_hz.saturating_sub(5).min(255) as u8;
    descriptor[6] = refresh_hz.saturating_add(5).min(255) as u8;
    let horizontal_khz = timing.horizontal_khz();
    descriptor[7] = horizontal_khz.saturating_sub(5).min(255) as u8;
    descriptor[8] = horizontal_khz.saturating_add(5).min(255) as u8;
    descriptor[9] = timing.pixel_clock_10_khz.div_ceil(1000).min(255) as u8;
    descriptor[10] = 0x04; // CVT support
}

fn checksum(bytes: &[u8]) -> u8 {
    0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(width: u32, height: u32) -> EdidRequest {
        EdidRequest {
            width,
            height,
            refresh_hz: 60,
            width_mm: 345.0,
            height_mm: 224.0,
            scale: 2.0,
            product_id: 0x2338,
            serial: 0x3600_2338,
        }
    }

    #[test]
    fn identifies_the_display_as_arcen_not_the_predecessor_namespace() {
        let edid = generate(request(2560, 1440)).unwrap();
        // Windows derives DISPLAY\<XXXNNNN> from these two bytes, so sharing the
        // predecessor's `TRG` made Arcen's monitor indistinguishable from it.
        assert_eq!(
            u16::from_be_bytes([edid[8], edid[9]]),
            manufacturer_id(*b"ARN"),
            "manufacturer id must be Arcen's"
        );
        assert_ne!(
            u16::from_be_bytes([edid[8], edid[9]]),
            manufacturer_id(*b"TRG"),
            "must not reuse the predecessor's manufacturer id"
        );
        // The human-readable descriptor already said Arcen; keep it that way.
        assert!(
            edid[72..90].windows(5).any(|window| window == b"Arcen"),
            "monitor name descriptor must name Arcen"
        );
    }

    #[test]
    fn generates_valid_arbitrary_client_edid() {
        let edid = generate(request(3600, 2338)).unwrap();
        validate(&edid, 3600, 2338).unwrap();
        assert_eq!(
            edid.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)),
            0
        );
        assert_eq!((edid[21], edid[22]), (35, 22));
    }

    #[test]
    fn encodes_requested_timing_with_bounded_refresh_error() {
        let timing = cvt_reduced_blanking(3600, 2338, 60).unwrap();
        assert_eq!((timing.width, timing.height), (3600, 2338));
        assert!(timing.refresh_millihz.abs_diff(60_000) <= 5);
        assert!(timing.pixel_clock_10_khz > 0);
        assert_eq!(timing.horizontal_blank, 160);
    }

    #[test]
    fn derives_physical_size_from_retina_scale_when_unknown() {
        let mut value = request(3600, 2338);
        value.width_mm = 0.0;
        value.height_mm = 0.0;
        let edid = generate(value).unwrap();
        assert_eq!((edid[21], edid[22]), (48, 31));
    }

    /// The synthesized physical size is the *only* channel by which a
    /// requested scale reaches Windows: Windows divides the current mode by
    /// the EDID's physical size to get DPI, then recommends a scale from it.
    ///
    /// Every multi-monitor path used to pass `scale: 1.0`, which produces a
    /// display declaring exactly 96 DPI — so Windows recommended 100% no
    /// matter what the client asked for. Measured on pier-windows.example.internal as 200%
    /// requested and 100% applied.
    #[test]
    fn implied_dpi_follows_the_requested_scale_not_a_hardcoded_96() {
        // Physical size lands in EDID bytes 21/22 in whole centimetres, so
        // recover DPI from the centimetre value the generator actually wrote.
        let implied_dpi = |scale: f32| {
            let mut value = request(1920, 1080);
            value.width_mm = 0.0;
            value.height_mm = 0.0;
            value.scale = scale;
            let edid = generate(value).unwrap();
            f64::from(1920) / (f64::from(edid[21]) * 10.0 / 25.4)
        };

        let at_100 = implied_dpi(1.0);
        assert!(
            (at_100 - 96.0).abs() < 2.0,
            "100% must imply ~96 DPI, got {at_100}",
        );

        // The regression that mattered: 200% must imply ~192 DPI so Windows
        // recommends 200%, not 96 DPI / 100%.
        let at_200 = implied_dpi(2.0);
        assert!(
            (at_200 - 192.0).abs() < 4.0,
            "200% must imply ~192 DPI so Windows recommends 200%, got {at_200}",
        );
        assert!(
            at_200 > at_100 * 1.8,
            "a doubled scale must roughly double implied DPI: {at_100} -> {at_200}",
        );
    }

    #[test]
    fn rejects_modes_that_base_edid_cannot_represent() {
        let error = generate(request(7680, 4320)).unwrap_err();
        assert!(error.contains("12-bit limit"));
    }

    #[test]
    fn validation_detects_checksum_and_timing_corruption() {
        let mut edid = generate(request(3600, 2338)).unwrap();
        edid[20] ^= 1;
        assert!(validate(&edid, 3600, 2338)
            .unwrap_err()
            .contains("checksum"));

        let mut edid = generate(request(3600, 2338)).unwrap();
        edid[DETAILED_TIMING_OFFSET + 2] = 0;
        edid[127] = checksum(&edid[..127]);
        assert!(validate(&edid, 3600, 2338)
            .unwrap_err()
            .contains("preferred timing"));
    }
}

// ---------------------------------------------------------------------------
// HDR10 signalling
// ---------------------------------------------------------------------------

/// Length of a full EDID carrying one extension block.
pub const EDID_HDR10_LEN: usize = 256;

/// Video Input Definition for a 10-bit digital HDMI sink.
///
/// The base block ships `0x80`, which says "digital" and leaves colour bit
/// depth *undefined*. An undefined depth is why nothing downstream can decide
/// the sink is capable of more than eight bits. Bits 6..4 carry the depth code
/// (`0b011` = 10 bpc) and bits 3..0 the interface (`0b0010` = HDMI-a).
///
/// NVIDIA reports GRID's virtual connector as HDMI, but an EDID without an
/// HDMI VSDB is classified as a DVI PC display and the control panel exposes
/// only 8 bpc. The base interface and CTA HDMI blocks must agree.
const VIDEO_INPUT_10BPC_HDMI: u8 = 0x80 | (0b011 << 4) | 0b0010;

/// Build a 256-byte EDID that advertises HDR10, from the ordinary base block.
///
/// Why this exists: capture bit depth is decided by what the *desktop* is
/// composited at, and Windows only composites wide when an output carries
/// Advanced Color. An output only offers Advanced Color when its EDID says the
/// sink can receive it. The 128-byte base block cannot say that at all —
/// HDR10 is signalled by a CTA-861 extension block, which by definition lives
/// in a second 128 bytes.
///
/// The CTA data-block collection carries the complete sink claim:
///
/// - **Colorimetry** (extended tag 0x05) declaring BT.2020 RGB and YCC.
/// - **HDR Static Metadata** (extended tag 0x06) declaring SMPTE ST 2084 (PQ),
///   which is the electro-optical transfer function HDR10 *is*, plus Static
///   Metadata Type 1.
/// - **HDMI VSDB** declaring HDMI identity and 30-bit deep-colour support.
/// - **HDMI Forum VSDB** declaring SCDC and a 550 MHz character-rate ceiling.
/// - **Video Capability** declaring selectable RGB/YCC quantization.
///
/// This only makes the offer. Whether Windows accepts it, and whether the
/// resulting frames carry more than eight bits of real information rather than
/// SDR content widened into a float buffer, are separate questions — the second
/// is what `capenc color-probe` measures.
pub fn generate_hdr10(request: EdidRequest) -> Result<[u8; EDID_HDR10_LEN], String> {
    let base = generate(request)?;
    let mut edid = [0u8; EDID_HDR10_LEN];
    edid[..EDID_LEN].copy_from_slice(&base);

    // Say the sink is 10-bit, not merely digital.
    edid[20] = VIDEO_INPUT_10BPC_HDMI;
    // One extension block follows, and the base checksum must be redone
    // because both of those bytes changed.
    edid[126] = 1;
    edid[127] = checksum(&edid[..127]);

    let ext = &mut edid[EDID_LEN..];
    ext[0] = 0x02; // CTA-861 extension
    ext[1] = 0x03; // revision 3
                   // Data blocks occupy bytes 4..34. There are no CTA detailed
                   // timings, so this points immediately after the collection.
    ext[2] = 0x22;
    // No native DTDs, no audio, RGB only.
    ext[3] = 0x00;

    // HDR Static Metadata Data Block: use-extended-tag (7), 6-byte payload.
    ext[4] = (7 << 5) | 6;
    ext[5] = 0x06; // extended tag: HDR static metadata
                   // Supported EOTFs: bit0 traditional SDR gamma, bit2 SMPTE ST 2084. Bit 2 is
                   // the one Windows reads to decide an output can carry HDR10; bit 0 keeps
                   // the sink usable as an ordinary SDR display, which it must remain.
    ext[6] = 0x05;
    ext[7] = 0x01; // Static Metadata Type 1
                   // CTA luminance codes: approximately 1000-nit peak, 400-nit max-frame
                   // average and 0.005-nit minimum. These match the Deck's HDR10 EDR
                   // metadata instead of leaving Windows to invent a different mastering
                   // envelope for the same virtual sink.
    ext[8] = 138;
    ext[9] = 96;
    ext[10] = 6;

    // HDMI Licensing Administrator VSDB, OUI 0x000c03 (least-significant
    // byte first on the wire). Physical address 0.0.0.0, 30-bit deep colour,
    // and a 550 MHz maximum TMDS clock. The deep-colour bit is the distinction
    // NVIDIA Control Panel uses between this HDMI sink and a DVI-class sink.
    ext[11] = (3 << 5) | 7;
    ext[12..15].copy_from_slice(&[0x03, 0x0c, 0x00]);
    ext[15..17].copy_from_slice(&[0x00, 0x00]);
    ext[17] = 0x10; // DC_30: 10 bpc / 30-bit deep colour.
    ext[18] = 0x6e; // 550 MHz in 5 MHz units.

    // Colorimetry Data Block: BT.2020 RGB and YCC.
    ext[19] = (7 << 5) | 3;
    ext[20] = 0x05;
    ext[21] = 0xc0;
    ext[22] = 0x00;

    // HDMI Forum VSDB, OUI 0xc45dd8. Version 1, 550 MHz maximum character
    // rate, SCDC present. This is the compact block used by working HDR
    // virtual-display EDIDs and prevents modern HDMI capability from being
    // inferred from the legacy VSDB alone.
    ext[23] = (3 << 5) | 7;
    ext[24..27].copy_from_slice(&[0xd8, 0x5d, 0xc4]);
    ext[27..31].copy_from_slice(&[0x01, 0x6e, 0x80, 0x00]);

    // Video Capability Data Block. RGB and YCC quantization ranges are
    // selectable; the remaining scan-behaviour bits mirror a proven HDR
    // monitor profile rather than leaving range handling undefined.
    ext[31] = (7 << 5) | 2;
    ext[32] = 0x00;
    ext[33] = 0xcb;

    ext[127] = checksum(&ext[..127]);
    Ok(edid)
}

#[cfg(test)]
mod hdr10_tests {
    use super::*;

    fn request() -> EdidRequest {
        EdidRequest {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            width_mm: 600.0,
            height_mm: 340.0,
            scale: 1.0,
            product_id: 1,
            serial: 0x1234_5678,
        }
    }

    /// Both blocks must checksum, or the sink is ignored outright rather than
    /// treated as SDR — a malformed EDID is worse than a conservative one.
    #[test]
    fn both_blocks_checksum() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        assert_eq!(
            edid[..EDID_LEN].iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0,
            "base block checksum"
        );
        assert_eq!(
            edid[EDID_LEN..].iter().fold(0u8, |a, b| a.wrapping_add(*b)),
            0,
            "extension block checksum"
        );
    }

    /// The whole point: an undefined colour depth cannot advertise anything.
    #[test]
    fn the_base_block_declares_ten_bits_per_component() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        assert_eq!(edid[20] & 0x80, 0x80, "must remain a digital input");
        assert_eq!((edid[20] >> 4) & 0b111, 0b011, "10 bpc");
        assert_eq!(edid[20] & 0x0f, 0b0010, "HDMI-a interface");
        // The plain generator must be left alone: SDR outputs still use it.
        assert_eq!(generate(request()).expect("base")[20], 0x80);
    }

    #[test]
    fn one_extension_block_is_declared_and_present() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        assert_eq!(edid[126], 1, "extension count");
        assert_eq!(edid[EDID_LEN], 0x02, "CTA-861 extension tag");
        assert_eq!(edid[EDID_LEN + 1], 0x03, "CTA revision 3");
    }

    /// Windows decides an output can carry HDR10 from the ST 2084 bit. If this
    /// regresses the EDID still looks valid and HDR silently never appears,
    /// which is the failure mode worth a dedicated test.
    #[test]
    fn the_hdr_block_declares_smpte_st_2084() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        let ext = &edid[EDID_LEN..];
        assert_eq!(ext[4], (7 << 5) | 6, "use-extended-tag, 6-byte payload");
        assert_eq!(ext[5], 0x06, "HDR static metadata extended tag");
        assert_eq!(ext[6] & 0x04, 0x04, "SMPTE ST 2084 (PQ) must be declared");
        assert_eq!(
            ext[6] & 0x01,
            0x01,
            "the sink must remain usable as plain SDR"
        );
        assert_eq!(ext[7] & 0x01, 0x01, "static metadata type 1");
        assert_eq!(&ext[8..11], &[138, 96, 6], "HDR luminance metadata");
    }

    #[test]
    fn the_colorimetry_block_declares_bt2020() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        let ext = &edid[EDID_LEN..];
        assert_eq!(ext[19], (7 << 5) | 3);
        assert_eq!(ext[20], 0x05, "colorimetry extended tag");
        assert_eq!(ext[21] & 0x80, 0x80, "BT.2020 RGB");
    }

    #[test]
    fn hdmi_blocks_declare_identity_deep_colour_and_scdc() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        let ext = &edid[EDID_LEN..];
        assert_eq!(&ext[12..15], &[0x03, 0x0c, 0x00], "HDMI OUI");
        assert_eq!(ext[17] & 0x10, 0x10, "30-bit deep colour");
        assert_eq!(&ext[24..27], &[0xd8, 0x5d, 0xc4], "HDMI Forum OUI");
        assert_eq!(ext[29] & 0x80, 0x80, "SCDC present");
    }

    /// The data blocks must end exactly where the descriptor offset says, or a
    /// parser reads padding as a malformed block.
    #[test]
    fn the_descriptor_offset_matches_the_data_blocks_written() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        let ext = &edid[EDID_LEN..];
        let mut offset = 4usize;
        while offset < ext[2] as usize {
            offset += 1 + usize::from(ext[offset] & 0x1f);
        }
        assert_eq!(ext[2] as usize, offset, "DTD offset must follow the blocks");
    }

    /// It must still be the display that was asked for.
    #[test]
    fn the_base_block_still_validates_for_its_geometry() {
        let edid = generate_hdr10(request()).expect("hdr10 edid");
        validate(&edid[..EDID_LEN], 1920, 1080).expect("base block still valid");
    }

    /// NVAPI carries 256 bytes; anything larger cannot be applied.
    #[test]
    fn the_result_fits_the_nvapi_edid_buffer() {
        assert_eq!(EDID_HDR10_LEN, 256);
    }
}
