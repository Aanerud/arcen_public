//! What an HEVC stream really is, read from its sequence parameter set.
//!
//! A session's hello says what the host meant to send, and the host's encoder
//! configuration says what it asked `VideoToolbox`, Media Foundation or NVENC
//! for. Neither is proof. An encoder given a 10-bit 4:4:4 surface and no
//! profile is free to encode 8-bit 4:2:0, and one given no colour metadata
//! writes none, so a stream can carry less than every log line about it
//! claims. The sequence parameter set is what a decoder obeys, so this module
//! reads it: the profile, the chroma format, the sample depth and the colour
//! description, exactly as the bitstream states them.
//!
//! Only the fields up to and including the VUI colour description are read.
//! Everything else in the SPS is skipped by the syntax the specification
//! gives for it, so the parser follows the real layout rather than guessing
//! offsets.

/// The subset of an HEVC SPS that decides what a stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcStreamTruth {
    /// `general_profile_idc`: 1 Main, 2 Main 10, 4 Range extensions.
    pub profile_idc: u8,
    /// `general_level_idc`, thirty times the level number.
    pub level_idc: u8,
    /// `chroma_format_idc`: 0 monochrome, 1 4:2:0, 2 4:2:2, 3 4:4:4.
    pub chroma_format_idc: u8,
    /// Luma sample depth in bits.
    pub bit_depth_luma: u8,
    /// Chroma sample depth in bits.
    pub bit_depth_chroma: u8,
    /// Coded width in luma samples (`pic_width_in_luma_samples`), padded to
    /// the encoder's coding-block alignment.
    pub width: u32,
    /// Coded height in luma samples (`pic_height_in_luma_samples`).
    pub height: u32,
    /// Width a decoder outputs: the coded width less the conformance
    /// window, i.e. the picture the host meant to send.
    pub display_width: u32,
    /// Height a decoder outputs after the conformance window.
    pub display_height: u32,
    /// The VUI colour description, when the stream states one.
    pub colour: Option<HevcColourDescription>,
}

/// The VUI `video_signal_type` fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HevcColourDescription {
    /// `video_full_range_flag`.
    pub full_range: bool,
    /// `colour_primaries` (ITU-T H.273): 1 BT.709, 9 BT.2020, 12 P3-D65.
    pub primaries: u8,
    /// `transfer_characteristics` (H.273): 1 BT.709, 13 sRGB, 16 PQ, 18 HLG.
    pub transfer: u8,
    /// `matrix_coeffs` (H.273): 0 identity, 1 BT.709, 9 BT.2020 NCL.
    pub matrix: u8,
}

impl HevcStreamTruth {
    /// A short, stable description for logs, such as
    /// `rext 4:4:4 10-bit bt709/bt709/bt709 full 1800x1168 (coded 1824x1184)`.
    /// The size is the displayed picture; the coded size follows only when
    /// the conformance window crops alignment padding from it.
    #[must_use]
    pub fn summary(&self) -> String {
        let profile = match self.profile_idc {
            1 => "main",
            2 => "main10",
            3 => "main-still",
            4 => "rext",
            _ => "other",
        };
        let chroma = match self.chroma_format_idc {
            0 => "4:0:0",
            1 => "4:2:0",
            2 => "4:2:2",
            _ => "4:4:4",
        };
        let colour = self.colour.map_or_else(
            || "untagged".to_owned(),
            |colour| {
                format!(
                    "{}/{}/{} {}",
                    h273_primaries(colour.primaries),
                    h273_transfer(colour.transfer),
                    h273_matrix(colour.matrix),
                    if colour.full_range { "full" } else { "limited" }
                )
            },
        );
        let coded = if (self.display_width, self.display_height) == (self.width, self.height) {
            String::new()
        } else {
            format!(" (coded {}x{})", self.width, self.height)
        };
        format!(
            "{profile} {chroma} {}-bit {colour} {}x{}{coded}",
            self.bit_depth_luma, self.display_width, self.display_height
        )
    }
}

/// Names an H.273 `colour_primaries` value.
#[must_use]
pub const fn h273_primaries(value: u8) -> &'static str {
    match value {
        1 => "bt709",
        2 => "unspecified",
        9 => "bt2020",
        12 => "p3d65",
        _ => "other",
    }
}

/// Names an H.273 `transfer_characteristics` value.
#[must_use]
pub const fn h273_transfer(value: u8) -> &'static str {
    match value {
        1 | 6 | 14 | 15 => "bt709",
        2 => "unspecified",
        13 => "srgb",
        16 => "pq",
        18 => "hlg",
        _ => "other",
    }
}

/// Names an H.273 `matrix_coeffs` value.
#[must_use]
pub const fn h273_matrix(value: u8) -> &'static str {
    match value {
        0 => "identity",
        1 => "bt709",
        2 => "unspecified",
        9 => "bt2020ncl",
        _ => "other",
    }
}

/// Why an SPS could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HevcSpsError {
    /// The NAL unit is not an SPS.
    NotSps,
    /// The unit ended before a field the parser needed.
    Truncated,
    /// A field held a value the specification does not allow.
    Invalid(&'static str),
}

impl std::fmt::Display for HevcSpsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSps => formatter.write_str("NAL unit is not an HEVC SPS"),
            Self::Truncated => formatter.write_str("HEVC SPS ended early"),
            Self::Invalid(field) => write!(formatter, "HEVC SPS field {field} is out of range"),
        }
    }
}

impl std::error::Error for HevcSpsError {}

/// HEVC `nal_unit_type` of a sequence parameter set.
const SPS_NAL_TYPE: u8 = 33;

/// Finds and reads the first SPS in an Annex B access unit.
///
/// # Errors
///
/// Returns [`HevcSpsError::NotSps`] when the access unit carries no SPS, or
/// the parse error of the SPS it does carry.
pub fn stream_truth_in_access_unit(access_unit: &[u8]) -> Result<HevcStreamTruth, HevcSpsError> {
    let mut found = None;
    crate::annexb::for_each_annex_b(access_unit, |nal| {
        if found.is_none() && nal.len() > 2 && (nal[0] >> 1) & 0x3F == SPS_NAL_TYPE {
            found = Some(parse_sps(nal));
        }
    });
    found.unwrap_or(Err(HevcSpsError::NotSps))
}

/// Reads an SPS NAL unit, starting at its two-byte NAL header.
///
/// # Errors
///
/// Returns [`HevcSpsError`] when the unit is not an SPS, ends early, or holds
/// a value outside the specification's range.
pub fn parse_sps(nal: &[u8]) -> Result<HevcStreamTruth, HevcSpsError> {
    if nal.len() < 3 || (nal[0] >> 1) & 0x3F != SPS_NAL_TYPE {
        return Err(HevcSpsError::NotSps);
    }
    let rbsp = unescape(&nal[2..]);
    let mut bits = Bits::new(&rbsp);

    bits.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = bits.read(3)?;
    bits.skip(1)?; // sps_temporal_id_nesting_flag
    let (profile_idc, level_idc) = profile_tier_level(&mut bits, max_sub_layers_minus1)?;
    bits.ue()?; // sps_seq_parameter_set_id
    let chroma_format_idc = bits.ue()?;
    if chroma_format_idc > 3 {
        return Err(HevcSpsError::Invalid("chroma_format_idc"));
    }
    let separate_colour_planes = chroma_format_idc == 3 && bits.flag()?;
    let width = bits.ue()?;
    let height = bits.ue()?;
    let (display_width, display_height) = conformance_window(
        &mut bits,
        chroma_format_idc,
        separate_colour_planes,
        width,
        height,
    )?;
    let bit_depth_luma = bits.ue()? + 8;
    let bit_depth_chroma = bits.ue()? + 8;
    if bit_depth_luma > 16 || bit_depth_chroma > 16 {
        return Err(HevcSpsError::Invalid("bit_depth"));
    }
    let log2_max_poc_lsb = bits.ue()? + 4;
    if log2_max_poc_lsb > 16 {
        return Err(HevcSpsError::Invalid("log2_max_pic_order_cnt_lsb_minus4"));
    }
    let sub_layer_ordering_info_present = bits.flag()?;
    let first = if sub_layer_ordering_info_present {
        0
    } else {
        max_sub_layers_minus1
    };
    for _ in first..=max_sub_layers_minus1 {
        bits.ue()?; // sps_max_dec_pic_buffering_minus1
        bits.ue()?; // sps_max_num_reorder_pics
        bits.ue()?; // sps_max_latency_increase_plus1
    }
    bits.ue()?; // log2_min_luma_coding_block_size_minus3
    bits.ue()?; // log2_diff_max_min_luma_coding_block_size
    bits.ue()?; // log2_min_luma_transform_block_size_minus2
    bits.ue()?; // log2_diff_max_min_luma_transform_block_size
    bits.ue()?; // max_transform_hierarchy_depth_inter
    bits.ue()?; // max_transform_hierarchy_depth_intra
    if bits.flag()? && bits.flag()? {
        // scaling_list_enabled_flag && sps_scaling_list_data_present_flag
        scaling_list_data(&mut bits)?;
    }
    bits.skip(2)?; // amp_enabled_flag, sample_adaptive_offset_enabled_flag
    if bits.flag()? {
        // pcm_enabled_flag
        bits.skip(8)?; // pcm sample bit depths
        bits.ue()?;
        bits.ue()?;
        bits.skip(1)?; // pcm_loop_filter_disabled_flag
    }
    let num_short_term_ref_pic_sets = bits.ue()?;
    if num_short_term_ref_pic_sets > 64 {
        return Err(HevcSpsError::Invalid("num_short_term_ref_pic_sets"));
    }
    let mut delta_pocs: Vec<u32> = Vec::with_capacity(num_short_term_ref_pic_sets as usize);
    for index in 0..num_short_term_ref_pic_sets {
        let count = st_ref_pic_set(&mut bits, index, &delta_pocs)?;
        delta_pocs.push(count);
    }
    if bits.flag()? {
        // long_term_ref_pics_present_flag
        let count = bits.ue()?;
        if count > 32 {
            return Err(HevcSpsError::Invalid("num_long_term_ref_pics_sps"));
        }
        for _ in 0..count {
            bits.skip(log2_max_poc_lsb)?; // lt_ref_pic_poc_lsb_sps
            bits.skip(1)?; // used_by_curr_pic_lt_sps_flag
        }
    }
    bits.skip(2)?; // sps_temporal_mvp_enabled_flag, strong_intra_smoothing_enabled_flag
    let colour = if bits.flag()? {
        vui_colour(&mut bits)?
    } else {
        None
    };

    Ok(HevcStreamTruth {
        profile_idc,
        level_idc,
        chroma_format_idc: u8::try_from(chroma_format_idc).unwrap_or(u8::MAX),
        bit_depth_luma: u8::try_from(bit_depth_luma).unwrap_or(u8::MAX),
        bit_depth_chroma: u8::try_from(bit_depth_chroma).unwrap_or(u8::MAX),
        width,
        height,
        display_width,
        display_height,
        colour,
    })
}

/// The picture a decoder outputs: the coded size less the conformance
/// window, whose offsets count chroma samples (H.265 7.4.3.2: `SubWidthC` and
/// `SubHeightC`), so 4:2:0 crops two luma rows or columns per unit.
fn conformance_window(
    bits: &mut Bits<'_>,
    chroma_format_idc: u32,
    separate_colour_planes: bool,
    width: u32,
    height: u32,
) -> Result<(u32, u32), HevcSpsError> {
    if !bits.flag()? {
        return Ok((width, height));
    }
    let (sub_width, sub_height) = match (chroma_format_idc, separate_colour_planes) {
        (1, false) => (2, 2),
        (2, false) => (2, 1),
        _ => (1, 1),
    };
    let left = bits.ue()?;
    let right = bits.ue()?;
    let top = bits.ue()?;
    let bottom = bits.ue()?;
    let cropped = |size: u32, start: u32, end: u32, unit: u32| {
        start
            .checked_add(end)
            .and_then(|total| total.checked_mul(unit))
            .and_then(|crop| size.checked_sub(crop))
            .filter(|&visible| visible > 0)
            .ok_or(HevcSpsError::Invalid("conformance_window"))
    };
    Ok((
        cropped(width, left, right, sub_width)?,
        cropped(height, top, bottom, sub_height)?,
    ))
}

fn profile_tier_level(
    bits: &mut Bits<'_>,
    max_sub_layers_minus1: u32,
) -> Result<(u8, u8), HevcSpsError> {
    bits.skip(3)?; // general_profile_space, general_tier_flag
    let profile_idc = bits.read(5)?;
    bits.skip(32)?; // general_profile_compatibility_flags
    bits.skip(48)?; // progressive..inbld and reserved bits
    let level_idc = bits.read(8)?;
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for layer in 0..max_sub_layers_minus1 as usize {
        profile_present[layer] = bits.flag()?;
        level_present[layer] = bits.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            bits.skip(2)?; // reserved_zero_2bits
        }
    }
    for layer in 0..max_sub_layers_minus1 as usize {
        if profile_present[layer] {
            bits.skip(88)?;
        }
        if level_present[layer] {
            bits.skip(8)?;
        }
    }
    Ok((
        u8::try_from(profile_idc).unwrap_or(u8::MAX),
        u8::try_from(level_idc).unwrap_or(u8::MAX),
    ))
}

fn scaling_list_data(bits: &mut Bits<'_>) -> Result<(), HevcSpsError> {
    for size_id in 0..4 {
        let step = if size_id == 3 { 3 } else { 1 };
        let mut matrix_id = 0;
        while matrix_id < 6 {
            if bits.flag()? {
                // scaling_list_pred_mode_flag: explicit coefficients
                let coefficients = 64.min(1 << (4 + (size_id << 1)));
                if size_id > 1 {
                    bits.se()?; // scaling_list_dc_coef_minus8
                }
                for _ in 0..coefficients {
                    bits.se()?;
                }
            } else {
                bits.ue()?; // scaling_list_pred_matrix_id_delta
            }
            matrix_id += step;
        }
    }
    Ok(())
}

/// Skips one `st_ref_pic_set` and returns how many pictures it references,
/// which a later set predicted from this one needs.
fn st_ref_pic_set(bits: &mut Bits<'_>, index: u32, previous: &[u32]) -> Result<u32, HevcSpsError> {
    let predicted = index != 0 && bits.flag()?;
    if predicted {
        // In the SPS, delta_idx_minus1 is absent and the reference is the
        // set immediately before this one.
        let reference = *previous
            .last()
            .ok_or(HevcSpsError::Invalid("inter_ref_pic_set_prediction_flag"))?;
        bits.skip(1)?; // delta_rps_sign
        bits.ue()?; // abs_delta_rps_minus1
        let mut count = 0;
        for _ in 0..=reference {
            let used = bits.flag()?;
            let kept = used || bits.flag()?;
            if kept {
                count += 1;
            }
        }
        Ok(count)
    } else {
        let negative = bits.ue()?;
        let positive = bits.ue()?;
        if negative > 16 || positive > 16 {
            return Err(HevcSpsError::Invalid("num_pics"));
        }
        for _ in 0..negative + positive {
            bits.ue()?; // delta_poc_minus1
            bits.skip(1)?; // used_by_curr_pic_flag
        }
        Ok(negative + positive)
    }
}

fn vui_colour(bits: &mut Bits<'_>) -> Result<Option<HevcColourDescription>, HevcSpsError> {
    if bits.flag()? {
        // aspect_ratio_info_present_flag
        if bits.read(8)? == 255 {
            bits.skip(32)?; // sar_width, sar_height
        }
    }
    if bits.flag()? {
        bits.skip(1)?; // overscan_appropriate_flag
    }
    if !bits.flag()? {
        // video_signal_type_present_flag
        return Ok(None);
    }
    bits.skip(3)?; // video_format
    let full_range = bits.flag()?;
    if !bits.flag()? {
        // colour_description_present_flag: range stated, colours unspecified
        return Ok(Some(HevcColourDescription {
            full_range,
            primaries: 2,
            transfer: 2,
            matrix: 2,
        }));
    }
    let primaries = bits.read(8)?;
    let transfer = bits.read(8)?;
    let matrix = bits.read(8)?;
    Ok(Some(HevcColourDescription {
        full_range,
        primaries: u8::try_from(primaries).unwrap_or(u8::MAX),
        transfer: u8::try_from(transfer).unwrap_or(u8::MAX),
        matrix: u8::try_from(matrix).unwrap_or(u8::MAX),
    }))
}

/// Removes emulation-prevention bytes (`00 00 03` becomes `00 00`).
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn bit(&mut self) -> Result<u32, HevcSpsError> {
        let byte = *self
            .data
            .get(self.position / 8)
            .ok_or(HevcSpsError::Truncated)?;
        let bit = (byte >> (7 - self.position % 8)) & 1;
        self.position += 1;
        Ok(u32::from(bit))
    }

    fn flag(&mut self) -> Result<bool, HevcSpsError> {
        Ok(self.bit()? == 1)
    }

    fn read(&mut self, count: u32) -> Result<u32, HevcSpsError> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    fn skip(&mut self, count: u32) -> Result<(), HevcSpsError> {
        let end = self.position + count as usize;
        if end > self.data.len() * 8 {
            return Err(HevcSpsError::Truncated);
        }
        self.position = end;
        Ok(())
    }

    fn ue(&mut self) -> Result<u32, HevcSpsError> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(HevcSpsError::Invalid("exp-golomb"));
            }
        }
        Ok((1 << leading) - 1 + self.read(leading)?)
    }

    fn se(&mut self) -> Result<i32, HevcSpsError> {
        let value = self.ue()?;
        let magnitude = i32::try_from(value.div_ceil(2)).unwrap_or(i32::MAX);
        Ok(if value % 2 == 1 {
            magnitude
        } else {
            -magnitude
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes bits most significant first, for building SPS fixtures.
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl Writer {
        fn bit(&mut self, value: bool) {
            if self.bits % 8 == 0 {
                self.bytes.push(0);
            }
            if value {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 1 << (7 - self.bits % 8);
            }
            self.bits += 1;
        }
        fn put(&mut self, value: u32, count: u32) {
            for shift in (0..count).rev() {
                self.bit((value >> shift) & 1 == 1);
            }
        }
        fn ue(&mut self, value: u32) {
            let coded = value + 1;
            let length = 32 - coded.leading_zeros();
            self.put(0, length - 1);
            self.put(coded, length);
        }
        fn finish(mut self) -> Vec<u8> {
            self.bit(true); // rbsp_stop_one_bit
            self.bytes
        }
    }

    struct Fixture {
        profile_idc: u32,
        chroma: u32,
        depth: u32,
        colour: Option<(bool, u32, u32, u32)>,
        predicted_rps: bool,
        /// Coded size and conformance window `[left, right, top, bottom]`;
        /// `None` keeps the historical 1920x1088 with bottom offset 4.
        geometry: Option<Geometry>,
    }

    /// Coded `(width, height)` and conformance window `[left, right, top, bottom]`.
    type Geometry = ((u32, u32), Option<[u32; 4]>);

    fn sps(fixture: &Fixture) -> Vec<u8> {
        let mut w = Writer::default();
        w.put(0, 4); // vps id
        w.put(0, 3); // max_sub_layers_minus1
        w.bit(true); // temporal id nesting
        w.put(0, 2);
        w.bit(false);
        w.put(fixture.profile_idc, 5);
        w.put(1 << (31 - fixture.profile_idc), 32);
        w.put(0, 32);
        w.put(0, 16);
        w.put(120, 8); // level 4
        w.ue(0); // sps id
        w.ue(fixture.chroma);
        if fixture.chroma == 3 {
            w.bit(false);
        }
        let ((width, height), window) = fixture
            .geometry
            .unwrap_or(((1920, 1088), Some([0, 0, 0, 4])));
        w.ue(width);
        w.ue(height);
        w.bit(window.is_some()); // conformance window
        if let Some(window) = window {
            for offset in window {
                w.ue(offset);
            }
        }
        w.ue(fixture.depth - 8);
        w.ue(fixture.depth - 8);
        w.ue(4); // log2_max_poc_lsb_minus4
        w.bit(true); // sub layer ordering info
        w.ue(1);
        w.ue(0);
        w.ue(0);
        for value in [0, 3, 0, 3, 0, 0] {
            w.ue(value);
        }
        w.bit(false); // scaling list
        w.bit(false); // amp
        w.bit(true); // sao
        w.bit(false); // pcm
        if fixture.predicted_rps {
            w.ue(2);
            w.ue(1); // negative
            w.ue(0); // positive
            w.ue(0);
            w.bit(true);
            w.bit(true); // inter_ref_pic_set_prediction_flag
            w.bit(false); // delta_rps_sign
            w.ue(0); // abs_delta_rps_minus1
            w.bit(true); // used_by_curr_pic_flag j=0
            w.bit(false); // j=1 not used
            w.bit(true); // use_delta_flag
        } else {
            w.ue(1);
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.bit(true);
        }
        w.bit(false); // long term
        w.bit(true); // temporal mvp
        w.bit(true); // strong intra smoothing
        match fixture.colour {
            None => w.bit(false),
            Some((full, primaries, transfer, matrix)) => {
                w.bit(true); // vui present
                w.bit(false); // aspect ratio
                w.bit(false); // overscan
                w.bit(true); // video signal type
                w.put(5, 3);
                w.bit(full);
                w.bit(true);
                w.put(primaries, 8);
                w.put(transfer, 8);
                w.put(matrix, 8);
            }
        }
        let rbsp = w.finish();
        let mut nal = vec![SPS_NAL_TYPE << 1, 1];
        let mut zeros = 0;
        for byte in rbsp {
            if zeros >= 2 && byte <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            nal.push(byte);
        }
        nal
    }

    #[test]
    fn a_tagged_ten_bit_four_four_four_stream_reads_as_itself() {
        let truth = parse_sps(&sps(&Fixture {
            profile_idc: 4,
            chroma: 3,
            depth: 10,
            colour: Some((true, 1, 1, 1)),
            predicted_rps: false,
            geometry: None,
        }))
        .expect("parses");
        assert_eq!(truth.profile_idc, 4);
        assert_eq!(truth.chroma_format_idc, 3);
        assert_eq!((truth.bit_depth_luma, truth.bit_depth_chroma), (10, 10));
        assert_eq!((truth.width, truth.height), (1920, 1088));
        // The fixture's bottom offset of 4 is in luma rows at 4:4:4.
        assert_eq!((truth.display_width, truth.display_height), (1920, 1084));
        assert_eq!(
            truth.colour,
            Some(HevcColourDescription {
                full_range: true,
                primaries: 1,
                transfer: 1,
                matrix: 1
            })
        );
        assert_eq!(
            truth.summary(),
            "rext 4:4:4 10-bit bt709/bt709/bt709 full 1920x1084 (coded 1920x1088)"
        );
    }

    #[test]
    fn an_untagged_main_stream_says_so() {
        let truth = parse_sps(&sps(&Fixture {
            profile_idc: 1,
            chroma: 1,
            depth: 8,
            colour: None,
            predicted_rps: true,
            geometry: None,
        }))
        .expect("parses");
        assert_eq!(truth.colour, None);
        assert_eq!(
            truth.summary(),
            "main 4:2:0 8-bit untagged 1920x1080 (coded 1920x1088)",
            "4:2:0 offsets count chroma rows, two luma rows each"
        );
    }

    #[test]
    fn hdr_tags_are_named() {
        let truth = parse_sps(&sps(&Fixture {
            profile_idc: 2,
            chroma: 1,
            depth: 10,
            colour: Some((false, 9, 16, 9)),
            predicted_rps: true,
            geometry: None,
        }))
        .expect("parses");
        assert_eq!(
            truth.summary(),
            "main10 4:2:0 10-bit bt2020/pq/bt2020ncl limited 1920x1080 (coded 1920x1088)"
        );
    }

    #[test]
    fn it_is_found_inside_an_annex_b_access_unit_and_refuses_what_is_not_an_sps() {
        let sps = sps(&Fixture {
            profile_idc: 2,
            chroma: 1,
            depth: 10,
            colour: None,
            predicted_rps: false,
            geometry: None,
        });
        let mut access_unit = vec![0, 0, 0, 1, 0x40, 1, 0x0C, 1, 0xFF];
        access_unit.extend_from_slice(&[0, 0, 0, 1]);
        access_unit.extend_from_slice(&sps);
        assert_eq!(
            stream_truth_in_access_unit(&access_unit).map(|truth| truth.bit_depth_luma),
            Ok(10)
        );
        assert_eq!(
            stream_truth_in_access_unit(&[0, 0, 0, 1, 0x02, 1, 0xAA]),
            Err(HevcSpsError::NotSps)
        );
        assert_eq!(parse_sps(&sps[..8]), Err(HevcSpsError::Truncated));
    }

    #[test]
    fn a_windows_pier_grading_stream_reads_as_its_visible_size() {
        // NVENC codes 1800x1168 as 1824x1184 and crops 24 columns and 16 rows
        // with the conformance window; at 4:4:4 the offsets are luma samples.
        let truth = parse_sps(&sps(&Fixture {
            profile_idc: 4,
            chroma: 3,
            depth: 10,
            colour: Some((true, 1, 1, 1)),
            predicted_rps: false,
            geometry: Some(((1824, 1184), Some([0, 24, 0, 16]))),
        }))
        .expect("parses");
        assert_eq!((truth.width, truth.height), (1824, 1184));
        assert_eq!((truth.display_width, truth.display_height), (1800, 1168));
        assert_eq!(
            truth.summary(),
            "rext 4:4:4 10-bit bt709/bt709/bt709 full 1800x1168 (coded 1824x1184)"
        );
    }

    #[test]
    fn an_uncropped_stream_reports_one_size_and_an_impossible_window_is_refused() {
        let exact = parse_sps(&sps(&Fixture {
            profile_idc: 1,
            chroma: 1,
            depth: 8,
            colour: None,
            predicted_rps: false,
            geometry: Some(((1280, 720), None)),
        }))
        .expect("parses");
        assert_eq!(exact.summary(), "main 4:2:0 8-bit untagged 1280x720");
        assert_eq!(
            parse_sps(&sps(&Fixture {
                profile_idc: 1,
                chroma: 1,
                depth: 8,
                colour: None,
                predicted_rps: false,
                geometry: Some(((64, 64), Some([0, 0, 0, 32]))),
            })),
            Err(HevcSpsError::Invalid("conformance_window"))
        );
    }
}
